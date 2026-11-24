use futures::{StreamExt, TryStreamExt};
use senate_core::alias::{AliasMatch, AliasName, KnownAliases, display_name};
use senate_core::continuity::roster_active;

use super::*;
use crate::ipc::{RosterCommittee, RosterMember};

impl Shared {
    pub(super) async fn on_roster(&self) -> Result<Vec<RosterCommittee>, IpcError> {
        let (me, aliases) =
            tokio::try_join!(self.api.me(), self.api.aliases()).map_err(server_error)?;
        let local = self.on_status()?;
        let paused = self.store.all_paused().map_err(internal)?;
        futures::stream::iter(me.committees)
            .map(|committee| self.roster_committee(committee, &aliases, &local, &paused))
            .buffered(8)
            .try_collect()
            .await
    }

    async fn roster_committee(
        &self,
        committee: CommitteeId,
        aliases: &KnownAliases,
        local: &StatusReply,
        paused: &[crate::state::Paused],
    ) -> Result<RosterCommittee, IpcError> {
        let selector = SessionSelector::default();
        let (chair, presence) =
            tokio::join!(self.api.chair(&committee), self.api.presence(&committee));
        let presence = presence.map_err(server_error)?;
        let chair = match chair {
            Ok(info) => {
                self.note_chair(&info);
                info.chair
            }
            // A committee without an appointed chair was a 404 on older servers.
            Err(e) if e.status() == Some(404) => self.chair_of(&committee),
            Err(e) => return Err(server_error(e)),
        };
        let inbox = self
            .on_inbox(&selector, Some(committee.clone()), None, false)
            .await?;
        let pending = inbox.threads.len();
        let mut presence_states = BTreeMap::new();
        let mut members = BTreeMap::new();
        let mut chair_live = false;
        for p in presence.agents {
            let live = p.state == senate_core::PresenceState::Online;
            chair_live |= live && chair.as_ref() == Some(&p.agent);
            let state = if p.paused {
                "paused"
            } else if live {
                "unknown"
            } else {
                "away"
            };
            presence_states.insert(p.agent.clone(), state);
            members.insert(p.agent, roster_active(Some(p.state), false, p.paused));
        }
        for agent in presence.duties.into_keys() {
            members.entry(agent).or_insert(false);
        }
        for session in local
            .sessions
            .iter()
            .filter(|s| s.committee() == Some(&committee))
        {
            let running = local.hosted.contains_key(&session.session_id);
            chair_live |= running && chair.as_ref() == Some(&session.agent);
            *members.entry(session.agent.clone()).or_insert(false) |=
                roster_active(None, running, false);
        }
        // Paused sessions can already have ended in the local registry.
        for pause in paused.iter().filter(|p| p.committee == committee) {
            members.insert(pause.agent.clone(), true);
        }
        if let KnownAliases::Scoped(table) = aliases {
            for seat in table.iter().filter(|seat| seat.committee == committee) {
                members.entry(seat.primary).or_insert(false);
            }
        }
        let name = |senator: &SenatorName| display_name(Some(aliases), Some(&committee), senator);
        let chair_display_name = chair.as_ref().map(name);
        let members = members
            .into_iter()
            .map(|(senator, active)| {
                let session = local
                    .sessions
                    .iter()
                    .filter(|s| s.agent == senator && s.committee() == Some(&committee))
                    .max_by_key(|s| (s.is_live(), s.last_seen));
                let state = if paused
                    .iter()
                    .any(|p| p.agent == senator && p.committee == committee)
                {
                    "paused"
                } else if let Some(session) = session.filter(|s| s.is_live()) {
                    if self.session_working(session) {
                        "working"
                    } else {
                        "idle"
                    }
                } else {
                    presence_states.get(&senator).copied().unwrap_or("unknown")
                };
                let status_title = inbox
                    .threads
                    .iter()
                    .filter(|t| t.owner.as_ref().is_some_and(|o| o.agent == senator))
                    .max_by_key(|t| t.updated)
                    .map(|t| t.title.chars().take(200).collect());
                RosterMember {
                    display_name: name(&senator),
                    runtime: session
                        .map(|s| s.runtime)
                        .or_else(|| native_runtime(&senator)),
                    session_state: Some(state.into()),
                    status_title,
                    session_id: session
                        .filter(|s| s.is_live())
                        .map(|s| s.session_id.clone()),
                    senator,
                    active,
                }
            })
            .collect();
        Ok(RosterCommittee {
            committee,
            chair,
            chair_display_name,
            chair_live,
            members,
            pending,
        })
    }

    pub(super) async fn on_set_alias(
        &self,
        senator: AliasName,
        alias: SenatorName,
        committee: Option<CommitteeId>,
    ) -> Result<SenatorName, IpcError> {
        let (me, aliases) =
            tokio::try_join!(self.api.me(), self.api.aliases()).map_err(server_error)?;
        let committee = committee.or_else(|| senator.committee.clone());
        let primary = match aliases.resolve(&senator, committee.as_ref()) {
            AliasMatch::NotAlias if senator.committee.is_some() => {
                return Err(IpcError::bad_request(format!("unknown alias {senator}")));
            }
            found => found
                .non_interactive(&senator.alias)
                .map_err(|e| IpcError::bad_request(e.to_string()))?,
        };
        if committee
            .as_ref()
            .is_some_and(|c| !me.committees.contains(c))
        {
            return Err(IpcError::bad_request("unknown alias committee"));
        }
        if !me.agents.contains(&primary)
            && !self
                .store
                .sessions()
                .map_err(internal)?
                .iter()
                .any(|s| s.agent == primary)
        {
            return Err(IpcError::bad_request(format!(
                "no owned senator named {primary}"
            )));
        }
        let alias = self
            .api
            .set_alias(&primary, &alias, committee.as_ref(), false)
            .await
            .map_err(server_error)?;
        *lock(&self.display_aliases) = None;
        Ok(alias)
    }
}

// Native primary names carry a runtime prefix. Leave custom names unknown;
// a remote heartbeat only reports connectivity and whether the seat paused.
fn native_runtime(senator: &SenatorName) -> Option<Runtime> {
    let name = senator.to_string();
    let (prefix, id) = name.split_once('-')?;
    let id = id.as_bytes();
    if id.len() != 36
        || !id.iter().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    Runtime::parse(prefix)
}
