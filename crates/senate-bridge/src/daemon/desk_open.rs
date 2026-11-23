//! `open_committee`: the Speaker's office opens a committee for a project
//! and starts its chair there in one step.
//!
//! The one authority this adds to what the office could already do: open a
//! committee. Starting the chair is the `spawn` tool's own authority
//! ([`Shared::spawn_authorized`]), unchanged, with the same directory rule
//! (any directory on this machine; the secretary is told to use the ones
//! the person names).
//!
//! Partial failure: the opening note is posted first, with an id derived
//! from the committee and the chair's spawn attempt id
//! ([`super::spawn_attempt`]) in its meta, so the committee is open even
//! when its chair fails to start. Calling again with the same committee
//! and arguments finds the note (the committee's first event carries that
//! id), posts nothing, and retries the same attempt: a chair whose session
//! had launched is found rather than started twice, and its appointment is
//! confirmed. Once a chair is appointed, a call starts nothing more and
//! names it. A committee this office did not open is refused:
//! add a chair or members to it with `spawn`.

use senate_core::{EventId, Kind};

use super::host_policy::SpawnInput;
use super::*;
use crate::ipc::{DeskOpenReply, HostedModel, HostedRuntime, SpawnProfile};

/// The opening note's id: one per committee, whichever desk worker posts it.
fn opening_id(committee: &CommitteeId) -> EventId {
    EventId::derived(0, &format!("desk-open-committee\0{committee}"))
}

/// Where the opening note records its chair's spawn attempt id.
const CHAIR_ATTEMPT: &str = "chair_attempt";

/// What a desk-opened committee's chair is started with.
pub(super) struct ChairOrder {
    pub(super) cwd: PathBuf,
    pub(super) runtime: Option<HostedRuntime>,
    pub(super) profile: Option<SpawnProfile>,
    pub(super) alias: Option<SenatorName>,
    pub(super) prompt: Option<String>,
}

impl Shared {
    pub(super) async fn on_desk_open_committee(
        &self,
        selector: &SessionSelector,
        committee: CommitteeId,
        chair: ChairOrder,
    ) -> Result<DeskOpenReply, IpcError> {
        let auth = self.authorize_host_action(selector).await?;
        if auth.sender != Address::Speaker || auth.acted_by.is_none() {
            return Err(IpcError::new(
                codes::NOT_PRESIDING,
                self.locale.pick(
                    "只有为 Speaker 的 desk 工作的会话可以开设委员会。",
                    "only a session working the Speaker's desk may open a committee",
                ),
            ));
        }
        let me = self.api.me().await.map_err(server_error)?;
        let (opened, attempt) = if me.committees.contains(&committee) {
            let Some(attempt) = self.opened_here(&committee).await? else {
                return Err(IpcError::bad_request(self.locale.pick(
                    format!(
                        "委员会 {committee} 已存在，不是由 Speaker 办公室开设的。要给它加主席或成员，用 `spawn`。"
                    ),
                    format!(
                        "committee {committee} already exists and was not opened by the Speaker's office: \
                         add a chair or members to it with `spawn`"
                    ),
                )));
            };
            (None, attempt)
        } else {
            let person = &self.config.person;
            let dir = chair.cwd.display();
            let body = self.locale.pick(
                format!(
                    "{} 为 {person} 开设了委员会 {committee}，项目目录 {dir}。",
                    auth.actor()
                ),
                format!(
                    "{} opened committee {committee} for {person}, in the project {dir}.",
                    auth.actor()
                ),
            );
            // The chair's spawn attempt, recorded with the opening: a call
            // made again reuses it, so a chair that launched is found, not
            // started twice.
            let attempt = crate::ipc::new_spawn_attempt_id();
            let event = auth
                .event(committee.clone(), Kind::Note, body)
                .with_id(opening_id(&committee))
                .with_meta(CHAIR_ATTEMPT, serde_json::json!(attempt));
            let posted = self.post(event).await?;
            self.refresh_me().await;
            (Some(posted.seq), attempt)
        };
        let existing = self
            .api
            .chair(&committee)
            .await
            .map_err(server_error)?
            .chair;
        if let Some(existing) = existing {
            return Ok(DeskOpenReply {
                committee,
                opened,
                chair: None,
                existing_chair: Some(existing),
            });
        }
        let input = SpawnInput {
            runtime: chair.runtime,
            profile: chair.profile,
            committee: committee.clone(),
            alias: chair.alias,
            cwd: chair.cwd,
            chair: true,
            secretary: false,
            prompt: chair.prompt,
            model: HostedModel::default(),
        };
        let spawned = self
            .on_spawn_attempt(selector, input, &attempt)
            .await
            .map_err(|error| {
            IpcError::new(
                &error.code,
                self.locale.pick(
                    format!(
                        "委员会 {committee} 已开设，但主席没有启动：{}。用同样的参数再调用一次 open_committee 来完成。",
                        error.message
                    ),
                    format!(
                        "committee {committee} is open, but its chair did not start: {}. Call open_committee \
                         again with the same arguments to finish.",
                        error.message
                    ),
                ),
            )
        })?;
        // An attempt that had launched before is returned as it was, its
        // appointment perhaps never confirmed: confirm it now.
        let chair_now = self
            .api
            .chair(&committee)
            .await
            .map_err(server_error)?
            .chair;
        if chair_now.as_ref() != Some(&spawned.senator) {
            self.appoint_hosted_chair(&auth, &spawned).await?;
        }
        Ok(DeskOpenReply {
            committee,
            opened,
            chair: Some(spawned),
            existing_chair: None,
        })
    }

    /// When `committee`'s first event is the office's opening note: the
    /// chair's spawn attempt it recorded.
    async fn opened_here(&self, committee: &CommitteeId) -> Result<Option<String>, IpcError> {
        let page = self
            .api
            .events(committee, Seq::ZERO, Some(1))
            .await
            .map_err(server_error)?;
        let want = opening_id(committee).to_string();
        Ok(page
            .events
            .first()
            .filter(|event| event.get("id").and_then(serde_json::Value::as_str) == Some(&want))
            .and_then(|event| event.get("meta")?.get(CHAIR_ATTEMPT)?.as_str())
            .map(str::to_owned))
    }
}
