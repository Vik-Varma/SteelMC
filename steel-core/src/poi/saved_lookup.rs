//! POI lookups at single positions in chunks that are not loaded as Full.
//!
//! Vanilla's `PoiManager` reads a chunk's POI data from disk the first time a query needs it,
//! so a query such as a lodestone compass checking its target works wherever the target is.
//! [`PointOfInterestStorage`](super::PointOfInterestStorage) only covers loaded Full chunks,
//! so these lookups read the chunk in the background instead and report once they finish.

use std::{
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use rustc_hash::FxHashMap;
use steel_utils::{BlockPos, ChunkPos, locks::SyncMutex};

use super::PoiSquareColumns;
use crate::world::World;

/// How long a finished lookup's answer is reused before the chunk is read again.
const ANSWER_REUSED_FOR: Duration = Duration::from_secs(5);
/// How long a lookup may stay unfinished before another is started in its place.
const LOOKUP_GIVEN_UP_AFTER: Duration = Duration::from_secs(30);
/// Lookups this old are dropped once there are more than [`KEPT_LOOKUPS`].
const LOOKUP_DROPPED_AFTER: Duration = Duration::from_secs(60);
const KEPT_LOOKUPS: usize = 256;

/// What is known about the POI at a position in a chunk not loaded as Full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnloadedPoi {
    /// The chunk is still being read.
    Unknown,
    /// The POI type there, if any.
    Known(Option<usize>),
}

#[derive(Clone, Copy)]
enum Lookup {
    Running {
        started: Instant,
        /// The previous answer, still given while this lookup runs.
        previous: UnloadedPoi,
    },
    Finished {
        poi_type: Option<usize>,
        at: Instant,
    },
}

/// Per-world record of POI lookups in chunks not loaded as Full.
#[derive(Default)]
pub(crate) struct SavedPoiLookups {
    lookups: Arc<SyncMutex<FxHashMap<BlockPos, Lookup>>>,
}

impl World {
    /// The POI type at `pos`, which must be in a chunk not loaded as Full, as vanilla's
    /// `PoiManager` would load it: from the chunk's blocks if it is in memory at a lower
    /// status, otherwise from its save. A chunk that was never saved has no POIs, and one
    /// that cannot be read is treated as having none, as vanilla treats unreadable POI data.
    ///
    /// Returns [`UnloadedPoi::Unknown`] while the chunk is being read, and starts that read.
    /// An answer is reused for a few seconds; after that the next call reads the chunk again,
    /// giving the old answer until the new one is ready.
    pub(crate) fn unloaded_poi_type_at(&self, pos: BlockPos) -> UnloadedPoi {
        let now = Instant::now();
        let mut lookups = self.saved_poi_lookups.lookups.lock();
        let previous = match lookups.get(&pos) {
            Some(&Lookup::Finished { poi_type, at }) => {
                if now.duration_since(at) < ANSWER_REUSED_FOR {
                    return UnloadedPoi::Known(poi_type);
                }
                UnloadedPoi::Known(poi_type)
            }
            Some(&Lookup::Running { started, previous }) => {
                if now.duration_since(started) < LOOKUP_GIVEN_UP_AFTER {
                    return previous;
                }
                previous
            }
            None => UnloadedPoi::Unknown,
        };
        if lookups.len() >= KEPT_LOOKUPS {
            lookups.retain(|_, lookup| {
                let at = match *lookup {
                    Lookup::Running { started, .. } => started,
                    Lookup::Finished { at, .. } => at,
                };
                now.duration_since(at) < LOOKUP_DROPPED_AFTER
            });
        }
        lookups.insert(
            pos,
            Lookup::Running {
                started: now,
                previous,
            },
        );
        drop(lookups);

        let chunk_map = Arc::clone(&self.chunk_map);
        let lookups = Arc::clone(&self.saved_poi_lookups.lookups);
        let columns = PoiSquareColumns::single(&chunk_map, ChunkPos::from_block_pos(pos));
        self.chunk_map.chunk_runtime.spawn(async move {
            let found = columns
                .pois(
                    &chunk_map.storage,
                    &chunk_map.generation_pool,
                    &Arc::new(AtomicBool::new(false)),
                )
                .await;
            let poi_type = match found {
                Ok(found) => found
                    .into_iter()
                    .find(|&(poi_pos, _)| poi_pos == pos)
                    .map(|(_, poi_type)| poi_type),
                Err(error) => {
                    tracing::warn!("Couldn't read the POIs of the chunk holding {pos:?}: {error}");
                    None
                }
            };
            lookups.lock().insert(
                pos,
                Lookup::Finished {
                    poi_type,
                    at: Instant::now(),
                },
            );
        });
        previous
    }
}
