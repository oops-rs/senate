//! `senate session replace`: a fresh hosted session takes over a seat.
//!
//! The new session has the old one's runtime, directory and committee (and
//! its model and effort, unless the command changes them). It is handed
//! everything the old seat held, in this order ([`ReplaceStep`]):
//!
//! 1. **spawn** — start it, and wait until it is seated and its stream is
//!    connected. Should that fail, nothing after it runs, the new session is
//!    stopped again, and the old seat keeps everything.
//! 2. **alias** — the old seat's alias on the committee moves to it, only
//!    now, so a message to the alias never goes to a session that did not
//!    start.
//! 3. **duty** — its duty is the old seat's.
//! 4. **desk** — where the old seat stood among the desk workers (the
//!    person's secretaries, and the Speaker's when it presides here), the
//!    new one stands, at the same place.
//! 5. **chair** — the chair, when the old seat held it.
//! 6. **handover** — each open question or request put to the old seat
//!    (not waiting on a person) gets one message in its own thread, to the
//!    new seat, with [`meta::HANDED_TO`]: the new seat answers it there, in
//!    the asker's thread.
//! 7. **prompt** — the starting prompt, as an ordinary request from the
//!    person, as `senate spawn --prompt` sends it.
//! 8. **stop** — the old session's terminal is stopped (skipped when it no
//!    longer runs).
//! 9. **unseat** — the old seat is unseated. What could not move is closed
//!    by the Speaker's gone-seat rule; a question handed over is not, since
//!    the brief counts the new seat among its recipients.
//!
//! The replacement is recorded in `state.db` (`replacements`) before the
//! spawn and after every step, with the event ids each step writes, so a
//! failure stops at a named step and running the same command again
//! resumes there: nothing done is done twice and no second session starts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::host_policy::{HostAuthority, SpawnOrder};
use super::*;
use crate::ipc::{
    HostedModel, HostedRuntime, ReplaceReply, ReplaceStep, ReplaceStepReport, StreamState,
};

/// How long the new session has to be seated and connected.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// A replacement under way, as `state.db` keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    old: SenatorName,
    committee: CommitteeId,
    runtime: HostedRuntime,
    cwd: PathBuf,
    model: HostedModel,
    #[serde(default)]
    prompt: Option<String>,
    /// What the old seat held when the replacement began.
    #[serde(default)]
    alias: Option<SenatorName>,
    #[serde(default)]
    duty: Option<String>,
    #[serde(default)]
    chair: bool,
    /// The new seat, once it was started.
    #[serde(default)]
    new: Option<NewSeat>,
    /// The steps done, with what each did.
    #[serde(default)]
    done: Vec<ReplaceStepReport>,
    /// The id of each event a step writes, chosen before it is first sent,
    /// so a resumed step writes the same event again (the server keeps one).
    #[serde(default)]
    ids: BTreeMap<String, EventId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NewSeat {
    senator: SenatorName,
    session_id: String,
}

impl Record {
    fn has(&self, step: ReplaceStep) -> bool {
        self.done.iter().any(|r| r.step == step)
    }

    fn new_seat(&self) -> Result<&NewSeat, IpcError> {
        self.new
            .as_ref()
            .ok_or_else(|| IpcError::internal("the replacement has no new seat recorded"))
    }
}

fn step_failed(step: ReplaceStep, record: &Record, error: IpcError, locale: Locale) -> IpcError {
    let old = Address::senator(&record.old).in_text();
    let done: Vec<&str> = record.done.iter().map(|r| r.step.as_str()).collect();
    let done = if done.is_empty() {
        locale.pick("无", "none").to_owned()
    } else {
        done.join(", ")
    };
    let message = match (step, locale) {
        (ReplaceStep::Spawn, Locale::Zh) => format!(
            "替换 {old} 停在 spawn 这一步：{}。没有任何东西被移交；旧席位保留一切。",
            error.message
        ),
        (ReplaceStep::Spawn, Locale::En) => format!(
            "Replacing {old} stopped at the spawn step: {}. Nothing was handed over; the old \
             seat keeps everything.",
            error.message
        ),
        (_, Locale::Zh) => format!(
            "替换 {old} 停在 {} 这一步：{}。已完成：{done}。再运行同一条命令会从这一步继续。",
            step.as_str(),
            error.message
        ),
        (_, Locale::En) => format!(
            "Replacing {old} stopped at the {} step: {}. Done: {done}. Run the same command \
             again to resume from this step.",
            step.as_str(),
            error.message
        ),
    };
    IpcError::new(&error.code, message)
}

impl Shared {
    pub(super) async fn on_replace(
        &self,
        name: &AliasName,
        prompt: Option<String>,
        model: HostedModel,
    ) -> Result<ReplaceReply, IpcError> {
        let prompt = prompt.filter(|p| !p.trim().is_empty());
        let auth = self
            .authorize_host_action(&SessionSelector::default())
            .await?;
        let mut record = match self.replacement_named(name)? {
            Some(mut record) => {
                // Resumed: what the first run recorded stands; a prompt it
                // did not have may be given now.
                if record.prompt.is_none() && !record.has(ReplaceStep::Prompt) {
                    record.prompt = prompt;
                }
                record
            }
            None => self.begin_replacement(name, prompt, model).await?,
        };
        auth.check_target(&record.committee, false)?;
        self.save(&record)?;
        for step in ReplaceStep::ALL {
            if record.has(step) {
                continue;
            }
            let detail = match self.run_step(step, &mut record, &auth).await {
                Ok(detail) => detail,
                Err(error) => {
                    if step == ReplaceStep::Spawn {
                        // Nothing was handed over: the replacement is not
                        // under way any more.
                        let _ = self.store.remove_replacement(&record.old);
                    }
                    return Err(step_failed(step, &record, error, self.locale));
                }
            };
            record.done.push(ReplaceStepReport {
                step,
                earlier: false,
                detail,
            });
            self.save(&record)?;
        }
        self.store
            .remove_replacement(&record.old)
            .map_err(internal)?;
        let seat = record.new_seat()?.clone();
        Ok(ReplaceReply {
            committee: record.committee,
            old: record.old,
            new: seat.senator,
            session_id: seat.session_id,
            steps: record.done,
            alias: record.alias,
        })
    }

    fn save(&self, record: &Record) -> Result<(), IpcError> {
        let json = serde_json::to_string(record).map_err(IpcError::internal)?;
        self.store
            .save_replacement(&record.old, &json)
            .map_err(internal)
    }

    /// The replacement under way that `name` names: by the old seat, the
    /// new one, or the alias it moves. Its earlier steps are marked so.
    fn replacement_named(&self, name: &AliasName) -> Result<Option<Record>, IpcError> {
        for (_, json) in self.store.replacements().map_err(internal)? {
            let Ok(mut record) = serde_json::from_str::<Record>(&json) else {
                continue;
            };
            if name
                .committee
                .as_ref()
                .is_some_and(|c| c != &record.committee)
            {
                continue;
            }
            let named = name.alias == record.old
                || record.alias.as_ref() == Some(&name.alias)
                || record.new.as_ref().is_some_and(|n| n.senator == name.alias);
            if named {
                for report in &mut record.done {
                    report.earlier = true;
                }
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    /// What the old seat holds now, recorded before anything changes.
    async fn begin_replacement(
        &self,
        name: &AliasName,
        prompt: Option<String>,
        model: HostedModel,
    ) -> Result<Record, IpcError> {
        let target = self.resolve_restore(name, None).await?;
        let session = &target.session;
        if self
            .store
            .session_host(&session.session_id)
            .map_err(internal)?
            .is_none()
        {
            return Err(IpcError::bad_request(self.locale.pick(
                format!(
                    "{} 不是 Senate 托管的会话；只有托管的会话可以替换。",
                    Address::senator(&session.agent).in_text()
                ),
                format!(
                    "{} is not a Senate-hosted session; only a hosted one can be replaced.",
                    Address::senator(&session.agent).in_text()
                ),
            )));
        }
        let old = session.agent.clone();
        let committee = target.committee.clone();
        let model = target.model.overridden_by(&model);
        model.check(target.runtime).map_err(IpcError::bad_request)?;
        let alias = tokio::time::timeout(STEP_TIMEOUT, self.api.aliases())
            .await
            .map_err(|_| IpcError::new(codes::SERVER, "could not read the aliases in time"))?
            .map_err(server_error)?
            .alias_of(&committee, &old)
            .cloned();
        let duty = lock(&self.duties)
            .get(&(committee.clone(), old.clone()))
            .cloned()
            .filter(|d| !d.is_empty());
        let chair = match tokio::time::timeout(STEP_TIMEOUT, self.api.chair(&committee)).await {
            Ok(Ok(info)) => info.chair.as_ref() == Some(&old),
            // No chair on record.
            Ok(Err(e)) if e.status() == Some(404) => false,
            Ok(Err(e)) => return Err(server_error(e)),
            Err(_) => {
                return Err(IpcError::new(
                    codes::SERVER,
                    "could not read the committee's chair in time",
                ));
            }
        };
        Ok(Record {
            old,
            committee,
            runtime: target.runtime,
            cwd: session.cwd.clone(),
            model,
            prompt,
            alias,
            duty,
            chair,
            new: None,
            done: Vec::new(),
            ids: BTreeMap::new(),
        })
    }

    /// The id `key` writes with, chosen once and kept in the record.
    fn id_for(&self, record: &mut Record, key: String) -> Result<EventId, IpcError> {
        if let Some(id) = record.ids.get(&key) {
            return Ok(id.clone());
        }
        let id = EventId::new();
        record.ids.insert(key, id.clone());
        // Kept before it is first sent, so a retry reuses it.
        self.save(record)?;
        Ok(id)
    }

    async fn run_step(
        &self,
        step: ReplaceStep,
        record: &mut Record,
        auth: &HostAuthority,
    ) -> Result<String, IpcError> {
        let l = self.locale;
        match step {
            ReplaceStep::Spawn => self.replace_spawn(record).await,
            ReplaceStep::Alias => {
                let Some(alias) = record.alias.clone() else {
                    return Ok(l.pick("旧席位没有别名", "the old seat had no alias").into());
                };
                let new = record.new_seat()?.senator.clone();
                let committee = record.committee.clone();
                let aliases = timed(self.api.aliases()).await?;
                if aliases.alias_of(&committee, &new) != Some(&alias) {
                    timed(self.api.remove_alias(&record.old, &alias, Some(&committee))).await?;
                    timed(self.api.set_alias(&new, &alias, Some(&committee), false)).await?;
                }
                Ok(format!("{committee}::{alias} -> {new}"))
            }
            ReplaceStep::Duty => {
                let Some(duty) = record.duty.clone() else {
                    return Ok(l.pick("旧席位没有职责", "the old seat had no duty").into());
                };
                let new = record.new_seat()?.senator.clone();
                let id = self.id_for(record, "duty".into())?;
                let event = DutyPayload {
                    senator: new,
                    duty: duty.clone(),
                }
                .note(record.committee.clone(), auth.sender.clone())
                .with_id(id);
                self.post(event).await?;
                Ok(duty)
            }
            ReplaceStep::Desk => self.replace_desk(record, auth).await,
            ReplaceStep::Chair => {
                if !record.chair {
                    return Ok(l
                        .pick("旧席位不是主席", "the old seat was not the chair")
                        .into());
                }
                let new = record.new_seat()?.senator.clone();
                self.on_set_chair(
                    &SessionSelector::default(),
                    Some(record.committee.clone()),
                    new.clone(),
                    auth.acted_by.as_ref(),
                )
                .await?;
                Ok(Address::senator(&new).in_text())
            }
            ReplaceStep::Handover => self.replace_handover(record, auth).await,
            ReplaceStep::Prompt => {
                let Some(prompt) = record.prompt.clone() else {
                    return Ok(l.pick("没有起始提示", "no starting prompt").into());
                };
                let new = record.new_seat()?.senator.clone();
                let id = self.id_for(record, "prompt".into())?;
                let event = auth
                    .event(record.committee.clone(), Kind::Request, prompt)
                    .to(Address::senator(&new))
                    .with_id(id);
                let posted = self.post(event).await?;
                Ok(format!("seq {}", posted.seq))
            }
            ReplaceStep::Stop => {
                let name = AliasName {
                    committee: None,
                    alias: record.old.clone(),
                };
                match self
                    .on_hosted_stop(&SessionSelector::default(), &name, true)
                    .await
                {
                    Ok(_) => Ok(l.pick("已停止", "stopped").into()),
                    // Its terminal no longer runs: nothing to stop.
                    Err(e) if e.code == codes::NO_SESSION => Ok(l
                        .pick("它的终端已不在运行", "its terminal was not running")
                        .into()),
                    Err(e) => Err(e),
                }
            }
            ReplaceStep::Unseat => {
                let id = self.id_for(record, "unseat".into())?;
                let request = api::SetCommitteeSeat {
                    senator: record.old.clone(),
                    action: senate_core::CommitteeSeatAction::Unseat,
                    id,
                    acted_by: auth.acted_by.clone(),
                };
                timed(self.api.set_committee_seat(&record.committee, &request)).await?;
                self.seed_chair(&record.committee).await;
                Ok(format!(
                    "{} ({})",
                    Address::senator(&record.old).in_text(),
                    record.committee
                ))
            }
        }
    }

    /// Start the new session with no alias, and wait until it is seated and
    /// its stream connected. A session that started but never connected is
    /// stopped again.
    async fn replace_spawn(&self, record: &mut Record) -> Result<String, IpcError> {
        let order = SpawnOrder {
            runtime: record.runtime,
            committee: record.committee.clone(),
            alias: None,
            default_alias: false,
            cwd: record.cwd.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: record.model.clone(),
        };
        let reply = self
            .on_hosted_spawn(&SessionSelector::default(), order)
            .await?;
        record.new = Some(NewSeat {
            senator: reply.senator.clone(),
            session_id: reply.session_id.clone(),
        });
        self.save(record)?;
        if let Err(error) = self.wait_connected(&reply.senator, &record.committee).await {
            let name = AliasName {
                committee: None,
                alias: reply.senator.clone(),
            };
            let rollback = self
                .on_hosted_stop(&SessionSelector::default(), &name, true)
                .await;
            let mut message = error.message;
            if let Err(stop) = rollback {
                message.push_str(&format!(
                    "; stopping the new session {} failed too: {}",
                    Address::senator(&reply.senator).in_text(),
                    stop.message
                ));
            }
            return Err(IpcError::new(&error.code, message));
        }
        Ok(format!(
            "{} ({})",
            Address::senator(&reply.senator).in_text(),
            reply.session_id
        ))
    }

    async fn wait_connected(
        &self,
        senator: &SenatorName,
        committee: &CommitteeId,
    ) -> Result<(), IpcError> {
        let seat = (senator.clone(), committee.clone());
        let connected = async {
            loop {
                let live = self
                    .store
                    .live_sessions()
                    .map_err(internal)?
                    .iter()
                    .any(|s| &s.agent == senator && s.committee() == Some(committee));
                let stream = lock(&self.streams).get(&seat).map(|w| w.state());
                if live && matches!(stream, Some(StreamState::Connected { .. })) {
                    return Ok::<_, IpcError>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        tokio::time::timeout(CONNECT_TIMEOUT, connected)
            .await
            .map_err(|_| {
                IpcError::new(
                    codes::SERVER,
                    format!(
                        "{} started but was not seated and connected within {}s",
                        Address::senator(senator).in_text(),
                        CONNECT_TIMEOUT.as_secs()
                    ),
                )
            })??;
        // Seated on the server now, not at the next heartbeat.
        self.heartbeat(senator, committee).await;
        Ok(())
    }

    /// The new seat takes the old one's place in each desk-worker list that
    /// has it: the person's secretaries, and the Speaker's when it presides
    /// here. Other principals' lists are theirs to change.
    async fn replace_desk(
        &self,
        record: &Record,
        auth: &HostAuthority,
    ) -> Result<String, IpcError> {
        let new = record.new_seat()?.senator.clone();
        let mut principals = vec![
            SenatorName::new(self.config.person.clone())
                .map_err(|e| IpcError::bad_request(e.to_string()))?,
        ];
        if let Some(speaker) = self.office().and_then(|o| o.desk_owner().cloned())
            && !principals.contains(&speaker)
        {
            principals.push(speaker);
        }
        let all = timed(self.api.secretaries()).await?;
        let mut moved = Vec::new();
        for principal in principals {
            let Some(list) = all.get(&principal) else {
                continue;
            };
            let Some(at) = list.iter().position(|s| s == &record.old) else {
                continue;
            };
            let mut list = list.clone();
            if list.contains(&new) {
                list.remove(at);
            } else {
                list[at] = new.clone();
            }
            self.api
                .set_secretaries(&principal, list, auth.acted_by.clone())
                .await
                .map_err(|e| self.refusal(e))?;
            moved.push(format!("{principal} #{}", at + 1));
        }
        if moved.is_empty() {
            return Ok(self
                .locale
                .pick("旧席位不在任何 desk 名单里", "the old seat worked no desk")
                .into());
        }
        self.refresh_me().await;
        Ok(moved.join(", "))
    }

    /// Each open question or request put to the old seat, not waiting on a
    /// person, is handed to the new seat in its own thread.
    async fn replace_handover(
        &self,
        record: &mut Record,
        auth: &HostAuthority,
    ) -> Result<String, IpcError> {
        let new = record.new_seat()?.senator.clone();
        let committee = record.committee.clone();
        let page = timed(self.api.threads(&committee, Some(&record.old))).await?;
        let open: Vec<_> = page
            .threads
            .into_iter()
            .filter(|t| matches!(t.kind, Kind::Question | Kind::Request))
            .filter(|t| t.state != senate_core::StatusState::Archived)
            .filter(|t| t.escalated_to.is_none())
            .filter(|t| {
                t.agents.contains(&record.old) || t.to.contains(&Address::senator(&record.old))
            })
            .filter(|t| !(t.kind == Kind::Question && t.answered))
            .collect();
        let mut handed = Vec::new();
        for thread in open {
            let root = self.root_event(&committee, thread.root).await?;
            let id = self.id_for(record, format!("handover-{}", thread.root))?;
            let body = handover_body(&record.old, &new, &root.body, self.locale);
            let event = auth
                .event(committee.clone(), thread.kind.clone(), body)
                .in_thread(thread.root)
                .to(Address::senator(&new))
                .with_meta(meta::HANDED_TO, serde_json::json!(new.to_string()))
                .with_id(id);
            self.post(event).await?;
            handed.push(format!("seq {}", thread.root));
        }
        if handed.is_empty() {
            return Ok(self
                .locale
                .pick("没有未结的讨论串", "no open threads")
                .into());
        }
        Ok(handed.join(", "))
    }

    async fn root_event(&self, committee: &CommitteeId, root: Seq) -> Result<Event, IpcError> {
        let page = timed(
            self.api
                .events(committee, Seq(root.0.saturating_sub(1)), Some(1)),
        )
        .await?;
        page.events
            .into_iter()
            .find_map(|value| match crate::client::decode_event(value) {
                crate::client::Decoded::Event(e) if e.seq == root => Some(*e),
                _ => None,
            })
            .ok_or_else(|| IpcError::internal(format!("seq {root} is not in the log")))
    }
}

/// What the new seat reads in a thread handed to it.
fn handover_body(old: &SenatorName, new: &SenatorName, root: &str, locale: Locale) -> String {
    let old = Address::senator(old).in_text();
    let new = Address::senator(new).in_text();
    match locale {
        Locale::Zh => format!(
            "{old} 已被 {new} 替换（senate session replace），这个讨论串现在交给 {new}。\
             先读一下讨论串里已有的内容，再在这里回答。原来的内容：\n\n{root}"
        ),
        Locale::En => format!(
            "{old} was replaced by {new} (senate session replace), and this thread is {new}'s \
             now. Read what the thread already holds, then answer here. What was put to it:\n\n\
             {root}"
        ),
    }
}

/// One server call, bounded.
async fn timed<T>(
    call: impl std::future::Future<Output = Result<T, crate::client::ApiClientError>>,
) -> Result<T, IpcError> {
    tokio::time::timeout(STEP_TIMEOUT, call)
        .await
        .map_err(|_| IpcError::new(codes::SERVER, "the server did not answer in time"))?
        .map_err(server_error)
}
