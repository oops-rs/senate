//! `senated doctor --config <file>`: check a Senate server the way `senate
//! setup doctor` checks a developer machine, with the same report. It
//! reads the configuration, looks at the log without creating or
//! migrating it, asks the running server whether it answers, and reads
//! whether a chat gateway adapter is attached.

use std::path::Path;
use std::time::Duration;

use senate_bridge::doctor::Check;
use senate_core::Locale;
use senate_speaker::{ModelReport, ModelVerdict};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::ParkedReason;
use crate::SenatedConfig;
use crate::gateway_ipc::{GatewayStatus, RemoteGateway};

const ASK: Duration = Duration::from_secs(10);

/// Every server check, for the configuration at `path`.
pub async fn checks(path: &Path) -> Vec<Check> {
    checks_with_model(path, true).await
}

/// Run the server checks, optionally making live model calls.
pub async fn checks_with_model(path: &Path, probe_model: bool) -> Vec<Check> {
    let config = match SenatedConfig::load(path) {
        Ok(config) => config,
        Err(e) => {
            return vec![Check::fail(
                "config",
                format!("{}: {e}", path.display()),
                None::<String>,
            )];
        }
    };
    let l = config.server.locale;
    let mut checks = vec![Check::ok(
        "config",
        l.pick(
            format!("{} 有效", path.display()),
            format!("{} is valid", path.display()),
        ),
    )];
    checks.push(database_check(&config.server.database_url, l).await);
    let running = health(&config.server.listen).await;
    checks.push(match &running {
        Ok(addr) => Check::ok(
            "server",
            l.pick(format!("{addr} 有回应"), format!("{addr} answers")),
        ),
        Err(e) => Check::fail(
            "server",
            l.pick(
                format!("服务器没有回应：{e}"),
                format!("the server does not answer: {e}"),
            ),
            Some(format!("senated --config {}", path.display())),
        ),
    });
    checks.extend(gateway_checks(&config, running.is_ok(), l));
    if probe_model {
        let secretary = crate::config::load_secretary(&config, path);
        let report = Box::pin(crate::check_model(secretary.into(), None)).await;
        checks.push(model_check(&report, l));
    }
    checks
}

fn model_check(report: &ModelReport, l: Locale) -> Check {
    if let Some(problem) = &report.problem {
        return Check::warn(
            "model",
            l.pick(
                format!("{problem}；固定语法仍可用"),
                format!("{problem}. Fixed syntax still works."),
            ),
            None::<String>,
        );
    }

    let mut fixes = Vec::new();
    for tier in &report.tiers {
        let name = &tier.tier;
        if !tier.passed() {
            fixes.push(l.pick(
                format!("检查 {name} 的 base_url、密钥及工具调用支持"),
                format!("check the {name} model's base_url, key and tool support"),
            ));
        }
        if let Some(seconds) = tier.suggested_triage_deadline_secs {
            fixes.push(l.pick(
                format!("{name}：设置 triage_deadline_secs = {seconds}"),
                format!("{name}: set triage_deadline_secs = {seconds}"),
            ));
        }
    }
    let fix = (!fixes.is_empty()).then(|| fixes.join("; "));
    match report.verdict() {
        ModelVerdict::Passed => {
            let passed = l.pick(
                "所有模型层均通过自检",
                "all model tiers passed their self-checks",
            );
            if fix.is_some() {
                Check::warn(
                    "model",
                    l.pick(
                        format!("{passed}，但分流时限过短"),
                        format!("{passed}, but the triage deadline is too short"),
                    ),
                    fix,
                )
            } else {
                Check::ok("model", passed)
            }
        }
        ModelVerdict::Degraded { serving } => Check::warn(
            "model",
            l.pick(
                format!("部分模型自检失败，{serving} 仍在服务"),
                format!("a model tier failed its self-checks. The {serving} tier is serving."),
            ),
            fix,
        ),
        ModelVerdict::Down => Check::fail(
            "model",
            l.pick(
                "所有模型层均未通过自检，仅可使用固定语法",
                "no model tier passed its self-checks. Only fixed syntax is available.",
            ),
            fix,
        ),
    }
}

async fn database_check(url: &str, l: Locale) -> Check {
    use senate_log::Inspection;
    let shown = senate_server::redact_url(url);
    match tokio::time::timeout(ASK, senate_log::inspect(url)).await {
        Err(_) => Check::fail(
            "database",
            l.pick(
                format!("{shown} 在 {} 秒内没有回应", ASK.as_secs()),
                format!("{shown} did not answer in {}s", ASK.as_secs()),
            ),
            None::<String>,
        ),
        Ok(Err(e)) => Check::fail(
            "database",
            l.pick(format!("{shown}：{e}"), format!("{shown}: {e}")),
            None::<String>,
        ),
        Ok(Ok(Inspection::Memory)) => Check::warn(
            "database",
            l.pick(
                "日志在内存中：senated 停止后就没了",
                "the log is in memory: it is gone when senated stops",
            ),
            None::<String>,
        ),
        Ok(Ok(Inspection::Missing)) => Check::ok(
            "database",
            l.pick(
                format!("{shown} 中还没有日志；senated 首次启动时创建"),
                format!("no log at {shown} yet; senated creates it when it first starts"),
            ),
        ),
        Ok(Ok(Inspection::Log { pending: true, .. })) => Check::warn(
            "database",
            l.pick(
                format!("{shown} 的日志需要迁移；senated 下次启动时进行"),
                format!("the log at {shown} needs migrating; senated does it when it next starts"),
            ),
            None::<String>,
        ),
        Ok(Ok(Inspection::Log { epoch, .. })) => Check::ok(
            "database",
            l.pick(
                format!(
                    "{shown} 的日志是最新的（纪元 {}）",
                    epoch.as_deref().unwrap_or("?")
                ),
                format!(
                    "the log at {shown} is current (epoch {})",
                    epoch.as_deref().unwrap_or("?")
                ),
            ),
        ),
    }
}

/// The address to reach a server listening on `listen` from this host.
fn reachable(listen: &str) -> Option<String> {
    let (host, port) = listen.rsplit_once(':')?;
    if port == "0" {
        return None;
    }
    let host = match host {
        "0.0.0.0" | "" => "127.0.0.1",
        "[::]" => "[::1]",
        host => host,
    };
    Some(format!("{host}:{port}"))
}

/// `GET /healthz` on the server, answered `200 … ok`.
async fn health(listen: &str) -> Result<String, String> {
    let addr = reachable(listen).ok_or_else(|| format!("listen = {listen:?} has no fixed port"))?;
    let ask = async {
        let mut stream = tokio::net::TcpStream::connect(&addr)
            .await
            .map_err(|e| format!("{addr}: {e}"))?;
        stream
            .write_all(
                format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let mut answer = String::new();
        stream
            .read_to_string(&mut answer)
            .await
            .map_err(|e| e.to_string())?;
        let status = answer.lines().next().unwrap_or_default();
        if status.contains(" 200 ") {
            Ok(addr.clone())
        } else {
            Err(format!("{addr} answered {status:?}"))
        }
    };
    tokio::time::timeout(ASK, ask)
        .await
        .unwrap_or_else(|_| Err(format!("{addr}: no answer in {}s", ASK.as_secs())))
}

fn gateway_checks(config: &SenatedConfig, running: bool, l: Locale) -> Vec<Check> {
    if !config.server.extra.contains_key("chat_gateway") {
        return Vec::new();
    }
    let remote = match RemoteGateway::from_config(config) {
        Ok(Some(remote)) => remote,
        Ok(None) => return Vec::new(),
        Err(e) => {
            return vec![Check::fail(
                "gateway.config",
                format!("[chat_gateway]: {e:#}"),
                None::<String>,
            )];
        }
    };
    let mut checks = vec![Check::ok(
        "gateway.config",
        l.pick("[chat_gateway] 有效", "[chat_gateway] is valid"),
    )];
    if !running {
        return checks;
    }
    let socket = remote.socket();
    {
        #[cfg(unix)]
        let listening = {
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(socket).is_ok_and(|m| m.file_type().is_socket())
        };
        // A pipe's listener marks its path with a file (senate_bridge::local).
        #[cfg(windows)]
        let listening = std::fs::metadata(socket).is_ok_and(|m| m.is_file());
        checks.push(if listening {
            Check::ok(
                "gateway.socket",
                l.pick(
                    format!("{} 在监听", socket.display()),
                    format!("{} is listening", socket.display()),
                ),
            )
        } else {
            Check::fail(
                "gateway.socket",
                l.pick(
                    format!("{} 不是套接字：服务器没有在提供网关", socket.display()),
                    format!(
                        "{} is not a socket: the server does not serve the gateway",
                        socket.display()
                    ),
                ),
                None::<String>,
            )
        });
    }
    let status = GatewayStatus::read(&config.server.data_dir);
    checks.push(match &status {
        Some(status) if status.connected => Check::ok(
            "gateway.connected",
            l.pick(
                format!("{} 适配器自 {} 起已连接", status.platform, status.at.format("%F %T UTC")),
                format!(
                    "the {} adapter is connected since {}",
                    status.platform,
                    status.at.format("%F %T UTC")
                ),
            ),
        ),
        Some(status) => Check::fail(
            "gateway.connected",
            l.pick(
                format!(
                    "没有 {} 适配器连接（自 {} 起）",
                    status.platform,
                    status.at.format("%F %T UTC")
                ),
                format!(
                    "no {} adapter is connected (since {})",
                    status.platform,
                    status.at.format("%F %T UTC")
                ),
            ),
            None::<String>,
        ),
        None => Check::warn(
            "gateway.connected",
            l.pick(
                "不知道是否有适配器连接：服务器没有写下状态（版本较旧？）",
                "whether an adapter is connected is unknown: the server wrote no status (an older senated?)",
            ),
            None::<String>,
        ),
    });
    if let Some(status) = &status {
        checks.push(parked_check(status, l));
    }
    checks
}

/// The bound groups the gateway cannot join by itself, with what fixes each.
fn parked_check(status: &GatewayStatus, l: Locale) -> Check {
    if status.parked.is_empty() {
        return Check::ok(
            "gateway.groups",
            l.pick(
                "没有卡住的已绑定群",
                "no bound group is stuck joining or leaving",
            ),
        );
    }
    let lines: Vec<String> = status
        .parked
        .iter()
        .map(|p| {
            let since = p.since.format("%F %T UTC");
            match &p.reason {
                ParkedReason::Rejected { error } => l.pick(
                    format!("{}：平台拒绝加入或退出（{error}），自 {since} 起不再重试", p.group_id),
                    format!(
                        "{}: the platform refused to join or leave it ({error}); not retried since {since}",
                        p.group_id
                    ),
                ),
                ParkedReason::NoRealMessage => l.pick(
                    format!("{}：没有真实消息可用来加入（模拟消息不行），自 {since} 起等待", p.group_id),
                    format!(
                        "{}: no real message to join it by (simulated ones cannot); waiting since {since}",
                        p.group_id
                    ),
                ),
            }
        })
        .collect();
    Check::warn(
        "gateway.groups",
        lines.join("; "),
        Some(l.pick(
            "在群里发一条真实消息，或重新绑定该群；重启 senated 也会再试一次",
            "send a real message in the group, or bind it again; restarting senated also tries again",
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use senate_bridge::doctor::Status;

    #[test]
    fn model_verdict_and_deadline_determine_severity_and_fix() {
        use senate_speaker::{CheckResult, TierReport};

        let tier = |name: &str, passed, suggestion| TierReport {
            tier: name.into(),
            provider: None,
            model: None,
            base_url: None,
            wire: None,
            checks: vec![CheckResult {
                check: "triage".into(),
                passed,
                reason: "scripted result".into(),
                millis: 4000,
            }],
            triage_deadline_secs: Some(5),
            suggested_triage_deadline_secs: suggestion,
        };
        let check = |tiers| {
            model_check(
                &ModelReport {
                    committee: None,
                    problem: None,
                    tiers,
                },
                Locale::En,
            )
        };
        assert_eq!(check(vec![tier("primary", true, None)]).status, Status::Ok);
        let slow = check(vec![tier("primary", true, Some(10))]);
        assert_eq!(slow.status, Status::Warn);
        assert_eq!(
            slow.fix.as_deref(),
            Some("primary: set triage_deadline_secs = 10")
        );
        for (primary, serving, failed) in [
            (true, "primary", "fallback"),
            (false, "fallback", "primary"),
        ] {
            let degraded = check(vec![
                tier("primary", primary, None),
                tier("fallback", !primary, None),
            ]);
            assert_eq!(degraded.status, Status::Warn);
            assert!(
                degraded
                    .found
                    .contains(&format!("{serving} tier is serving"))
            );
            assert!(degraded.fix.unwrap().contains(failed));
        }
        let down = check(vec![tier("primary", false, Some(10))]);
        assert_eq!(down.status, Status::Fail);
        let json = serde_json::to_value(&down).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"id":"model","status":"fail","found":down.found,"fix":down.fix})
        );
        assert!(
            json["fix"]
                .as_str()
                .unwrap()
                .contains("triage_deadline_secs = 10")
        );
    }

    #[test]
    fn a_wildcard_listen_is_reached_on_loopback() {
        assert_eq!(reachable("0.0.0.0:7878").as_deref(), Some("127.0.0.1:7878"));
        assert_eq!(reachable("[::]:7878").as_deref(), Some("[::1]:7878"));
        assert_eq!(reachable("10.0.0.2:80").as_deref(), Some("10.0.0.2:80"));
        assert_eq!(reachable("127.0.0.1:0"), None);
    }

    fn by_id<'a>(checks: &'a [Check], id: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no {id} in {checks:#?}"))
    }

    #[tokio::test]
    async fn a_stopped_server_with_a_fresh_log_and_a_bad_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let file = dir.path().join("senated.toml");
        // A port nothing listens on: bind one, then let it go.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(
            &file,
            format!(
                "listen = \"127.0.0.1:{port}\"\ndatabase_url = 'sqlite:{}'\ndata_dir = '{}'\n\
                 [chat_gateway]\nsocket = '{}'\nplatform = \"chat\"\ntoken_env = \"SENATED_DOCTOR_TEST_UNSET\"\n",
                dir.path().join("log.db").display(),
                data.display(),
                dir.path().join("gw.sock").display(),
            ),
        )
        .unwrap();
        let checks = checks(&file).await;
        assert_eq!(by_id(&checks, "config").status, Status::Ok, "{checks:#?}");
        assert_eq!(by_id(&checks, "model").status, Status::Warn);
        let db = by_id(&checks, "database");
        assert_eq!(db.status, Status::Ok);
        assert!(db.found.contains("no log"), "{db:?}");
        assert!(!dir.path().join("log.db").exists(), "doctor creates no log");
        assert_eq!(by_id(&checks, "server").status, Status::Fail);
        let gateway = by_id(&checks, "gateway.config");
        assert_eq!(gateway.status, Status::Fail);
        assert!(
            gateway.found.contains("environment variable"),
            "{gateway:?}"
        );
    }

    #[tokio::test]
    async fn an_unreadable_config_is_one_failure() {
        let checks = checks(Path::new("/nonexistent/senated.toml")).await;
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Fail);
    }

    #[test]
    fn the_gateway_status_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(GatewayStatus::read(dir.path()), None);
        let status = GatewayStatus {
            platform: "chat".into(),
            connected: true,
            at: chrono::Utc::now(),
            parked: vec![crate::ParkedGroup {
                group_id: "g_1".into(),
                reason: ParkedReason::Rejected {
                    error: "rejected: unknown chat".into(),
                },
                since: chrono::Utc::now(),
            }],
        };
        std::fs::write(
            dir.path().join(GatewayStatus::FILE),
            serde_json::to_string(&status).unwrap(),
        )
        .unwrap();
        assert_eq!(GatewayStatus::read(dir.path()), Some(status));
    }

    #[test]
    fn an_older_status_without_parked_groups_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(GatewayStatus::FILE),
            r#"{"platform":"chat","connected":true,"at":"2026-11-22T00:00:00Z"}"#,
        )
        .unwrap();
        let status = GatewayStatus::read(dir.path()).unwrap();
        assert_eq!(status.parked, []);
        assert_eq!(parked_check(&status, Locale::En).status, Status::Ok);
    }

    #[test]
    fn parked_groups_are_a_warning_with_the_fix() {
        let since = chrono::Utc::now();
        let status = GatewayStatus {
            platform: "chat".into(),
            connected: true,
            at: since,
            parked: vec![
                crate::ParkedGroup {
                    group_id: "g_refused".into(),
                    reason: ParkedReason::Rejected {
                        error: "rejected: unknown chat".into(),
                    },
                    since,
                },
                crate::ParkedGroup {
                    group_id: "g_quiet".into(),
                    reason: ParkedReason::NoRealMessage,
                    since,
                },
            ],
        };
        let check = parked_check(&status, Locale::En);
        assert_eq!(check.status, Status::Warn, "{check:?}");
        assert!(check.found.contains("g_refused"), "{check:?}");
        assert!(check.found.contains("unknown chat"), "{check:?}");
        assert!(check.found.contains("g_quiet"), "{check:?}");
        let fix = check.fix.as_deref().unwrap_or_default();
        assert!(
            fix.contains("real message") && fix.contains("bind it again"),
            "{fix}"
        );
    }
}
