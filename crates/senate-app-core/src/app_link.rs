//! `senate://open/v1/…` links, for the app's one validator: the protocol
//! crate's parser ([`senate_observe_protocol::link`]) handed to Swift as a
//! typed route, and the two lookups only the daemon can answer (which Senate
//! an `id:` selector names, and which ended session an Observe History link
//! means). A link only says where to look; nothing here acts.

use senate_bridge::ipc::Request;
use senate_observe::AdminRequest;
use senate_observe_protocol::link::{AppLink, Target};
use serde::Deserialize;

use crate::ffi::{AppCore, CoreError};

/// Where a link asks the app to go.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AppRoute {
    Committee { name: String },
    /// A local session id, or `committee::alias`.
    Session { selector: String },
    Thread { committee: String, root: u64 },
    /// A senator's name, or `committee::alias`.
    Senator { name: String },
    Desk,
    DeskItem { committee: String, seq: u64 },
    Approvals,
    Inbox,
    History,
    /// A local session id, or an Observe History `obs-…` id.
    HistoryEntry { id: String },
    Profile,
}

/// A link that passed the v1 grammar.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AppLinkRoute {
    pub route: AppRoute,
    /// The decoded `?senate=` selector: `/team/local` or `id:<installation>`;
    /// `None` is the Senate the app has selected.
    pub senate: Option<String>,
    /// The link written canonically.
    pub canonical: String,
}

/// Parse a link by the v1 grammar; `None` for anything else, including
/// `senate://intent/…` and every authority but `open`.
#[uniffi::export]
pub fn parse_app_link(url: String) -> Option<AppLinkRoute> {
    let link: AppLink = url.parse().ok()?;
    let route = match link.target().clone() {
        Target::Committee(name) => AppRoute::Committee { name },
        Target::Session(selector) => AppRoute::Session { selector },
        Target::Thread(committee, seq) => AppRoute::Thread {
            committee,
            root: seq.0.get(),
        },
        Target::Senator(name) => AppRoute::Senator { name },
        Target::Desk => AppRoute::Desk,
        Target::DeskItem(committee, seq) => AppRoute::DeskItem {
            committee,
            seq: seq.0.get(),
        },
        Target::Approvals => AppRoute::Approvals,
        Target::Inbox => AppRoute::Inbox,
        Target::History => AppRoute::History,
        Target::HistoryEntry(id) => AppRoute::HistoryEntry { id },
        Target::Profile => AppRoute::Profile,
    };
    Some(AppLinkRoute {
        route,
        senate: link.senate().map(str::to_owned),
        canonical: link.to_string(),
    })
}

#[derive(Deserialize)]
struct Installation {
    id: String,
}

#[uniffi::export]
impl AppCore {
    /// This machine's public Senate ID, the one Observe links carry as
    /// `?senate=id:…` (the person-guarded `observe_admin` identity; no
    /// token). Fails where the daemon hosts no Observe.
    pub async fn observe_installation_id(&self) -> Result<String, CoreError> {
        let request = Request::ObserveAdmin {
            command: AdminRequest::Identity,
        };
        let installation: Installation = self
            .on_runtime(move |driver| async move { driver.read(&request).await })
            .await?;
        Ok(installation.id)
    }

    /// The local session behind an Observe History id (`obs-…`), if it is
    /// an ended session of this machine's primary Senate.
    pub async fn observe_history_session(&self, id: String) -> Result<Option<String>, CoreError> {
        let request = Request::ObserveAdmin {
            command: AdminRequest::HistorySession { id },
        };
        self.on_runtime(move |driver| async move { driver.read(&request).await })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Cases {
        valid: Vec<(String, String)>,
        invalid: Vec<String>,
    }

    #[test]
    fn the_public_fixtures_parse_the_same_through_the_facade() {
        let cases: Cases = serde_json::from_str(include_str!(
            "../../../docs/observe/v1/fixtures/links.json"
        ))
        .unwrap();
        for (input, canonical) in cases.valid {
            let parsed = parse_app_link(format!("senate://open/v1/{input}")).unwrap();
            assert_eq!(parsed.canonical, format!("senate://open/v1/{canonical}"));
        }
        for input in cases.invalid {
            assert_eq!(parse_app_link(input.clone()), None, "{input}");
        }
        assert_eq!(parse_app_link("senate://intent/v1/reply/x".into()), None);
    }

    #[test]
    fn routes_carry_their_decoded_values() {
        let parsed =
            parse_app_link("senate://open/v1/session/senate%3A%3Aagent?senate=%2Fteam%2Flocal".into())
                .unwrap();
        assert_eq!(
            parsed.route,
            AppRoute::Session {
                selector: "senate::agent".into()
            }
        );
        assert_eq!(parsed.senate.as_deref(), Some("/team/local"));
        assert_eq!(
            parse_app_link("senate://open/v1/desk/senate/42".into()).map(|p| p.route),
            Some(AppRoute::DeskItem {
                committee: "senate".into(),
                seq: 42
            })
        );
    }
}
