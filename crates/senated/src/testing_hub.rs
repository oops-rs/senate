//! An empty [`Hub`] for this crate's unit tests: nothing is stored, and
//! nothing a test here reads.

use std::sync::Arc;

use async_trait::async_trait;
use senate_core::{
    Appended, CommitteeId, Event, Hub, HubError, MirrorRef, NewEvent, Roster, SenatorPath, Seq,
};
use tokio::sync::broadcast;

struct Empty(broadcast::Sender<Event>);

pub(crate) fn empty() -> Arc<dyn Hub> {
    Arc::new(Empty(broadcast::channel(1).0))
}

#[async_trait]
impl Hub for Empty {
    async fn append(&self, _event: NewEvent) -> Result<Appended, HubError> {
        Err(HubError::Rejected("this hub stores nothing".into()))
    }

    async fn read(
        &self,
        _committee: &CommitteeId,
        _after: Seq,
        _limit: usize,
    ) -> Result<Vec<Event>, HubError> {
        Ok(Vec::new())
    }

    async fn get(&self, _committee: &CommitteeId, _seq: Seq) -> Result<Option<Event>, HubError> {
        Ok(None)
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.subscribe()
    }

    async fn committees(&self) -> Result<Vec<CommitteeId>, HubError> {
        Ok(Vec::new())
    }

    async fn roster(&self) -> Result<Roster, HubError> {
        Ok(Roster::default())
    }

    async fn is_agent_connected(&self, _committee: &CommitteeId, _agent: &SenatorPath) -> bool {
        false
    }

    async fn cursor(&self, _committee: &CommitteeId, _name: &str) -> Result<Seq, HubError> {
        Ok(Seq::ZERO)
    }

    async fn set_cursor(
        &self,
        _committee: &CommitteeId,
        _name: &str,
        _seq: Seq,
    ) -> Result<(), HubError> {
        Ok(())
    }

    async fn epoch(&self) -> Result<String, HubError> {
        Ok("empty".into())
    }

    async fn put_mirror(&self, _mirror: MirrorRef) -> Result<(), HubError> {
        Ok(())
    }

    async fn mirrors_of_seq(
        &self,
        _committee: &CommitteeId,
        _seq: Seq,
        _platform: &str,
    ) -> Result<Vec<MirrorRef>, HubError> {
        Ok(Vec::new())
    }

    async fn mirror_of_message(
        &self,
        _platform: &str,
        _message_ref: &str,
    ) -> Result<Option<MirrorRef>, HubError> {
        Ok(None)
    }
}
