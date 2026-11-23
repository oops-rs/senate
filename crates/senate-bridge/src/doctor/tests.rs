//! Each check against a fake home, good and bad.

use std::cell::RefCell;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use senate_core::api::Me;
use senate_core::{CommitteeId, EventId, Locale, Seq};
use serde_json::{Value, json};

use super::*;
use crate::ipc::{
    HostState, HostedPrompt, Registration, SessionRow, SessionsReply, StatusReply, UplinkStatus,
};
use crate::service::{Cmd, Manager, Platform, Ran, Spec};

const EN: Locale = Locale::En;

fn by_id<'a>(checks: &'a [Check], id: &str) -> &'a Check {
    checks
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no check {id} in {checks:#?}"))
}

fn program(dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

// ---- the service -----------------------------------------------------------

struct Manned {
    running: bool,
    asked: RefCell<usize>,
}

impl Manager for Manned {
    fn run(&self, _: &Cmd) -> io::Result<Ran> {
        *self.asked.borrow_mut() += 1;
        Ok(Ran {
            ok: true,
            stdout: if self.running {
                "state = running\n".into()
            } else {
                "state = waiting\n".into()
            },
            stderr: String::new(),
        })
    }

    fn sleep(&self, _: std::time::Duration) {}
}

fn manager(running: bool) -> Manned {
    Manned {
        running,
        asked: RefCell::new(0),
    }
}

fn spec(home: &Path, path: &str) -> Spec {
    Spec {
        platform: Platform::Macos,
        user_home: home.to_owned(),
        senate_home: home.join(".senate"),
        uid: 501,
        path: Some(path.to_owned()),
        config: home.join(".senate/senate.toml"),
    }
}

#[test]
fn a_current_running_service_that_finds_its_programs_is_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bin = home.join("bin");
    program(&bin, "claude");
    program(&bin, "codex");
    let senated = program(&bin, "senated");
    let spec = spec(home, &bin.to_string_lossy());
    let plan = service::plan_install(&spec, &senated);
    std::fs::create_dir_all(plan.file.parent().unwrap()).unwrap();
    std::fs::write(&plan.file, &plan.contents).unwrap();

    let checks = service_checks(&spec, Some(&senated), &manager(true), EN);
    for id in [
        "service.file",
        "service.running",
        "service.program.claude",
        "service.program.codex",
        "service.program.agy",
    ] {
        assert_eq!(by_id(&checks, id).status, Status::Ok, "{checks:#?}");
    }
    assert!(
        by_id(&checks, "service.program.agy")
            .found
            .contains("not installed")
    );
}

#[test]
fn a_launch_agent_without_interactive_scheduling_needs_reinstall() {
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec(tmp.path(), "/usr/bin");
    let senated = program(tmp.path(), "senated");
    let plan = service::plan_install(&spec, &senated);
    let legacy = plan.contents.replace(
        "  <key>ProcessType</key>\n  <string>Interactive</string>\n",
        "",
    );
    std::fs::create_dir_all(plan.file.parent().unwrap()).unwrap();
    std::fs::write(&plan.file, legacy).unwrap();
    let checks = service_checks(&spec, Some(&senated), &manager(true), EN);
    let file = by_id(&checks, "service.file");
    assert_eq!(file.status, Status::Fail);
    assert!(file.found.contains("ProcessType"), "{file:?}");
    assert_eq!(file.fix.as_deref(), Some(SERVICE_INSTALL));
}

#[test]
fn a_stale_stopped_service_that_misses_a_program_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let old = home.join("old");
    let new = home.join("new");
    program(&new, "codex");
    let senated = program(&new, "senated");
    // Installed with a PATH that has no codex.
    let installed = spec(home, &old.to_string_lossy());
    let plan = service::plan_install(&installed, &senated);
    std::fs::create_dir_all(plan.file.parent().unwrap()).unwrap();
    std::fs::write(&plan.file, &plan.contents).unwrap();
    // And a recorded claude that is gone.
    std::fs::create_dir_all(home.join(".senate")).unwrap();
    std::fs::write(
        home.join(".senate/senate.toml"),
        "[companion]\nclaude_bin = \"/gone/claude\"\n",
    )
    .unwrap();

    let now = spec(home, &new.to_string_lossy());
    let checks = service_checks(&now, Some(&senated), &manager(false), EN);
    // Only the PATH differs: a warning naming what it misses, not a failure.
    let file = by_id(&checks, "service.file");
    assert_eq!(file.status, Status::Warn, "{file:?}");
    assert!(file.found.contains("does not find codex"), "{file:?}");
    assert_eq!(file.fix.as_deref(), Some(SERVICE_INSTALL));
    assert_eq!(by_id(&checks, "service.running").status, Status::Fail);
    let claude = by_id(&checks, "service.program.claude");
    assert_eq!(claude.status, Status::Fail);
    assert!(claude.found.contains("/gone/claude"), "{claude:?}");
    let codex = by_id(&checks, "service.program.codex");
    assert_eq!(codex.status, Status::Fail);
    assert!(codex.found.contains("cannot find codex"), "{codex:?}");
    assert_eq!(codex.fix.as_deref(), Some(SERVICE_INSTALL));
}

#[test]
fn a_minimal_shell_path_does_not_fail_a_complete_service_file() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bin = home.join("bin");
    program(&bin, "claude");
    program(&bin, "codex");
    let senated = program(&bin, "senated");
    // Installed from a full login shell.
    let full = format!("{}:/usr/bin:/bin", bin.display());
    let installed = spec(home, &full);
    let plan = service::plan_install(&installed, &senated);
    std::fs::create_dir_all(plan.file.parent().unwrap()).unwrap();
    std::fs::write(&plan.file, &plan.contents).unwrap();

    // doctor over SSH, with a PATH that has none of it.
    let ssh = spec(home, "/usr/bin:/bin");
    let checks = service_checks(&ssh, Some(&senated), &manager(true), EN);
    let file = by_id(&checks, "service.file");
    assert_eq!(file.status, Status::Ok, "{file:?}");
    assert!(file.found.contains("PATH differs"), "{file:?}");
    for id in ["service.program.claude", "service.program.codex"] {
        assert_eq!(by_id(&checks, id).status, Status::Ok, "{checks:#?}");
    }
}

#[test]
fn a_service_file_running_another_binary_still_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bin = home.join("bin");
    let senated = program(&bin, "senated");
    let spec = spec(home, "/usr/bin:/bin");
    let plan = service::plan_install(&spec, &senated);
    std::fs::create_dir_all(plan.file.parent().unwrap()).unwrap();
    let managed = spec.managed_senated().to_string_lossy().into_owned();
    let wrong = plan.contents.replace(&managed, "/somewhere/else/senated");
    assert_ne!(wrong, plan.contents);
    std::fs::write(&plan.file, wrong).unwrap();

    // Even with this shell's PATH in it, the program differs.
    let checks = service_checks(&spec, Some(&senated), &manager(true), EN);
    let file = by_id(&checks, "service.file");
    assert_eq!(file.status, Status::Fail, "{file:?}");
    assert!(file.found.contains("/somewhere/else/senated"), "{file:?}");

    // And with a different shell PATH too.
    let other = super::service::Spec {
        path: Some(bin.to_string_lossy().into_owned()),
        ..spec
    };
    let checks = service_checks(&other, Some(&senated), &manager(true), EN);
    assert_eq!(by_id(&checks, "service.file").status, Status::Fail);
}

#[test]
fn no_service_is_a_warning_and_a_missing_senated_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec(tmp.path(), "/nowhere");
    let m = manager(true);
    let checks = service_checks(&spec, Some(Path::new("/x/senated")), &m, EN);
    assert_eq!(by_id(&checks, "service.file").status, Status::Warn);
    assert!(checks.iter().all(|c| c.id != "service.running"));
    assert_eq!(
        *m.asked.borrow(),
        0,
        "no service manager asked without a file"
    );
    let checks = service_checks(&spec, None, &m, EN);
    assert_eq!(by_id(&checks, "service.file").status, Status::Fail);
}

// ---- registrations ---------------------------------------------------------

fn options(home: &Path) -> InstallOptions {
    InstallOptions {
        exe: home.join("bin/senate"),
        claude_dir: home.join(".claude"),
        config_dir_env: None,
        user_config: home.join(".claude.json"),
        claude_bin: PathBuf::from("claude"),
        codex_config: Some(home.join(".codex/config.toml")),
        gemini_dir: Some(home.join(".gemini")),
        locale: EN,
    }
}

fn write_json(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

/// Everything as `setup install` leaves it.
fn installed(opts: &InstallOptions) {
    let plan = install::plan(opts).unwrap();
    write_json(&plan.settings_path, &plan.settings_after);
    let exe = opts.exe.to_string_lossy();
    write_json(
        &opts.user_config,
        &json!({"mcpServers": {"senate": {"type": "stdio", "command": exe, "args": ["mcp"]}}}),
    );
    std::fs::create_dir_all(opts.command_path().parent().unwrap()).unwrap();
    std::fs::write(opts.command_path(), install::command_md(EN)).unwrap();
    let codex = plan.codex.unwrap();
    std::fs::create_dir_all(codex.path.parent().unwrap()).unwrap();
    std::fs::write(&codex.path, &codex.after).unwrap();
    let agy = plan.agy.unwrap();
    for file in [&agy.mcp, &agy.hooks, &agy.settings] {
        write_json(&file.path, &file.after);
    }
    assert!(install::plan(opts).unwrap().is_noop());
}

#[test]
fn registrations_are_current_after_install() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = options(tmp.path());
    installed(&opts);
    let checks = install_checks(&opts, EN);
    for check in &checks {
        assert_eq!(check.status, Status::Ok, "{check:?}");
    }
    for id in [
        "install.claude",
        "install.codex",
        "install.codex.approval",
        "install.agy",
        "install.agy.grant",
    ] {
        by_id(&checks, id);
    }
}

#[test]
fn an_old_hook_form_and_missing_approvals_fail_on_their_own() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = options(tmp.path());
    installed(&opts);
    // A hook from before `--runtime`.
    let settings = opts.settings_path();
    let text = std::fs::read_to_string(&settings).unwrap();
    std::fs::write(&settings, text.replace(" --runtime claude", "")).unwrap();
    // Codex without the approval mode, agy without the grant.
    let codex = opts.codex_config.clone().unwrap();
    let text = std::fs::read_to_string(&codex).unwrap();
    std::fs::write(
        &codex,
        text.replace("default_tools_approval_mode = \"approve\"", ""),
    )
    .unwrap();
    write_json(
        &tmp.path().join(".gemini/antigravity-cli/settings.json"),
        &json!({"permissions": {"allow": []}}),
    );

    let checks = install_checks(&opts, EN);
    let claude = by_id(&checks, "install.claude");
    assert_eq!(claude.status, Status::Fail);
    assert!(claude.found.contains("--runtime claude"), "{claude:?}");
    assert_eq!(claude.fix.as_deref(), Some(INSTALL));
    assert_eq!(by_id(&checks, "install.codex").status, Status::Ok);
    assert_eq!(
        by_id(&checks, "install.codex.approval").status,
        Status::Fail
    );
    assert_eq!(by_id(&checks, "install.agy").status, Status::Ok);
    let grant = by_id(&checks, "install.agy.grant");
    assert_eq!(grant.status, Status::Fail);
    assert!(grant.found.contains("mcp(senate/*)"));
}

#[test]
fn a_runtime_not_on_path_is_not_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let mut opts = options(tmp.path());
    opts.codex_config = None;
    opts.gemini_dir = None;
    let checks = install_checks(&opts, EN);
    assert_eq!(by_id(&checks, "install.codex").status, Status::Ok);
    assert_eq!(by_id(&checks, "install.agy").status, Status::Ok);
    assert_eq!(by_id(&checks, "install.claude").status, Status::Fail);
}

// ---- leftovers -------------------------------------------------------------

#[test]
fn no_leftovers_after_install() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = options(tmp.path());
    installed(&opts);
    let checks = leftovers::checks(&opts, EN);
    assert_eq!(checks.len(), 1);
    assert_eq!(by_id(&checks, "leftovers").status, Status::Ok);
}

#[test]
fn another_bridge_and_an_old_senate_path_are_leftovers() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = options(tmp.path());
    installed(&opts);
    let exe = opts.exe.to_string_lossy().to_string();
    // Claude: acme-bridge's hook and MCP server, and a second Senate MCP
    // server under another name at an old path.
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(opts.settings_path()).unwrap()).unwrap();
    settings["hooks"]["SessionStart"]
        .as_array_mut()
        .unwrap()
        .push(json!({"hooks": [{"type": "command", "command": "/u/.cargo/bin/acme-bridge hook session-start"}]}));
    write_json(&opts.settings_path(), &settings);
    write_json(
        &opts.user_config,
        &json!({"mcpServers": {
            "senate": {"command": exe, "args": ["mcp"]},
            "acme": {"command": "/u/.cargo/bin/acme-bridge", "args": ["mcp"]},
            "senate-old": {"command": "/old/senate-bridge", "args": ["mcp"]},
            "github": {"command": "npx", "args": ["github-mcp"]},
        }}),
    );
    // Codex: acme-bridge's server.
    let codex = opts.codex_config.clone().unwrap();
    let mut text = std::fs::read_to_string(&codex).unwrap();
    text.push_str(
        "\n[mcp_servers.acme]\ncommand = \"/u/.cargo/bin/acme-bridge\"\nargs = [\"mcp\"]\n",
    );
    std::fs::write(&codex, text).unwrap();
    // agy: a second named hook running an old Senate.
    let hooks = tmp.path().join(".gemini/config/hooks.json");
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    doc["old"] = json!({"SessionStart": [{"type": "command", "command": "/old/senate hook session-start --agy"}]});
    write_json(&hooks, &doc);

    let checks = leftovers::checks(&opts, EN);
    let ids: Vec<&str> = checks.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "leftovers.claude.mcp.acme",
            "leftovers.claude.mcp.senate-old",
            "leftovers.claude.hook.SessionStart",
            "leftovers.codex.mcp.acme",
            "leftovers.agy.hook.old.SessionStart",
        ],
        "{checks:#?}"
    );
    assert!(checks.iter().all(|c| c.status == Status::Warn));
    assert_eq!(
        by_id(&checks, "leftovers.claude.mcp.acme").fix.as_deref(),
        Some("claude mcp remove --scope user acme")
    );
    assert_eq!(
        by_id(&checks, "leftovers.codex.mcp.acme").fix.as_deref(),
        Some("codex mcp remove acme")
    );
    let hook = by_id(&checks, "leftovers.claude.hook.SessionStart");
    assert_eq!(hook.fix, None);
    assert!(
        hook.found.contains("acme-bridge hook session-start"),
        "{hook:?}"
    );
    assert!(hook.found.contains("its program is gone"), "{hook:?}");
    assert!(hook.found.contains("settings.json"), "{hook:?}");
}

// ---- bindings --------------------------------------------------------------

fn row(worktree: &str, committee: &str, stale: Option<&'static str>) -> binding::Row {
    binding::Row {
        repository: PathBuf::from("/r"),
        branch: Some("main".into()),
        worktree: PathBuf::from(worktree),
        committee: Some(committee.into()),
        saved_at: chrono::Utc::now(),
        head: None,
        stale,
        dir: PathBuf::from("/d"),
    }
}

#[test]
fn expired_bindings_are_a_warning_with_clean() {
    let rows = [row("/w/a", "a", None)];
    assert_eq!(binding_checks(&rows, EN)[0].status, Status::Ok);
    let rows = [
        row("/w/a", "a", None),
        row("/w/gone", "b", Some("worktree gone")),
    ];
    let check = &binding_checks(&rows, EN)[0];
    assert_eq!(check.status, Status::Warn);
    assert!(check.found.contains("/w/gone (worktree gone)"), "{check:?}");
    assert_eq!(check.fix.as_deref(), Some("senate setup clean --yes"));
}

// ---- WSL -------------------------------------------------------------------

#[test]
fn wsl_needs_systemd_linger_and_no_windows_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let wsl = wsl::fake(root);
    let home = root.join("home/alice");
    let mut spec = spec(&home, "/usr/bin:/mnt/c/Program Files/nodejs");
    spec.platform = Platform::Linux;
    let file = spec.file();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        service::service_file_contents(&spec, Path::new("/x/senated")),
    )
    .unwrap();

    let checks = wsl::checks(&wsl, Some(&spec), EN);
    assert_eq!(by_id(&checks, "wsl.systemd").status, Status::Fail);
    let linger = by_id(&checks, "wsl.linger");
    assert_eq!(linger.status, Status::Warn);
    assert_eq!(
        linger.fix.as_deref(),
        Some("sudo loginctl enable-linger alice")
    );
    let path = by_id(&checks, "wsl.service-path");
    assert_eq!(path.status, Status::Fail);
    assert!(
        path.found.contains("/mnt/c/Program Files/nodejs"),
        "{path:?}"
    );

    std::fs::create_dir_all(root.join("run/systemd/system")).unwrap();
    std::fs::create_dir_all(root.join("var/lib/systemd/linger")).unwrap();
    std::fs::write(root.join("var/lib/systemd/linger/alice"), "").unwrap();
    spec.path = Some("/usr/bin".into());
    std::fs::write(
        &file,
        service::service_file_contents(&spec, Path::new("/x/senated")),
    )
    .unwrap();
    let checks = wsl::checks(&wsl, Some(&spec), EN);
    assert!(checks.iter().all(|c| c.status == Status::Ok), "{checks:#?}");

    let mut on_c = wsl.clone();
    on_c.cwd = PathBuf::from("/mnt/c/Users/alice/src/repo");
    let checks = wsl::checks(&on_c, None, EN);
    assert_eq!(by_id(&checks, "wsl.repository").status, Status::Warn);
}

// ---- the daemon and the Senates ----------------------------------------------

fn status_reply(version: &str) -> StatusReply {
    serde_json::from_value(json!({
        "pid": 42,
        "version": version,
        "person": "alice",
        "locale": "en",
        "server_url": "http://127.0.0.1:1",
        "committees": [],
        "sessions": [],
        "queue": serde_json::to_value(crate::state::QueueCounts::default()).unwrap(),
    }))
    .unwrap()
}

fn me(version: Option<&str>, epoch: &str, committees: &[&str]) -> Me {
    let mut me: Me = serde_json::from_value(json!({"person": "alice"})).unwrap();
    me.version = version.map(str::to_owned);
    me.epoch = epoch.to_owned();
    me.committees = committees
        .iter()
        .map(|c| CommitteeId::new(*c).unwrap())
        .collect();
    me
}

fn session(label: &str, running: bool, prompt: bool) -> SessionRow {
    SessionRow {
        paused: false,
        chair: false,
        shown_name: None,
        host_id: EventId::new(),
        session_id: format!("s-{label}"),
        primary: label.parse().unwrap(),
        alias: None,
        committee: None,
        runtime: crate::session::Runtime::Claude,
        working: false,
        last_activity: chrono::DateTime::default(),
        title: None,
        branch: None,
        model: Default::default(),
        dir: PathBuf::from("/w"),
        state: if running {
            HostState::Running
        } else {
            HostState::Stopped
        },
        started_at: None,
        attached: false,
        typed_secs_ago: None,
        tty: PathBuf::from("/t"),
        prompt: prompt.then(|| HostedPrompt {
            tool: Some("Bash".into()),
            since: chrono::Utc::now(),
            screen: false,
        }),
    }
}

fn healthy() -> Live {
    Live {
        version: "0.4.0".into(),
        config: Ok(()),
        daemon: Ok(status_reply("0.4.0")),
        sessions: Some(SessionsReply {
            rows: vec![
                session("claude", true, false),
                session("codex", false, false),
            ],
            local_rows: Vec::new(),
            orphans: Vec::new(),
        }),
        served: Some(Probe {
            url: "http://127.0.0.1:1".into(),
            me: Ok(me(Some("0.4.0"), "e1", &["senate"])),
        }),
        hosted: true,
        parent: Some(Probe {
            url: "https://team".into(),
            me: Ok(me(Some("0.4.2"), "p1", &[])),
        }),
        registration: Some(UplinkStatus::now(
            "https://team",
            None,
            senate_core::Name::new("alice-desk").unwrap(),
            "senator::alice".parse().unwrap(),
            Registration::Registered,
        )),
        epoch: Some("e1".into()),
        ahead: Vec::new(),
        bindings: vec![
            row("/w/a", "senate", None),
            row("/w/old", "gone", Some("branch gone")),
        ],
        codex: None,
        secretary: None,
    }
}

#[test]
fn the_default_secretary_is_reported_and_only_a_stop_on_purpose_needs_a_command() {
    use crate::ipc::{DefaultSecretary, DefaultSecretaryState as S};
    let mut machine = healthy();
    assert!(
        live(&machine, EN)
            .iter()
            .all(|c| c.id != "secretary.default"),
        "not asked: not shown"
    );
    let secretary = |state, senator: Option<&str>| DefaultSecretary {
        committee: senate_core::CommitteeId::new("office").unwrap(),
        alias: "secretary".parse().unwrap(),
        senator: senator.map(|s| s.parse().unwrap()),
        state,
        action: None,
    };
    for (state, said) in [
        (
            S::NotRecorded,
            "not recorded yet; the first `senate secretary ask` starts it",
        ),
        (S::Running, "office::secretary (claude-1) running"),
        (
            S::Paused,
            "office::secretary (claude-1) paused; the next ask resumes it",
        ),
        (
            S::Ended,
            "office::secretary (claude-1) not running; the next ask restores it",
        ),
    ] {
        let senator = (state != S::NotRecorded).then_some("claude-1");
        machine.secretary = Some(secretary(state, senator));
        let checks = live(&machine, EN);
        let check = by_id(&checks, "secretary.default");
        assert_eq!(check.status, Status::Ok, "{state:?}");
        assert_eq!(check.found, said);
    }
    machine.secretary = Some(secretary(S::Stopped, Some("claude-1")));
    let checks = live(&machine, EN);
    let check = by_id(&checks, "secretary.default");
    assert_eq!(check.status, Status::Warn);
    assert_eq!(
        check.found,
        "office::secretary (claude-1) stopped on purpose; an ask does not start it"
    );
    assert_eq!(
        check.fix.as_deref(),
        Some("senate session restore office::secretary")
    );
    assert!(
        by_id(&live(&machine, Locale::Zh), "secretary.default")
            .found
            .contains("被有意停止")
    );
}

#[test]
fn a_codex_app_server_that_is_down_names_the_daemon_command() {
    let mut machine = healthy();
    machine.codex = Some(Ok(()));
    let checks = live(&machine, EN);
    assert_eq!(by_id(&checks, "codex.app_server").status, Status::Ok);

    machine.codex = Some(Err(CodexProbe::Down {
        text: "the Codex app-server daemon is not running".into(),
        managed: true,
    }));
    let checks = live(&machine, EN);
    let codex = by_id(&checks, "codex.app_server");
    assert_eq!(codex.status, Status::Fail);
    assert!(codex.found.contains("cannot start or restore"), "{codex:?}");
    assert_eq!(
        codex.fix.as_deref(),
        Some("codex app-server daemon bootstrap")
    );

    // A socket of the person's own: there is no managed daemon to start.
    machine.codex = Some(Err(CodexProbe::Down {
        text: "no Codex app-server is listening on /s".into(),
        managed: false,
    }));
    assert_eq!(by_id(&live(&machine, EN), "codex.app_server").fix, None);

    machine.codex = Some(Err(CodexProbe::Failed("odd".into())));
    assert_eq!(
        by_id(&live(&machine, EN), "codex.app_server").status,
        Status::Warn
    );
}

#[test]
fn a_healthy_machine_passes_every_live_check() {
    let checks = live(&healthy(), EN);
    assert!(checks.iter().all(|c| c.status == Status::Ok), "{checks:#?}");
    let ids: Vec<&str> = checks.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "daemon",
            "sessions.hosted",
            "uplink.parent",
            "uplink.registration",
            "uplink.version",
            "senate.server",
            "senate.epoch",
            "senate.cursors",
            "bindings.committee",
        ]
    );
    assert!(
        by_id(&checks, "sessions.hosted")
            .found
            .contains("1 hosted sessions running, 1 stopped")
    );
}

#[test]
fn every_live_problem_is_named_with_its_fix() {
    let mut l = healthy();
    l.daemon = Ok(status_reply("0.3.9"));
    l.sessions = Some(SessionsReply {
        rows: vec![session("claude", true, true)],
        local_rows: Vec::new(),
        orphans: vec![EventId::new()],
    });
    l.parent = Some(Probe {
        url: "https://team".into(),
        me: Ok(me(Some("0.5.0"), "p1", &[])),
    });
    l.registration.as_mut().unwrap().registration = Registration::Refused {
        reason: "name taken".into(),
    };
    l.epoch = Some("e0".into());
    l.ahead = vec![(CommitteeId::new("senate").unwrap(), Seq(900))];
    l.bindings.push(row("/w/b", "brand-new", None));

    let checks = live(&l, EN);
    let status = |id| by_id(&checks, id).status;
    let fix = |id| by_id(&checks, id).fix.clone();
    assert_eq!(status("daemon.version"), Status::Warn);
    assert_eq!(fix("daemon.version").as_deref(), Some(SERVICE_INSTALL));
    assert_eq!(status("sessions.orphans"), Status::Warn);
    assert_eq!(
        fix("sessions.orphans").as_deref(),
        Some("senate session prune")
    );
    assert_eq!(status("sessions.prompt.claude"), Status::Warn);
    assert_eq!(
        fix("sessions.prompt.claude").as_deref(),
        Some("senate attach claude")
    );
    assert!(
        by_id(&checks, "sessions.prompt.claude")
            .found
            .contains("(Bash)")
    );
    assert_eq!(status("uplink.registration"), Status::Fail);
    assert!(
        by_id(&checks, "uplink.registration")
            .found
            .contains("name taken")
    );
    assert_eq!(status("uplink.version"), Status::Warn);
    assert_eq!(
        fix("uplink.version").as_deref(),
        Some("cargo install senate senated")
    );
    assert_eq!(status("senate.epoch"), Status::Warn);
    assert_eq!(status("senate.cursors"), Status::Fail);
    assert!(
        by_id(&checks, "senate.cursors")
            .found
            .contains("senate at 900")
    );
    assert_eq!(status("bindings.committee"), Status::Warn);
    assert!(
        by_id(&checks, "bindings.committee")
            .found
            .contains("brand-new for /w/b")
    );
}

#[test]
fn an_older_parent_says_its_version_is_unknown_and_does_not_fail() {
    let mut l = healthy();
    l.parent = Some(Probe {
        url: "https://team".into(),
        me: Ok(me(None, "p1", &[])),
    });
    let check = by_id(&live(&l, EN), "uplink.version").clone();
    assert_eq!(check.status, Status::Ok);
    assert!(check.found.contains("version is unknown"), "{check:?}");
}

#[test]
fn a_down_daemon_and_an_unreachable_parent_fail() {
    let mut l = healthy();
    l.daemon = Err("no socket".into());
    l.sessions = None;
    l.served = None;
    l.parent = Some(Probe {
        url: "https://team".into(),
        me: Err("connection refused".into()),
    });
    let checks = live(&l, EN);
    assert_eq!(by_id(&checks, "daemon").status, Status::Fail);
    assert_eq!(
        by_id(&checks, "daemon").fix.as_deref(),
        Some(SERVICE_INSTALL)
    );
    assert_eq!(by_id(&checks, "uplink.parent").status, Status::Fail);
    // Nothing is asked of a daemon that is not there.
    assert!(checks.iter().all(|c| !c.id.starts_with("sessions.")));
    assert!(checks.iter().all(|c| !c.id.starts_with("senate.")));
}

#[test]
fn a_team_server_without_a_local_senate_is_checked_as_the_one_above() {
    let mut l = healthy();
    l.hosted = false;
    l.parent = None;
    l.registration = None;
    l.served = Some(Probe {
        url: "https://team".into(),
        me: Ok(me(Some("0.6.1"), "e1", &["senate"])),
    });
    let checks = live(&l, EN);
    assert_eq!(by_id(&checks, "senate.version").status, Status::Warn);
    assert!(checks.iter().all(|c| !c.id.starts_with("uplink.")));
}

// ---- the report ------------------------------------------------------------

#[test]
fn the_json_has_checks_and_a_summary() {
    let report = Report::new(vec![
        Check::ok("a", "fine"),
        Check::warn("b", "hm", Some("do b")),
        Check::fail("c", "bad", None::<String>),
    ]);
    assert!(report.failed());
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        json!({
            "checks": [
                {"id": "a", "status": "ok", "found": "fine"},
                {"id": "b", "status": "warn", "found": "hm", "fix": "do b"},
                {"id": "c", "status": "fail", "found": "bad"},
            ],
            "summary": {"ok": 1, "warn": 1, "fail": 1},
        })
    );
    let text = report.render(EN);
    assert!(text.contains("warn  b  hm\n         fix: do b\n"), "{text}");
    assert!(text.ends_with("1 ok, 1 warn, 1 fail\n"), "{text}");
    assert!(!Report::new(vec![Check::warn("w", "x", None::<String>)]).failed());
}

struct Says(&'static str);

impl Manager for Says {
    fn run(&self, _: &Cmd) -> io::Result<Ran> {
        Ok(Ran {
            ok: true,
            stdout: self.0.into(),
            stderr: String::new(),
        })
    }

    fn sleep(&self, _: std::time::Duration) {}
}

fn versioned(dir: &Path, name: &str, version: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\necho '{name} {version}'\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn a_daemon_launchd_refused_to_sign_names_the_reinstall() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bin = home.join("bin");
    let senated = versioned(&bin, "senated", "0.5.0");
    let spec = spec(home, &bin.to_string_lossy());
    let plan = service::plan_install(&spec, &senated);
    std::fs::create_dir_all(plan.file.parent().unwrap()).unwrap();
    std::fs::write(&plan.file, &plan.contents).unwrap();
    let checks = service_checks(
        &spec,
        Some(&senated),
        &Says("\tstate = not running\n\tlast exit reason = OS_REASON_CODESIGNING\n"),
        EN,
    );
    let running = by_id(&checks, "service.running");
    assert_eq!(running.status, Status::Fail);
    assert!(
        running.found.contains("OS_REASON_CODESIGNING")
            && running.found.contains("overwritten in place"),
        "{running:?}"
    );
    assert_eq!(running.fix.as_deref(), Some(SERVICE_INSTALL));

    let checks = service_checks(
        &spec,
        Some(&senated),
        &Says("\tstate = not running\n\tlast exit code = 1\n"),
        EN,
    );
    let running = by_id(&checks, "service.running");
    assert!(
        running.found.contains("(last exit code = 1)"),
        "{running:?}"
    );
}

#[test]
fn a_service_copy_behind_the_installed_senated_says_both_versions() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bin = home.join("cargo");
    let senated = versioned(&bin, "senated", "0.5.0");
    let spec = spec(home, &bin.to_string_lossy());
    let plan = service::plan_install(&spec, &senated);
    service::apply_install(&plan, &senated, &manager(true)).unwrap();
    let checks = service_checks(&spec, Some(&senated), &manager(true), EN);
    assert_eq!(by_id(&checks, "service.binary").status, Status::Ok);

    versioned(&bin, "senated", "0.5.1");
    let checks = service_checks(&spec, Some(&senated), &manager(true), EN);
    let copy = by_id(&checks, "service.binary");
    assert_eq!(copy.status, Status::Warn);
    assert!(
        copy.found.contains("the service runs 0.5.0") && copy.found.contains("0.5.1 is installed"),
        "{copy:?}"
    );
    assert_eq!(copy.fix.as_deref(), Some(SERVICE_INSTALL));
}
