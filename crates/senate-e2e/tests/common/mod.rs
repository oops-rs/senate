//! One world for the end-to-end tests: a daemon assembled by
//! `senated::Assembly` exactly as a private layer would assemble one (the
//! real server over a real log store, the real Speaker with a scripted
//! brain, optionally the fake runner and forge), a chat platform in memory
//! (`senated::testing::FakeChat`, mounted through the real `ChatGateway`),
//! and developer machines running real bridge daemons whose coding sessions
//! are fake inbox sockets (Claude Code) or a fake app-server (Codex).
//!
//! An "agent" is the test itself: it reads what the bridge pushed into the
//! session and answers through the daemon's IPC, which is exactly what the
//! MCP shim does for a real session. Nothing here touches the real home
//! directory, a real model, Docker, `gh`, or anything off the loopback.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

pub mod fake_codex;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use senate_bridge::daemon::{self, DaemonHandle, DaemonOptions, Timing};
use senate_bridge::hook::{self, HookEnv, HookKind, HookTimeouts};
use senate_bridge::ipc::{
    self, CatchUpReply, CheckpointReply, Posted, Request, SearchReply, SessionSelector,
};
use senate_bridge::session::Runtime;
use senate_bridge::{BridgeConfig, Paths};
use senate_core::{
    Address, Brief, CommitteeId, Event, Hub, Kind, Locale, Name, SenatorPath, Seq, StatusPayload,
};
use senate_server::ServerHub;
use senate_speaker::{Brain, ScriptedBrain, SpeakerConfig, WorkConfig};
use senate_work::testing::{FakeForge, FakeRunner};
use senate_work::{Forge, RepoAllowlist, Runner};
use senated::testing::{FakeChat, Message, Post};
use senated::{Assembly, ChatGateway, Delivered, Machine, Senated, SenatedConfig};
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

pub const WAIT: Duration = Duration::from_secs(20);
/// The people group bound to the committee.
pub const GROUP: &str = "refund-backend";
pub const GROUP_NAME: &str = "退款 v2 · 后端群";
/// A second people group bound to the same committee.
pub const PRODUCT_GROUP: &str = "refund-product";
pub const PRODUCT_NAME: &str = "退款 v2 · 需求群";
/// A group adopted as the committee's Agent group.
pub const AGENTS: &str = "refund-agents";
pub const COMMITTEE: &str = "refund-v2";
/// The one repository work runs may use.
pub const REPO: &str = "example/refund";

pub fn committee() -> CommitteeId {
    CommitteeId::new(COMMITTEE).unwrap()
}

pub fn name(h: &str) -> Name {
    Name::new(h).unwrap()
}

pub fn path(p: &str) -> SenatorPath {
    p.parse().unwrap()
}

/// `senator::<p>`: a person (`alice`) or an agent (`alice/claude`).
pub fn senator(p: &str) -> Address {
    Address::senator(&path(p))
}

/// `senator::<person>`.
pub fn person(h: &str) -> Address {
    Address::senior(&name(h))
}

/// `senator::<person>/claude`: what a Claude Code session of that person
/// speaks as.
pub fn claude(h: &str) -> Address {
    senator(&format!("{h}/claude"))
}

/// `senator::<person>/codex`: what a Codex session of that person speaks as.
pub fn codex(h: &str) -> Address {
    senator(&format!("{h}/codex"))
}

/// `senator::<person>/*`: whichever of that person's agents should get it.
pub fn agent_of(h: &str) -> Address {
    Address::agent_of(&name(h))
}

/// The chat platform's id for a person on the roster.
pub fn user_id(h: &str) -> String {
    format!("u_{h}")
}

pub async fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub async fn eventually_async<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// What the Speaker does work with.
struct Hands {
    runner: Arc<dyn Runner>,
    forge: Option<Arc<dyn Forge>>,
}

type Tweak<T> = Box<dyn FnOnce(&mut T)>;
/// How the chat platform is set up, on every start of the daemon.
type Platform = Arc<dyn Fn(&FakeChat) + Send + Sync>;

/// How a world is put together; [`World::start`] covers the common case.
pub struct Setup {
    answer_fallback_secs: u64,
    locale: Locale,
    database_url: Option<String>,
    sqlite: bool,
    chat: bool,
    speaker_on: bool,
    platform: Option<Platform>,
    hands: Option<Hands>,
    server: Vec<Tweak<senate_server::ServerConfig>>,
    speaker: Vec<Tweak<SpeakerConfig>>,
    /// People besides pm, alice, bob and carol: `(handle, has a chat account)`.
    people: Vec<(String, bool)>,
}

impl Setup {
    pub fn new() -> Self {
        Self {
            answer_fallback_secs: 60,
            locale: Locale::Zh,
            database_url: None,
            sqlite: false,
            chat: true,
            speaker_on: true,
            platform: None,
            hands: None,
            server: Vec::new(),
            speaker: Vec::new(),
            people: Vec::new(),
        }
    }

    pub fn answer_fallback(mut self, secs: u64) -> Self {
        self.answer_fallback_secs = secs;
        self
    }

    /// The deployment's language: the top-level `locale`, which the Speaker
    /// and the bridges follow.
    pub fn locale(mut self, locale: Locale) -> Self {
        self.locale = locale;
        self
    }

    /// Keep the log at `url` instead of `memory:`.
    pub fn database(mut self, url: String) -> Self {
        self.database_url = Some(url);
        self
    }

    /// Keep the log in an SQLite file in the world's temp dir.
    pub fn sqlite(mut self) -> Self {
        self.sqlite = true;
        self
    }

    /// Set the chat platform up with `f` before each start of the daemon.
    pub fn platform(mut self, f: impl Fn(&FakeChat) + Send + Sync + 'static) -> Self {
        self.platform = Some(Arc::new(f));
        self
    }

    /// No Speaker and no chat platform: `senated` with no `[speaker]`.
    pub fn without_speaker(mut self) -> Self {
        self.speaker_on = false;
        self.chat = false;
        self
    }

    /// No chat platform: `senated` as the binary runs it.
    pub fn without_chat(mut self) -> Self {
        self.chat = false;
        self
    }

    /// The Speaker gets hands: `runner` and `forge`, with `[speaker.work]`
    /// allowing [`REPO`] and a short card debounce.
    pub fn work(mut self, runner: &FakeRunner, forge: &FakeForge) -> Self {
        self.hands = Some(Hands {
            runner: Arc::new(runner.clone()),
            forge: Some(Arc::new(forge.clone())),
        });
        self
    }

    pub fn server(mut self, f: impl FnOnce(&mut senate_server::ServerConfig) + 'static) -> Self {
        self.server.push(Box::new(f));
        self
    }

    pub fn speaker(mut self, f: impl FnOnce(&mut SpeakerConfig) + 'static) -> Self {
        self.speaker.push(Box::new(f));
        self
    }

    /// Another person on the roster; `chat`: with an account on the chat
    /// platform.
    pub fn person(mut self, handle: &str, chat: bool) -> Self {
        self.people.push((handle.to_owned(), chat));
        self
    }

    pub async fn start(self) -> World {
        World::start_from(self).await
    }
}

impl Default for Setup {
    fn default() -> Self {
        Self::new()
    }
}

pub struct World {
    pub dir: tempfile::TempDir,
    pub config: SenatedConfig,
    pub daemon: Option<Senated>,
    pub chat: Option<Arc<FakeChat>>,
    pub gateway: Option<ChatGateway>,
    pub brain: Arc<ScriptedBrain>,
    pub locale: Locale,
    hands: Option<Hands>,
    platform: Option<Platform>,
}

impl World {
    /// A world in Chinese with the chat platform, the Speaker answering for
    /// an agent after `answer_fallback_secs`.
    pub async fn start(answer_fallback_secs: u64) -> Self {
        Setup::new()
            .answer_fallback(answer_fallback_secs)
            .start()
            .await
    }

    async fn start_from(setup: Setup) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("server");
        let database_url = match (&setup.database_url, setup.sqlite) {
            (Some(url), _) => url.clone(),
            (None, true) => format!("sqlite:{}", dir.path().join("log.db").display()),
            (None, false) => "memory:".into(),
        };
        let mut people = String::new();
        for (handle, chat) in &setup.people {
            people.push_str(&format!(
                "\n[[roster.seniors]]\nhandle = \"{handle}\"\n{}\n[[tokens]]\nperson = \"{handle}\"\ntoken = \"tok-{handle}\"\n",
                if *chat {
                    format!("accounts = [{{ platform = \"chat\", id = \"u_{handle}\" }}]")
                } else {
                    String::new()
                }
            ));
        }
        // `[speaker]` names neither data_dir nor locale: both come from the
        // top level, as a deployment writes them once.
        let mut config = SenatedConfig::from_toml_str(&format!(
            r#"
listen = "127.0.0.1:0"
database_url = "{database_url}"
data_dir = "{data}"
locale = "{locale}"
sse_ping_secs = 1

[speaker]
answer_fallback_secs = {fallback}
brief_top_debounce_secs = 1

[[roster.seniors]]
handle = "alice"
names = ["Alice", "爱丽丝"]
accounts = [{{ platform = "chat", id = "u_alice" }}]

[[roster.seniors]]
handle = "bob"
names = ["Bob"]
accounts = [{{ platform = "chat", id = "u_bob" }}]

[[roster.seniors]]
handle = "pm"
names = ["小王"]
accounts = [{{ platform = "chat", id = "u_pm" }}]

[[roster.seniors]]
handle = "carol"
accounts = [{{ platform = "chat", id = "u_carol" }}]

[[tokens]]
person = "alice"
token = "tok-alice"

[[tokens]]
person = "bob"
token = "tok-bob"

[[tokens]]
person = "pm"
token = "tok-pm"

[[tokens]]
person = "carol"
token = "tok-carol"
{people}"#,
            data = data.display(),
            locale = setup.locale,
            fallback = setup.answer_fallback_secs,
        ))
        .unwrap();
        for f in setup.server {
            f(&mut config.server);
        }
        config.server.validate().unwrap();
        let speaker = config.speaker.as_mut().expect("[speaker]");
        assert_eq!(
            speaker.locale, setup.locale,
            "the Speaker follows the top level"
        );
        if setup.hands.is_some() {
            speaker.card_debounce_ms = 200;
            speaker.work = Some(WorkConfig::new(RepoAllowlist::new([REPO])));
        }
        for f in setup.speaker {
            f(speaker);
        }
        if !setup.speaker_on {
            config.speaker = None;
        }
        let mut world = Self {
            dir,
            config,
            daemon: None,
            chat: None,
            gateway: None,
            brain: ScriptedBrain::new(),
            locale: setup.locale,
            hands: setup.hands,
            platform: setup.platform,
        };
        world.launch(setup.chat).await;
        world
    }

    /// Assemble and start the daemon from `self.config`.
    async fn launch(&mut self, chat: bool) {
        let brain: Arc<dyn Brain> = self.brain.clone();
        let mut assembly = Assembly::new(self.config.clone()).brain(brain);
        if let Some(hands) = &self.hands {
            assembly = assembly.work(hands.runner.clone(), hands.forge.clone());
        }
        if chat {
            let (fake, gateway) = FakeChat::new();
            if let Some(platform) = &self.platform {
                platform(&fake);
            }
            assembly = assembly.chat_gateway(gateway.clone()).unwrap();
            self.chat = Some(fake);
            self.gateway = Some(gateway);
        } else {
            self.chat = None;
            self.gateway = None;
        }
        let daemon = assembly.start().await.unwrap();
        // A restart binds the same address, so bridges find it again.
        self.config.server.listen = daemon.local_addr.to_string();
        self.daemon = Some(daemon);
    }

    /// Stop the daemon and start it again on the same address and log.
    pub async fn restart(&mut self) {
        let chat = self.chat.is_some();
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await.unwrap();
        }
        self.launch(chat).await;
    }

    pub fn daemon(&self) -> &Senated {
        self.daemon.as_ref().unwrap()
    }

    /// The hub the server, the Speaker and the gateway share.
    pub fn hub(&self) -> &Arc<ServerHub> {
        &self.daemon().hub
    }

    pub fn server_url(&self) -> String {
        self.daemon().base_url()
    }

    pub fn fake(&self) -> &Arc<FakeChat> {
        self.chat.as_ref().expect("a world with the chat platform")
    }

    /// Someone says something on the chat platform; what the gateway made
    /// of it.
    pub async fn say(&self, message: Message) -> Delivered {
        self.fake().deliver(message).await.unwrap()
    }

    /// `@Speaker /bind refund-v2` in the group, from the PM, the way a group
    /// starts; returns once the gateway routes the group to the committee.
    pub async fn bind(&self) {
        let title = self.locale.pick(GROUP_NAME, "Refund v2 backend");
        self.bind_group(GROUP, title, "m_bind").await;
    }

    /// `/bind refund-v2` in the product group too.
    pub async fn bind_product(&self) {
        self.bind_group(PRODUCT_GROUP, PRODUCT_NAME, "m_bind_product")
            .await;
    }

    async fn bind_group(&self, group: &str, title: &str, message_ref: &str) {
        self.say(
            Message::mention(group, "u_pm", message_ref, &format!("/bind {COMMITTEE}"))
                .titled(title),
        )
        .await;
        eventually("the group to be bound", || {
            (self.gateway.as_ref()?.committee_of(group) == Some(committee())).then_some(())
        })
        .await;
    }

    /// Bind the people group, then adopt a second group as the Agent group.
    pub async fn with_agent_group(&self) {
        self.bind().await;
        self.say(
            Message::mention(AGENTS, "u_pm", "m_adopt", &format!("/agents {COMMITTEE}"))
                .titled("[Agents] refund-v2"),
        )
        .await;
        eventually("the Agent group to be adopted", || {
            (self.gateway.as_ref()?.committee_of(AGENTS) == Some(committee())).then_some(())
        })
        .await;
    }

    pub async fn events(&self) -> Vec<Event> {
        self.events_of(&committee()).await
    }

    pub async fn events_of(&self, committee: &CommitteeId) -> Vec<Event> {
        self.hub().read(committee, Seq::ZERO, 1000).await.unwrap()
    }

    /// Wait until `agent` (`alice/claude`) is, or is not, connected.
    pub async fn wait_connected(&self, agent: &str, connected: bool) {
        let agent = path(agent);
        eventually_async(&format!("{agent} connected={connected}"), || async {
            (self.hub().is_agent_connected(&committee(), &agent).await == connected).then_some(())
        })
        .await;
    }

    pub fn posts(&self) -> Vec<Post> {
        self.chat.as_ref().map(|c| c.posts()).unwrap_or_default()
    }

    /// The first post matching `f`.
    pub async fn wait_post(&self, what: &str, f: impl Fn(&Post) -> bool) -> Post {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(post) = self.posts().into_iter().find(|p| f(p)) {
                return post;
            }
            if tokio::time::Instant::now() > deadline {
                let posts: Vec<String> = self
                    .posts()
                    .iter()
                    .map(|p| format!("{:?} {:?}: {}", p.to, p.message_ref, p.markdown))
                    .collect();
                let events: Vec<String> = self
                    .events()
                    .await
                    .iter()
                    .map(|e| format!("{} {} {} -> {:?}: {}", e.seq, e.kind, e.from, e.to, e.body))
                    .collect();
                panic!(
                    "timed out waiting for {what}\nposts:\n{}\nevents:\n{}",
                    posts.join("\n"),
                    events.join("\n")
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The platform thread a reply in the thread of `message_ref` goes in.
    pub fn thread_of(&self, message_ref: &str) -> String {
        self.fake().thread_of(message_ref)
    }

    /// `sender` replies in the thread of the bot's post `post`, whose
    /// thread root is `root` (the person's message it answered, or the post
    /// itself), quoting the post.
    pub async fn reply_to(
        &self,
        group: &str,
        sender: &str,
        message_ref: &str,
        text: &str,
        post: &Post,
        root: &str,
    ) {
        let thread = post
            .thread_ref
            .clone()
            .unwrap_or_else(|| self.thread_of(root));
        self.say(
            Message::reply(group, sender, message_ref, text, &post.message_ref)
                .in_thread(&thread, root),
        )
        .await;
    }

    /// The Speaker has caught up with everything so far.
    pub async fn settle(&self) {
        self.daemon().settle_speaker().await;
    }

    /// A developer machine for `who`, its sessions working in a checkout
    /// bound to the committee.
    pub fn laptop(&self, who: &str) -> Laptop {
        let home = self.dir.path().join(format!("laptop-{who}"));
        let work = home.join("work");
        std::fs::create_dir_all(work.join(".senate")).unwrap();
        std::fs::write(work.join(".senate/committee"), format!("{COMMITTEE}\n")).unwrap();
        let paths = Paths::new(home.join("senate"));
        std::fs::create_dir_all(paths.home()).unwrap();
        // A Codex session is pushed to through the fake app-server at this
        // socket, never a real one.
        std::fs::write(
            paths.config(),
            format!(
                "[codex]\nlisten = \"unix://{}\"\n",
                home.join("codex.sock").display()
            ),
        )
        .unwrap();
        let mut config = BridgeConfig::new(&self.server_url(), &format!("tok-{who}"), name(who));
        config.locale = self.locale;
        Laptop {
            paths,
            config,
            work,
            home,
            daemon: None,
        }
    }

    pub async fn stop(mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await.unwrap();
        }
    }
}

/// One developer machine: a bridge daemon and its sessions.
pub struct Laptop {
    pub home: PathBuf,
    pub paths: Paths,
    pub config: BridgeConfig,
    pub work: PathBuf,
    pub daemon: Option<DaemonHandle>,
}

pub fn fast_timing() -> Timing {
    Timing {
        heartbeat: Duration::from_millis(500),
        batch_window: Duration::from_millis(150),
        batch_max_wait: Duration::from_secs(2),
        backoff_base: Duration::from_millis(50),
        backoff_max: Duration::from_millis(300),
        stall_timeout: Duration::from_secs(5),
        cursor_debounce: Duration::from_millis(50),
        sweep: Duration::from_millis(500),
        inbox_timeout: Duration::from_secs(1),
        push_spacing: Duration::from_millis(100),
        retry_base: Duration::from_millis(100),
        retry_max: Duration::from_millis(500),
        // The Speaker judges each root; the production wait only matters
        // when a verdict is slow.
        triage_wait: Duration::from_secs(10),
        checkpoint_poll: Duration::from_secs(3600),
        checkpoint_interval: Duration::from_secs(7200),
    }
}

impl Laptop {
    /// Open the lid: start the daemon.
    pub async fn open(&mut self) {
        let handle = daemon::start(DaemonOptions {
            paths: self.paths.clone(),
            config: self.config.clone(),
            timing: fast_timing(),
        })
        .await
        .unwrap();
        self.daemon = Some(handle);
    }

    /// Close the lid: the daemon goes away, sessions stay registered.
    pub async fn close(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown().await;
        }
    }

    pub fn inbox(&self, name: &str) -> FakeInbox {
        FakeInbox::bind(self.home.join(format!("{name}.sock")))
    }

    /// The fake Codex app-server this machine's bridge pushes to.
    pub fn codex_app_server(&self) -> fake_codex::FakeCodex {
        fake_codex::FakeCodex::bind(self.home.join("codex.sock"))
    }

    /// A Claude Code session starting in this machine's checkout.
    pub async fn session_start(&self, session_id: &str, inbox: &FakeInbox) -> Option<Value> {
        self.hook(HookKind::SessionStart, session_id, inbox).await
    }

    /// The session's Claude Code exits (`SessionEnd`); it stays known.
    pub async fn session_end(&self, session_id: &str, inbox: &FakeInbox) -> Option<Value> {
        self.hook(HookKind::SessionEnd, session_id, inbox).await
    }

    /// A Codex thread starting in this machine's checkout (`hook
    /// session-start --codex`).
    pub async fn codex_start(&self, thread: &str) -> Option<Value> {
        let stdin = serde_json::json!({
            "session_id": thread,
            "cwd": self.work,
            "transcript_path": format!("/tmp/rollout-2026-11-16T00-00-00-{thread}.jsonl"),
            "hook_event_name": HookKind::SessionStart.event_name(),
            "source": "startup",
        })
        .to_string();
        hook::run_as(
            HookKind::SessionStart,
            &self.paths.socket(),
            &stdin,
            &HookEnv::default(),
            Some(Runtime::Codex),
            HookTimeouts::default(),
        )
        .await
        .map(|line| serde_json::from_str(&line).unwrap())
    }

    /// A Claude Code session that pins its agent name (`SENATE_AGENT`).
    pub async fn session_start_as(
        &self,
        session_id: &str,
        inbox: &FakeInbox,
        agent: &str,
    ) -> Option<Value> {
        self.hook_as(HookKind::SessionStart, session_id, inbox, Some(agent))
            .await
    }

    /// Run one of the session's hooks the way Claude Code does.
    pub async fn hook(&self, kind: HookKind, session_id: &str, inbox: &FakeInbox) -> Option<Value> {
        self.hook_as(kind, session_id, inbox, None).await
    }

    pub async fn hook_as(
        &self,
        kind: HookKind,
        session_id: &str,
        inbox: &FakeInbox,
        agent: Option<&str>,
    ) -> Option<Value> {
        let stdin = serde_json::json!({
            "session_id": session_id,
            "cwd": self.work,
            "transcript_path": format!("/tmp/{session_id}.jsonl"),
            "hook_event_name": kind.event_name(),
            "source": "startup",
        })
        .to_string();
        let env = HookEnv {
            socket: Some(inbox.path.clone()),
            token: Some(format!("tok-{session_id}")),
            claude_pid: Some(std::process::id()),
            session_id: None,
            agent: agent.map(str::to_owned),
        };
        hook::run(
            kind,
            &self.paths.socket(),
            &stdin,
            &env,
            HookTimeouts::default(),
        )
        .await
        .map(|line| serde_json::from_str(&line).unwrap())
    }

    pub async fn ipc<T: serde::de::DeserializeOwned>(&self, request: Request) -> T {
        self.try_ipc(request.clone())
            .await
            .unwrap_or_else(|e| panic!("{request:?}: {e}"))
    }

    pub async fn try_ipc<T: serde::de::DeserializeOwned>(
        &self,
        request: Request,
    ) -> Result<T, ipc::ClientError> {
        ipc::request(
            &self.paths.socket(),
            &request,
            Duration::from_secs(2),
            Duration::from_secs(10),
        )
        .await
    }

    pub fn session(session_id: &str) -> SessionSelector {
        SessionSelector {
            session_id: Some(session_id.to_string()),
            claude_pid: None,
        }
    }

    fn ask_request(session_id: &str, to: Address, body: &str, request: bool) -> Request {
        Request::Ask {
            session: Self::session(session_id),
            to: vec![to],
            body: body.to_string(),
            refs: None,
            id: None,
            request,
        }
    }

    /// What the agent's `ask` tool does.
    pub async fn ask(&self, session_id: &str, to: Address, body: &str) -> Posted {
        self.ipc(Self::ask_request(session_id, to, body, false))
            .await
    }

    /// [`Self::ask`], refused or not.
    pub async fn try_ask(
        &self,
        session_id: &str,
        to: Address,
        body: &str,
    ) -> Result<Posted, ipc::ClientError> {
        self.try_ipc(Self::ask_request(session_id, to, body, false))
            .await
    }

    /// What the agent's `ask(request=true)` does.
    pub async fn request(&self, session_id: &str, to: Address, body: &str) -> Posted {
        self.ipc(Self::ask_request(session_id, to, body, true))
            .await
    }

    /// What the agent's `status` tool does.
    pub async fn status(&self, session_id: &str, thread: Seq, status: StatusPayload) -> Posted {
        self.ipc(Request::Task {
            session: Self::session(session_id),
            thread,
            status,
            id: None,
            committee: None,
            agent: None,
            as_person: false,
        })
        .await
    }

    /// What the agent's `escalate` tool does.
    pub async fn escalate(&self, session_id: &str, thread: Seq, reason: &str) -> Posted {
        self.ipc(Request::Escalate {
            session: Self::session(session_id),
            thread,
            to: None,
            reason: Some(reason.to_string()),
            id: None,
        })
        .await
    }

    /// What the agent's `answer` tool does.
    pub async fn answer(&self, session_id: &str, thread: Seq, body: &str) -> Posted {
        self.ipc(Request::Answer {
            session: Self::session(session_id),
            thread,
            body: body.to_string(),
            id: None,
            committee: None,
            agent: None,
            as_person: false,
        })
        .await
    }

    /// What the agent's `search` tool does.
    pub async fn search(&self, session_id: &str, query: &str) -> SearchReply {
        self.ipc(Request::Search {
            session: Self::session(session_id),
            query: query.to_string(),
            limit: None,
            committee: None,
        })
        .await
    }

    /// What the agent's `post` tool does.
    pub async fn post(&self, session_id: &str, body: &str) -> Posted {
        self.ipc(Request::Post {
            session: Self::session(session_id),
            body: body.to_string(),
            proposal: false,
            committee: None,
            agent: None,
            id: None,
        })
        .await
    }

    /// What the agent's `propose_decision` tool does.
    pub async fn propose_decision(&self, session_id: &str, body: &str) -> Posted {
        self.ipc(Request::Post {
            session: Self::session(session_id),
            body: body.to_string(),
            proposal: true,
            committee: None,
            agent: None,
            id: None,
        })
        .await
    }

    /// What reading the committee's brief resource does.
    pub async fn brief(&self, session_id: &str) -> Brief {
        self.ipc(Request::Brief {
            session: Self::session(session_id),
            committee: None,
        })
        .await
    }

    /// What the agent's `checkpoint` tool does: write the files it is given
    /// into `<worktree>/.senate/` and send a checkpoint.
    pub async fn checkpoint(
        &self,
        session_id: &str,
        progress: Option<&str>,
        decisions: Option<&str>,
        open: Option<&str>,
    ) -> CheckpointReply {
        self.ipc(Request::Checkpoint {
            session: Self::session(session_id),
            summary: None,
            progress: progress.map(Into::into),
            decisions: decisions.map(Into::into),
            open: open.map(Into::into),
            push: false,
            id: None,
        })
        .await
    }

    /// [`Self::checkpoint`] with `push = true`, the way a hand-off sends one.
    pub async fn handoff(&self, session_id: &str, progress: &str) -> CheckpointReply {
        self.ipc(Request::Checkpoint {
            session: Self::session(session_id),
            summary: None,
            progress: Some(progress.into()),
            decisions: None,
            open: None,
            push: true,
            id: None,
        })
        .await
    }

    /// What the agent's `catch_up` tool does.
    pub async fn catch_up(&self, session_id: &str) -> CatchUpReply {
        self.ipc(Request::CatchUp {
            session: Self::session(session_id),
            limit: None,
        })
        .await
    }

    /// Make the checkout a git repository on `branch` with one commit; its
    /// `HEAD`.
    pub fn git_init(&self, branch: &str) -> String {
        git(&self.work, &["init", "-q", "-b", branch]);
        std::fs::write(self.work.join("README.md"), "refund v2\n").unwrap();
        git(&self.work, &["add", "README.md"]);
        git(&self.work, &["commit", "-q", "-m", "first"]);
        git(&self.work, &["rev-parse", "HEAD"])
    }

    /// [`Self::git_init`], with `origin` at GitHub's [`REPO`] and pushes
    /// redirected to a local bare repository (`pushInsteadOf`), so a
    /// hand-off checkpoint is pushed and keeps the GitHub remote URL.
    /// Returns `HEAD` and the bare repository.
    pub fn git_init_github(&self, branch: &str) -> (String, PathBuf) {
        let head = self.git_init(branch);
        let bare = self.home.join("origin.git");
        git(
            &self.home,
            &["init", "-q", "--bare", &bare.display().to_string()],
        );
        let github = format!("git@github.com:{REPO}.git");
        git(&self.work, &["remote", "add", "origin", &github]);
        git(
            &self.work,
            &[
                "config",
                &format!("url.{}.pushInsteadOf", bare.display()),
                &github,
            ],
        );
        (head, bare)
    }

    /// Turn on companion sessions in this machine's `bridge.toml`, run by
    /// `claude_bin`; takes effect at the next [`Self::open`].
    pub fn enable_companion(&self, claude_bin: &Path, max_secs: u64) {
        let existing = std::fs::read_to_string(self.paths.config()).unwrap_or_default();
        std::fs::write(
            self.paths.config(),
            format!(
                "{existing}[companion]\nenabled = true\nclaude_bin = \"{}\"\nmax_secs = {max_secs}\n",
                claude_bin.display()
            ),
        )
        .unwrap();
    }
}

/// Run git in `dir` with a throwaway identity and no global or system
/// configuration; its trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A Claude Code session's inbox socket: records every connection's lines.
pub struct FakeInbox {
    pub path: PathBuf,
    conns: Arc<Mutex<Vec<Vec<String>>>>,
    task: JoinHandle<()>,
}

impl Drop for FakeInbox {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeInbox {
    pub fn bind(path: PathBuf) -> Self {
        let listener = UnixListener::bind(&path).unwrap();
        let conns: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let c = Arc::clone(&conns);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let c = Arc::clone(&c);
                tokio::spawn(async move {
                    let mut lines = tokio::io::BufReader::new(stream).lines();
                    let mut got = Vec::new();
                    while let Ok(Some(line)) = lines.next_line().await {
                        got.push(line);
                    }
                    c.lock().unwrap().push(got);
                });
            }
        });
        Self { path, conns, task }
    }

    /// The `content` of each user frame, in arrival order.
    pub fn messages(&self) -> Vec<String> {
        self.conns
            .lock()
            .unwrap()
            .iter()
            .filter_map(|lines| {
                lines.iter().find_map(|l| {
                    let v: Value = serde_json::from_str(l).ok()?;
                    (v["type"] == "user")
                        .then(|| v["message"]["content"].as_str().unwrap().to_string())
                })
            })
            .collect()
    }

    /// The first message containing `needle`.
    pub async fn wait_for(&self, needle: &str) -> String {
        eventually(&format!("an inbox message containing {needle:?}"), || {
            self.messages().into_iter().find(|m| m.contains(needle))
        })
        .await
    }
}

/// The thread a push tells the agent to answer in.
pub fn thread_of(push: &str, body_needle: &str) -> Seq {
    let at = push.find(body_needle).expect("body in push");
    let rest = &push[at..];
    let key = "answer(thread=";
    let start = rest.find(key).expect("an answer hint after the body") + key.len();
    let digits: String = rest[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Seq(digits.parse().unwrap())
}

/// Position of seq `n`'s header in a push.
pub fn pos(push: &str, seq: Seq) -> Option<usize> {
    push.find(&format!("· seq {} ·", seq.0))
}

/// `additionalContext` of a hook's output.
pub fn context(output: &Value) -> &str {
    output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
}

/// Every event of `kind` in `events`.
pub fn of_kind(events: &[Event], kind: Kind) -> Vec<&Event> {
    events.iter().filter(|e| e.kind == kind).collect()
}

// ---- a developer machine running `senated --machine` ------------------------

/// A developer machine whose daemon is `senated --machine`: its
/// `senate.toml` is `person`, `locale`, a fake Codex app-server, and `body`
/// (the Senate above, `[local]`, `[[preside]]`). The returned [`Laptop`]
/// drives its sessions; [`Desk::start`] runs the machine.
pub struct Desk {
    pub laptop: Laptop,
    pub machine: Option<Machine>,
}

impl Desk {
    pub fn new(root: &Path, who: &str, locale: Locale, body: &str) -> Self {
        let home = root.join(format!("desk-{who}"));
        let work = home.join("work");
        std::fs::create_dir_all(work.join(".senate")).unwrap();
        std::fs::write(work.join(".senate/committee"), format!("{COMMITTEE}\n")).unwrap();
        let paths = Paths::new(home.join("senate"));
        std::fs::create_dir_all(paths.home()).unwrap();
        std::fs::write(
            paths.senate_toml(),
            format!(
                "person = \"{who}\"\nlocale = \"{locale}\"\n{body}\n[codex]\nlisten = \"unix://{}\"\n",
                home.join("codex.sock").display()
            ),
        )
        .unwrap();
        // Only for the Laptop's own fields: the machine reads senate.toml.
        let mut config = BridgeConfig::new("http://127.0.0.1:1", "unused", name(who));
        config.locale = locale;
        Self {
            laptop: Laptop {
                paths,
                config,
                work,
                home,
                daemon: None,
            },
            machine: None,
        }
    }

    /// `senated --machine`, in process, with the tests' quick timing.
    pub async fn start(&mut self) {
        let machine = Machine::start(self.laptop.paths.clone(), fast_timing())
            .await
            .unwrap();
        // The developer-machine side is up once its socket answers.
        let socket = self.laptop.paths.socket();
        eventually_async("the machine's socket", || {
            let socket = socket.clone();
            async move {
                ipc::request::<serde_json::Value>(
                    &socket,
                    &Request::Ping,
                    Duration::from_millis(200),
                    Duration::from_secs(1),
                )
                .await
                .ok()
            }
        })
        .await;
        self.machine = Some(machine);
    }

    pub async fn stop(&mut self) {
        if let Some(machine) = self.machine.take() {
            machine.shutdown().await;
        }
    }

    pub fn machine(&self) -> &Machine {
        self.machine.as_ref().expect("the machine is running")
    }

    /// The hosted Senate presided over by `presider` (`None`: the local top).
    pub fn senate(&self, presider: Option<&str>) -> &senated::HostedSenate {
        let presider = presider.map(path);
        self.machine()
            .senate(presider.as_ref())
            .expect("a hosted Senate")
    }

    /// Wait until `agent` has a live connection to the hosted Senate.
    pub async fn wait_connected(&self, presider: Option<&str>, agent: &str, connected: bool) {
        let hub = self.senate(presider).daemon.hub.clone();
        let agent = path(agent);
        eventually_async(&format!("{agent} connected = {connected}"), || {
            let hub = hub.clone();
            let agent = agent.clone();
            async move {
                (hub.is_agent_connected(&committee(), &agent).await == connected).then_some(())
            }
        })
        .await;
    }
}

impl std::ops::Deref for Desk {
    type Target = Laptop;
    fn deref(&self) -> &Laptop {
        &self.laptop
    }
}

impl Laptop {
    /// A Codex thread ends (`hook session-end --codex`).
    pub async fn codex_end(&self, thread: &str) {
        let stdin = serde_json::json!({
            "session_id": thread,
            "cwd": self.work,
            "hook_event_name": HookKind::SessionEnd.event_name(),
        })
        .to_string();
        hook::run_as(
            HookKind::SessionEnd,
            &self.paths.socket(),
            &stdin,
            &HookEnv::default(),
            Some(Runtime::Codex),
            HookTimeouts::default(),
        )
        .await;
    }

    /// A Claude Code session's prompt (`UserPromptSubmit`), pinned or not.
    pub async fn prompt(
        &self,
        session_id: &str,
        inbox: &FakeInbox,
        agent: Option<&str>,
    ) -> Option<Value> {
        self.hook_as(HookKind::UserPromptSubmit, session_id, inbox, agent)
            .await
    }
}

/// A person speaking to a Senate over HTTP, as themselves: what a chat
/// layer or a person's own client does. The stored event, or the refusal's
/// status and body.
pub async fn speak(
    url: &str,
    token: &str,
    event: &senate_core::NewEvent,
) -> Result<Event, (u16, senate_core::api::ApiError)> {
    let response = reqwest::Client::new()
        .post(format!(
            "{url}{}",
            senate_core::api::events_path(&event.committee)
        ))
        .bearer_auth(token)
        .json(event)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    if response.status().is_success() {
        let appended: senate_core::api::AppendResponse = response.json().await.unwrap();
        Ok(appended.event)
    } else {
        Err((status, response.json().await.unwrap()))
    }
}

/// `GET /v1/me` on `url`.
pub async fn me_at(url: &str, token: &str) -> senate_core::api::Me {
    reqwest::Client::new()
        .get(format!("{url}/v1/me"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}
