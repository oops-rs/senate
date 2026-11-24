//! The machine Senate's observer grant store and HTTP boundary. Only the
//! machine assembly mounts this router and gives the local bridge a handle.

mod audit;
mod diff;
mod grants;
mod http;
mod journal;
mod machine;
mod projection;

pub use grants::{
    AdminRequest, CreateGrant, GrantInfo, Installation, IssuedGrant, Registry, Scope,
};
pub use http::router;

pub use journal::Hint;
pub use machine::{LocalSeat, MachineFacts, MachineSource};
pub use projection::{Capture, ProjectionError, SenateSource};
