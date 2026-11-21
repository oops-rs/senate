//! `senate.toml` (formerly `bridge.toml`): who this machine is on the Senate server.
//!
//! ```toml
//! server_url = "https://senate.example.com"
//! token = "<the person's token>"
//! person = "alice"
//! # Optional: the language of everything the bridge writes for people and
//! # agents (`zh` or `en`). Defaults to the deployment default (`en`).
//! locale = "zh"
//! # Optional: keep these committee streams open for this machine's agents
//! # even with no session bound.
//! committees = ["refund-v2"]
//! # Optional: characters of the committee brief a session start injects.
//! brief_chars = 6000
//! # Optional: also bind a session to the committee a branch is named for,
//! # `<prefix>/<committee>/<person>`. Off by default; `branch_prefix` alone
//! # does not turn it on. When on, it is read before `.senate/committee`.
//! committee_from_branch = true
//! # Optional: the first segment of `<prefix>/<committee>/<person>` branches
//! # (the convention above, and the Speaker's work branches).
//! branch_prefix = "feature"
//! # Optional: `senate setup clean` forgets the queued items and cursors of an
//! # agent with no session and no contact for this many days.
//! free_after_days = 30
//! # Optional: at most this many Senate-hosted sessions on the machine.
//! # Absent: no limit. 0: none may start.
//! hosted_session_limit = 4
//!
//! # Optional: the agent each kind of coding session speaks as. These are
//! # the defaults.
//! [agents]
//! claude = "claude"
//! codex = "codex"
//!
//! # Optional: what this machine's agents do, by default, on a committee
//! # (one line). Posted as a seat's duty when it joins a committee where it
//! # has none; a duty set on the seat wins.
//! [agent]
//! duty = "reviews the server crate"
//! ```
//!
//! # A machine is a person running agents
//!
//! The token names a **person**. The bridge never speaks as "the person's
//! agent" in the singular: each coding session is one senator, chosen per
//! session ([`crate::agent`]), a flat name the person runs. By default a
//! Claude Code session is `claude` and a Codex session `codex`; `[agents]`
//! renames them, and a session can pin its own name with `SENATE_AGENT` or
//! a `.senate/agent` file.
//!
//! # One Senate
//!
//! A machine serves one Senate: the one at `server_url` (a team's), or the
//! one `senated --machine` hosts for it (`[local]`, which fills in the
//! address and token itself). Senates nested under an agent
//! (`[[preside]]`) are no longer hosted: an agent's own team is a child
//! Senate.
//!
//! Unknown keys are ignored so a newer file still loads.

use std::fmt;
use std::path::{Path, PathBuf};

use senate_core::{
    BranchNaming, CommitteeId, DEFAULT_BRANCH_PREFIX, Locale, Name, SenatePath, SenatorId,
    SenatorName, SpeakerName,
};
use serde::{Deserialize, Serialize};

use crate::session::Runtime;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// Base URL of the Senate server, e.g. `https://senate.example.com`.
    /// Empty in a file for a machine used alone: `senated --machine` fills
    /// it in for the Senate it hosts.
    #[serde(default)]
    pub server_url: String,
    /// The person's bearer token. It names the person to the server.
    #[serde(default)]
    pub token: String,
    /// The person's handle. Every agent this bridge seats is under it.
    pub person: Name,
    /// The language of everything the bridge writes for people and agents.
    #[serde(default)]
    pub locale: Locale,
    /// Committees whose streams stay open with no session bound.
    #[serde(default)]
    pub committees: Vec<CommitteeId>,
    /// Most characters of the committee brief injected at session start; 0
    /// injects none.
    #[serde(default = "default_brief_chars")]
    pub brief_chars: usize,
    /// Whether a `<prefix>/<committee>/<person>` branch name binds a
    /// session to that committee ([`crate::session`]), ahead of
    /// `.senate/committee`. Off unless set: `branch_prefix` alone does not
    /// enable it.
    #[serde(default)]
    pub committee_from_branch: bool,
    /// `senate setup clean` forgets what an agent with no session and no contact
    /// for this many days left in the daemon's state.
    #[serde(default = "default_free_after_days")]
    pub free_after_days: u64,
    /// Maximum running or starting Senate-hosted sessions. `None` (the
    /// key absent): no limit. Zero disables starting and restoring hosted
    /// sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_session_limit: Option<usize>,
    /// First segment of a developer branch, `<prefix>/<committee>/<person>`.
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,
    /// The agent name each kind of session speaks as.
    #[serde(default)]
    pub agents: AgentNames,
    /// Defaults for every agent this machine seats.
    #[serde(default, skip_serializing_if = "AgentDefaults::is_empty")]
    pub agent: AgentDefaults,
    /// The Senate above the one this bridge serves, when the machine hosts
    /// its own Senate under a parent (`[local]` with `server_url` and
    /// `token`): `senated --machine` sets it, it is never read from the
    /// file. A qualified name is resolved here, nearest first ([own,
    /// parent]); a person's message for a seat above is posted there as
    /// them, and the desk's forward to a seat above as the child's office.
    #[serde(skip)]
    pub uplink: Option<Uplink>,
}

/// `[agent]`: defaults for every agent this machine seats.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefaults {
    /// The seat's duty ([`senate_core::duty`]) the bridge posts when an
    /// agent joins a committee where its seat has none. A duty set on the
    /// seat (by the agent, its person or the chair) wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty: Option<String>,
}

impl AgentDefaults {
    pub fn is_empty(&self) -> bool {
        self.duty.is_none()
    }

    /// The default duty, normalized; `None` when unset or blank.
    pub fn duty(&self) -> Option<String> {
        self.duty
            .as_deref()
            .map(senate_core::duty::normalize)
            .filter(|d| !d.is_empty())
    }
}

/// The Senate above, as the machine reaches it: its address and the
/// person's token there.
#[derive(Clone, PartialEq, Eq)]
pub struct Uplink {
    pub url: String,
    pub token: String,
}

impl fmt::Debug for Uplink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Uplink")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// `[agents]`: the agent a session of each harness speaks as, unless the
/// session pins another ([`crate::agent`]): one flat senator name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNames {
    #[serde(default = "default_claude")]
    pub claude: SenatorName,
    #[serde(default = "default_codex")]
    pub codex: SenatorName,
    #[serde(default = "default_agy")]
    pub agy: SenatorName,
    #[serde(default = "default_acp")]
    pub acp: SenatorName,
}

fn default_claude() -> SenatorName {
    Runtime::Claude
        .as_str()
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid name"))
}

fn default_codex() -> SenatorName {
    Runtime::Codex
        .as_str()
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid name"))
}

fn default_agy() -> SenatorName {
    "agy"
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid name"))
}

fn default_acp() -> SenatorName {
    "acp"
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid name"))
}

impl Default for AgentNames {
    fn default() -> Self {
        Self {
            claude: default_claude(),
            codex: default_codex(),
            agy: default_agy(),
            acp: default_acp(),
        }
    }
}

impl AgentNames {
    /// The agent for a session of `runtime`.
    pub fn for_runtime(&self, runtime: Runtime) -> &SenatorName {
        match runtime {
            Runtime::Claude => &self.claude,
            Runtime::Codex => &self.codex,
            Runtime::Agy => &self.agy,
            Runtime::Acp => &self.acp,
        }
    }
}

/// Default of [`BridgeConfig::brief_chars`].
pub const DEFAULT_BRIEF_CHARS: usize = 6_000;

fn default_brief_chars() -> usize {
    DEFAULT_BRIEF_CHARS
}

fn default_free_after_days() -> u64 {
    crate::binding::DEFAULT_FREE_AFTER_DAYS
}

fn default_branch_prefix() -> String {
    DEFAULT_BRANCH_PREFIX.to_string()
}

impl fmt::Debug for BridgeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeConfig")
            .field("server_url", &self.server_url)
            .field("token", &"<redacted>")
            .field("person", &self.person)
            .field("locale", &self.locale)
            .field("committees", &self.committees)
            .field("brief_chars", &self.brief_chars)
            .field("committee_from_branch", &self.committee_from_branch)
            .field("free_after_days", &self.free_after_days)
            .field("hosted_session_limit", &self.hosted_session_limit)
            .field("branch_prefix", &self.branch_prefix)
            .field("agents", &self.agents)
            .field("agent", &self.agent)
            .field("uplink", &self.uplink)
            .finish()
    }
}

/// What `senated --machine` hosts on this machine, read from the same
/// `senate.toml`: the machine's one Senate (`[local]`).
///
/// ```toml
/// [local]                         # alone, or under the Senate at server_url
/// name = "alice-desk"             # default: <person>-desk
/// speaker = "senator::alice"      # default: the person (see Hosted::office_config)
/// secretaries = ["claude"]        # who works the Speaker's desk, in order
/// listen = "127.0.0.1:7979"
/// ```
///
/// With `server_url` and `token` too, the machine is **under a parent**:
/// it hosts its Senate and registers it with the Senate at `server_url` as
/// a child, its Speaker the person (`/<parent>::senator::<person>`).
///
/// `[[preside]]` (a Senate nested under an agent) is still read, only so
/// the machine can say once that such Senates are no longer hosted and
/// where their logs stay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Hosted {
    #[serde(default)]
    pub local: Option<LocalSenate>,
    #[serde(default)]
    pub preside: Vec<PresideSenate>,
}

/// `[local]`: this machine's Senate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LocalSenate {
    /// The Senate's name. Absent: [`default_desk_name`] of the person.
    #[serde(default)]
    pub name: Option<Name>,
    /// The Speaker, a senator id: `senator::<name>` (a senator of this
    /// Senate: a root's), or `/<parent>::senator::<name>` (the parent's: a
    /// child's). A bare name is read for compatibility
    /// ([`Hosted::office_config`]). Absent: the person.
    #[serde(default)]
    pub speaker: Option<SpeakerName>,
    /// The Speaker's secretaries, in order: agents of this Senate (or the
    /// built-in session, by its name) who work the Speaker's desk; the
    /// first present acts.
    #[serde(default)]
    pub secretaries: Vec<SenatorName>,
    /// A loopback address to serve it on. Default: a free port, kept for
    /// the next start.
    #[serde(default)]
    pub listen: Option<String>,
}

/// `[[preside]]`, as an older file wrote it: a Senate one level down,
/// presided over by `agent`. No longer hosted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PresideSenate {
    /// The presiding agent, as written (a name or a path).
    pub agent: String,
    #[serde(default)]
    pub listen: Option<String>,
}

/// The name of a Senate hosted on a person's machine when `[local]` gives
/// none: the person's handle followed by `-desk` (`alice-desk`). Not the
/// bare handle: the Senate's name must not be taken for a person's handle
/// in the Senate above. A handle too long for the suffix is cut to fit.
pub fn default_desk_name(person: &Name) -> Name {
    const SUFFIX: &str = "-desk";
    let mut stem = person.as_str().to_owned();
    stem.truncate(63 - SUFFIX.len());
    let stem = stem.trim_end_matches('-');
    Name::new(format!("{stem}{SUFFIX}"))
        .unwrap_or_else(|_| Name::new("desk").unwrap_or_else(|_| unreachable!("a valid name")))
}

impl Hosted {
    /// `[local]` and `[[preside]]` of the file at `path`; none when it has
    /// neither.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let hosted: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(hosted)
    }

    /// The rules: a Senate is served (`[local]`, or the one at
    /// `server_url`); with both, the Senate at `server_url` is the parent
    /// of the machine's own, and its address and token are complete; a
    /// Speaker named in a Senate above needs one; the machine's Senate
    /// listens on loopback; its name is not an address keyword.
    pub fn validate(&self, config: &BridgeConfig) -> Result<(), String> {
        match (&self.local, config.server_url.is_empty()) {
            (Some(_), false) => config
                .validate()
                .map_err(|e| format!("the Senate above (server_url, token): {e}"))?,
            (None, true) => {
                return Err("set server_url and token (the Senate above), or [local]".into());
            }
            _ => {}
        }
        if let Some(local) = &self.local {
            if let Some(speaker) = &local.speaker
                && speaker.id.senate().is_some()
                && config.server_url.is_empty()
            {
                return Err(OfficeError::AboveWithoutParent {
                    written: speaker.to_string(),
                }
                .text(config.locale));
            }
            if let Some(listen) = &local.listen {
                check_loopback(listen)?;
            }
            senate_core::check_not_keyword(self.name(&config.person).as_str(), "a Senate")
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// The name of the Senate this machine hosts.
    pub fn name(&self, person: &Name) -> Name {
        self.local
            .as_ref()
            .and_then(|l| l.name.clone())
            .unwrap_or_else(|| default_desk_name(person))
    }

    /// Whether this machine hosts a Senate.
    pub fn is_empty(&self) -> bool {
        self.local.is_none()
    }

    /// Whether the machine's Senate sits under the Senate at `server_url`.
    pub fn under_a_parent(&self, config: &BridgeConfig) -> bool {
        self.local.is_some() && !config.server_url.is_empty()
    }

    /// The machine's office: its Speaker (a senator id), the Speaker's
    /// secretaries in order, and a note to show once when the file was
    /// read for compatibility. `parent` is the path of the Senate above,
    /// when the machine is under one and it answered.
    ///
    /// - absent: the person (`senator::<person>`; under a parent
    ///   `/<parent>::senator::<person>`);
    /// - `senator::<name>`: a senator of this Senate, for a root (the
    ///   person, an agent of it, or the built-in session). Under a parent,
    ///   `senator::<person>` is read as the parent's (noted); any other is
    ///   refused: a child's Speaker is a senator of its parent;
    /// - `/<path>::senator::<name>`: the parent's senator; `path` must be
    ///   the parent's;
    /// - a bare name, read for compatibility and noted: the person's own
    ///   handle is the person; any other (`speaker = "claude"`, as older
    ///   homes say) makes the person the Speaker and that agent the first
    ///   secretary. Never refused.
    pub fn office_config(
        &self,
        person: &Name,
        parent: Option<&SenatePath>,
    ) -> Result<(SpeakerName, Vec<SenatorName>, Option<OfficeNote>), Box<OfficeError>> {
        let local = self.local.clone().unwrap_or_default();
        let me = SenatorName::new(person.clone())
            .unwrap_or_else(|_| unreachable!("a person's handle is a senator name"));
        let own = |name: SenatorName| match parent {
            Some(parent) => SenatorId::of(parent.clone(), name),
            None => SenatorId::here(name),
        };
        let mut secretaries = Vec::new();
        let (speaker, note) = match local.speaker {
            None => (own(me.clone()), None),
            Some(written) if written.bare => {
                let name = written.id.name().clone();
                let speaker = own(me.clone());
                if name == me {
                    let note = OfficeNote::BareName {
                        written: name,
                        read_as: speaker.clone(),
                    };
                    (speaker, Some(note))
                } else {
                    secretaries.push(name.clone());
                    let note = OfficeNote::AgentWasSpeaker {
                        agent: name,
                        speaker: speaker.clone(),
                    };
                    (speaker, Some(note))
                }
            }
            Some(written) => match (written.id.senate(), parent) {
                (None, None) | (Some(_), None) => (written.id, None),
                (None, Some(_)) if written.id.name() == &me => {
                    let speaker = own(me.clone());
                    let note = OfficeNote::SpeakerIsAbove {
                        written: written.id,
                        read_as: speaker.clone(),
                    };
                    (speaker, Some(note))
                }
                (None, Some(parent)) => {
                    return Err(Box::new(OfficeError::NotAbove {
                        written: written.id,
                        person: own(me),
                        parent: parent.clone().absolute(),
                    }));
                }
                (Some(theirs), Some(parent)) => {
                    if theirs.clone().absolute() != parent.clone().absolute() {
                        return Err(Box::new(OfficeError::OtherParent {
                            written: written.id,
                            parent: parent.clone().absolute(),
                        }));
                    }
                    (written.id, None)
                }
            },
        };
        for secretary in local.secretaries {
            if !secretaries.contains(&secretary) {
                secretaries.push(secretary);
            }
        }
        Ok((SpeakerName::id(speaker), secretaries, note))
    }
}

/// A parent's agent named as the Speaker of the machine's Senate: refused
/// at start, before anything is registered. `agents_above` are the agents
/// the person runs in the Senate above (its `/v1/me`), when it answered;
/// `own_agents` the names this machine's sessions speak as. A Speaker that
/// is the person, or a senator of this Senate, is never refused here.
pub fn check_speaker_above(
    speaker: &SenatorId,
    person: &Name,
    agents_above: &[SenatorName],
    own_agents: &[SenatorName],
) -> Result<(), Box<OfficeError>> {
    if speaker.senate().is_none() || speaker.name().name() == person {
        return Ok(());
    }
    if agents_above.contains(speaker.name()) || own_agents.contains(speaker.name()) {
        return Err(Box::new(OfficeError::ParentAgent {
            speaker: speaker.clone(),
        }));
    }
    Ok(())
}

/// What the machine says once when `[local] speaker` was read for
/// compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficeNote {
    /// A bare name that is the person's handle.
    BareName {
        written: SenatorName,
        read_as: SenatorId,
    },
    /// A bare name of an agent (`speaker = "claude"`): the person is the
    /// Speaker, the agent the first secretary.
    AgentWasSpeaker {
        agent: SenatorName,
        speaker: SenatorId,
    },
    /// `senator::<person>` under a parent: the parent's senator.
    SpeakerIsAbove {
        written: SenatorId,
        read_as: SenatorId,
    },
}

impl OfficeNote {
    /// The note in `locale`.
    pub fn text(&self, locale: Locale) -> String {
        match (self, locale) {
            (Self::BareName { written, read_as }, Locale::Zh) => format!(
                "senate.toml 的 [local] speaker = \"{written}\" 是一个裸名字：按 senator id {} 读取。\
                 写成 speaker = \"{read_as}\"，这条提示就不再出现。",
                read_as.in_text()
            ),
            (Self::BareName { written, read_as }, Locale::En) => format!(
                "senate.toml: [local] speaker = \"{written}\" is a bare name, read as the senator id {}. \
                 Write speaker = \"{read_as}\" and this note stops.",
                read_as.in_text()
            ),
            (Self::AgentWasSpeaker { agent, speaker }, Locale::Zh) => format!(
                "senate.toml 的 [local] speaker = \"{agent}\" 把一个 agent 写成了这个 Senate 的 Speaker。\
                 现在 Speaker 是你 {}，{} 作为你的 secretary 处理 Speaker 的 desk（secretaries = [\"{agent}\"]）。\
                 写成 speaker = \"{speaker}\" 和 secretaries = [\"{agent}\"]，这条提示就不再出现；\
                 要让 agent 继续做这个 Senate 的 Speaker，写 speaker = \"senator::{agent}\"。",
                speaker.in_text(),
                senate_core::Address::senator(agent).in_text()
            ),
            (Self::AgentWasSpeaker { agent, speaker }, Locale::En) => format!(
                "senate.toml: [local] speaker = \"{agent}\" named an agent this Senate's Speaker. \
                 The Speaker is now you, {}; {} works your desk as your secretary \
                 (secretaries = [\"{agent}\"]). Write speaker = \"{speaker}\" and \
                 secretaries = [\"{agent}\"] and this note stops; to keep the agent as this \
                 Senate's Speaker, write speaker = \"senator::{agent}\".",
                speaker.in_text(),
                senate_core::Address::senator(agent).in_text()
            ),
            (Self::SpeakerIsAbove { written, read_as }, Locale::Zh) => format!(
                "senate.toml 的 [local] speaker = \"{written}\"：子 Senate 的 Speaker 是上一级 Senate 的 senator，\
                 按 {} 读取。写成 speaker = \"{read_as}\"，这条提示就不再出现。",
                read_as.in_text()
            ),
            (Self::SpeakerIsAbove { written, read_as }, Locale::En) => format!(
                "senate.toml: [local] speaker = \"{written}\": a child Senate's Speaker is a senator of \
                 the Senate above, read as {}. Write speaker = \"{read_as}\" and this note stops.",
                read_as.in_text()
            ),
        }
    }
}

/// Why the machine's office cannot be what `[local]` says.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.text(Locale::En))]
pub enum OfficeError {
    /// Under a parent, a senator of the machine's own Senate other than
    /// the person.
    NotAbove {
        written: SenatorId,
        person: SenatorId,
        parent: SenatePath,
    },
    /// A Senate path that is not the parent's.
    OtherParent {
        written: SenatorId,
        parent: SenatePath,
    },
    /// A Speaker in a Senate above, and no Senate above.
    AboveWithoutParent { written: String },
    /// A parent's agent as the Speaker.
    ParentAgent { speaker: SenatorId },
}

impl OfficeError {
    /// The refusal in `locale`.
    pub fn text(&self, locale: Locale) -> String {
        match (self, locale) {
            (
                Self::NotAbove {
                    written,
                    person,
                    parent,
                },
                Locale::Zh,
            ) => format!(
                "senate.toml 的 [local] speaker = \"{written}\"：这台机器的 Senate 在 Senate [{parent}] 之下，\
                 它的 Speaker 是上一级的 senator（通常就是你：speaker = \"{person}\"）；\
                 让本 Senate 的 agent 做子 Senate 的 Speaker 暂不支持，把它写进 secretaries。"
            ),
            (
                Self::NotAbove {
                    written,
                    person,
                    parent,
                },
                Locale::En,
            ) => format!(
                "senate.toml: [local] speaker = \"{written}\": this machine's Senate is under the Senate \
                 [{parent}], so its Speaker is a senator of the Senate above (usually you: \
                 speaker = \"{person}\"); an agent of this Senate as a child Senate's Speaker is not \
                 supported yet: list it in secretaries."
            ),
            (Self::OtherParent { written, parent }, Locale::Zh) => format!(
                "senate.toml 的 [local] speaker = \"{written}\" 指向另一个 Senate：这台机器的上一级是 Senate [{parent}]。"
            ),
            (Self::OtherParent { written, parent }, Locale::En) => format!(
                "senate.toml: [local] speaker = \"{written}\" names another Senate: the Senate above this \
                 machine's is [{parent}]."
            ),
            (Self::AboveWithoutParent { written }, Locale::Zh) => format!(
                "senate.toml 的 [local] speaker = \"{written}\" 指向上一级 Senate 的 senator，但没有配置上一级\
                 （server_url 和 token）。单独使用时写 speaker = \"senator::<你的名字>\"。"
            ),
            (Self::AboveWithoutParent { written }, Locale::En) => format!(
                "senate.toml: [local] speaker = \"{written}\" names a senator of a Senate above, and no \
                 Senate above is configured (server_url and token). Alone, write \
                 speaker = \"senator::<your handle>\"."
            ),
            (Self::ParentAgent { speaker }, Locale::Zh) => format!(
                "senate.toml 的 [local] speaker = \"{speaker}\" 是上一级 Senate 的 agent：\
                 由上一级的 agent 做子 Senate 的 Speaker 暂不支持。写你自己（或上一级的另一个 principal），\
                 把 agent 写进 secretaries。"
            ),
            (Self::ParentAgent { speaker }, Locale::En) => format!(
                "senate.toml: [local] speaker = \"{speaker}\" is an agent of the Senate above: a parent's \
                 agent as a child Senate's Speaker is not supported yet. Name yourself (or another \
                 principal of the Senate above) and list the agent in secretaries."
            ),
        }
    }
}

/// A hosted Senate listens on loopback only: its token never leaves the
/// machine, and neither does its traffic.
pub fn check_loopback(listen: &str) -> Result<(), String> {
    let addr: std::net::SocketAddr = listen
        .parse()
        .map_err(|e| format!("listen = {listen:?} is not an address: {e}"))?;
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "listen = {listen:?}: a Senate on this machine listens on loopback only"
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "no configuration at {0}; write one with server_url, token and person (under a team's Senate), or run `senate setup init --local` (alone)"
    )]
    Missing(PathBuf),
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

impl BridgeConfig {
    /// A configuration with every optional key at its default.
    pub fn new(server_url: &str, token: &str, person: Name) -> Self {
        Self {
            server_url: server_url.to_string(),
            token: token.to_string(),
            person,
            locale: Locale::default(),
            committees: Vec::new(),
            brief_chars: DEFAULT_BRIEF_CHARS,
            committee_from_branch: false,
            free_after_days: default_free_after_days(),
            hosted_session_limit: None,
            branch_prefix: default_branch_prefix(),
            agents: AgentNames::default(),
            agent: AgentDefaults::default(),
            uplink: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let config = Self::load_unchecked(path)?;
        config.validate().map_err(|message| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        })?;
        Ok(config)
    }

    /// Read and parse, without [`BridgeConfig::validate`]: for `senated
    /// --machine`, which fills in the Senate it hosts itself before the
    /// configuration is complete.
    pub fn load_unchecked(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigError::Missing(path.to_path_buf()));
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// The `locale` of the file at `path`, for the thin clients (the MCP
    /// shim, `install`) that need only the language. Anything unreadable,
    /// including a missing file, gives the default: these clients must
    /// still work before the bridge is configured.
    pub fn locale_at(path: &Path) -> Locale {
        #[derive(Deserialize)]
        struct OnlyLocale {
            #[serde(default)]
            locale: Locale,
        }
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<OnlyLocale>(&text).ok())
            .map(|f| f.locale)
            .unwrap_or_default()
    }

    /// The Senate's address is an http(s) URL, and the token is set.
    pub fn validate(&self) -> Result<(), String> {
        if self.server_url.is_empty() {
            return Err("server_url is empty".into());
        }
        if !(self.server_url.starts_with("http://") || self.server_url.starts_with("https://")) {
            return Err(format!(
                "server_url must start with http:// or https://, got {:?}",
                self.server_url
            ));
        }
        if self.token.trim().is_empty() {
            return Err(format!("the token for {} is empty", self.server_url));
        }
        Ok(())
    }

    /// How developer branches are named on this machine.
    pub fn branches(&self) -> BranchNaming {
        BranchNaming::new(&self.branch_prefix)
    }

    /// The naming a branch is read by to find its committee, when
    /// `committee_from_branch` is on.
    pub fn branch_convention(&self) -> Option<BranchNaming> {
        self.committee_from_branch.then(|| self.branches())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_redacts() {
        let c: BridgeConfig = toml::from_str(
            r#"
            server_url = "http://localhost:8080"
            token = "secret"
            person = "alice"
            committees = ["refund-v2"]
            future_key = 1
            "#,
        )
        .unwrap();
        assert!(c.validate().is_ok());
        assert_eq!(c.person.as_str(), "alice");
        assert_eq!(c.committees, vec![CommitteeId::new("refund-v2").unwrap()]);
        assert_eq!(c.brief_chars, DEFAULT_BRIEF_CHARS);
        assert_eq!(c.locale, Locale::default());
        assert_eq!(c.branches().prefix(), "feature");
        assert_eq!(c.agents, AgentNames::default());
        assert!(!format!("{c:?}").contains("secret"));
        let c: BridgeConfig = toml::from_str(
            "server_url = \"http://x\"\ntoken = \"t\"\nperson = \"a\"\nbrief_chars = 0\n",
        )
        .unwrap();
        assert_eq!(c.brief_chars, 0);
    }

    #[test]
    fn the_hosted_session_limit_is_none_unless_set() {
        let base = "server_url = \"http://x\"\ntoken = \"t\"\nperson = \"a\"\n";
        let limit = |extra: &str| {
            toml::from_str::<BridgeConfig>(&format!("{base}{extra}"))
                .unwrap()
                .hosted_session_limit
        };
        assert_eq!(limit(""), None, "absent: no limit");
        assert_eq!(limit("hosted_session_limit = 6\n"), Some(6));
        assert_eq!(
            limit("hosted_session_limit = 0\n"),
            Some(0),
            "0: none allowed"
        );
        assert_eq!(
            BridgeConfig::new("http://x", "t", Name::new("a").unwrap()).hosted_session_limit,
            None
        );
    }

    #[test]
    fn locale_agents_and_branch_prefix_are_configurable() {
        let c: BridgeConfig = toml::from_str(
            r#"
            server_url = "http://x"
            token = "t"
            person = "alice"
            locale = "zh-CN"
            branch_prefix = "team/work"
            [agents]
            codex = "reviewer"
            "#,
        )
        .unwrap();
        assert_eq!(c.locale, Locale::Zh);
        assert_eq!(c.branches().prefix(), "team/work");
        assert_eq!(c.agents.for_runtime(Runtime::Claude).to_string(), "claude");
        assert_eq!(c.agents.for_runtime(Runtime::Codex).to_string(), "reviewer");
        // An agent name is one flat senator name: validated where it is
        // written. A path (an owner, or a nested agent) is not one.
        for bad in [
            "[agents]\nclaude = \"Claude\"",
            "[agents]\ncodex = \"a//b\"",
            "[agents]\ncodex = \"claude/codex\"",
            "[agents]\ncodex = \"alice/codex\"",
            "locale = \"fr\"",
            "person = \"Alice\"",
        ] {
            let text = "server_url = \"http://x\"\ntoken = \"t\"\nperson = \"alice\"\n".to_string();
            let text = if bad.starts_with("person") {
                text.replace("person = \"alice\"", bad)
            } else {
                format!("{text}{bad}\n")
            };
            assert!(toml::from_str::<BridgeConfig>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_thin_clients_read_only_the_locale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.toml");
        assert_eq!(BridgeConfig::locale_at(&path), Locale::default());
        std::fs::write(&path, "locale = \"zh\"\n[codex]\nlisten = \"x\"\n").unwrap();
        assert_eq!(BridgeConfig::locale_at(&path), Locale::Zh);
        std::fs::write(&path, "locale = \"fr\"\n").unwrap();
        assert_eq!(BridgeConfig::locale_at(&path), Locale::default());
    }

    #[test]
    fn missing_file_is_named() {
        let err = BridgeConfig::load(Path::new("/nonexistent/bridge.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Missing(_)));
    }

    #[test]
    fn a_machine_serves_one_senate() {
        // Replaces "several Senates are listed with their presiders": no
        // Senate is nested under an agent, so `[[senates]]` is gone; an
        // older file that still has it loads, and the entries are ignored.
        let c: BridgeConfig = toml::from_str(
            r#"
            server_url = "https://senate.example.com"
            token = "t"
            person = "alice"
            [[senates]]
            presider = "claude"
            server_url = "http://127.0.0.1:7979"
            token = "nested-secret"
            "#,
        )
        .unwrap();
        assert!(c.validate().is_ok());
        assert_eq!(c.server_url, "https://senate.example.com");
        for bad in [
            "person = \"a\"\n",
            "person = \"a\"\nserver_url = \"ftp://y\"\ntoken = \"u\"\n",
            "person = \"a\"\nserver_url = \"http://y\"\ntoken = \" \"\n",
        ] {
            let c: BridgeConfig = toml::from_str(bad).unwrap();
            assert!(c.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn what_a_machine_hosts_is_checked_against_the_rest() {
        let parse = |text: &str| -> (BridgeConfig, Hosted) {
            (toml::from_str(text).unwrap(), toml::from_str(text).unwrap())
        };
        let (config, hosted) = parse(
            "person = \"alice\"\n[local]\nspeaker = \"claude\"\nlisten = \"127.0.0.1:7979\"\n",
        );
        assert_eq!(hosted.validate(&config), Ok(()));
        let local = hosted.local.clone().unwrap();
        assert_eq!(local.speaker.unwrap().to_string(), "claude");
        // The Senate's name: given, or the person's handle and `-desk`.
        assert_eq!(hosted.name(&config.person).as_str(), "alice-desk");
        let (config, hosted) = parse("person = \"alice\"\n[local]\nname = \"north-desk\"\n");
        assert_eq!(hosted.validate(&config), Ok(()));
        assert_eq!(hosted.name(&config.person).as_str(), "north-desk");
        // An older file's nested Senates are read, to be named, not hosted.
        let (config, hosted) = parse(
            "person = \"alice\"\n[local]\nspeaker = \"claude\"\n[[preside]]\nagent = \"claude\"\n",
        );
        assert_eq!(hosted.validate(&config), Ok(()));
        assert_eq!(hosted.preside[0].agent, "claude");

        // Under a parent: the machine's Senate and the Senate above it.
        let (config, hosted) = parse(
            "person = \"alice\"\nserver_url = \"https://s\"\ntoken = \"t\"\n[local]\nspeaker = \"/acme::senator::alice\"\n",
        );
        assert_eq!(hosted.validate(&config), Ok(()));
        assert!(hosted.under_a_parent(&config));

        for bad in [
            // Under a parent with no token there.
            "person = \"a\"\nserver_url = \"https://s\"\ntoken = \" \"\n[local]\n",
            // A Speaker above, and no Senate above.
            "person = \"a\"\n[local]\nspeaker = \"/acme::senator::a\"\n",
            // Neither.
            "person = \"a\"\n[[preside]]\nagent = \"claude\"\n",
            // Off the machine.
            "person = \"a\"\n[local]\nlisten = \"0.0.0.0:7979\"\n",
            // A keyword is not a Senate's name.
            "person = \"a\"\n[local]\nname = \"speaker\"\n",
        ] {
            let (config, hosted) = parse(bad);
            assert!(hosted.validate(&config).is_err(), "{bad}");
        }
        // The speaker is one flat name.
        assert!(toml::from_str::<Hosted>("[local]\nspeaker = \"alice/claude\"\n").is_err());
    }

    fn office(
        local: &str,
        parent: Option<&str>,
    ) -> Result<(String, Vec<String>, Option<OfficeNote>), Box<OfficeError>> {
        let hosted: Hosted = toml::from_str(&format!("[local]\n{local}")).unwrap();
        let parent: Option<SenatePath> = parent.map(|p| p.parse().unwrap());
        hosted
            .office_config(&Name::new("alice").unwrap(), parent.as_ref())
            .map(|(speaker, secretaries, note)| {
                (
                    speaker.to_string(),
                    secretaries.iter().map(ToString::to_string).collect(),
                    note,
                )
            })
    }

    #[test]
    fn the_office_defaults_to_the_person_at_either_level() {
        assert_eq!(
            office("", None).unwrap(),
            ("senator::alice".into(), vec![], None)
        );
        assert_eq!(
            office("", Some("/acme")).unwrap(),
            ("/acme::senator::alice".into(), vec![], None)
        );
        assert_eq!(
            office("secretaries = [\"claude\", \"codex\"]\n", Some("acme")).unwrap(),
            (
                "/acme::senator::alice".into(),
                vec!["claude".into(), "codex".into()],
                None
            )
        );
        // An agent as the Speaker of a root, as written: today's alone.
        assert_eq!(
            office("speaker = \"senator::claude\"\n", None).unwrap(),
            ("senator::claude".into(), vec![], None)
        );
        assert_eq!(
            office(
                "speaker = \"senator::claude\"\nsecretaries = [\"codex\"]\n",
                None
            )
            .unwrap(),
            ("senator::claude".into(), vec!["codex".into()], None)
        );
        assert_eq!(
            office("speaker = \"/acme::senator::alice\"\n", Some("/acme"))
                .unwrap()
                .0,
            "/acme::senator::alice"
        );
    }

    #[test]
    fn an_agent_named_speaker_by_an_older_home_becomes_the_persons_secretary() {
        let (speaker, secretaries, note) = office(
            "speaker = \"claude\"\nsecretaries = [\"codex\", \"claude\"]\n",
            None,
        )
        .unwrap();
        assert_eq!(speaker, "senator::alice");
        assert_eq!(secretaries, ["claude", "codex"]);
        let note = note.expect("said once");
        assert!(matches!(note, OfficeNote::AgentWasSpeaker { .. }));
        let en = note.text(Locale::En);
        let zh = note.text(Locale::Zh);
        for text in [&en, &zh] {
            assert!(text.contains("[senator::alice]"), "{text}");
            assert!(text.contains("[senator::claude]"), "{text}");
            assert!(
                text.contains("Speaker") && text.contains("Senate"),
                "{text}"
            );
        }
        assert!(en.contains("works your desk as your secretary"), "{en}");
        assert!(zh.contains("secretary"), "{zh}");
        // Under a parent the same: the person, as the parent's senator.
        let (speaker, secretaries, _) = office("speaker = \"claude\"\n", Some("/acme")).unwrap();
        assert_eq!(speaker, "/acme::senator::alice");
        assert_eq!(secretaries, ["claude"]);
        // The person's bare handle is the person, noted.
        let (speaker, secretaries, note) = office("speaker = \"alice\"\n", None).unwrap();
        assert_eq!((speaker.as_str(), secretaries.len()), ("senator::alice", 0));
        assert!(matches!(note, Some(OfficeNote::BareName { .. })));
    }

    #[test]
    fn a_childs_speaker_is_the_parents_senator() {
        // The person written as this Senate's: read as the parent's, noted.
        let (speaker, _, note) = office("speaker = \"senator::alice\"\n", Some("/acme")).unwrap();
        assert_eq!(speaker, "/acme::senator::alice");
        assert!(matches!(note, Some(OfficeNote::SpeakerIsAbove { .. })));
        // An agent of the child: not supported.
        let err = office("speaker = \"senator::claude\"\n", Some("/acme")).unwrap_err();
        assert!(matches!(*err, OfficeError::NotAbove { .. }));
        assert!(err.text(Locale::En).contains("not supported yet"));
        assert!(err.text(Locale::Zh).contains("暂不支持"));
        // Another Senate's senator: the path must be the parent's.
        let err = office("speaker = \"/other::senator::alice\"\n", Some("/acme")).unwrap_err();
        assert!(matches!(*err, OfficeError::OtherParent { .. }));
        assert!(
            err.text(Locale::En).contains("[/acme]"),
            "{}",
            err.text(Locale::En)
        );
    }

    #[test]
    fn a_parents_agent_as_speaker_is_refused_in_both_languages() {
        let alice = Name::new("alice").unwrap();
        let claude: SenatorName = "claude".parse().unwrap();
        let above: SenatorId = "/acme::senator::claude".parse().unwrap();
        let err =
            check_speaker_above(&above, &alice, &[], std::slice::from_ref(&claude)).unwrap_err();
        assert!(err.text(Locale::En).contains("not supported yet"), "{err}");
        assert!(err.text(Locale::Zh).contains("暂不支持"));
        assert!(err.text(Locale::En).contains("/acme::senator::claude"));
        assert!(check_speaker_above(&above, &alice, std::slice::from_ref(&claude), &[]).is_err());
        // The person, a principal above, or a senator here: not refused here.
        for ok in [
            "/acme::senator::alice",
            "/acme::senator::bob",
            "senator::claude",
        ] {
            let id: SenatorId = ok.parse().unwrap();
            assert!(
                check_speaker_above(
                    &id,
                    &alice,
                    std::slice::from_ref(&claude),
                    std::slice::from_ref(&claude)
                )
                .is_ok(),
                "{ok}"
            );
        }
    }

    #[test]
    fn the_default_desk_name_is_never_the_bare_handle() {
        let name = |s: &str| Name::new(s).unwrap();
        assert_eq!(default_desk_name(&name("wendell")).as_str(), "wendell-desk");
        let long = "a".repeat(63);
        let desk = default_desk_name(&name(&long));
        assert!(desk.as_str().ends_with("-desk") && desk.as_str().len() <= 63);
        assert_ne!(default_desk_name(&name("bob")).as_str(), "bob");
    }

    #[test]
    fn rejects_bad_url() {
        let c: BridgeConfig =
            toml::from_str("server_url = \"ftp://x\"\ntoken = \"t\"\nperson = \"a\"\n").unwrap();
        assert!(c.validate().is_err());
    }
}
