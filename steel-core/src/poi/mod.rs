//! Point of Interest (POI) system.
//!
//! Tracks special blocks (beds, workstations, bells, nether portals, etc.)
//! for efficient spatial queries without scanning every block.

pub mod poi_instance;
pub mod poi_set;
pub mod poi_storage;
mod saved_lookup;
pub mod search;

pub use poi_instance::PointOfInterest;
pub use poi_set::PointOfInterestSet;
pub use poi_storage::{OccupationStatus, PointOfInterestStorage};
pub(crate) use saved_lookup::{SavedPoiLookups, UnloadedPoi};
pub use search::{PoiSquareColumns, find_closest_with_type};
