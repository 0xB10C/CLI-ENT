//! Network layer: address resolution, the v1/v2 transport enum, and connection
//! setup with auto-fallback (PLAN.md §5).

pub mod connect;
pub mod resolve;
pub mod transport;
pub mod v1;
pub mod v2;
