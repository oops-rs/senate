//! Speaker tests: the flows with a [`ScriptedBrain`](crate::ScriptedBrain),
//! an in-memory Hub and a fake chat surface; the basis Brain with a scripted
//! provider.

// The flow suites are carried over in the next steps; until then parts of
// the shared scaffolding have no caller.
#![allow(dead_code)]

mod agent;
mod agents;
mod basis_brain;
// mod continuity;
mod decisions;
mod flows;
pub(crate) mod hub;
mod review;
mod rig;
mod surface;
mod tasks;
// mod work;
