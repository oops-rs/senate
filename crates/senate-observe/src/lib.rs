//! The machine Senate's observer grant store and HTTP boundary. Only the
//! machine assembly mounts this router and gives the local bridge a handle.

mod audit;
mod grants;
mod http;

pub use grants::{AdminRequest, CreateGrant, GrantInfo, IssuedGrant, Registry, Scope};
pub use http::router;
