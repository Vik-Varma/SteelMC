//! Points of interest read from saved chunks without loading them.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use steel_registry::REGISTRY;
use steel_utils::{BlockPos, ChunkPos, SectionPos};
use tokio::sync::oneshot;

use super::super::{
    SimpleRAMChunk,
    bit_pack::unpack_indices,
    format::{BLOCKS_PER_SECTION, PersistentChunk, PersistentSection},
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
    /// sections hold for generated chunks. Missing chunks yield nothing; unreadable ones
    /// yield their error.
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
    ) -> io::Result<Vec<(ChunkPos, io::Result<Vec<SavedPoi>>)>> {
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
        if saved.is_empty() {
            return Ok(Vec::new());
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
                by_chunk.push((chunk_pos, found));
            }
            let _ = sender.send(Ok(by_chunk));
        });
        receiver
            .await
            .map_err(|_| io::Error::other("saved POI scan ended without a result"))?
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
    /// Only sections whose palette holds a POI state are unpacked, and only what the scan reads
    /// is validated: the section count, each palette entry as it is resolved, and the packing
    /// of the unpacked sections. Loading validates the whole chunk, so a chunk damaged only
    /// elsewhere (other sections, biomes, block entities) is still scanned here although
    /// loading it would discard it.
    fn persistent_pois(
        persistent: &PersistentChunk<'_>,
        chunk_pos: ChunkPos,
        min_y: i32,
        height: i32,
    ) -> io::Result<Vec<SavedPoi>> {
        Self::validate_section_layout(persistent, min_y, height)?;

        let mut palette_types: Vec<Option<Option<usize>>> =
            vec![None; persistent.block_states.len()];
        let mut poi_type_at = |index: u16| -> io::Result<Option<usize>> {
            let slot = usize::from(index);
            if let Some(&Some(poi_type)) = palette_types.get(slot) {
                return Ok(poi_type);
            }
            // Fails for an index outside the chunk palette, so `slot` is in range below.
            let state = Self::resolve_block_state(persistent, index)?;
            let poi_type = REGISTRY.poi_types.type_id_for_state(state);
            palette_types[slot] = Some(poi_type);
            Ok(poi_type)
        };

        let mut pois = Vec::new();
        let min_section_y = SectionPos::block_to_section_coord(min_y);
        for (section_index, section) in persistent.sections.iter().enumerate() {
            let section_pos = SectionPos::new(
                chunk_pos.0.x,
                min_section_y + section_index as i32,
                chunk_pos.0.y,
            );
            match section {
                PersistentSection::Homogeneous { block_state, .. } => {
                    if let Some(poi_type) = poi_type_at(*block_state)? {
                        pois.extend(
                            (0..BLOCKS_PER_SECTION)
                                .map(|index| (section_pos.block_at_index(index), poi_type)),
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
                    Self::validate_packed_palette(
                        palette,
                        *bits_per_entry,
                        block_data,
                        BLOCKS_PER_SECTION,
                        persistent.block_states.len(),
                        "block-state",
                    )?;
                    let indices =
                        unpack_indices(block_data, *bits_per_entry).take(BLOCKS_PER_SECTION);
                    for (index, palette_index) in indices.enumerate() {
                        if let Some(poi_type) = section_types[palette_index as usize] {
                            pois.push((section_pos.block_at_index(index), poi_type));
                        }
                    }
                }
            }
        }
        Ok(pois)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs, io, process,
        sync::{Arc, atomic::AtomicBool},
        time::Duration,
    };

    use steel_registry::{RegistryEntry, vanilla_blocks, vanilla_poi_types};
    use steel_utils::{BlockPos, ChunkPos, types::UpdateFlags};
    use tokio::{
        runtime::Builder,
        time::{sleep, timeout},
    };

    use super::super::{ChunkStorage, PersistentSection, RegionManager};
    use crate::{
        behavior::init_behaviors,
        block_entity::init_block_entities,
        chunk::status::ChunkStatus,
        poi::PoiSquareColumns,
        test_support::{BlockedPool, fresh_test_world, insert_ready_full_chunk},
    };

    use crate::chunk_saver::SavedPoi;

    /// Unwraps each chunk's result; the tests here only save readable chunks.
    fn readable(
        by_chunk: Vec<(ChunkPos, io::Result<Vec<SavedPoi>>)>,
    ) -> Vec<(ChunkPos, Vec<SavedPoi>)> {
        by_chunk
            .into_iter()
            .map(|(chunk_pos, found)| {
                (
                    chunk_pos,
                    found.expect("the saved chunk should be readable"),
                )
            })
            .collect()
    }

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
        // A runtime of its own: while the test awaits, only the scan task can run on it, so
        // the scan reliably reaches the pool before the checks below.
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime should start");

        runtime.block_on(async {
            assert!(matches!(
                storage.save_chunk_data(prepare(), &pool).await,
                Ok(true)
            ));

            let blocked = BlockedPool::new(&pool);
            let scan = tokio::spawn({
                let storage = Arc::clone(&storage);
                let pool = Arc::clone(&pool);
                async move {
                    let cancelled = Arc::new(AtomicBool::new(false));
                    storage
                        .saved_chunk_pois(&[chunk_pos], min_y, height, &pool, &cancelled)
                        .await
                        .map(readable)
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

            // A cancelled scan stops before scanning anything.
            let _busy = BlockedPool::new(&pool);
            let cancelled = Arc::new(AtomicBool::new(true));
            let positions = [chunk_pos];
            let scan = storage.saved_chunk_pois(&positions, min_y, height, &pool, &cancelled);
            let result = timeout(Duration::from_secs(5), scan).await;
            assert!(
                matches!(result, Ok(Err(ref error)) if error.kind() == io::ErrorKind::Interrupted),
                "a cancelled scan must end without waiting for the pool"
            );
        });
    }

    #[test]
    fn poi_positions_survive_the_in_memory_scan_and_a_real_region_round_trip() {
        steel_registry::init_vanilla_registry();
        init_behaviors();
        init_block_entities();
        let world_fixture = fresh_test_world("poi-region-round-trip");
        let world = &world_fixture.world;
        // Negative block x and asymmetric local coordinates catch swapped or dropped axes; the
        // two heights land in the bottom section and a higher one.
        let chunk_pos = ChunkPos::new(-3, 2);
        let holder = insert_ready_full_chunk(world, chunk_pos);
        let low = BlockPos::new(-34, -50, 34);
        let high = BlockPos::new(-47, 70, 41);
        for pos in [low, high] {
            assert!(world.set_block(
                pos,
                vanilla_blocks::LODESTONE.default_state(),
                UpdateFlags::UPDATE_ALL,
            ));
        }
        let lodestone = vanilla_poi_types::LODESTONE
            .try_id()
            .expect("vanilla lodestone POI type should be registered");
        let expected = vec![(low, lodestone), (high, lodestone)];
        let (min_y, height) = (world.get_min_y(), world.get_height());
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("test pool should build");
        let not_cancelled = Arc::new(AtomicBool::new(false));
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime should start");

        let columns = PoiSquareColumns::capture(&world.chunk_map, high, 0);
        let in_memory = runtime
            .block_on(columns.pois(&world.chunk_map.storage, &pool, &not_cancelled))
            .expect("the in-memory scan should succeed");
        assert_eq!(in_memory, expected);

        let Some(chunk) = holder.try_chunk(ChunkStatus::Full) else {
            panic!("inserted chunk should be Full");
        };
        let Some(prepared) = ChunkStorage::prepare_chunk_save(chunk, ChunkStatus::Full, &[], true)
        else {
            panic!("a forced save should be prepared");
        };
        let directory =
            env::temp_dir().join(format!("steel-poi-region-round-trip-{}", process::id()));
        let disk = ChunkStorage::Disk(RegionManager::new(&directory));
        let read = runtime.block_on(async {
            assert!(matches!(
                disk.save_chunk_data(prepared, &pool).await,
                Ok(true)
            ));
            disk.saved_chunk_pois(&[chunk_pos], min_y, height, &pool, &not_cancelled)
                .await
                .map(readable)
        });
        let _ = fs::remove_dir_all(&directory);
        let Ok(read) = read else {
            panic!("the saved chunk should be read back");
        };
        assert_eq!(read, [(chunk_pos, expected)]);
    }

    #[test]
    fn damaged_packing_in_a_poi_section_rejects_the_chunk() {
        steel_registry::init_vanilla_registry();
        init_behaviors();
        init_block_entities();
        let world_fixture = fresh_test_world("poi-damaged-section");
        let world = &world_fixture.world;
        let chunk_pos = ChunkPos::new(1, 1);
        let holder = insert_ready_full_chunk(world, chunk_pos);
        assert!(world.set_block(
            BlockPos::new(20, 70, 20),
            vanilla_blocks::LODESTONE.default_state(),
            UpdateFlags::UPDATE_ALL,
        ));
        let Some(chunk) = holder.try_chunk(ChunkStatus::Full) else {
            panic!("inserted chunk should be Full");
        };
        let Some(mut prepared) =
            ChunkStorage::prepare_chunk_save(chunk, ChunkStatus::Full, &[], true)
        else {
            panic!("a forced save should be prepared");
        };
        let (min_y, height) = (world.get_min_y(), world.get_height());
        assert_eq!(
            ChunkStorage::persistent_pois(&prepared.persistent, chunk_pos, min_y, height)
                .map(|pois| pois.len())
                .ok(),
            Some(1)
        );

        // Truncate the packed blocks of the section holding the lodestone.
        let section_index = usize::try_from((70 - min_y) / 16).expect("section index");
        let PersistentSection::Heterogeneous { block_data, .. } =
            &mut prepared.persistent.sections[section_index]
        else {
            panic!("the lodestone's section should be heterogeneous");
        };
        *block_data = block_data[..block_data.len() / 2].into();
        assert!(matches!(
            ChunkStorage::persistent_pois(&prepared.persistent, chunk_pos, min_y, height),
            Err(error) if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn persistent_pois_validates_only_what_it_reads() {
        steel_registry::init_vanilla_registry();
        init_behaviors();
        init_block_entities();
        let world_fixture = fresh_test_world("poi-scan-validation");
        let world = &world_fixture.world;
        let chunk_pos = ChunkPos::new(1, 1);
        let holder = insert_ready_full_chunk(world, chunk_pos);
        assert!(world.set_block(
            BlockPos::new(20, 70, 20),
            vanilla_blocks::LODESTONE.default_state(),
            UpdateFlags::UPDATE_ALL,
        ));
        assert!(world.set_block(
            BlockPos::new(20, 0, 20),
            vanilla_blocks::STONE.default_state(),
            UpdateFlags::UPDATE_ALL,
        ));
        let Some(chunk) = holder.try_chunk(ChunkStatus::Full) else {
            panic!("inserted chunk should be Full");
        };
        let saved = || {
            let Some(prepared) =
                ChunkStorage::prepare_chunk_save(chunk, ChunkStatus::Full, &[], true)
            else {
                panic!("a forced save should be prepared");
            };
            prepared.persistent
        };
        let (min_y, height) = (world.get_min_y(), world.get_height());
        let section_of = |y: i32| usize::try_from((y - min_y) / 16).expect("section index");
        let scan = |persistent: &super::super::PersistentChunk<'_>, min_y, height| {
            ChunkStorage::persistent_pois(persistent, chunk_pos, min_y, height)
        };

        // Damage outside POI sections is not read, so the chunk is still scanned.
        let mut damaged = saved();
        let PersistentSection::Heterogeneous { block_data, .. } =
            &mut damaged.sections[section_of(0)]
        else {
            panic!("the stone's section should be heterogeneous");
        };
        *block_data = Box::default();
        assert_eq!(
            scan(&damaged, min_y, height).map(|pois| pois.len()).ok(),
            Some(1)
        );

        // A POI section with an impossible bit width is rejected, not unpacked.
        let mut damaged = saved();
        let PersistentSection::Heterogeneous { bits_per_entry, .. } =
            &mut damaged.sections[section_of(70)]
        else {
            panic!("the lodestone's section should be heterogeneous");
        };
        *bits_per_entry = 0;
        assert!(matches!(
            scan(&damaged, min_y, height),
            Err(error) if error.kind() == io::ErrorKind::InvalidData
        ));

        // A section made entirely of a POI block reports every block.
        let mut solid = saved();
        let lodestone_state = u16::try_from(
            solid
                .block_states
                .iter()
                .position(|state| state.name == vanilla_blocks::LODESTONE.key)
                .expect("the chunk palette should hold the lodestone"),
        )
        .expect("palette index fits u16");
        let PersistentSection::Homogeneous { block_state, .. } =
            &mut solid.sections[section_of(200)]
        else {
            panic!("an empty section should be homogeneous");
        };
        *block_state = lodestone_state;
        assert_eq!(
            scan(&solid, min_y, height).map(|pois| pois.len()).ok(),
            Some(1 + 4096)
        );

        // A world range that is not section-aligned is refused.
        assert!(matches!(
            scan(&saved(), min_y + 8, height),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput
        ));
    }
}
