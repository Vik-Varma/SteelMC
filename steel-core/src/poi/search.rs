//! POI range queries over every generated chunk, loaded or not.
//!
//! Vanilla's `PoiManager` keeps the POI sections of every chunk it has seen and reads the
//! rest from disk on demand, so its range queries also see unloaded chunks.
//! [`PointOfInterestStorage`](super::PointOfInterestStorage) only covers loaded Full chunks,
//! so these queries scan the blocks of in-memory chunks and read saved chunks from storage.

use std::{io, sync::Arc};

use rayon::ThreadPool;
use rustc_hash::FxHashMap;
use steel_utils::{BlockPos, ChunkPos, SectionPos};
use tokio::sync::oneshot;

use crate::{
    chunk::{Chunk, chunk_holder::ChunkHolder, chunk_map::ChunkMap, status::ChunkStatus},
    chunk_saver::{ChunkStorage, SavedPoi},
};

/// The chunks a vanilla POI square query visits, captured with any in-memory holder.
pub struct PoiSquareColumns {
    columns: Vec<(ChunkPos, Option<Arc<ChunkHolder>>)>,
    min_y: i32,
    height: i32,
}

impl PoiSquareColumns {
    /// Captures the chunks `PoiManager.getInSquare` visits around `center`, in its order
    /// (x fastest, then z).
    #[must_use]
    pub fn capture(chunk_map: &ChunkMap, center: BlockPos, radius: i32) -> Self {
        let center_chunk = ChunkPos::from_block_pos(center);
        let chunk_radius = radius.div_euclid(16) + 1;
        let side = (2 * chunk_radius + 1) as usize;
        let mut columns = Vec::with_capacity(side * side);
        for z in center_chunk.0.y - chunk_radius..=center_chunk.0.y + chunk_radius {
            for x in center_chunk.0.x - chunk_radius..=center_chunk.0.x + chunk_radius {
                let pos = ChunkPos::new(x, z);
                let holder = chunk_map
                    .chunks
                    .read_sync(&pos, |_, holder| Arc::clone(holder))
                    .or_else(|| {
                        chunk_map
                            .unloading_chunks
                            .read_sync(&pos, |_, holder| Arc::clone(holder))
                    });
                columns.push((pos, holder));
            }
        }
        Self {
            columns,
            min_y: chunk_map.world_gen_context.min_y(),
            height: chunk_map.world_gen_context.height(),
        }
    }

    /// Lists every POI in the captured chunks: chunks in capture order, then sections from
    /// the bottom, then y, z, x within a section.
    ///
    /// Chunks with in-memory data are scanned on `thread_pool`; the rest are read from
    /// `storage`, and chunks that were never saved contribute nothing.
    pub async fn pois(
        self,
        storage: &ChunkStorage,
        thread_pool: &ThreadPool,
    ) -> io::Result<Vec<SavedPoi>> {
        let (min_y, height) = (self.min_y, self.height);
        let (sender, receiver) = oneshot::channel();
        thread_pool.spawn(move || {
            let scanned: Vec<_> = self
                .columns
                .into_iter()
                .map(|(chunk_pos, holder)| {
                    let in_memory = holder
                        .as_deref()
                        .and_then(|holder| holder.try_chunk(ChunkStatus::Empty))
                        .map(|chunk| in_memory_pois(chunk, chunk_pos));
                    (chunk_pos, in_memory)
                })
                .collect();
            let _ = sender.send(scanned);
        });
        let scanned = receiver
            .await
            .map_err(|_| io::Error::other("in-memory POI scan ended without a result"))?;

        let unloaded: Vec<ChunkPos> = scanned
            .iter()
            .filter(|(_, in_memory)| in_memory.is_none())
            .map(|&(pos, _)| pos)
            .collect();
        let mut saved: FxHashMap<ChunkPos, Vec<SavedPoi>> = storage
            .saved_chunk_pois(&unloaded, min_y, height, thread_pool)
            .await?
            .into_iter()
            .collect();

        let mut found = Vec::new();
        for (chunk_pos, in_memory) in scanned {
            found.extend(
                in_memory
                    .or_else(|| saved.remove(&chunk_pos))
                    .unwrap_or_default(),
            );
        }
        Ok(found)
    }
}

fn in_memory_pois(chunk: &Chunk, chunk_pos: ChunkPos) -> Vec<SavedPoi> {
    let min_section_y = chunk.min_y() >> 4;
    let mut found = Vec::new();
    for (index, section) in chunk.sections().sections.iter().enumerate() {
        let section = section.read();
        if section.contains_poi() {
            let section_pos =
                SectionPos::new(chunk_pos.0.x, min_section_y + index as i32, chunk_pos.0.y);
            found.extend(section.pois(section_pos));
        }
    }
    found
}

/// `PoiManager.findClosestWithType` with `Occupancy.ANY`: the closest POI whose type
/// `matches`, within `radius` horizontally and as a sphere measured to block corners.
///
/// Vanilla breaks ties by its stream order, part of which is Java hash-map order that is not
/// reproducible; equally distant POIs resolve to the first in `pois` order instead.
pub fn find_closest_with_type(
    pois: impl IntoIterator<Item = SavedPoi>,
    matches: impl Fn(usize) -> bool,
    center: BlockPos,
    radius: i32,
) -> Option<SavedPoi> {
    let radius_sqr = f64::from(radius * radius);
    let mut closest: Option<(f64, SavedPoi)> = None;
    for (pos, poi_type) in pois {
        if !matches(poi_type)
            || (pos.x() - center.x()).abs() > radius
            || (pos.z() - center.z()).abs() > radius
        {
            continue;
        }
        let dx = f64::from(pos.x()) - f64::from(center.x());
        let dy = f64::from(pos.y()) - f64::from(center.y());
        let dz = f64::from(pos.z()) - f64::from(center.z());
        let distance_sqr = dx * dx + dy * dy + dz * dz;
        if distance_sqr <= radius_sqr
            && closest.is_none_or(|(closest_sqr, _)| distance_sqr < closest_sqr)
        {
            closest = Some((distance_sqr, (pos, poi_type)));
        }
    }
    closest.map(|(_, poi)| poi)
}

#[cfg(test)]
mod tests {
    use super::find_closest_with_type;
    use steel_utils::BlockPos;

    const HOME: usize = 1;
    const MEETING: usize = 2;

    #[test]
    fn closest_is_limited_to_vanillas_sphere_and_filtered_by_type() {
        let center = BlockPos::new(0, 64, 0);
        let pois = [
            // Inside the 256-block square but outside the sphere.
            (BlockPos::new(200, 64, 200), HOME),
            // Exactly on the sphere.
            (BlockPos::new(256, 64, 0), HOME),
            (BlockPos::new(257, 64, 0), HOME),
            (BlockPos::new(5, 64, 5), MEETING),
        ];

        assert_eq!(
            find_closest_with_type(pois, |poi_type| poi_type == HOME, center, 256),
            Some((BlockPos::new(256, 64, 0), HOME))
        );
        assert_eq!(
            find_closest_with_type(pois[..1].iter().copied(), |_| true, center, 256),
            None
        );
    }

    #[test]
    fn equally_distant_pois_resolve_to_the_first_listed() {
        let center = BlockPos::new(0, 64, 0);
        let pois = [
            (BlockPos::new(10, 64, 0), HOME),
            (BlockPos::new(-10, 64, 0), MEETING),
            (BlockPos::new(0, 64, 10), HOME),
        ];

        assert_eq!(
            find_closest_with_type(pois, |_| true, center, 256),
            Some((BlockPos::new(10, 64, 0), HOME))
        );
        assert_eq!(
            find_closest_with_type(pois[1..].iter().copied(), |_| true, center, 256),
            Some((BlockPos::new(-10, 64, 0), MEETING))
        );
    }
}
