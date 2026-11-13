//! Point of Interest (POI) system.
//!
//! Tracks special blocks (beds, workstations, bells, nether portals, etc.)
//! for efficient spatial queries without scanning every block.

pub mod poi_instance;
pub mod poi_set;
pub mod poi_storage;

pub use poi_instance::PointOfInterest;
pub use poi_set::PointOfInterestSet;
pub use poi_storage::{OccupationStatus, PointOfInterestStorage};

use steel_utils::{BlockPos, ChunkPos};

use crate::world::World;

impl World {
    /// Claims one of the POI at `pos`'s tickets, as a villager claims a bed or job site.
    /// Returns `false` if there is no POI there or all its tickets are taken.
    ///
    /// Ticket counts are saved with the chunk, so a claim marks the chunk dirty; vanilla's
    /// `PoiRecord.acquireTicket` marks its POI section dirty the same way.
    pub fn reserve_poi_ticket(&self, pos: BlockPos) -> bool {
        let reserved = self.poi_storage.lock().reserve_ticket(pos);
        if reserved {
            self.mark_chunk_dirty(ChunkPos::from_block_pos(pos));
        }
        reserved
    }

    /// Releases a ticket claimed with [`Self::reserve_poi_ticket`]. Returns `false` if there
    /// is no POI at `pos` or none of its tickets are taken.
    pub fn release_poi_ticket(&self, pos: BlockPos) -> bool {
        let released = self.poi_storage.lock().release_ticket(pos);
        if released {
            self.mark_chunk_dirty(ChunkPos::from_block_pos(pos));
        }
        released
    }
}

#[cfg(test)]
mod tests {
    use steel_registry::{RegistryEntry, vanilla_poi_types};
    use steel_utils::{BlockPos, ChunkPos};

    use crate::{
        chunk::status::ChunkStatus,
        test_support::{fresh_test_world, insert_ready_full_chunk},
    };

    #[test]
    fn claiming_or_releasing_a_poi_ticket_marks_its_chunk_for_saving() {
        let world_fixture = fresh_test_world("poi-ticket-dirty");
        let world = &world_fixture.world;
        let holder = insert_ready_full_chunk(world, ChunkPos::new(0, 0));
        let Some(chunk) = holder.try_chunk(ChunkStatus::Full) else {
            panic!("the chunk should be Full");
        };
        let bed = BlockPos::new(3, 64, 5);
        let home = vanilla_poi_types::HOME
            .try_id()
            .expect("vanilla POI types should be registered");
        world.poi_storage.lock().add(bed, home, 1);

        chunk.clear_dirty();
        assert!(world.reserve_poi_ticket(bed));
        assert!(chunk.is_dirty(), "a claim must be saved");

        chunk.clear_dirty();
        assert!(!world.reserve_poi_ticket(bed), "a bed has one ticket");
        assert!(!chunk.is_dirty(), "a refused claim changes nothing");

        assert!(world.release_poi_ticket(bed));
        assert!(chunk.is_dirty(), "a release must be saved");

        chunk.clear_dirty();
        assert!(!world.release_poi_ticket(bed), "no ticket is taken");
        assert!(!world.reserve_poi_ticket(BlockPos::new(4, 64, 5)));
        assert!(!chunk.is_dirty());
    }
}
