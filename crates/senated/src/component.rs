//! Components: what runs beside the server on its [`Hub`], supervised
//! together.
//!
//! A component is anything a private layer attaches to the daemon: a chat
//! gateway's transport, an agent-to-agent entry point, a scheduler. It gets
//! the [`Hub`] and a shutdown signal ([`ComponentContext`]) and runs until
//! that signal. The built-in Speaker runs the same way.
//!
//! # Supervision
//!
//! Every component rebuilds what it needs from the log, so one that fails
//! (returns an error, or panics) is started again with the same value, after
//! a backoff that doubles per failure. The restart budget is shared:
//! more than [`MAX_RESTARTS`] failures inside [`RESTART_WINDOW`] stop the
//! whole set, and the daemon reports it ([`Senated::failed`](crate::Senated::failed)),
//! because a daemon that keeps serving without its components would look
//! healthy to a process manager while it no longer does its job.
//!
//! A component that returns `Ok(())` before shutdown is done, and is not
//! started again.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::FutureExt;
use senate_core::Hub;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// More failures than this inside [`RESTART_WINDOW`] stop every component.
pub const MAX_RESTARTS: usize = 5;
pub const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// First delay before a failed component is started again; doubles per
/// failure in the window.
pub const RESTART_BACKOFF: Duration = Duration::from_millis(500);
/// How long a component has to stop after shutdown is signalled before it
/// is aborted.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// What a component is given each time it starts.
#[derive(Clone)]
pub struct ComponentContext {
    /// The log of the Senate this daemon serves.
    pub hub: Arc<dyn Hub>,
    /// Cancelled when the daemon shuts down: return soon after.
    pub shutdown: CancellationToken,
}

/// Something that runs on the [`Hub`] beside the server.
#[async_trait]
pub trait Component: Send + Sync + 'static {
    /// A short name for logs (`chat-gateway`, `speaker`).
    fn name(&self) -> &str;

    /// Run until `ctx.shutdown` is cancelled. An error (or a panic) is a
    /// failure, and the component is started again; `Ok(())` before
    /// shutdown means it is done.
    async fn run(&self, ctx: ComponentContext) -> anyhow::Result<()>;
}

type BoxedRun = Box<
    dyn Fn(ComponentContext) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
        + Send
        + Sync,
>;

/// A component from a closure, for the common case:
///
/// ```
/// # use senated::{component_fn, ComponentContext};
/// let ticker = component_fn("ticker", |ctx: ComponentContext| async move {
///     ctx.shutdown.cancelled().await;
///     Ok(())
/// });
/// # let _ = ticker;
/// ```
pub fn component_fn<F, Fut>(name: &str, run: F) -> impl Component
where
    F: Fn(ComponentContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    FnComponent {
        name: name.to_owned(),
        run: Box::new(move |ctx| Box::pin(run(ctx))),
    }
}

struct FnComponent {
    name: String,
    run: BoxedRun,
}

#[async_trait]
impl Component for FnComponent {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, ctx: ComponentContext) -> anyhow::Result<()> {
        (self.run)(ctx).await
    }
}

/// Why the supervisor stopped on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GaveUp {
    /// The component whose failure spent the budget.
    pub component: String,
    pub error: String,
}

impl std::fmt::Display for GaveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} kept failing and was not started again: {}",
            self.component, self.error
        )
    }
}

/// The running set of components.
pub(crate) struct Supervisor {
    shutdown: CancellationToken,
    tasks: JoinSet<()>,
    gave_up: watch::Receiver<Option<GaveUp>>,
}

struct Budget {
    failures: Mutex<VecDeque<Instant>>,
}

impl Budget {
    /// Count one failure; the number in the window, or `None` when the
    /// budget is spent.
    fn charge(&self) -> Option<usize> {
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        failures.push_back(now);
        while failures
            .front()
            .is_some_and(|t| now.duration_since(*t) > RESTART_WINDOW)
        {
            failures.pop_front();
        }
        (failures.len() <= MAX_RESTARTS).then_some(failures.len())
    }
}

impl Supervisor {
    pub(crate) fn start(hub: Arc<dyn Hub>, components: Vec<Arc<dyn Component>>) -> Self {
        let shutdown = CancellationToken::new();
        let (report, gave_up) = watch::channel(None);
        let report = Arc::new(report);
        let budget = Arc::new(Budget {
            failures: Mutex::default(),
        });
        let mut tasks = JoinSet::new();
        for component in components {
            let ctx = ComponentContext {
                hub: hub.clone(),
                shutdown: shutdown.child_token(),
            };
            tasks.spawn(supervise(
                component,
                ctx,
                budget.clone(),
                report.clone(),
                shutdown.clone(),
            ));
        }
        Self {
            shutdown,
            tasks,
            gave_up,
        }
    }

    /// Resolves when the budget is spent and every component was stopped.
    pub(crate) fn gave_up(&self) -> watch::Receiver<Option<GaveUp>> {
        self.gave_up.clone()
    }

    /// Signal shutdown, wait up to [`STOP_GRACE`], abort what is left.
    pub(crate) async fn stop(mut self) {
        self.shutdown.cancel();
        let drained = tokio::time::timeout(STOP_GRACE, async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::warn!("components did not stop in time; aborting them");
            self.tasks.abort_all();
            while self.tasks.join_next().await.is_some() {}
        }
    }
}

async fn supervise(
    component: Arc<dyn Component>,
    ctx: ComponentContext,
    budget: Arc<Budget>,
    report: Arc<watch::Sender<Option<GaveUp>>>,
    everyone: CancellationToken,
) {
    let name = component.name().to_owned();
    loop {
        let run = {
            let component = component.clone();
            let ctx = ctx.clone();
            async move { component.run(ctx).await }
        };
        // A panic is a failure like any other.
        let outcome = std::panic::AssertUnwindSafe(run).catch_unwind().await;
        if ctx.shutdown.is_cancelled() {
            return;
        }
        let error = match outcome {
            Ok(Ok(())) => {
                tracing::info!(component = %name, "finished");
                return;
            }
            Ok(Err(error)) => format!("{error:#}"),
            Err(panic) => panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panicked".to_owned()),
        };
        let Some(failures) = budget.charge() else {
            tracing::error!(component = %name, %error, "failing repeatedly; giving up");
            report.send_replace(Some(GaveUp {
                component: name,
                error,
            }));
            everyone.cancel();
            return;
        };
        let delay = RESTART_BACKOFF.saturating_mul(1 << (failures - 1).min(6));
        tracing::error!(component = %name, %error, ?delay, "failed; starting it again");
        tokio::select! {
            () = ctx.shutdown.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A hub nobody reads: components here never touch it.
    fn hub() -> Arc<dyn Hub> {
        crate::testing_hub::empty()
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_component_is_started_again_until_the_budget_is_spent() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let flaky = component_fn("flaky", move |_ctx| {
            let runs = counted.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("no")
            }
        });
        let quiet = component_fn("quiet", |ctx: ComponentContext| async move {
            ctx.shutdown.cancelled().await;
            Ok(())
        });
        let supervisor = Supervisor::start(hub(), vec![Arc::new(flaky), Arc::new(quiet)]);
        let mut gave_up = supervisor.gave_up();
        gave_up.wait_for(Option::is_some).await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), MAX_RESTARTS + 1);
        let why = gave_up.borrow().clone().unwrap();
        assert_eq!(why.component, "flaky");
        assert!(why.to_string().contains("no"), "{why}");
        // Everyone else was told to stop, and does.
        supervisor.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_panic_is_a_failure_and_done_is_done() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let once = component_fn("once", move |_ctx| {
            let runs = counted.clone();
            async move {
                if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("first run panics");
                }
                Ok(())
            }
        });
        let supervisor = Supervisor::start(hub(), vec![Arc::new(once)]);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "restarted once, then done");
        assert!(supervisor.gave_up().borrow().is_none());
        supervisor.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_component_that_ignores_shutdown_is_aborted() {
        let stuck = component_fn("stuck", |_ctx| async move {
            std::future::pending::<()>().await;
            Ok(())
        });
        let supervisor = Supervisor::start(hub(), vec![Arc::new(stuck)]);
        tokio::time::sleep(Duration::from_millis(10)).await;
        supervisor.stop().await;
    }
}
