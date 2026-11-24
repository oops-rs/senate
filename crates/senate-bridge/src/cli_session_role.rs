//! Role selectors belong to session commands, independently of senator names.

use anyhow::{Context as _, bail};
use senate_core::{CommitteeId, Name};

use super::{Paths, binding, daemon_call, runtime};
use crate::ipc::{Request, SessionRole, SessionRoleHolder};

enum Selector {
    Chair(Option<CommitteeId>),
    Secretary(Option<senate_core::SenatePath>),
    Speaker,
}

impl Selector {
    fn parse(written: &str) -> anyhow::Result<Option<Self>> {
        let (scope, role) = written
            .rsplit_once("::")
            .map_or((None, written), |(scope, role)| (Some(scope), role));
        Ok(match role {
            "chair" => Some(Self::Chair(scope.map(CommitteeId::new).transpose()?)),
            "secretary" => Some(Self::Secretary(match scope {
                Some(scope) if scope.contains('/') => Some(scope.parse()?),
                Some(scope) => {
                    CommitteeId::new(scope)?;
                    None
                }
                None => None,
            })),
            "speaker" if scope.is_none() => Some(Self::Speaker),
            _ => None,
        })
    }
}

pub(super) fn resolve(
    paths: &Paths,
    written: &str,
    machine: Option<&Name>,
) -> anyhow::Result<Option<SessionRoleHolder>> {
    let Some(selector) = Selector::parse(written)? else {
        return Ok(None);
    };
    runtime(false)?.block_on(async {
        let role = match selector {
            Selector::Chair(committee) => SessionRole::Chair {
                committee: match committee {
                    Some(committee) => committee,
                    None => {
                        let settings = super::Settings::load(&paths.config());
                        let cwd = std::env::current_dir()?;
                        binding::resolve(paths, &cwd, &settings)
                            .await
                            .committee
                            .map(|(committee, _)| committee)
                            .context("this directory has no committee. Use `<committee>::chair`")?
                    }
                },
            },
            Selector::Secretary(senate) => SessionRole::Secretary { senate },
            Selector::Speaker => SessionRole::Speaker,
        };
        let holder: SessionRoleHolder = daemon_call(paths, &Request::ResolveSessionRole { role }).await?;
        if machine.is_none() && holder.session_id.is_none() {
            let locations = super::remote_cli::find_senators(paths, &holder.candidates).await;
            if let [machine] = locations.as_slice() {
                bail!(
                    "{written} has a hosted session on {machine}. Use `senate attach --machine {machine} {written}`"
                );
            }
            if !locations.is_empty() {
                let machines = locations.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                bail!(
                    "{written} has hosted sessions on {machines}. Choose one with `senate attach --machine <machine> {written}`"
                );
            }
            bail!(
                "{written} ({}) has no Senate-owned session on this machine",
                holder.primary
            );
        }
        Ok(Some(holder))
    })
}
