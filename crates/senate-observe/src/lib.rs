//! The machine Senate's observer grant store and HTTP boundary. Only the
//! machine assembly mounts this router and gives the local bridge a handle.

mod audit;
mod cards;
mod diff;
mod grants;
mod http;
mod identities;
mod journal;
mod machine;
mod notices;
mod previews;
mod projection;
mod releases;
#[cfg(feature = "replay")]
mod replay;

pub use cards::CardKey;
pub use grants::{
    AdminRequest, AppConsentedGrants, CreateGrant, GrantError, GrantInfo, Installation,
    IssuedGrant, Registry, Scope,
};
pub use http::{router, valid_authority};
pub use previews::{LinePolicy, Preview, first_line};

pub use journal::Hint;
pub use machine::{LocalApproval, LocalHistory, LocalSeat, MachineFacts, MachineSource};
pub use projection::{Capture, ProjectionError, SenateSource, ToolDemand};

pub use notices::LocalNotice;

#[cfg(feature = "replay")]
pub use replay::{Recording, ReplayError};

pub use identities::WireIdentities;
