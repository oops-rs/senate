//! The supervision tree:
//!
//! ```text
//! SenateSupervisor
//! └─ CommitteeActor (one per committee, lazily spawned; the committee's single writer)
//!    │  (also the committee's presence: its leases, ticked by the actor)
//!    └─ AgentProxy (one per (committee, agent path) with a live stream)
//! ```

pub(crate) mod committee;
pub(crate) mod lease;
pub(crate) mod presence;
pub(crate) mod proxy;
pub(crate) mod supervisor;
