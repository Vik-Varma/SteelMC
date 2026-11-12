//! `/locate biome`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use steel_registry::biome::BiomeRef;
use steel_utils::{BlockPos, translations};
use text_components::TextComponent;
use tokio::sync::oneshot::{self, error::TryRecvError};

use super::super::super::{
    brigadier::CommandSyntaxError,
    execution::{
        BiomeOrTag, CommandResultSuspension, CommandResultSuspensionPoll, CommandSource,
        SteelCommandContext,
    },
};
use super::show_locate_result;
use crate::worldgen::generator::ChunkGenerator;

const MAX_BIOME_SEARCH_RADIUS: i32 = 6400;
const BIOME_SAMPLE_RESOLUTION_HORIZONTAL: i32 = 32;
const BIOME_SAMPLE_RESOLUTION_VERTICAL: i32 = 64;

type SearchResult = Option<(BlockPos, BiomeRef)>;

/// Starts the search on the server's locate search pool.
///
/// Vanilla searches on the server thread. When nothing matches, the search samples about
/// 1.1 million climate points (seconds of CPU), so Steel runs it in the background and polls
/// for the result instead of stalling ticks.
pub(super) fn start_search(
    context: &SteelCommandContext<CommandSource>,
) -> Result<LocateBiomeSearch, CommandSyntaxError> {
    let query = context.biome_or_tag("biome")?.clone();
    let source = context.source().clone();
    let world = source.world();
    let origin = BlockPos::from(source.position());
    let (min_y, max_y) = (world.get_min_y(), world.get_max_y());
    let generator = Arc::clone(&world.chunk_map.world_gen_context.generator);
    let cancelled = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = oneshot::channel();

    let search_query = query.clone();
    let search_cancelled = Arc::clone(&cancelled);
    source.server().locate_search_pool.spawn(move || {
        let found = generator.biome_source().find_closest_biome_3d(
            origin,
            MAX_BIOME_SEARCH_RADIUS,
            BIOME_SAMPLE_RESOLUTION_HORIZONTAL,
            BIOME_SAMPLE_RESOLUTION_VERTICAL,
            &|biome| search_query.matches(biome),
            min_y,
            max_y,
            &search_cancelled,
        );
        // The command may have been cancelled and dropped its receiver.
        let _ = sender.send(found);
    });

    Ok(LocateBiomeSearch {
        source,
        query,
        origin,
        receiver,
        cancelled,
        started_at: Instant::now(),
    })
}

pub(super) struct LocateBiomeSearch {
    source: CommandSource,
    query: BiomeOrTag,
    origin: BlockPos,
    receiver: oneshot::Receiver<SearchResult>,
    cancelled: Arc<AtomicBool>,
    started_at: Instant,
}

impl CommandResultSuspension for LocateBiomeSearch {
    fn poll(&mut self) -> CommandResultSuspensionPoll {
        match self.receiver.try_recv() {
            Ok(Some((pos, biome))) => {
                let distance = show_locate_result(
                    &self.source,
                    self.origin,
                    pos,
                    &translations::COMMANDS_LOCATE_BIOME_SUCCESS,
                    true,
                    &self.query.found_name(biome),
                    self.started_at.elapsed(),
                );
                CommandResultSuspensionPoll::Ready(Ok(distance))
            }
            Ok(None) => CommandResultSuspensionPoll::Ready(Err(biome_not_found(&self.query))),
            Err(TryRecvError::Empty) => CommandResultSuspensionPoll::Pending,
            Err(TryRecvError::Closed) => CommandResultSuspensionPoll::Ready(Err(
                CommandSyntaxError::dynamic("Biome search ended without a result"),
            )),
        }
    }

    fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Drop for LocateBiomeSearch {
    fn drop(&mut self) {
        // Stop the worker if the command is discarded without being cancelled.
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn biome_not_found(query: &BiomeOrTag) -> CommandSyntaxError {
    CommandSyntaxError::dynamic(
        translations::COMMANDS_LOCATE_BIOME_NOT_FOUND
            .message([TextComponent::from(query.as_printable())])
            .component(),
    )
}
