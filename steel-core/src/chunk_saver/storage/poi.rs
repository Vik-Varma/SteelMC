//! Points of interest read from saved chunks without loading them.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use steel_registry::REGISTRY;
use steel_utils::{BlockPos, ChunkPos};
use tokio::sync::oneshot;

use super::super::{
    SimpleRAMChunk,
    bit_pack::unpack_indices,
    format::{PersistentChunk, PersistentSection},
};
use super::ChunkStorage;

/// A POI block found in a saved chunk: its position and POI type id.
pub type SavedPoi = (BlockPos, usize);

/// The error a cancelled POI search ends with.
#[must_use]
pub fn poi_search_cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "POI search was cancelled")
}

/// A saved chunk as storage hands it out, before decoding.
enum SavedChunk {
    Compressed(io::Result<Vec<u8>>),
    InMemory(Arc<SimpleRAMChunk>),
}

impl ChunkStorage {
    /// Returns the POI blocks of saved chunks, by chunk, without loading the chunks.
    ///
    /// Types come from the saved block states, so this sees the same POIs vanilla's POI
    /// sections hold for generated chunks. Missing chunks yield nothing; unreadable ones are
    /// logged and skipped, as vanilla does with a POI section it cannot read.
    ///
    /// Decoding and scanning run on `thread_pool`, outside any storage lock. Once `cancelled`
    /// is set, the work stops at the next chunk and the call fails with
    /// [`poi_search_cancelled`].
    pub async fn saved_chunk_pois(
        &self,
        positions: &[ChunkPos],
        min_y: i32,
        height: i32,
        thread_pool: &rayon::ThreadPool,
        cancelled: &Arc<AtomicBool>,
    ) -> io::Result<Vec<(ChunkPos, Vec<SavedPoi>)>> {
        let saved: Vec<(ChunkPos, SavedChunk)> = match self {
            Self::Disk(regions) => regions
                .read_saved_chunks(positions, cancelled)
                .await
                .into_iter()
                .map(|(pos, payload)| (pos, SavedChunk::Compressed(payload)))
                .collect(),
            Self::RamOnly(ram) => ram
                .saved_chunks(positions)
                .await
                .into_iter()
                .map(|(pos, chunk)| (pos, SavedChunk::InMemory(chunk)))
                .collect(),
        };
        if cancelled.load(Ordering::Relaxed) {
            return Err(poi_search_cancelled());
        }

        let (sender, receiver) = oneshot::channel();
        let cancelled = Arc::clone(cancelled);
        thread_pool.spawn(move || {
            let mut by_chunk = Vec::with_capacity(saved.len());
            for (chunk_pos, saved_chunk) in saved {
                if cancelled.load(Ordering::Relaxed) {
                    let _ = sender.send(Err(poi_search_cancelled()));
                    return;
                }
                let found = match saved_chunk {
                    SavedChunk::Compressed(payload) => payload.and_then(|compressed| {
                        Self::decode_saved_pois(&compressed, chunk_pos, min_y, height)
                    }),
                    SavedChunk::InMemory(chunk) => {
                        Self::persistent_pois(&chunk.prepared.persistent, chunk_pos, min_y, height)
                    }
                };
                by_chunk.extend(Self::readable_pois(chunk_pos, found));
            }
            let _ = sender.send(Ok(by_chunk));
        });
        receiver
            .await
            .map_err(|_| io::Error::other("saved POI scan ended without a result"))?
    }

    fn readable_pois(
        chunk_pos: ChunkPos,
        found: io::Result<Vec<SavedPoi>>,
    ) -> Option<(ChunkPos, Vec<SavedPoi>)> {
        match found {
            Ok(found) => Some((chunk_pos, found)),
            Err(error) => {
                tracing::warn!(chunk = ?chunk_pos, "Skipping unreadable saved chunk while reading POIs: {error}");
                None
            }
        }
    }

    fn decode_saved_pois(
        compressed: &[u8],
        chunk_pos: ChunkPos,
        min_y: i32,
        height: i32,
    ) -> io::Result<Vec<SavedPoi>> {
        let data = zstd::decode_all(compressed).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("zstd decode failed: {error}"),
            )
        })?;
        let persistent: PersistentChunk<'_> = wincode::deserialize(&data).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("chunk decode failed: {error}"),
            )
        })?;
        Self::persistent_pois(&persistent, chunk_pos, min_y, height)
    }

    /// Lists a saved chunk's POI blocks: sections from the bottom, then y, z, x within each.
    ///
    /// Only sections whose palette holds a POI state are unpacked.
    fn persistent_pois(
        persistent: &PersistentChunk<'_>,
        chunk_pos: ChunkPos,
        min_y: i32,
        height: i32,
    ) -> io::Result<Vec<SavedPoi>> {
        Self::validate_persistent_chunk(persistent, min_y, height)?;

        let mut palette_types: Vec<Option<Option<usize>>> =
            vec![None; persistent.block_states.len()];
        let mut poi_type_at = |index: u16| -> io::Result<Option<usize>> {
            if let Some(poi_type) = palette_types[usize::from(index)] {
                return Ok(poi_type);
            }
            let state = Self::resolve_block_state(persistent, index)?;
            let poi_type = REGISTRY.poi_types.type_id_for_state(state);
            palette_types[usize::from(index)] = Some(poi_type);
            Ok(poi_type)
        };

        let mut pois = Vec::new();
        for (section_index, section) in persistent.sections.iter().enumerate() {
            let origin = BlockPos::new(
                chunk_pos.0.x * 16,
                min_y + section_index as i32 * 16,
                chunk_pos.0.y * 16,
            );
            match section {
                PersistentSection::Homogeneous { block_state, .. } => {
                    if let Some(poi_type) = poi_type_at(*block_state)? {
                        pois.extend(
                            (0..4096).map(|index| (section_offset(origin, index), poi_type)),
                        );
                    }
                }
                PersistentSection::Heterogeneous {
                    palette,
                    bits_per_entry,
                    block_data,
                    ..
                } => {
                    let section_types = palette
                        .iter()
                        .map(|&index| poi_type_at(index))
                        .collect::<io::Result<Vec<_>>>()?;
                    if section_types.iter().all(Option::is_none) {
                        continue;
                    }
                    let indices = unpack_indices(block_data, *bits_per_entry).take(4096);
                    for (index, palette_index) in indices.enumerate() {
                        if let Some(poi_type) = section_types[palette_index as usize] {
                            pois.push((section_offset(origin, index), poi_type));
                        }
                    }
                }
            }
        }
        Ok(pois)
    }
}

/// The block at `index` of a section stored in y, z, x order.
const fn section_offset(origin: BlockPos, index: usize) -> BlockPos {
    let index = index as i32;
    origin.offset(index & 15, index >> 8, (index >> 4) & 15)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, atomic::AtomicBool},
        time::Duration,
    };

    use steel_registry::{RegistryEntry, vanilla_blocks, vanilla_poi_types};
    use steel_utils::{BlockPos, ChunkPos, types::UpdateFlags};
    use tokio::time::{sleep, timeout};

    use super::super::ChunkStorage;
    use crate::{
        behavior::init_behaviors,
        block_entity::init_block_entities,
        chunk::status::ChunkStatus,
        test_support::{block_pool, fresh_test_world, insert_ready_full_chunk},
    };

    #[test]
    fn ram_scan_runs_on_the_pool_without_holding_the_storage_lock() {
        steel_registry::init_vanilla_registry();
        init_behaviors();
        init_block_entities();
        let world_fixture = fresh_test_world("ram-poi-scan");
        let world = &world_fixture.world;
        let chunk_pos = ChunkPos::new(2, 1);
        let holder = insert_ready_full_chunk(world, chunk_pos);
        let lodestone_pos = BlockPos::new(33, 70, 25);
        assert!(world.set_block(
            lodestone_pos,
            vanilla_blocks::LODESTONE.default_state(),
            UpdateFlags::UPDATE_ALL,
        ));
        let Some(chunk) = holder.try_chunk(ChunkStatus::Full) else {
            panic!("inserted chunk should be Full");
        };
        let prepare = || {
            let Some(prepared) =
                ChunkStorage::prepare_chunk_save(chunk, ChunkStatus::Full, &[], true)
            else {
                panic!("a forced save should be prepared");
            };
            prepared
        };
        let storage = Arc::clone(&world.chunk_map.storage);
        assert!(matches!(*storage, ChunkStorage::RamOnly(_)));
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("test pool should build"),
        );
        let (min_y, height) = (world.get_min_y(), world.get_height());

        world.chunk_map.chunk_runtime.block_on(async {
            assert!(matches!(
                storage.save_chunk_data(prepare(), &pool).await,
                Ok(true)
            ));

            let blocked = block_pool(&pool);
            let scan = tokio::spawn({
                let storage = Arc::clone(&storage);
                let pool = Arc::clone(&pool);
                async move {
                    let cancelled = Arc::new(AtomicBool::new(false));
                    storage
                        .saved_chunk_pois(&[chunk_pos], min_y, height, &pool, &cancelled)
                        .await
                }
            });

            // The scan can only finish on the blocked pool...
            sleep(Duration::from_millis(100)).await;
            assert!(!scan.is_finished(), "the RAM scan must run on the pool");
            // ...and storage stays usable while it waits.
            let save = timeout(
                Duration::from_secs(5),
                storage.save_chunk_data(prepare(), &pool),
            )
            .await;
            assert!(
                matches!(save, Ok(Ok(true))),
                "the scan must not hold the storage lock"
            );

            drop(blocked);
            let Ok(Ok(found)) = scan.await else {
                panic!("the scan should finish once the pool is free");
            };
            let lodestone = vanilla_poi_types::LODESTONE
                .try_id()
                .expect("vanilla lodestone POI type should be registered");
            assert_eq!(found, [(chunk_pos, vec![(lodestone_pos, lodestone)])]);
        });
    }
}
