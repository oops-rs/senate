use std::sync::Mutex;

use senate_observe::LocalNotice;

use super::*;
use crate::timeline::{self, observe_tools::Tail};

const MAX_SESSIONS: usize = 64;
const MAX_CALLS: usize = 256;
const SCAN_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct Tools(Mutex<Tracking>);

#[derive(Default)]
struct Tracking {
    epoch: u64,
    sampled: Option<Instant>,
    tails: BTreeMap<String, SessionTail>,
    aliases: BTreeMap<String, String>,
}

struct SessionTail {
    observation: Tail,
    id: String,
    calls: BTreeMap<String, Call>,
}

struct Call {
    id: String,
    kind: String,
}

impl Tools {
    pub(super) fn clear(&self) {
        *lock(&self.0) = Tracking::default();
    }

    pub(super) fn poll(
        &self,
        shared: &Shared,
        registry: &senate_observe::Registry,
        sessions: &[Session],
    ) {
        let Some(demand) = registry.tool_demand() else {
            return;
        };
        let mut tracking = lock(&self.0);
        if tracking.epoch != demand.epoch {
            *tracking = Tracking {
                epoch: demand.epoch,
                aliases: aliases(&Paths::new(&shared.home)),
                ..Tracking::default()
            };
        }
        if tracking
            .sampled
            .is_some_and(|at| at.elapsed() < SCAN_INTERVAL)
        {
            return;
        }
        tracking.sampled = Some(Instant::now());
        let Tracking { tails, aliases, .. } = &mut *tracking;
        let active: BTreeMap<_, _> = sessions
            .iter()
            .filter(|session| {
                matches!(session.runtime, Runtime::Claude | Runtime::Codex)
                    && session.committee().is_some_and(|id| demand.includes(id))
            })
            .take(MAX_SESSIONS)
            .map(|session| (session.session_id.as_str(), session))
            .collect();
        tails.retain(|id, _| active.contains_key(id.as_str()));
        for (id, session) in active {
            let (Some(path), Some(committee)) = (timeline::path(session), session.committee())
            else {
                continue;
            };
            if !tails
                .get(id)
                .is_some_and(|tail| tail.observation.matches(&path))
            {
                tails.remove(id);
                if let Ok(observation) = Tail::open(path) {
                    tails.insert(
                        id.into(),
                        SessionTail {
                            observation,
                            id: uuid::Uuid::new_v4().to_string(),
                            calls: BTreeMap::new(),
                        },
                    );
                }
                continue;
            }
            let Some(tail) = tails.get_mut(id) else {
                continue;
            };
            let Ok(observations) = tail.observation.read(session.runtime, id, aliases) else {
                continue;
            };
            for item in observations {
                let call = if item.started {
                    if tail.calls.len() >= MAX_CALLS || tail.calls.contains_key(&item.call_id) {
                        continue;
                    }
                    let Some(kind) = item.kind else {
                        continue;
                    };
                    let active = Call {
                        id: uuid::Uuid::new_v4().to_string(),
                        kind,
                    };
                    let borrowed = Call {
                        id: active.id.clone(),
                        kind: active.kind.clone(),
                    };
                    tail.calls.insert(item.call_id, active);
                    borrowed
                } else if let Some(call) = tail.calls.remove(&item.call_id) {
                    call
                } else if let Some(kind) = item.kind {
                    Call {
                        id: uuid::Uuid::new_v4().to_string(),
                        kind,
                    }
                } else {
                    continue;
                };
                registry.notice(LocalNotice::Tool {
                    committee: committee.clone(),
                    senator: session.agent.clone(),
                    session: tail.id.clone(),
                    call: call.id,
                    kind: call.kind,
                    started: item.started,
                });
            }
        }
    }
}

fn aliases(paths: &Paths) -> BTreeMap<String, String> {
    let file = std::fs::read(paths.hosted("top").join("observe-mcp-aliases.json")).ok();
    file.filter(|bytes| bytes.len() <= 8192)
        .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, String>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|(key, alias)| !key.is_empty() && key.len() <= 128 && alias.parse::<Name>().is_ok())
        .take(64)
        .collect()
}
