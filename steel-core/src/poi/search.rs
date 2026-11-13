//! POI range queries over every generated chunk, loaded or not.
//!
//! Vanilla's `PoiManager` keeps the POI sections of every chunk it has seen and reads the
//! rest from disk on demand, so its range queries also see unloaded chunks.
//! [`PointOfInterestStorage`](super::PointOfInterestStorage) only covers loaded Full chunks,
//! so these queries scan the blocks of in-memory chunks and read saved chunks from storage.

use std::{
    io,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use rayon::ThreadPool;
use rustc_hash::FxHashMap;
use steel_utils::{BlockPos, ChunkPos, SectionPos};
use tokio::sync::oneshot;

use crate::{
    chunk::{Chunk, chunk_holder::ChunkHolder, chunk_map::ChunkMap, status::ChunkStatus},
    chunk_saver::{ChunkStorage, SavedPoi, poi_search_cancelled, readable_pois},
};

/// The chunks a vanilla POI square query visits, captured with any in-memory holder.
///
/// Holders are kept as weak references so a pending search never holds up unloading. At scan
/// time the chunk's current holder wins, so a chunk unloaded and loaded again in the meantime
/// is read live. Otherwise the captured holder is used while it is alive (it may be between
/// the active and unloading maps, or finalized, whose blocks match its save), and only
/// without either is storage read; holders are finalized only after saving.
///
/// The scan looks the current holder up off the server thread, and a holder moves between the
/// active and unloading maps in two steps. A holder caught mid-move is therefore missed; if it
/// was not captured either, its chunk is read from storage, which lacks changes not yet saved.
pub struct PoiSquareColumns {
    columns: Vec<(ChunkPos, Option<Weak<ChunkHolder>>)>,
    chunk_map: Weak<ChunkMap>,
    min_y: i32,
    height: i32,
}

impl PoiSquareColumns {
    /// Captures the chunks `PoiManager.getInSquare` visits around `center`, in its order
    /// (x fastest, then z).
    #[must_use]
    pub fn capture(chunk_map: &Arc<ChunkMap>, center: BlockPos, radius: i32) -> Self {
        let center_chunk = ChunkPos::from_block_pos(center);
        let chunk_radius = radius.div_euclid(16) + 1;
        let side = (2 * chunk_radius + 1) as usize;
        let mut columns = Vec::with_capacity(side * side);
        for z in center_chunk.0.y - chunk_radius..=center_chunk.0.y + chunk_radius {
            for x in center_chunk.0.x - chunk_radius..=center_chunk.0.x + chunk_radius {
                let pos = ChunkPos::new(x, z);
                let holder = current_holder(chunk_map, pos).map(|holder| Arc::downgrade(&holder));
                columns.push((pos, holder));
            }
        }
        Self {
            columns,
            chunk_map: Arc::downgrade(chunk_map),
            min_y: chunk_map.world_gen_context.min_y(),
            height: chunk_map.world_gen_context.height(),
        }
    }

    /// Captures the one chunk at `chunk_pos`.
    #[must_use]
    pub fn single(chunk_map: &Arc<ChunkMap>, chunk_pos: ChunkPos) -> Self {
        let holder = current_holder(chunk_map, chunk_pos).map(|holder| Arc::downgrade(&holder));
        Self {
            columns: vec![(chunk_pos, holder)],
            chunk_map: Arc::downgrade(chunk_map),
            min_y: chunk_map.world_gen_context.min_y(),
            height: chunk_map.world_gen_context.height(),
        }
    }

    /// Lists every POI in the captured chunks: chunks in capture order, then sections from
    /// the bottom, then y, z, x within a section.
    ///
    /// Chunks with in-memory data are scanned on `thread_pool`; the rest are read from
    /// `storage`, and chunks that were never saved contribute nothing. Once `cancelled` is
    /// set, the work stops at the next chunk and the call fails with
    /// [`poi_search_cancelled`].
    pub async fn pois(
        self,
        storage: &ChunkStorage,
        thread_pool: &ThreadPool,
        cancelled: &Arc<AtomicBool>,
    ) -> io::Result<Vec<SavedPoi>> {
        Ok(self
            .pois_by_chunk(storage, thread_pool, cancelled)
            .await?
            .into_iter()
            .filter_map(|(chunk_pos, found)| readable_pois(chunk_pos, found))
            .flat_map(|(_, found)| found)
            .collect())
    }

    /// [`Self::pois`] per captured chunk, in capture order, with a saved chunk that cannot be
    /// read reported as its error rather than skipped.
    pub async fn pois_by_chunk(
        self,
        storage: &ChunkStorage,
        thread_pool: &ThreadPool,
        cancelled: &Arc<AtomicBool>,
    ) -> io::Result<Vec<(ChunkPos, io::Result<Vec<SavedPoi>>)>> {
        let (min_y, height) = (self.min_y, self.height);
        let (sender, receiver) = oneshot::channel();
        let scan_cancelled = Arc::clone(cancelled);
        thread_pool.spawn(move || {
            let chunk_map = self.chunk_map.upgrade();
            let mut scanned = Vec::with_capacity(self.columns.len());
            for (chunk_pos, holder) in self.columns {
                if scan_cancelled.load(Ordering::Relaxed) {
                    let _ = sender.send(Err(poi_search_cancelled()));
                    return;
                }
                let holder = chunk_map
                    .as_deref()
                    .and_then(|chunk_map| current_holder(chunk_map, chunk_pos))
                    .or_else(|| holder.and_then(|holder| holder.upgrade()));
                let in_memory = holder.and_then(|holder| {
                    let chunk = holder.try_chunk(ChunkStatus::Empty)?;
                    Some(in_memory_pois(chunk, chunk_pos))
                });
                scanned.push((chunk_pos, in_memory));
            }
            let _ = sender.send(Ok(scanned));
        });
        let scanned = receiver
            .await
            .map_err(|_| io::Error::other("in-memory POI scan ended without a result"))??;

        let unloaded: Vec<ChunkPos> = scanned
            .iter()
            .filter(|(_, in_memory)| in_memory.is_none())
            .map(|&(pos, _)| pos)
            .collect();
        let mut saved: FxHashMap<ChunkPos, io::Result<Vec<SavedPoi>>> = storage
            .saved_chunk_poi_results(&unloaded, min_y, height, thread_pool, cancelled)
            .await?
            .into_iter()
            .collect();

        Ok(scanned
            .into_iter()
            .map(|(chunk_pos, in_memory)| {
                let found = in_memory
                    .map(Ok)
                    .or_else(|| saved.remove(&chunk_pos))
                    .unwrap_or_else(|| Ok(Vec::new()));
                (chunk_pos, found)
            })
            .collect())
    }
}

/// The holder at `pos`, active or pending unload.
fn current_holder(chunk_map: &ChunkMap, pos: ChunkPos) -> Option<Arc<ChunkHolder>> {
    chunk_map
        .chunks
        .read_sync(&pos, |_, holder| Arc::clone(holder))
        .or_else(|| {
            chunk_map
                .unloading_chunks
                .read_sync(&pos, |_, holder| Arc::clone(holder))
        })
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
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use super::{PoiSquareColumns, find_closest_with_type};
    use crate::test_support::{block_pool, fresh_test_world, insert_ready_full_chunk};
    use steel_utils::{BlockPos, ChunkPos};

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

    #[test]
    fn cancelling_a_queued_search_ends_it_as_interrupted() {
        steel_registry::init_vanilla_registry();
        let world_fixture = fresh_test_world("poi-search-cancel");
        let world = &world_fixture.world;
        insert_ready_full_chunk(world, ChunkPos::new(0, 0));
        let columns = PoiSquareColumns::capture(&world.chunk_map, BlockPos::new(0, 64, 0), 16);
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("test pool should build"),
        );
        let cancelled = Arc::new(AtomicBool::new(false));

        let blocked = block_pool(&pool);
        let search = world.chunk_map.chunk_runtime.spawn({
            let storage = Arc::clone(&world.chunk_map.storage);
            let pool = Arc::clone(&pool);
            let cancelled = Arc::clone(&cancelled);
            async move { columns.pois(&storage, &pool, &cancelled).await }
        });
        cancelled.store(true, Ordering::Relaxed);
        drop(blocked);

        let Ok(result) = world.chunk_map.chunk_runtime.block_on(search) else {
            panic!("the search task should finish");
        };
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::Interrupted));
    }
}
