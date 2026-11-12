//! Points of interest read from saved chunks without loading them.

use std::io;

use steel_registry::REGISTRY;
use steel_utils::{BlockPos, ChunkPos};
use tokio::sync::oneshot;

use super::super::{
    bit_pack::unpack_indices,
    format::{PersistentChunk, PersistentSection},
};
use super::ChunkStorage;

/// A POI block found in a saved chunk: its position and POI type id.
pub type SavedPoi = (BlockPos, usize);

impl ChunkStorage {
    /// Returns the POI blocks of saved chunks, by chunk, without loading the chunks.
    ///
    /// Types come from the saved block states, so this sees the same POIs vanilla's POI
    /// sections hold for generated chunks. Missing chunks yield nothing; unreadable ones are
    /// logged and skipped, as vanilla does with a POI section it cannot read.
    pub async fn saved_chunk_pois(
        &self,
        positions: &[ChunkPos],
        min_y: i32,
        height: i32,
        thread_pool: &rayon::ThreadPool,
    ) -> io::Result<Vec<(ChunkPos, Vec<SavedPoi>)>> {
        match self {
            Self::Disk(regions) => {
                let payloads = regions.read_saved_chunks(positions).await;
                // Decompressing and decoding is CPU work; keep it off the async runtime.
                let (sender, receiver) = oneshot::channel();
                thread_pool.spawn(move || {
                    let by_chunk = payloads
                        .into_iter()
                        .filter_map(|(chunk_pos, payload)| {
                            let found = payload.and_then(|compressed| {
                                Self::decode_saved_pois(&compressed, chunk_pos, min_y, height)
                            });
                            Self::readable_pois(chunk_pos, found)
                        })
                        .collect();
                    let _ = sender.send(by_chunk);
                });
                receiver
                    .await
                    .map_err(|_| io::Error::other("saved POI scan ended without a result"))
            }
            Self::RamOnly(ram) => Ok(ram.saved_chunk_pois(positions, min_y, height).await),
        }
    }

    pub(crate) fn readable_pois(
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
    pub(crate) fn persistent_pois(
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
