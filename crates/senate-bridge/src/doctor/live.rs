//! The checks that ask: the daemon over its socket, the Senate this
//! machine's bridge serves, and the Senate above. [`gather`] asks;
//! [`live`] judges what came back.

use std::collections::BTreeMap;
use std::time::Duration;

use senate_core::api::Me;
use senate_core::{CommitteeId, Locale, Seq};

use super::{Check, SERVICE_INSTALL};
use crate::client::ApiClient;
use crate::config::Hosted;
use crate::delivery::codex::{AppServer, CodexConfig, DAEMON_BOOTSTRAP, Transport};
use crate::ipc::{self, Registration, Request, SessionsReply, StatusReply, UplinkStatus};
use crate::paths::HOSTED_KEY;
use crate::state::Store;
use crate::{BridgeConfig, Paths, binding};

/// How long one question to the daemon or a Senate may take.
const ASK: Duration = Duration::from_secs(5);

/// One Senate as asked: its address and what `/v1/me` answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub url: String,
    pub me: Result<Me, String>,
}

/// What was asked and answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    /// This `senate`'s version.
    pub version: String,
    /// Why senate.toml could not be read, if it could not.
    pub config: Result<(), String>,
    /// The daemon's status, or why it could not be asked.
    pub daemon: Result<StatusReply, String>,
    /// The hosted sessions; `None` with the daemon down.
    pub sessions: Option<SessionsReply>,
    /// The Senate this machine's bridge serves: the one it hosts, else the
    /// one at `server_url`. `None` when there is none to ask (no config, or
    /// the machine's own Senate with the daemon down).
    pub served: Option<Probe>,
    /// Whether the served Senate is this machine's own (`[local]`).
    pub hosted: bool,
    /// The Senate above, when the machine hosts its own under one.
    pub parent: Option<Probe>,
    /// What the uplink last wrote of its registration, when under a parent.
    pub registration: Option<UplinkStatus>,
    /// The log epoch the daemon last saw from the served Senate.
    pub epoch: Option<String>,
    /// Committees where the daemon's cursor is beyond the Senate's head,
    /// with that cursor.
    pub ahead: Vec<(CommitteeId, Seq)>,
    /// The saved worktree bindings.
    pub bindings: Vec<binding::Row>,
    /// Whether the Codex app-server answered: `None` when there is no
    /// Codex to ask (no `codex` and no `codex.listen`).
    pub codex: Option<Result<(), CodexProbe>>,
}

/// Why the Codex app-server did not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexProbe {
    /// Its daemon is not running; the text says how to start it.
    /// `managed`: the managed daemon (no `codex.listen`), which
    /// `codex app-server daemon bootstrap` starts and keeps running.
    Down { text: String, managed: bool },
    /// Something else went wrong.
    Failed(String),
}

/// Connect to the Codex app-server and `initialize`, if there is one to ask.
async fn probe_codex(paths: &Paths) -> Option<Result<(), CodexProbe>> {
    let config = CodexConfig::load(&paths.config()).ok()?;
    let transport = config.transport().ok()?;
    if let Transport::Proxy { program } = &transport {
        let found = if program.is_absolute() {
            program.is_file()
        } else {
            program
                .to_str()
                .and_then(crate::install::find_on_path)
                .is_some()
        };
        if !found {
            return None;
        }
    }
    Some(
        match tokio::time::timeout(ASK, AppServer::connect(&transport)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) if e.is_daemon_down() => Err(CodexProbe::Down {
                text: e.to_string(),
                managed: matches!(transport, Transport::Proxy { .. }),
            }),
            Ok(Err(e)) => Err(CodexProbe::Failed(e.to_string())),
            Err(_) => Err(CodexProbe::Failed(format!(
                "{transport}: no answer in {}s",
                ASK.as_secs()
            ))),
        },
    )
}

async fn probe(url: &str, token: &str) -> Probe {
    let me = match ApiClient::new(url, token) {
        Ok(client) => match tokio::time::timeout(ASK, client.me()).await {
            Ok(Ok(me)) => Ok(me),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("no answer in {}s", ASK.as_secs())),
        },
        Err(e) => Err(e.to_string()),
    };
    Probe {
        url: url.to_owned(),
        me,
    }
}

/// The address and token of the Senate this machine hosts, as `senated
/// --machine` last wrote them.
fn hosted_address(paths: &Paths) -> Option<(String, String)> {
    let dir = paths.hosted(HOSTED_KEY);
    let listen = std::fs::read_to_string(dir.join("listen")).ok()?;
    let token = std::fs::read_to_string(dir.join("token")).ok()?;
    Some((format!("http://{}", listen.trim()), token.trim().to_owned()))
}

/// Ask everything [`live`] judges.
pub async fn gather(paths: &Paths) -> Live {
    let daemon = ipc::request::<StatusReply>(
        &paths.socket(),
        &Request::Status,
        Duration::from_millis(500),
        ASK,
    )
    .await
    .map_err(|e| e.to_string());
    let sessions = match &daemon {
        Ok(_) => ipc::request::<SessionsReply>(
            &paths.socket(),
            &Request::Sessions,
            Duration::from_millis(500),
            ASK,
        )
        .await
        .ok(),
        Err(_) => None,
    };
    let mut live = Live {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        config: Ok(()),
        daemon,
        sessions,
        served: None,
        hosted: false,
        parent: None,
        registration: None,
        epoch: None,
        ahead: Vec::new(),
        bindings: binding::list(paths),
        codex: probe_codex(paths).await,
    };
    let config = match BridgeConfig::load_unchecked(&paths.config()) {
        Ok(config) => config,
        Err(e) => {
            live.config = Err(e.to_string());
            return live;
        }
    };
    let hosted = Hosted::load(&paths.config()).unwrap_or_default();
    live.hosted = hosted.local.is_some();
    let served = if live.hosted {
        // The machine's own Senate runs inside the daemon.
        hosted_address(paths).filter(|_| live.daemon.is_ok())
    } else {
        Some((config.server_url.clone(), config.token.clone()))
    };
    if hosted.under_a_parent(&config) {
        live.parent = Some(probe(&config.server_url, &config.token).await);
        live.registration = UplinkStatus::read(&paths.hosted(HOSTED_KEY));
    }
    let Some((url, token)) = served else {
        return live;
    };
    live.served = Some(probe(&url, &token).await);
    let Ok(store) = Store::open_read_only(&paths.state_db()) else {
        return live;
    };
    live.epoch = store.epoch(&url).ok().flatten();
    // The furthest any agent's cursor got, by committee.
    let mut furthest: BTreeMap<CommitteeId, Seq> = BTreeMap::new();
    for (_, committee, cursor) in store.cursors().unwrap_or_default() {
        let seq = furthest.entry(committee).or_insert(Seq::ZERO);
        *seq = (*seq).max(cursor.delivered);
    }
    if let Ok(client) = ApiClient::new(&url, &token) {
        for (committee, seq) in furthest {
            if seq == Seq::ZERO {
                continue;
            }
            if let Ok(Err(e)) =
                tokio::time::timeout(ASK, client.events(&committee, seq, Some(1))).await
                && e.is_cursor_ahead()
            {
                live.ahead.push((committee, seq));
            }
        }
    }
    live
}

/// `(major, minor, patch)` of a version like `0.4.0` or `0.4.0-dev`.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// This machine against the Senate above it: a parent on a newer minor
/// release may rely on what this machine does not have yet (aliases, seat
/// duty).
fn version_check(id: &str, ours: &str, probe: &Probe, l: Locale) -> Option<Check> {
    let me = probe.me.as_ref().ok()?;
    let Some(theirs) = &me.version else {
        return Some(Check::ok(
            id,
            l.pick(
                format!("{} 的版本未知（较旧的服务器）；本机为 {ours}", probe.url),
                format!(
                    "{}'s version is unknown (an older server); this machine runs {ours}",
                    probe.url
                ),
            ),
        ));
    };
    let newer = match (parse_version(theirs), parse_version(ours)) {
        (Some((a, b, _)), Some((c, d, _))) => (a, b) > (c, d),
        _ => false,
    };
    Some(if newer {
        Check::warn(
            id,
            l.pick(
                format!("{} 运行 {theirs}，本机是 {ours}：请更新本机", probe.url),
                format!(
                    "{} runs {theirs}, this machine {ours}: update this machine",
                    probe.url
                ),
            ),
            Some("cargo install senate senated"),
        )
    } else {
        Check::ok(
            id,
            l.pick(
                format!("{} 运行 {theirs}，本机是 {ours}", probe.url),
                format!("{} runs {theirs}, this machine {ours}", probe.url),
            ),
        )
    })
}

fn reach(id: &str, probe: &Probe, l: Locale) -> Check {
    match &probe.me {
        Ok(me) => Check::ok(
            id,
            l.pick(
                format!(
                    "{} 有回应：{}，你是 {}",
                    probe.url,
                    me.senate_path(),
                    me.person
                ),
                format!(
                    "{} answers: {}, you are {}",
                    probe.url,
                    me.senate_path(),
                    me.person
                ),
            ),
        ),
        Err(e) => Check::fail(
            id,
            l.pick(
                format!("无法访问 {}：{e}", probe.url),
                format!("cannot reach {}: {e}", probe.url),
            ),
            None::<String>,
        ),
    }
}

/// Judge what [`gather`] found.
pub fn live(live: &Live, l: Locale) -> Vec<Check> {
    let mut checks = Vec::new();
    if let Err(e) = &live.config {
        checks.push(Check::fail(
            "config",
            l.pick(
                format!("无法读取 senate.toml：{e}"),
                format!("cannot read senate.toml: {e}"),
            ),
            None::<String>,
        ));
    }
    match &live.daemon {
        Ok(status) => {
            checks.push(Check::ok(
                "daemon",
                l.pick(
                    format!("在运行（pid {}，{}）", status.pid, status.version),
                    format!("running (pid {}, {})", status.pid, status.version),
                ),
            ));
            if status.version != live.version {
                checks.push(Check::warn(
                    "daemon.version",
                    l.pick(
                        format!(
                            "守护进程运行 {}，这个 senate 是 {}：重启守护进程",
                            status.version, live.version
                        ),
                        format!(
                            "the daemon runs {}, this senate is {}: restart the daemon",
                            status.version, live.version
                        ),
                    ),
                    Some(SERVICE_INSTALL),
                ));
            }
        }
        Err(e) => checks.push(Check::fail(
            "daemon",
            l.pick(format!("没有运行：{e}"), format!("not running: {e}")),
            Some(SERVICE_INSTALL),
        )),
    }
    if let Some(sessions) = &live.sessions {
        checks.extend(session_checks(sessions, l));
    }
    if let Some(codex) = &live.codex {
        checks.push(codex_check(codex, l));
    }
    if let Some(parent) = &live.parent {
        checks.push(reach("uplink.parent", parent, l));
        checks.push(registration_check(live.registration.as_ref(), l));
        checks.extend(version_check("uplink.version", &live.version, parent, l));
    }
    if let Some(served) = &live.served {
        checks.push(reach("senate.server", served, l));
        if !live.hosted {
            checks.extend(version_check("senate.version", &live.version, served, l));
        }
        if let Ok(me) = &served.me {
            checks.push(epoch_check(live.epoch.as_deref(), &me.epoch, l));
            checks.push(cursor_check(&live.ahead, l));
            checks.push(committee_check(&live.bindings, &me.committees, l));
        }
    }
    checks
}

fn session_checks(sessions: &SessionsReply, l: Locale) -> Vec<Check> {
    let running = sessions.rows.iter().filter(|r| r.is_running()).count();
    let stopped = sessions.rows.len() - running;
    let mut checks = vec![Check::ok(
        "sessions.hosted",
        l.pick(
            format!("{running} 个托管会话在运行，{stopped} 个已停止"),
            format!("{running} hosted sessions running, {stopped} stopped"),
        ),
    )];
    if !sessions.orphans.is_empty() {
        checks.push(Check::warn(
            "sessions.orphans",
            l.pick(
                format!("{} 个托管终端在运行但没有会话记录", sessions.orphans.len()),
                format!(
                    "{} hosted terminals run with no session on record",
                    sessions.orphans.len()
                ),
            ),
            Some("senate session prune"),
        ));
    }
    for row in sessions.rows.iter().filter(|r| r.is_running()) {
        let Some(prompt) = &row.prompt else {
            continue;
        };
        let label = row.label();
        let tool = prompt.tool.as_deref().unwrap_or("?");
        let since = prompt.since.format("%H:%M");
        checks.push(Check::warn(
            format!("sessions.prompt.{label}"),
            l.pick(
                format!("{label} 自 {since} 起在等待权限确认（{tool}）"),
                format!("{label} waits on a permission prompt ({tool}) since {since}"),
            ),
            Some(format!("senate attach {label}")),
        ));
    }
    checks
}

/// Hosted Codex sessions start and resume through the app-server daemon.
fn codex_check(codex: &Result<(), CodexProbe>, l: Locale) -> Check {
    match codex {
        Ok(()) => Check::ok(
            "codex.app_server",
            l.pick("Codex app-server 有回应", "the Codex app-server answers"),
        ),
        Err(CodexProbe::Down { text, managed }) => Check::fail(
            "codex.app_server",
            l.pick(
                format!("Codex app-server 没有运行，托管的 Codex 无法启动或恢复：{text}"),
                format!("hosted Codex sessions cannot start or restore: {text}"),
            ),
            managed.then_some(DAEMON_BOOTSTRAP),
        ),
        Err(CodexProbe::Failed(e)) => Check::warn(
            "codex.app_server",
            l.pick(
                format!("Codex app-server 没有正常回应：{e}"),
                format!("the Codex app-server did not answer properly: {e}"),
            ),
            None::<String>,
        ),
    }
}

fn registration_check(status: Option<&UplinkStatus>, l: Locale) -> Check {
    let id = "uplink.registration";
    let Some(status) = status else {
        return Check::warn(
            id,
            l.pick(
                "还没有登记记录：守护进程还没连上上级 Senate",
                "no registration recorded: the daemon has not reached the Senate above yet",
            ),
            None::<String>,
        );
    };
    let name = &status.name;
    match &status.registration {
        Registration::Registered => Check::ok(
            id,
            l.pick(
                format!("已作为 {name} 在 {} 登记", status.url),
                format!("registered as {name} at {}", status.url),
            ),
        ),
        Registration::Pending => Check::warn(
            id,
            l.pick(
                format!("{name} 在 {} 的登记尚未完成", status.url),
                format!("{name}'s registration at {} is pending", status.url),
            ),
            None::<String>,
        ),
        Registration::Refused { reason } => Check::fail(
            id,
            l.pick(
                format!("{} 拒绝了 {name} 的登记：{reason}", status.url),
                format!("{} refused {name}'s registration: {reason}", status.url),
            ),
            None::<String>,
        ),
        Registration::Unreachable { error } => Check::fail(
            id,
            l.pick(
                format!("登记时无法访问 {}：{error}", status.url),
                format!("{} was unreachable to register: {error}", status.url),
            ),
            None::<String>,
        ),
    }
}

fn epoch_check(recorded: Option<&str>, server: &str, l: Locale) -> Check {
    match recorded {
        Some(recorded) if !server.is_empty() && recorded != server => Check::warn(
            "senate.epoch",
            l.pick(
                format!(
                    "守护进程记录的日志纪元是 {recorded}，服务器现在是 {server}：重启后会重置游标"
                ),
                format!(
                    "the daemon recorded log epoch {recorded}, the server is at {server}: a restart resets the cursors"
                ),
            ),
            Some(SERVICE_INSTALL),
        ),
        _ => Check::ok(
            "senate.epoch",
            l.pick("日志纪元一致", "the log epoch matches"),
        ),
    }
}

fn cursor_check(ahead: &[(CommitteeId, Seq)], l: Locale) -> Check {
    match ahead {
        [] => Check::ok(
            "senate.cursors",
            l.pick("没有超前于日志的游标", "no cursor is ahead of the log"),
        ),
        [(committee, seq), ..] => Check::fail(
            "senate.cursors",
            l.pick(
                format!(
                    "{} 个委员会的游标超前于日志，例如 {committee} 在 {seq}",
                    ahead.len()
                ),
                format!(
                    "cursors are ahead of the log on {} committees, e.g. {committee} at {seq}",
                    ahead.len()
                ),
            ),
            Some(SERVICE_INSTALL),
        ),
    }
}

fn committee_check(rows: &[binding::Row], known: &[CommitteeId], l: Locale) -> Check {
    let unknown: Vec<(&binding::Row, &str)> = rows
        .iter()
        .filter(|r| r.stale.is_none())
        .filter_map(|r| Some((r, r.committee.as_deref()?)))
        .filter(|(_, c)| !known.iter().any(|k| k.as_str() == *c))
        .collect();
    match unknown.as_slice() {
        [] => Check::ok(
            "bindings.committee",
            l.pick(
                "每个工作区绑定的委员会服务器都认得",
                "every bound committee is known to the server",
            ),
        ),
        [(row, committee), ..] => Check::warn(
            "bindings.committee",
            l.pick(
                format!(
                    "{} 个绑定的委员会服务器还不认得（还没有消息），例如 {} 绑定的 {committee}",
                    unknown.len(),
                    row.worktree.display()
                ),
                format!(
                    "{} bound committees are unknown to the server (no messages yet), e.g. {committee} for {}",
                    unknown.len(),
                    row.worktree.display()
                ),
            ),
            None::<String>,
        ),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_minor_release() {
        assert_eq!(parse_version("0.4.0"), Some((0, 4, 0)));
        assert_eq!(parse_version("1.2.3-dev+abc"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("x"), None);
    }
}
