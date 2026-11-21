//! A hosted Codex keeps the tools it listed when it started.
//!
//! Codex does not act on MCP `tools/list_changed` (its
//! `mcp_tool_list_changed` feature is off), so a hosted Codex made chair, or
//! a desk worker, never sees the tools that role brings, nor loses those of
//! a role it gave up. The daemon remembers the roles each hosted Codex was
//! published with. When the roles change ([`WatchEvent::Roles`]) it compares
//! again: a session whose roles moved is marked `restart needed` (in
//! `senate sessions` and `senate watch-all`) and told once, in its own
//! thread, to be restarted. A restore publishes it again with the roles it
//! has then. The record is the daemon's own: one that restarts takes each
//! running host's roles as they are.

use super::*;

/// The roles that decide a session's tools: it works the Speaker's desk;
/// it chairs its committee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Roles {
    presides: bool,
    chairs: bool,
}

impl Roles {
    fn describe(self) -> String {
        let mut roles = Vec::new();
        if self.presides {
            roles.push("works the Speaker's desk");
        }
        if self.chairs {
            roles.push("chairs its committee");
        }
        if roles.is_empty() {
            "is a member with no chair or desk role".to_owned()
        } else {
            roles.join(" and ")
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Baseline {
    started: Roles,
    /// Its roles moved since it started, and it was told.
    restart: bool,
}

/// What a hosted Codex is told, once, when its roles moved.
pub(super) fn restart_notice(name: &str, now: Roles) -> String {
    format!(
        "[senate] Your Senate role changed: this session now {}. Codex does not reload \
         MCP tools while it runs, so your Senate tools are still the ones it started \
         with. Ask your user to restart this session: `senate stop {name}`, then \
         `senate restore {name}`.",
        now.describe()
    )
}

impl Shared {
    fn roles_of(&self, session: &Session) -> Roles {
        Roles {
            presides: self.presided_by(&session.agent),
            chairs: session
                .committee()
                .is_some_and(|c| self.chair_of(c).as_ref() == Some(&session.agent)),
        }
    }

    /// Remember the roles a hosted Codex is published with: its tools.
    pub(super) fn note_codex_roles(&self, host: &EventId, session: &Session) {
        let started = self.roles_of(session);
        lock(&self.codex_roles).insert(
            host.clone(),
            Baseline {
                started,
                restart: false,
            },
        );
    }

    /// Whether the hosted Codex on `host` must be restarted for its tools.
    pub(crate) fn codex_restart_needed(&self, host: &EventId) -> bool {
        lock(&self.codex_roles)
            .get(host)
            .is_some_and(|baseline| baseline.restart)
    }

    /// Compare each running hosted Codex's roles with those it started
    /// with; tell each one whose roles moved, once.
    pub(super) async fn check_codex_roles(&self) {
        let paths = Paths::new(&self.home);
        let hosted = match self.store.hosted_sessions() {
            Ok(hosted) => hosted,
            Err(e) => {
                tracing::debug!("hosted sessions for the Codex role check: {e}");
                return;
            }
        };
        let mut tell = Vec::new();
        {
            let mut known = lock(&self.codex_roles);
            let mut running = BTreeSet::new();
            for (session, host) in hosted {
                if session.runtime != Runtime::Codex
                    || !crate::host::running(&paths, &host).unwrap_or(false)
                {
                    continue;
                }
                running.insert(host.clone());
                let now = self.roles_of(&session);
                let baseline = known.entry(host.clone()).or_insert(Baseline {
                    started: now,
                    restart: false,
                });
                if !baseline.restart && baseline.started != now {
                    baseline.restart = true;
                    tell.push((session, now));
                }
            }
            known.retain(|host, _| running.contains(host));
        }
        for (session, now) in tell {
            let name = session.agent.to_string();
            let text = restart_notice(&name, now);
            let pushed = tokio::time::timeout(CODEX_PUSH_TIMEOUT, async {
                let mut server =
                    crate::delivery::codex::AppServer::connect(&self.codex_transport).await?;
                server.push(&session.session_id, &text).await
            })
            .await;
            match pushed {
                Ok(Ok(_)) => {
                    tracing::info!(senator = %name, "told a hosted Codex to restart for its tools")
                }
                Ok(Err(e)) => {
                    tracing::warn!(senator = %name, "could not tell a hosted Codex to restart: {e}")
                }
                Err(_) => {
                    tracing::warn!(senator = %name, "timed out telling a hosted Codex to restart")
                }
            }
        }
        self.publish_status();
    }
}

/// Check the hosted Codex sessions whenever the roles change.
pub(super) async fn follow(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let mut events = shared.watch.subscribe();
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(WatchEvent::Roles) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    shared.check_codex_roles().await;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = shutdown.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_names_the_new_roles_and_the_restart() {
        let chair = Roles {
            presides: false,
            chairs: true,
        };
        let text = restart_notice("reviewer", chair);
        assert!(
            text.contains("this session now chairs its committee"),
            "{text}"
        );
        assert!(
            text.contains("`senate stop reviewer`, then `senate restore reviewer`"),
            "{text}"
        );
        let both = Roles {
            presides: true,
            chairs: true,
        };
        assert_eq!(
            both.describe(),
            "works the Speaker's desk and chairs its committee"
        );
        let none = Roles {
            presides: false,
            chairs: false,
        };
        assert_eq!(none.describe(), "is a member with no chair or desk role");
    }
}
