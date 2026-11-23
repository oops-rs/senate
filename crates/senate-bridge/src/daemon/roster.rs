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
        futures::stream::iter(me.committees)
            .map(|committee| self.roster_committee(committee, &aliases, &local))
            .buffered(8)
            .try_collect()
            .await
    }

    async fn roster_committee(
        &self,
        committee: CommitteeId,
        aliases: &KnownAliases,
        local: &StatusReply,
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
        let pending = self
            .on_inbox(&selector, Some(committee.clone()), None, false)
            .await?
            .threads
            .len();
        let mut members = BTreeMap::new();
        for p in presence.agents {
            members.insert(p.agent, roster_active(Some(p.state), false));
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
            *members.entry(session.agent.clone()).or_insert(false) |= roster_active(None, running);
        }
        if let KnownAliases::Scoped(table) = aliases {
            for seat in table.iter().filter(|seat| seat.committee == committee) {
                members.entry(seat.primary).or_insert(false);
            }
        }
        let chair_live = chair
            .as_ref()
            .and_then(|c| members.get(c))
            .copied()
            .unwrap_or(false);
        let name = |senator: &SenatorName| display_name(Some(aliases), Some(&committee), senator);
        let chair_display_name = chair.as_ref().map(name);
        let members = members
            .into_iter()
            .map(|(senator, active)| RosterMember {
                display_name: name(&senator),
                senator,
                active,
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
