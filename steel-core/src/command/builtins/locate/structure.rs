//! `/locate structure`.

use std::{sync::Arc, time::Instant};

use steel_utils::{BlockPos, Identifier, translations};
use text_components::TextComponent;

use super::super::super::{
    brigadier::CommandSyntaxError,
    execution::{
        CommandResultSuspension, CommandResultSuspensionPoll, CommandSource, SteelCommandContext,
        StructureOrTagKey,
    },
};
use super::show_locate_result;
use crate::{
    chunk::{
        chunk_request::{ChunkRequest, ChunkRequestHandle, ChunkRequestState, ChunkTicketKind},
        status::ChunkStatus,
    },
    world::World,
    worldgen::{
        generator::ChunkGenerator,
        structure::{StructureLocateCandidate, StructureLocatePlan, squared_distance},
    },
};

const MAX_STRUCTURE_SEARCH_RADIUS: i32 = 100;

pub(super) fn start_search(
    context: &SteelCommandContext<CommandSource>,
) -> Result<LocateStructureSearch, CommandSyntaxError> {
    let query = context.structure_or_tag_key("structure")?;
    let Some(structures) = query.resolve() else {
        return Err(invalid_structure(query));
    };
    if structures.is_empty() {
        return Err(structure_not_found(query));
    }

    let world = context.source().world();
    let Some(structure_generator) = world
        .chunk_map
        .world_gen_context
        .generator
        .structure_generator()
    else {
        return Err(structure_not_found(query));
    };
    let structure_keys = structures
        .iter()
        .map(|structure| structure.key.clone())
        .collect::<Vec<_>>();
    let Some(plan) = structure_generator.locate_plan_for_structures(&structure_keys) else {
        return Err(structure_not_found(query));
    };
    if plan.is_empty() {
        return Err(structure_not_found(query));
    }

    Ok(LocateStructureSearch {
        source: context.source().clone(),
        world: Arc::clone(world),
        query: query.clone(),
        plan,
        origin: BlockPos::from(context.source().position()),
        phase: LocatePhase::Start,
        pending: None,
        candidates: Vec::new(),
        best: None,
        random_radius: 0,
        started_at: Instant::now(),
    })
}

enum LocatePhase {
    Start,
    WaitingRings,
    RandomSpread,
    WaitingRandomSpread,
}

struct LocatedStructure {
    candidate: StructureLocateCandidate,
    found_structure: Identifier,
    distance_sqr: i64,
}

pub(super) struct LocateStructureSearch {
    source: CommandSource,
    world: Arc<World>,
    query: StructureOrTagKey,
    plan: StructureLocatePlan,
    origin: BlockPos,
    phase: LocatePhase,
    pending: Option<ChunkRequestHandle>,
    candidates: Vec<StructureLocateCandidate>,
    best: Option<LocatedStructure>,
    random_radius: i32,
    started_at: Instant,
}

impl CommandResultSuspension for LocateStructureSearch {
    fn poll(&mut self) -> CommandResultSuspensionPoll {
        loop {
            match self.phase {
                LocatePhase::Start => {
                    self.candidates = self.plan.ring_candidates(self.origin);
                    if self.candidates.is_empty() {
                        self.phase = LocatePhase::RandomSpread;
                        continue;
                    }
                    self.pending = Some(self.request_current_candidates());
                    self.phase = LocatePhase::WaitingRings;
                    return CommandResultSuspensionPoll::Pending;
                }
                LocatePhase::WaitingRings => match self.poll_pending_request() {
                    PendingRequest::Pending => return CommandResultSuspensionPoll::Pending,
                    PendingRequest::Cancelled => return Self::cancelled_result(),
                    PendingRequest::Ready => {
                        self.best = self.first_valid_candidate();
                        self.clear_request();

                        if self.best.is_some() && !self.plan.has_random_spread() {
                            return self.success_result();
                        }

                        self.phase = LocatePhase::RandomSpread;
                    }
                },
                LocatePhase::RandomSpread => {
                    if self.random_radius > MAX_STRUCTURE_SEARCH_RADIUS {
                        return self.finished_result();
                    }

                    self.candidates = self
                        .plan
                        .random_spread_candidates_at_radius(self.origin, self.random_radius);
                    self.random_radius += 1;

                    if self.candidates.is_empty() {
                        continue;
                    }

                    self.pending = Some(self.request_current_candidates());
                    self.phase = LocatePhase::WaitingRandomSpread;
                    return CommandResultSuspensionPoll::Pending;
                }
                LocatePhase::WaitingRandomSpread => match self.poll_pending_request() {
                    PendingRequest::Pending => return CommandResultSuspensionPoll::Pending,
                    PendingRequest::Cancelled => return Self::cancelled_result(),
                    PendingRequest::Ready => {
                        if self.update_best_after_random_radius() {
                            return self.success_result();
                        }

                        self.clear_request();
                        self.phase = LocatePhase::RandomSpread;
                    }
                },
            }
        }
    }

    fn cancel(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.cancel();
        }
    }
}

impl LocateStructureSearch {
    fn request_current_candidates(&self) -> ChunkRequestHandle {
        let positions = self
            .candidates
            .iter()
            .map(|candidate| candidate.chunk_pos)
            .collect();
        self.world.chunk_map.request_chunks(ChunkRequest {
            status: ChunkStatus::StructureStarts,
            positions,
            ticket_kind: ChunkTicketKind::StructureLocate,
        })
    }

    fn poll_pending_request(&self) -> PendingRequest {
        let Some(pending) = &self.pending else {
            return PendingRequest::Cancelled;
        };

        match pending.poll() {
            ChunkRequestState::Pending { .. } => PendingRequest::Pending,
            ChunkRequestState::Ready => PendingRequest::Ready,
            ChunkRequestState::Cancelled => PendingRequest::Cancelled,
        }
    }

    fn clear_request(&mut self) {
        self.pending = None;
        self.candidates.clear();
    }

    fn first_valid_candidate(&self) -> Option<LocatedStructure> {
        self.candidates.iter().copied().find_map(|candidate| {
            self.generated_structure_at_candidate(candidate)
                .map(|found_structure| LocatedStructure {
                    candidate,
                    found_structure,
                    distance_sqr: squared_distance(candidate.locate_pos, self.origin),
                })
        })
    }

    fn update_best_after_random_radius(&mut self) -> bool {
        let mut best = self.best.take();
        let mut current_scan = None;
        let mut found_current_scan = false;
        let mut found_in_this_radius = false;

        for candidate in &self.candidates {
            if current_scan != Some(candidate.scan_id()) {
                current_scan = Some(candidate.scan_id());
                found_current_scan = false;
            }

            if found_current_scan {
                continue;
            }

            let Some(found_structure) = self.generated_structure_at_candidate(*candidate) else {
                continue;
            };
            found_current_scan = true;
            found_in_this_radius = true;
            let located = LocatedStructure {
                candidate: *candidate,
                found_structure,
                distance_sqr: squared_distance(candidate.locate_pos, self.origin),
            };
            if best
                .as_ref()
                .is_none_or(|current| located.distance_sqr < current.distance_sqr)
            {
                best = Some(located);
            }
        }

        self.best = best;
        found_in_this_radius
    }

    fn generated_structure_at_candidate(
        &self,
        candidate: StructureLocateCandidate,
    ) -> Option<Identifier> {
        let holder = self
            .world
            .chunk_map
            .chunks
            .read_sync(&candidate.chunk_pos, |_, holder| Arc::clone(holder))?;
        let chunk = holder.try_chunk(ChunkStatus::StructureStarts)?;
        let starts = chunk.structure_starts();
        let structures = self.plan.structures_for_candidate(candidate)?;
        structures.iter().find_map(|structure| {
            starts
                .get(structure)
                .is_some_and(|start| !start.pieces.is_empty())
                .then(|| structure.clone())
        })
    }

    fn finished_result(&self) -> CommandResultSuspensionPoll {
        if self.best.is_some() {
            self.success_result()
        } else {
            CommandResultSuspensionPoll::Ready(Err(structure_not_found(&self.query)))
        }
    }

    fn success_result(&self) -> CommandResultSuspensionPoll {
        let Some(best) = &self.best else {
            return CommandResultSuspensionPoll::Ready(Err(structure_not_found(&self.query)));
        };
        let distance = show_locate_result(
            &self.source,
            self.origin,
            best.candidate.locate_pos,
            &translations::COMMANDS_LOCATE_STRUCTURE_SUCCESS,
            false,
            &self.query.found_name(&best.found_structure),
            self.started_at.elapsed(),
        );
        CommandResultSuspensionPoll::Ready(Ok(distance))
    }

    fn cancelled_result() -> CommandResultSuspensionPoll {
        CommandResultSuspensionPoll::Ready(Err(CommandSyntaxError::dynamic(
            "Structure search was cancelled",
        )))
    }
}

enum PendingRequest {
    Pending,
    Ready,
    Cancelled,
}

fn invalid_structure(query: &StructureOrTagKey) -> CommandSyntaxError {
    CommandSyntaxError::dynamic(
        translations::COMMANDS_LOCATE_STRUCTURE_INVALID
            .message([TextComponent::from(query.as_printable())])
            .component(),
    )
}

fn structure_not_found(query: &StructureOrTagKey) -> CommandSyntaxError {
    CommandSyntaxError::dynamic(
        translations::COMMANDS_LOCATE_STRUCTURE_NOT_FOUND
            .message([TextComponent::from(query.as_printable())])
            .component(),
    )
}
