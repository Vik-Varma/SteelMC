//! `/locate poi`.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use steel_registry::{REGISTRY, RegistryExt as _};
use steel_utils::{BlockPos, translations};
use text_components::TextComponent;
use tokio::{
    sync::oneshot::{self, error::TryRecvError},
    task::JoinHandle,
};

use super::super::super::{
    brigadier::CommandSyntaxError,
    execution::{
        CommandResultSuspension, CommandResultSuspensionPoll, CommandSource, PoiTypeOrTag,
        SteelCommandContext,
    },
};
use super::show_locate_result;
use crate::{
    chunk_saver::SavedPoi,
    poi::{PoiSquareColumns, find_closest_with_type},
};

const POI_SEARCH_RADIUS: i32 = 256;

/// Starts the search on the chunk runtime and the server's locate search pool.
///
/// The chunks in range are captured here, on the server thread. Vanilla reads POI data of
/// unloaded chunks from disk while the server waits; Steel reads their saved chunks in the
/// background and polls for the result instead.
pub(super) fn start_search(
    context: &SteelCommandContext<CommandSource>,
) -> Result<LocatePoiSearch, CommandSyntaxError> {
    let query = context.poi_type_or_tag("poi")?.clone();
    let source = context.source().clone();
    let world = source.world();
    let origin = BlockPos::from(source.position());
    let columns = PoiSquareColumns::capture(&world.chunk_map, origin, POI_SEARCH_RADIUS);
    let storage = Arc::clone(&world.chunk_map.storage);
    let thread_pool = Arc::clone(&source.server().locate_search_pool);
    let cancelled = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = oneshot::channel();

    let search_query = query.clone();
    let search_cancelled = Arc::clone(&cancelled);
    let started_at = Instant::now();
    let task = world.chunk_map.chunk_runtime.spawn(async move {
        let found = columns
            .pois(&storage, &thread_pool, &search_cancelled)
            .await
            .map(|pois| {
                let matches = |poi_type| {
                    REGISTRY
                        .poi_types
                        .by_id(poi_type)
                        .is_some_and(|poi_type| search_query.matches(poi_type))
                };
                find_closest_with_type(pois, matches, origin, POI_SEARCH_RADIUS)
            });
        // The command may have been cancelled and dropped its receiver.
        let _ = sender.send((found, Instant::now()));
    });

    Ok(LocatePoiSearch {
        source,
        query,
        origin,
        receiver,
        search: BackgroundSearch { task, cancelled },
        started_at,
    })
}

pub(super) struct LocatePoiSearch {
    source: CommandSource,
    query: PoiTypeOrTag,
    origin: BlockPos,
    /// The search's result and when the background task finished it.
    receiver: oneshot::Receiver<(io::Result<Option<SavedPoi>>, Instant)>,
    search: BackgroundSearch,
    started_at: Instant,
}

/// The background half of a POI search. Dropping it stops the search.
struct BackgroundSearch {
    task: JoinHandle<()>,
    cancelled: Arc<AtomicBool>,
}

impl BackgroundSearch {
    /// Stops the search, including scan and decode work already queued on the pool.
    fn stop(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

impl Drop for BackgroundSearch {
    fn drop(&mut self) {
        // Stops the search if the command is discarded without being cancelled.
        self.stop();
    }
}

impl CommandResultSuspension for LocatePoiSearch {
    fn poll(&mut self) -> CommandResultSuspensionPoll {
        let (found, finished_at) = match self.receiver.try_recv() {
            Ok((Ok(found), finished_at)) => (found, finished_at),
            Ok((Err(error), _)) => {
                return CommandResultSuspensionPoll::Ready(Err(CommandSyntaxError::dynamic(
                    format!("POI search failed: {error}"),
                )));
            }
            Err(TryRecvError::Empty) => return CommandResultSuspensionPoll::Pending,
            Err(TryRecvError::Closed) => {
                return CommandResultSuspensionPoll::Ready(Err(CommandSyntaxError::dynamic(
                    "POI search ended without a result",
                )));
            }
        };
        let Some((pos, poi_type)) =
            found.and_then(|(pos, id)| Some((pos, REGISTRY.poi_types.by_id(id)?)))
        else {
            return CommandResultSuspensionPoll::Ready(Err(poi_not_found(&self.query)));
        };
        let distance = show_locate_result(
            &self.source,
            self.origin,
            pos,
            &translations::COMMANDS_LOCATE_POI_SUCCESS,
            false,
            &self.query.found_name(poi_type),
            // Up to the search finishing, not this later poll.
            finished_at.saturating_duration_since(self.started_at),
        );
        CommandResultSuspensionPoll::Ready(Ok(distance))
    }

    fn cancel(&mut self) {
        self.search.stop();
    }
}

fn poi_not_found(query: &PoiTypeOrTag) -> CommandSyntaxError {
    CommandSyntaxError::dynamic(
        translations::COMMANDS_LOCATE_POI_NOT_FOUND
            .message([TextComponent::from(query.as_printable())])
            .component(),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use tokio::{runtime::Builder, sync::oneshot, time::timeout};

    use super::BackgroundSearch;

    #[test]
    fn stopping_or_dropping_a_background_search_cancels_all_of_it() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime should start");
        for drop_instead_of_stop in [false, true] {
            let cancelled = Arc::new(AtomicBool::new(false));
            let (task_alive, task_ended) = oneshot::channel::<()>();
            let task = runtime.spawn(async move {
                let _task_alive = task_alive;
                pending::<()>().await;
            });
            let search = BackgroundSearch {
                task,
                cancelled: Arc::clone(&cancelled),
            };

            if drop_instead_of_stop {
                drop(search);
            } else {
                search.stop();
            }

            assert!(
                cancelled.load(Ordering::Relaxed),
                "pool work must see the cancel flag"
            );
            let ended =
                runtime.block_on(async { timeout(Duration::from_secs(5), task_ended).await });
            assert!(
                matches!(ended, Ok(Err(_))),
                "the async task must be aborted"
            );
        }
    }
}
