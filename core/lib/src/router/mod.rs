//! Rocket's router.

mod router;
mod collider;
mod matcher;

pub use router::Router;
pub(crate) use router::*;
pub(crate) use collider::*;
