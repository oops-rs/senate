//! An in-person decision on a local cross-committee message awaiting the desk.
//! Only the socket handler can enter the mutation, after checking the peer.
use super::*;
use crate::ipc::{DeskDecision, DeskItemDetail, DeskOutcome};
use senate_core::{
    Origin, SenatorKind,
    api::{Me, Resolved},
};

struct Destination {
    committee: CommitteeId,
    address: Address,
}

impl Destination {
    fn name(&self) -> String {
        format!("{}::{}", self.committee, self.address)
    }
}

fn actor(me: &Me) -> Option<SenatorName> {
    let office = me.office.as_ref()?;
    let named = office.speaker.name();
    match office.kind {
        SenatorKind::Person if named.name() == &me.person => Some(named.clone()),
        SenatorKind::Agent if office.speaker_here() == Some(named) && me.agents.contains(named) => {
            Some(named.clone())
        }
        _ => None,
    }
}

fn origin_name(item: &Event) -> Option<String> {
    Origin::of(item).map(|origin| format!("{} #{}", origin.locator, origin.thread))
}

impl Shared {
    fn desk_no_authority(&self) -> IpcError {
        IpcError::new(codes::NOT_PRESIDING, self.locale.pick(
            "只有 Speaker 本人或担任 Speaker 的智能体的所有者可以处理此请求。",
            "Only the Speaker or the owner of the agent serving as Speaker can decide this request.",
        ))
    }

    async fn desk_destination(&self, item: &Event) -> Result<Destination, IpcError> {
        let unsupported = || {
            IpcError::bad_request(self.locale.pick(
                "这里只能处理本 Senate 内发往其他委员会的请求。",
                "This card can only decide requests for another committee in this Senate.",
            ))
        };
        let intended = item
            .meta
            .get(meta::INTENDED)
            .and_then(serde_json::Value::as_str)
            .and_then(|value| value.parse::<senate_core::Locator>().ok())
            .ok_or_else(unsupported)?;
        if item.kind != Kind::Request
            || item.thread.is_some()
            || !item.to.contains(&Address::Speaker)
        {
            return Err(unsupported());
        }
        let (committee, mut address) = match self
            .api
            .resolve(&intended, Some(&item.committee))
            .await
            .map_err(server_error)?
        {
            Resolved::Here {
                committee, address, ..
            } if committee != item.committee => (committee, address),
            Resolved::Ambiguous { .. } => {
                return Err(IpcError::bad_request(self.locale.pick(
                    "目标对应到多个席位。",
                    "The destination resolves to multiple seats.",
                )));
            }
            Resolved::Unknown { reason } => return Err(IpcError::bad_request(reason)),
            _ => return Err(unsupported()),
        };
        if address == Address::Chair {
            let chair = self.api.chair(&committee).await.map_err(server_error)?;
            address = Address::senator(&chair.chair.ok_or_else(|| {
                IpcError::bad_request(self.locale.pick(
                    "目标委员会尚未设立主席。",
                    "The destination committee has no chair.",
                ))
            })?);
        }
        if address.as_senator().is_none() {
            return Err(unsupported());
        }
        Ok(Destination { committee, address })
    }

    pub(super) async fn on_desk_item(
        &self,
        committee: CommitteeId,
        seq: Seq,
    ) -> Result<DeskItemDetail, IpcError> {
        let item = self.fetch_event(&committee, seq).await?;
        let root = match item.thread {
            Some(root) => self.fetch_event(&committee, root).await?,
            None => item.clone(),
        };
        let (_, events) = self.thread_events(&committee, root).await?;
        let me = self.api.me().await.map_err(server_error)?;
        let destination = self.desk_destination(&item).await;
        let blocked = if actor(&me).is_none() {
            Some(self.desk_no_authority().message)
        } else if let Err(error) = &destination {
            Some(error.message.clone())
        } else {
            self.docket_item(&committee, seq)
                .await
                .err()
                .map(|error| error.message)
        };
        Ok(DeskItemDetail {
            origin: origin_name(&item),
            item,
            events,
            destination: destination.ok().map(|target| target.name()),
            blocked,
        })
    }

    pub(super) async fn on_desk_decide(
        &self,
        committee: CommitteeId,
        seq: Seq,
        item_id: EventId,
        id: EventId,
        decision: DeskDecision,
    ) -> Result<DeskOutcome, IpcError> {
        let _guard = self.desk_decisions.lock().await;
        let me = self.api.me().await.map_err(server_error)?;
        let actor = actor(&me).ok_or_else(|| self.desk_no_authority())?;
        let item = self.fetch_event(&committee, seq).await?;
        if item.id != item_id {
            return Err(IpcError::bad_request(self.locale.pick(
                "此请求已更改。请重新打开后再处理。",
                "This desk request has changed. Reopen it before deciding.",
            )));
        }
        let (_, events) = self.thread_events(&committee, item.clone()).await?;
        if let Some(existing) = events.iter().find(|event| event.id == id) {
            let mut recorded = existing
                .meta
                .get(meta::DESK_OUTCOME)
                .and_then(|value| serde_json::from_value::<DeskOutcome>(value.clone()).ok());
            if let Some(DeskOutcome::Refused { seq, .. }) = &mut recorded {
                *seq = existing.seq;
            }
            let matches = match (&decision, &recorded) {
                (DeskDecision::Forward, Some(DeskOutcome::Delivered { .. })) => true,
                (DeskDecision::Refuse { reason }, Some(DeskOutcome::Refused { .. })) => {
                    existing.body == reason.trim()
                }
                _ => false,
            };
            if existing.from == Address::Speaker
                && existing.thread == Some(seq)
                && existing
                    .meta
                    .get(meta::DESK_PERSON)
                    .and_then(serde_json::Value::as_str)
                    == Some(me.person.as_str())
                && matches
                && let Some(outcome) = recorded
            {
                return Ok(outcome);
            }
            return Err(IpcError::bad_request(
                "the desk action id already names a different event",
            ));
        }
        self.docket_item(&committee, seq).await?;
        let target = self.desk_destination(&item).await?;
        let asker = item.on_behalf_of.clone().unwrap_or(item.from.clone());
        let office = |event: NewEvent| {
            event
                .with_meta(meta::ACTED_BY, serde_json::json!(actor))
                .with_meta(meta::DESK_PERSON, serde_json::json!(me.person))
        };
        let source = |kind, body| {
            office(
                NewEvent::new(committee.clone(), kind, Address::Speaker, body)
                    .to(asker.clone())
                    .in_thread(seq)
                    .with_id(id.clone()),
            )
        };
        match decision {
            DeskDecision::Forward => {
                let delivery_id = EventId::derived(
                    item.ts.timestamp_millis().cast_unsigned(),
                    &format!("person-desk-forward:{}:{id}", item.id),
                );
                let origin = Origin {
                    locator: self.locator_of(&committee, asker.clone()),
                    thread: seq,
                };
                let mut outbound = office(
                    NewEvent::new(target.committee, item.kind, Address::Speaker, item.body)
                        .to(target.address)
                        .with_id(delivery_id)
                        .with_meta(meta::ORIGIN, origin.value()),
                );
                if let Some(refs) = item.refs {
                    outbound = outbound.with_refs(refs);
                }
                let delivered = self.post(outbound).await?;
                let destination = delivered
                    .to
                    .first()
                    .map(|address| format!("{}::{address}", delivered.committee))
                    .ok_or_else(|| {
                        IpcError::internal("the Senate returned a receipt without a destination")
                    })?;
                let outcome = DeskOutcome::Delivered {
                    destination: destination.clone(),
                    committee: delivered.committee,
                    seq: delivered.seq,
                };
                let body = self.locale.pick(
                    format!(
                        "{} 批准转发；已送达 {destination} #{}。",
                        me.person, delivered.seq
                    ),
                    format!(
                        "{} approved forwarding. Delivered to {destination} #{}.",
                        me.person, delivered.seq
                    ),
                );
                self.post(
                    source(Kind::Note, body)
                        .with_meta(meta::FORWARDED_TO, serde_json::json!(destination))
                        .with_meta(meta::DESK_OUTCOME, serde_json::json!(outcome)),
                )
                .await.map_err(|error| IpcError::new(&error.code, self.locale.pick(
                    format!("已送达 {destination} #{}，但回执未能记录：{}", delivered.seq, error.message),
                    format!("Delivered to {destination} #{}, but recording the receipt failed. {}", delivered.seq, error.message),
                )))?;
                Ok(outcome)
            }
            DeskDecision::Refuse { reason } => {
                let reason = reason.trim();
                if reason.is_empty() {
                    return Err(IpcError::bad_request(
                        self.locale
                            .pick("请填写拒绝原因。", "A refusal reason is required."),
                    ));
                }
                // The answer is both the outcome and the audit record. Its
                // event ID makes the returned sequence replayable on retry.
                let destination = format!("{committee}::{asker}");
                let posted = self.post(source(Kind::Answer, reason.to_owned())
                    .with_meta(meta::DESK_OUTCOME, serde_json::json!({"kind":"refused","destination":destination,"committee":committee}))).await?;
                Ok(DeskOutcome::Refused {
                    destination,
                    committee,
                    seq: posted.seq,
                })
            }
        }
    }
}
