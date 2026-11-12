//! `/locate poi`.

use std::{io, sync::Arc, time::Instant};

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

/// Starts the search on the chunk runtime.
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
    let thread_pool = Arc::clone(&world.chunk_map.generation_pool);
    let (sender, receiver) = oneshot::channel();

    let search_query = query.clone();
    let task = world.chunk_map.chunk_runtime.spawn(async move {
        let found = columns.pois(&storage, &thread_pool).await.map(|pois| {
            let matches = |poi_type| {
                REGISTRY
                    .poi_types
                    .by_id(poi_type)
                    .is_some_and(|poi_type| search_query.matches(poi_type))
            };
            find_closest_with_type(pois, matches, origin, POI_SEARCH_RADIUS)
        });
        // The command may have been cancelled and dropped its receiver.
        let _ = sender.send(found);
    });

    Ok(LocatePoiSearch {
        source,
        query,
        origin,
        receiver,
        task,
        started_at: Instant::now(),
    })
}

pub(super) struct LocatePoiSearch {
    source: CommandSource,
    query: PoiTypeOrTag,
    origin: BlockPos,
    receiver: oneshot::Receiver<io::Result<Option<SavedPoi>>>,
    task: JoinHandle<()>,
    started_at: Instant,
}

impl CommandResultSuspension for LocatePoiSearch {
    fn poll(&mut self) -> CommandResultSuspensionPoll {
        let found = match self.receiver.try_recv() {
            Ok(Ok(found)) => found,
            Ok(Err(error)) => {
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
            self.started_at.elapsed(),
        );
        CommandResultSuspensionPoll::Ready(Ok(distance))
    }

    fn cancel(&mut self) {
        self.task.abort();
    }
}

impl Drop for LocatePoiSearch {
    fn drop(&mut self) {
        // Stop the search if the command is discarded without being cancelled.
        self.task.abort();
    }
}

fn poi_not_found(query: &PoiTypeOrTag) -> CommandSyntaxError {
    CommandSyntaxError::dynamic(
        translations::COMMANDS_LOCATE_POI_NOT_FOUND
            .message([TextComponent::from(query.as_printable())])
            .component(),
    )
}
