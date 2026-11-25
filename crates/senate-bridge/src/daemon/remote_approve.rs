//! Answering the permission prompt in a Senate-hosted terminal without
//! attaching to it (`senate session approve`, the app's buttons).
//!
//! The answer is the one a person would give in the terminal:
//!
//! - Codex asks through its app-server, which the approvals relay hears
//!   ([`crate::approvals`]); the relay sends the answer on the same
//!   connection, and the TUI is told the request was resolved.
//! - Claude Code asks in a dialog its hooks tell the daemon of
//!   ([`super::hosted_approvals`]). The host checks that its screen shows
//!   Claude Code's permission dialog and types the chosen option's number
//!   ([`host::prompt::permission_key`]), and types nothing when it does not.
//!
//! A choice the host only saw on the screen, which no runtime told of, is
//! not answered this way. Only a person asks: the connection is refused from
//! inside a coding session's process tree ([`super::approval_guard`]). Each
//! answer is a note in the committee log from the person, naming the
//! session, the tool and the choice, and a line in the daemon's log.

use super::host_policy::HostAuthority;
use super::*;
use crate::approvals::Decision;
use crate::host;
use crate::host::control::Answer;
use crate::host::prompt::PermissionChoice;
use crate::ipc::ApproveSessionReply;

const AUDIT_TIMEOUT: Duration = Duration::from_secs(4);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum PendingPrompt {
    Codex(String),
    Claude { fingerprint: String, at: u64 },
}

#[cfg(target_os = "macos")]
pub(super) struct HostedApprovalTarget {
    session_id: String,
    prompt: PendingPrompt,
    pub description: String,
}

impl Shared {
    pub(super) async fn on_approve_session(
        &self,
        senator: &AliasName,
        decision: Decision,
    ) -> Result<ApproveSessionReply, IpcError> {
        let auth = self
            .authorize_host_action(&SessionSelector::default())
            .await?;
        let session = self.find_local_senator(senator, None).await?;
        self.approve_hosted(&auth, &session.session_id, None, decision)
            .await
    }

    #[cfg(target_os = "macos")]
    pub(super) async fn pending_hosted_approval(
        &self,
        senator: &AliasName,
    ) -> Result<HostedApprovalTarget, IpcError> {
        let session = self.find_local_senator(senator, None).await?;
        let (host, session) = self
            .approval_host(&session.session_id)
            .map_err(internal)?
            .ok_or_else(|| IpcError::bad_request("This session has no running hosted terminal"))?;
        let (prompt, tool) = match session.runtime {
            Runtime::Codex => {
                let approval = self
                    .relay
                    .list()
                    .into_iter()
                    .find(|approval| approval.session_id == session.session_id)
                    .ok_or_else(|| self.nothing_pending(&host, &session))?;
                (
                    PendingPrompt::Codex(approval.id),
                    crate::host_approval::tool_label(&format!(
                        "{:?}: {}",
                        approval.kind,
                        approval
                            .command
                            .as_deref()
                            .unwrap_or(&approval.summary)
                            .lines()
                            .next()
                            .unwrap_or_default()
                    )),
                )
            }
            Runtime::Claude => {
                let candidate = self
                    .claude_candidate(&host, &session.session_id)
                    .map_err(internal)?
                    .ok_or_else(|| self.nothing_pending(&host, &session))?;
                (
                    PendingPrompt::Claude {
                        fingerprint: candidate.fingerprint,
                        at: candidate.at,
                    },
                    candidate.confirmation_label.unwrap_or(candidate.tool),
                )
            }
            Runtime::Agy | Runtime::Acp => return Err(self.nothing_pending(&host, &session)),
        };
        let alias = self.shown_session_name(&session.session_id);
        let subject = session.committee().map_or_else(
            || alias.clone(),
            |committee| format!("{committee}::{alias}"),
        );
        Ok(HostedApprovalTarget {
            session_id: session.session_id,
            prompt,
            description: format!("{tool} for {subject}"),
        })
    }

    #[cfg(target_os = "macos")]
    pub(super) async fn answer_confirmed_hosted_prompt(
        &self,
        target: &HostedApprovalTarget,
        decision: Decision,
    ) -> Result<ApproveSessionReply, IpcError> {
        let auth = self
            .authorize_host_action(&SessionSelector::default())
            .await?;
        self.approve_hosted(&auth, &target.session_id, Some(&target.prompt), decision)
            .await
    }

    /// Answer what waits in the terminal hosting `session_id`; for Codex,
    /// approval `codex_id` when given, else its oldest.
    async fn approve_hosted(
        &self,
        auth: &HostAuthority,
        session_id: &str,
        expected: Option<&PendingPrompt>,
        decision: Decision,
    ) -> Result<ApproveSessionReply, IpcError> {
        // The host keeps its session while it is answered.
        let _binding = self.session_bindings.read().await;
        let Some((host_id, session)) = self.approval_host(session_id).map_err(internal)? else {
            return Err(IpcError::new(
                codes::NO_SESSION,
                self.locale.pick(
                    "此会话没有正在运行的 Senate 托管终端。",
                    "This session has no running Senate-hosted terminal.",
                ),
            ));
        };
        if host::legacy_viewer(&Paths::new(&self.home), &host_id) {
            return Err(IpcError::bad_request(
                "The terminal host needs restart to be protected. Save work, then run senate session restart.",
            ));
        }
        let who = Address::senator(&session.agent).in_text();
        let committee = session
            .committee()
            .cloned()
            .ok_or_else(|| IpcError::bad_request(format!("no committee is recorded for {who}")))?;
        auth.check_target(&committee, false)?;
        let tool = match session.runtime {
            Runtime::Codex => {
                let approval = self
                    .relay
                    .list()
                    .into_iter()
                    .find(|a| {
                        a.session_id == session.session_id
                            && expected.is_none_or(
                                |prompt| matches!(prompt, PendingPrompt::Codex(id) if &a.id == id),
                            )
                    })
                    .ok_or_else(|| self.nothing_pending(&host_id, &session))?;
                let answered = self
                    .relay
                    .answer_with(&approval.id, decision, Some(auth.actor()), |_| Ok(()))
                    .await
                    .map_err(IpcError::bad_request)?;
                self.observe_approval_decided(
                    &answered.id,
                    &session,
                    super::observe::approval_tool(answered.kind),
                    decision,
                );
                Some(answered.summary)
            }
            Runtime::Claude => {
                let choice = match decision {
                    Decision::Allow => PermissionChoice::Once,
                    Decision::AllowForSession => PermissionChoice::Always,
                    Decision::Deny => PermissionChoice::Deny,
                    Decision::Cancel => {
                        return Err(IpcError::bad_request(self.locale.pick(
                            "Claude Code 的权限对话没有“取消”选项；请用拒绝。",
                            "Claude Code's permission dialog has no cancel choice; deny it instead.",
                        )));
                    }
                };
                let _answering = self.hosted_approvals.claude_answer.read().await;
                let candidate = self
                    .claude_candidate(&host_id, &session.session_id)
                    .map_err(internal)?
                    .ok_or_else(|| self.nothing_pending(&host_id, &session))?;
                if expected.is_some_and(|prompt| !matches!(prompt, PendingPrompt::Claude { fingerprint, at } if fingerprint == &candidate.fingerprint && *at == candidate.at)) {
                    return Err(IpcError::bad_request("The permission prompt changed while awaiting confirmation"));
                }
                let paths = Paths::new(&self.home);
                let attach = format!("senate attach {}", session.agent);
                let check_started = crate::host_approval::observed_at();
                match host::control::answer(&paths, &host_id, choice).await? {
                    Answer::Answered => {}
                    Answer::NoDialog => {
                        self.dismiss_claude(&host_id, &session.session_id, check_started)
                            .map_err(internal)?;
                        return Err(IpcError::bad_request(self.locale.pick(
                            format!("{who} 的终端已无 Claude Code 权限对话。已将该请求标记为未答复关闭，未输入任何内容。"),
                            format!("{who}'s terminal shows no Claude Code permission dialog. The pending request was dismissed unanswered and nothing was typed."),
                        )));
                    }
                    Answer::NoOption => {
                        return Err(IpcError::bad_request(self.locale.pick(
                            "这个权限对话没有“始终允许”选项，未输入任何内容。请选择允许一次或拒绝。",
                            "This permission dialog has no \"always\" option, so nothing was typed. Allow it once or deny it.",
                        )));
                    }
                    Answer::Unchanged => {
                        tracing::warn!(senator = %session.agent, ?decision, "typed a permission answer, but the terminal did not change");
                        return Err(IpcError::internal(self.locale.pick(
                            format!("已在 {who} 的终端输入选择，但终端没有变化。请运行 `{attach}` 查看。"),
                            format!("The choice was typed into {who}'s terminal, but the terminal did not change. Run `{attach}` to check."),
                        )));
                    }
                }
                self.settle_claude_answer(&host_id, &candidate, decision, auth.actor())
                    .map_err(internal)?;
                self.observe_approval_decided(
                    &super::observe::claude_id(&host_id, &candidate),
                    &session,
                    super::observe::claude_tool(&candidate.tool),
                    decision,
                );
                Some(candidate.tool)
            }
            Runtime::Agy | Runtime::Acp => {
                return Err(IpcError::bad_request(self.locale.pick(
                    format!(
                        "Senate 只能回答 Codex 和 Claude Code 的权限请求。请运行 `senate attach {}`。",
                        session.agent
                    ),
                    format!(
                        "Senate answers only Codex and Claude Code permission prompts. Run `senate attach {}`.",
                        session.agent
                    ),
                )));
            }
        };
        tracing::info!(
            senator = %session.agent,
            %committee,
            runtime = session.runtime.as_str(),
            tool = tool.as_deref().unwrap_or_default(),
            ?decision,
            "answered a hosted permission prompt from outside its terminal",
        );
        let body = audit_body(
            self.locale,
            &auth.actor(),
            &who,
            &committee,
            tool.as_deref(),
            decision,
        );
        let note = auth.note(committee.clone(), body).with_meta(
            meta::PROMPT_ANSWER,
            serde_json::json!({
                "decision": decision,
                "runtime": session.runtime.as_str(),
                "session_id": session.session_id,
            }),
        );
        let audit_pending = !matches!(
            tokio::time::timeout(AUDIT_TIMEOUT, self.post(note.clone())).await,
            Ok(Ok(_))
        );
        if audit_pending {
            self.store.queue_host_notice(&note).map_err(|error| {
                IpcError::internal(format!(
                    "the prompt was answered, but its log note could not be queued: {error}"
                ))
            })?;
            self.host_notices_ready.notify_one();
        }
        Ok(ApproveSessionReply {
            senator: session.agent,
            committee,
            runtime: session.runtime,
            tool,
            decision,
            audit_pending,
        })
    }

    /// Nothing Senate can answer waits: say whether something else does.
    fn nothing_pending(&self, host_id: &EventId, session: &Session) -> IpcError {
        let who = Address::senator(&session.agent).in_text();
        let attach = format!("senate attach {}", session.agent);
        if host::prompt::read(&Paths::new(&self.home), host_id).is_some() {
            return IpcError::bad_request(self.locale.pick(
                format!("{who} 的终端停在一个提示上，但它不是 Senate 知道的权限请求，Senate 不能回答。请运行 `{attach}` 在终端内选择。"),
                format!("{who}'s terminal waits on a prompt that is not a permission prompt Senate knows of, so Senate cannot answer it. Run `{attach}` and choose in the terminal."),
            ));
        }
        IpcError::bad_request(self.locale.pick(
            format!("{who} 没有等待中的权限请求。"),
            format!("{who} has no permission prompt waiting."),
        ))
    }
}

fn audit_body(
    locale: Locale,
    actor: &str,
    who: &str,
    committee: &CommitteeId,
    tool: Option<&str>,
    decision: Decision,
) -> String {
    let tool = tool.map_or_else(String::new, |tool| {
        let quoted = serde_json::json!(crate::host_approval::tool_label(tool));
        match locale {
            Locale::En => format!(" Tool or command {quoted}."),
            Locale::Zh => format!(" 工具或命令 {quoted}。"),
        }
    });
    match locale {
        Locale::En => {
            let verb = match decision {
                Decision::Allow => "allowed once",
                Decision::AllowForSession => "always allowed",
                Decision::Deny => "denied",
                Decision::Cancel => "denied and interrupted",
            };
            format!(
                "{actor} {verb} a permission prompt of {who} in committee `{committee}`, from outside its terminal.{tool}"
            )
        }
        Locale::Zh => {
            let verb = match decision {
                Decision::Allow => "允许了一次",
                Decision::AllowForSession => "始终允许了",
                Decision::Deny => "拒绝了",
                Decision::Cancel => "拒绝并中断了",
            };
            format!("{actor} 在终端之外{verb}委员会 `{committee}` 中 {who} 的权限请求。{tool}")
        }
    }
}
