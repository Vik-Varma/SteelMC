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
/// Lookups that no longer answer are dropped once this many are kept.
const KEPT_LOOKUPS: usize = 256;

/// What is known about the POI at a position in a chunk not loaded as Full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnloadedPoi {
    /// The chunk is being read, or could not be read.
    Unknown,
    /// The POI type there, if any.
    Known(Option<usize>),
}

#[derive(Clone, Copy)]
enum Lookup {
    Running {
        started: Instant,
    },
    Finished {
        /// [`UnloadedPoi::Unknown`] when the chunk could not be read.
        answer: UnloadedPoi,
        at: Instant,
    },
}

impl Lookup {
    /// The answer to give at `now`, or `None` once the chunk should be read again.
    fn answer(self, now: Instant) -> Option<UnloadedPoi> {
        match self {
            Self::Running { started } => (now.duration_since(started) < LOOKUP_GIVEN_UP_AFTER)
                .then_some(UnloadedPoi::Unknown),
            Self::Finished { answer, at } => {
                (now.duration_since(at) < ANSWER_REUSED_FOR).then_some(answer)
            }
        }
    }
}

/// Per-world record of POI lookups in chunks not loaded as Full.
#[derive(Default, Clone)]
pub(crate) struct SavedPoiLookups {
    lookups: Arc<SyncMutex<FxHashMap<BlockPos, Lookup>>>,
}

impl SavedPoiLookups {
    /// Records the result of the lookup at `pos` started at `started`, unless a lookup started
    /// after it was given up on owns the entry now.
    fn record(&self, pos: BlockPos, started: Instant, finished: Lookup) {
        let mut lookups = self.lookups.lock();
        if let Some(lookup) = lookups.get_mut(&pos)
            && matches!(*lookup, Lookup::Running { started: current } if current == started)
        {
            *lookup = finished;
        }
    }
}

impl World {
    /// The POI type at `pos`, in a chunk not loaded as Full: [`UnloadedPoi::Unknown`] while
    /// the chunk is read in the background, then its answer for a few seconds, after which
    /// the chunk is read again.
    ///
    /// Unlike vanilla, which treats POI data it cannot read as empty, an unreadable chunk
    /// stays `Unknown`: Steel refuses a whole region when part of its table is damaged.
    pub(crate) fn unloaded_poi_type_at(&self, pos: BlockPos) -> UnloadedPoi {
        let now = Instant::now();
        let mut lookups = self.saved_poi_lookups.lookups.lock();
        if let Some(answer) = lookups.get(&pos).and_then(|lookup| lookup.answer(now)) {
            return answer;
        }
        if lookups.len() >= KEPT_LOOKUPS {
            lookups.retain(|_, lookup| lookup.answer(now).is_some());
        }
        lookups.insert(pos, Lookup::Running { started: now });
        drop(lookups);

        let chunk_map = Arc::clone(&self.chunk_map);
        let lookups = self.saved_poi_lookups.clone();
        let columns = PoiSquareColumns::from_positions(&chunk_map, [ChunkPos::from_block_pos(pos)]);
        self.chunk_map.chunk_runtime.spawn(async move {
            let found = columns
                .pois_by_chunk(
                    &chunk_map.storage,
                    &chunk_map.generation_pool,
                    &Arc::new(AtomicBool::new(false)),
                )
                .await
                .and_then(|by_chunk| {
                    by_chunk
                        .into_iter()
                        .next()
                        .map_or(Ok(Vec::new()), |(_, found)| found)
                });
            let answer = match found {
                Ok(found) => UnloadedPoi::Known(
                    found
                        .into_iter()
                        .find_map(|(poi_pos, poi_type)| (poi_pos == pos).then_some(poi_type)),
                ),
                Err(error) => {
                    tracing::warn!("Couldn't read the POIs of the chunk holding {pos:?}: {error}");
                    UnloadedPoi::Unknown
                }
            };
            let finished = Lookup::Finished {
                answer,
                at: Instant::now(),
            };
            lookups.record(pos, now, finished);
        });
        UnloadedPoi::Unknown
    }
}

/// Helpers for tests reading POIs of unloaded chunks.
#[cfg(test)]
pub(crate) mod test_helpers {
    use std::{
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };

    use steel_registry::vanilla_blocks;
    use steel_utils::{BlockPos, ChunkPos, types::UpdateFlags};

    use super::UnloadedPoi;
    use crate::{
        chunk::status::ChunkStatus,
        chunk_saver::{ChunkStorage, PreparedChunkSave},
        test_support::insert_ready_full_chunk,
        world::World,
    };

    /// Saves a Full chunk at `chunk_pos`, after `edit` changes what is saved, then drops it
    /// from memory.
    pub(crate) fn save_and_unload(
        world: &Arc<World>,
        chunk_pos: ChunkPos,
        lodestone: Option<BlockPos>,
        edit: impl FnOnce(&mut PreparedChunkSave),
    ) {
        let holder = insert_ready_full_chunk(world, chunk_pos);
        if let Some(lodestone) = lodestone {
            assert!(world.set_block(
                lodestone,
                vanilla_blocks::LODESTONE.default_state(),
                UpdateFlags::UPDATE_ALL
            ));
        }
        let Some(chunk) = holder.try_chunk(ChunkStatus::Full) else {
            panic!("inserted chunk should be Full");
        };
        let Some(mut prepared) =
            ChunkStorage::prepare_chunk_save(chunk, ChunkStatus::Full, &[], true)
        else {
            panic!("a forced save should be prepared");
        };
        edit(&mut prepared);
        let saved = world.chunk_map.chunk_runtime.block_on(
            world
                .chunk_map
                .storage
                .save_chunk_data(prepared, &world.chunk_map.generation_pool),
        );
        assert!(matches!(saved, Ok(true)));
        let _ = world.chunk_map.chunks.remove_sync(&chunk_pos);
        world.poi_storage.lock().remove_chunk(chunk_pos);
    }

    /// Polls `ready` every few milliseconds, failing after a few seconds.
    pub(crate) fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
        let started = Instant::now();
        while !ready() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "timed out waiting for {what}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub(crate) fn wait_for_answer(world: &World, pos: BlockPos) -> UnloadedPoi {
        let mut answer = UnloadedPoi::Unknown;
        wait_until("an answer", || {
            answer = world.unloaded_poi_type_at(pos);
            answer != UnloadedPoi::Unknown
        });
        answer
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use steel_registry::{RegistryEntry, init_vanilla_registry, vanilla_poi_types};
    use steel_utils::{BlockPos, ChunkPos};

    use super::test_helpers::{save_and_unload, wait_for_answer, wait_until};
    use super::{ANSWER_REUSED_FOR, LOOKUP_GIVEN_UP_AFTER, Lookup, SavedPoiLookups, UnloadedPoi};
    use crate::{
        behavior::init_behaviors, block_entity::init_block_entities, test_support::fresh_test_world,
    };

    #[test]
    fn a_stale_answer_is_never_given_while_the_chunk_is_read_again() {
        init_vanilla_registry();
        init_behaviors();
        init_block_entities();
        let world_fixture = fresh_test_world("saved_poi_lookup_stale");
        let world = &world_fixture.world;
        let lodestone = BlockPos::new(40, 70, 2);
        save_and_unload(world, ChunkPos::new(2, 0), Some(lodestone), |_| {});

        // An answer from before the lodestone was placed has expired.
        let long_ago = Instant::now()
            .checked_sub(ANSWER_REUSED_FOR)
            .expect("the clock is past the reuse window");
        world.saved_poi_lookups.lookups.lock().insert(
            lodestone,
            Lookup::Finished {
                answer: UnloadedPoi::Known(None),
                at: long_ago,
            },
        );
        assert_eq!(world.unloaded_poi_type_at(lodestone), UnloadedPoi::Unknown);
        let lodestone_type = vanilla_poi_types::LODESTONE
            .try_id()
            .expect("vanilla POI types should be registered");
        assert_eq!(
            wait_for_answer(world, lodestone),
            UnloadedPoi::Known(Some(lodestone_type))
        );
    }

    #[test]
    fn an_unreadable_chunk_gives_no_answer() {
        init_vanilla_registry();
        init_behaviors();
        init_block_entities();
        let world_fixture = fresh_test_world("saved_poi_lookup_unreadable");
        let world = &world_fixture.world;
        let target = BlockPos::new(40, 70, 2);
        // A saved chunk missing a section cannot be read.
        save_and_unload(world, ChunkPos::new(2, 0), None, |prepared| {
            prepared.persistent.sections.pop();
        });

        assert_eq!(world.unloaded_poi_type_at(target), UnloadedPoi::Unknown);
        wait_until("the read to finish", || {
            matches!(
                world.saved_poi_lookups.lookups.lock().get(&target),
                Some(Lookup::Finished { .. })
            )
        });
        assert_eq!(world.unloaded_poi_type_at(target), UnloadedPoi::Unknown);
    }

    #[test]
    fn a_lookup_given_up_on_cannot_overwrite_the_one_that_replaced_it() {
        let pos = BlockPos::new(1, 2, 3);
        let first = Instant::now();
        let second = first + LOOKUP_GIVEN_UP_AFTER;
        let lookups = SavedPoiLookups::default();
        lookups
            .lookups
            .lock()
            .insert(pos, Lookup::Running { started: second });
        let late = Lookup::Finished {
            answer: UnloadedPoi::Known(Some(7)),
            at: second,
        };

        lookups.record(pos, first, late);
        assert!(matches!(
            lookups.lookups.lock().get(&pos),
            Some(Lookup::Running { .. })
        ));
        lookups.record(pos, second, late);
        assert!(matches!(
            lookups.lookups.lock().get(&pos),
            Some(Lookup::Finished {
                answer: UnloadedPoi::Known(Some(7)),
                ..
            })
        ));
    }
}
