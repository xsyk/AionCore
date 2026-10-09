//! Finding Node for the image generation script without ever holding a session up.
//!
//! The script runs on Node, and a session asks for the image generation server
//! whenever it is assembled: on every message of every conversation. Locating
//! Node validates a whole runtime by starting `node`, `npm` and `npx`, and on a
//! machine that has none it falls into a managed download that takes minutes, so
//! no session may wait on that. [`NodeCache`] runs the lookup in the background,
//! remembers how it went and lets a caller wait for it a few seconds at most; a
//! session that cannot have Node in time starts without the tool.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aionui_runtime::ResolvedCommand;
use tokio::sync::watch;
use tokio::time::{Instant, timeout};
use tracing::{info, warn};

/// The longest a caller waits for Node.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(3);

/// How long after a lookup found nothing the next one is held off.
pub(crate) const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);

pub(crate) type NodeLookup = Pin<Box<dyn Future<Output = Option<ResolvedCommand>> + Send>>;

/// How the service finds Node to run the script with: the program, any
/// arguments that must come before the script, and environment to launch it in.
pub(crate) type NodeResolver = Arc<dyn Fn() -> NodeLookup + Send + Sync>;

/// What is known about Node.
#[derive(Debug)]
enum State {
    /// Nobody has looked yet, or the runtime found earlier is gone.
    Unknown,
    /// A lookup is running in the background.
    Resolving,
    /// Found. Trusted for as long as its program is still a file.
    Ready(ResolvedCommand),
    /// The last lookup found nothing, at this instant.
    Failed(Instant),
}

struct Shared {
    resolve: NodeResolver,
    state: watch::Sender<State>,
    /// Whether the lookup in flight was already reported as taking long, so that
    /// is logged once and not by every caller that gives up waiting for it.
    slow_reported: AtomicBool,
}

/// Where the image generation server's Node comes from: `Unknown` -> `Resolving`
/// -> `Ready` | `Failed`.
///
/// A caller never runs the lookup itself and never waits for one longer than
/// [`MAX_WAIT`]. The first caller that finds no lookup running (nothing known
/// yet, or the last lookup failed at least [`RETRY_AFTER_FAILURE`] ago) starts
/// one in a background task, and everyone who comes while it runs waits for that
/// same lookup. A caller whose wait runs out gets `None` and the lookup carries
/// on, so the next caller finds its result. A lookup that found nothing is not
/// repeated for [`RETRY_AFTER_FAILURE`], and a runtime that was found is trusted
/// only while its program is still there. Clones share one state.
#[derive(Clone)]
pub(crate) struct NodeCache {
    shared: Arc<Shared>,
}

impl NodeCache {
    pub(crate) fn new(resolve: NodeResolver) -> Self {
        let (state, _) = watch::channel(State::Unknown);
        Self {
            shared: Arc::new(Shared {
                resolve,
                state,
                slow_reported: AtomicBool::new(false),
            }),
        }
    }

    /// Node, when it is known or turns up within [`MAX_WAIT`].
    pub(crate) async fn get(&self) -> Option<ResolvedCommand> {
        if let Some(known) = self.ready() {
            if still_installed(&known).await {
                return Some(known);
            }
            warn!(
                program = %known.program.display(),
                "image generation: the Node runtime found earlier is gone; looking again"
            );
            self.forget(&known);
        }
        self.start_if_due();
        self.wait().await
    }

    /// Start looking for Node in the background, so the first caller finds it
    /// ready. Does nothing while a lookup runs, when Node is already known or
    /// while a recent failure holds lookups off.
    pub(crate) fn warm_up(&self) {
        self.start_if_due();
    }

    fn ready(&self) -> Option<ResolvedCommand> {
        match &*self.shared.state.borrow() {
            State::Ready(command) => Some(command.clone()),
            State::Unknown | State::Resolving | State::Failed(_) => None,
        }
    }

    /// Drop a remembered runtime whose program is gone, unless another caller
    /// already replaced it.
    fn forget(&self, gone: &ResolvedCommand) {
        self.shared.state.send_if_modified(|state| {
            let stale = matches!(state, State::Ready(known) if known == gone);
            if stale {
                *state = State::Unknown;
            }
            stale
        });
    }

    /// Start a lookup when none is running, Node is not known and no recent
    /// failure holds it off. The check and the move to `Resolving` are one
    /// atomic step, so callers that arrive together start one lookup.
    fn start_if_due(&self) {
        let due = self.shared.state.send_if_modified(|state| {
            let due = match state {
                State::Unknown => true,
                State::Failed(failed_at) => failed_at.elapsed() >= RETRY_AFTER_FAILURE,
                State::Resolving | State::Ready(_) => false,
            };
            if due {
                *state = State::Resolving;
            }
            due
        });
        if due {
            self.spawn_lookup();
        }
    }

    fn spawn_lookup(&self) {
        self.shared.slow_reported.store(false, Ordering::Relaxed);
        info!("image generation: looking for a Node runtime in the background");
        let shared = self.shared.clone();
        tokio::spawn(async move {
            // The lookup is a task of its own, so a panic in it is recorded as a
            // failure instead of leaving every caller waiting for a lookup that
            // is gone.
            let lookup = tokio::spawn({
                let shared = shared.clone();
                async move { (shared.resolve)().await }
            });
            let found = match lookup.await {
                Ok(found) => found,
                Err(error) => {
                    warn!(%error, "image generation: the Node lookup ended abnormally");
                    None
                }
            };
            let outcome = match found {
                Some(command) => {
                    info!(program = %command.program.display(), "image generation: Node runtime found");
                    State::Ready(command)
                }
                None => {
                    warn!(
                        retry_after_secs = RETRY_AFTER_FAILURE.as_secs(),
                        "image generation: no Node runtime found; sessions start without the tool until the next lookup"
                    );
                    State::Failed(Instant::now())
                }
            };
            shared.state.send_replace(outcome);
        });
    }

    /// Wait for the lookup in flight, for [`MAX_WAIT`] at most.
    async fn wait(&self) -> Option<ResolvedCommand> {
        let mut changes = self.shared.state.subscribe();
        match timeout(MAX_WAIT, changes.wait_for(|state| !matches!(state, State::Resolving))).await {
            Ok(Ok(state)) => match &*state {
                State::Ready(command) => Some(command.clone()),
                State::Unknown | State::Resolving | State::Failed(_) => None,
            },
            // The sender lives as long as `self`, so this does not happen.
            Ok(Err(_closed)) => None,
            Err(_elapsed) => {
                if !self.shared.slow_reported.swap(true, Ordering::Relaxed) {
                    warn!(
                        wait_secs = MAX_WAIT.as_secs(),
                        "image generation: still looking for a Node runtime; sessions start without the tool until it is found"
                    );
                }
                None
            }
        }
    }
}

async fn still_installed(command: &ResolvedCommand) -> bool {
    tokio::fs::metadata(&command.program)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::time::{Instant, timeout};
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    /// A resolver that answers `answer` after `delay` and counts how many
    /// lookups were started.
    fn resolver(delay: Duration, answer: Option<ResolvedCommand>) -> (NodeResolver, Arc<AtomicUsize>) {
        let started = Arc::new(AtomicUsize::new(0));
        let counter = started.clone();
        let resolve: NodeResolver = Arc::new(move || -> NodeLookup {
            counter.fetch_add(1, Ordering::SeqCst);
            let answer = answer.clone();
            Box::pin(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                answer
            })
        });
        (resolve, started)
    }

    /// A resolver whose lookups never finish.
    fn never_finishing() -> (NodeResolver, Arc<AtomicUsize>) {
        let started = Arc::new(AtomicUsize::new(0));
        let counter = started.clone();
        let resolve: NodeResolver = Arc::new(move || -> NodeLookup {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        });
        (resolve, started)
    }

    /// A lookup that panics.
    fn blow_up() -> Option<ResolvedCommand> {
        panic!("the lookup blew up")
    }

    /// A Node program that exists on disk for as long as the directory lives.
    fn installed_node() -> (tempfile::TempDir, ResolvedCommand) {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("node");
        std::fs::write(&program, b"").unwrap();
        (dir, ResolvedCommand::plain(program))
    }

    fn started(lookups: &AtomicUsize) -> usize {
        lookups.load(Ordering::SeqCst)
    }

    /// Counts the warnings logged on this thread while the guard lives. Tests
    /// run on a current-thread runtime, so that includes the background tasks.
    fn count_warnings() -> (Arc<AtomicUsize>, tracing::subscriber::DefaultGuard) {
        struct Warnings(Arc<AtomicUsize>);

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
            fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
                if *event.metadata().level() == tracing::Level::WARN {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        let warnings = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(Warnings(warnings.clone()));
        (warnings, tracing::subscriber::set_default(subscriber))
    }

    /// `get()` bounded by a generous outer timeout, so a lookup that is waited on
    /// for too long fails the test instead of hanging it.
    async fn get_within_reason(cache: &NodeCache) -> Option<ResolvedCommand> {
        timeout(MAX_WAIT * 10, cache.get())
            .await
            .expect("get() must come back without waiting for the lookup to finish")
    }

    fn assert_waited(since: Instant, expected: Duration) {
        let waited = since.elapsed();
        assert!(
            waited >= expected && waited <= expected + Duration::from_millis(50),
            "expected to wait {expected:?}, waited {waited:?}"
        );
    }

    // -- remembering ---------------------------------------------------------

    #[tokio::test]
    async fn a_found_runtime_is_remembered_while_its_program_exists() {
        let (_dir, node) = installed_node();
        let (resolve, lookups) = resolver(Duration::ZERO, Some(node.clone()));
        let cache = NodeCache::new(resolve);

        assert_eq!(cache.get().await, Some(node.clone()));
        assert_eq!(cache.get().await, Some(node.clone()));
        assert_eq!(cache.get().await, Some(node));

        assert_eq!(started(&lookups), 1, "later asks are answered from memory");
    }

    #[tokio::test]
    async fn a_runtime_whose_program_is_gone_is_looked_up_again() {
        let (_dir, node) = installed_node();
        let (resolve, lookups) = resolver(Duration::ZERO, Some(node.clone()));
        let cache = NodeCache::new(resolve);
        assert!(cache.get().await.is_some());

        std::fs::remove_file(&node.program).unwrap();
        // The lookup still answers with the vanished program here; what matters
        // is that the memory was not trusted.
        assert_eq!(cache.get().await, Some(node));

        assert_eq!(started(&lookups), 2);
    }

    // -- never waiting for long ------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_lookup_that_never_finishes_costs_a_caller_three_seconds_at_most() {
        let (resolve, lookups) = never_finishing();
        let cache = NodeCache::new(resolve);

        let before = Instant::now();
        assert_eq!(get_within_reason(&cache).await, None);
        assert_waited(before, Duration::from_secs(3));

        // The next caller shares the lookup in flight and gets the same budget.
        let before = Instant::now();
        assert_eq!(get_within_reason(&cache).await, None);
        assert_waited(before, Duration::from_secs(3));
        assert_eq!(
            started(&lookups),
            1,
            "the lookup in flight is not started a second time"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_lookup_started_by_an_earlier_call_is_reused() {
        let (_dir, node) = installed_node();
        let (resolve, lookups) = resolver(Duration::from_secs(5), Some(node.clone()));
        let cache = NodeCache::new(resolve);
        let start = Instant::now();

        assert_eq!(
            get_within_reason(&cache).await,
            None,
            "the first caller gives up after its three seconds"
        );
        assert_waited(start, Duration::from_secs(3));

        assert_eq!(
            get_within_reason(&cache).await,
            Some(node.clone()),
            "the next caller gets the lookup that is still running"
        );
        assert_waited(start, Duration::from_secs(5));

        assert_eq!(
            get_within_reason(&cache).await,
            Some(node),
            "later ones the remembered runtime"
        );
        assert_eq!(started(&lookups), 1, "one lookup served all three");
    }

    // -- one lookup at a time --------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn callers_that_arrive_together_share_one_lookup() {
        let (_dir, node) = installed_node();
        let (resolve, lookups) = resolver(Duration::from_secs(1), Some(node.clone()));
        let cache = NodeCache::new(resolve);

        let answers = tokio::join!(cache.get(), cache.get(), cache.get(), cache.get());

        assert_eq!(
            answers,
            (Some(node.clone()), Some(node.clone()), Some(node.clone()), Some(node))
        );
        assert_eq!(started(&lookups), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn callers_on_different_threads_share_one_lookup() {
        let (_dir, node) = installed_node();
        let (resolve, lookups) = resolver(Duration::from_millis(100), Some(node.clone()));
        let cache = NodeCache::new(resolve);

        let callers: Vec<_> = (0..16)
            .map(|_| {
                let cache = cache.clone();
                tokio::spawn(async move { cache.get().await })
            })
            .collect();
        for caller in callers {
            assert_eq!(caller.await.unwrap(), Some(node.clone()));
        }

        assert_eq!(started(&lookups), 1);
    }

    // -- failures --------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_failed_lookup_is_not_repeated_inside_the_cooldown() {
        let (resolve, lookups) = resolver(Duration::ZERO, None);
        let cache = NodeCache::new(resolve);
        let before = Instant::now();

        for _ in 0..5 {
            assert_eq!(get_within_reason(&cache).await, None);
        }
        assert_eq!(started(&lookups), 1, "five callers, one lookup");
        assert_eq!(before.elapsed(), Duration::ZERO, "nobody waited for the failure");

        tokio::time::advance(RETRY_AFTER_FAILURE - Duration::from_secs(1)).await;
        assert_eq!(get_within_reason(&cache).await, None);
        assert_eq!(started(&lookups), 1, "still inside the cooldown");

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(get_within_reason(&cache).await, None);
        assert_eq!(
            started(&lookups),
            2,
            "the cooldown is over, the next caller tries again"
        );

        for _ in 0..3 {
            assert_eq!(get_within_reason(&cache).await, None);
        }
        assert_eq!(started(&lookups), 2, "and that failure starts a new cooldown");
    }

    #[tokio::test(start_paused = true)]
    async fn the_cooldown_counts_from_the_failure_not_from_the_start_of_the_lookup() {
        let (resolve, lookups) = resolver(Duration::from_secs(60), None);
        let cache = NodeCache::new(resolve);

        assert_eq!(
            get_within_reason(&cache).await,
            None,
            "the caller gives up after three seconds"
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(get_within_reason(&cache).await, None, "the lookup has failed by now");
        assert_eq!(started(&lookups), 1);

        tokio::time::advance(RETRY_AFTER_FAILURE - Duration::from_secs(1)).await;
        assert_eq!(get_within_reason(&cache).await, None);
        assert_eq!(started(&lookups), 1, "less than the cooldown after the failure");

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(get_within_reason(&cache).await, None);
        assert_eq!(started(&lookups), 2, "more than the cooldown after the failure");
    }

    #[tokio::test(start_paused = true)]
    async fn a_lookup_that_panics_counts_as_a_failure() {
        let started = Arc::new(AtomicUsize::new(0));
        let counter = started.clone();
        let resolve: NodeResolver = Arc::new(move || -> NodeLookup {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { blow_up() })
        });
        let cache = NodeCache::new(resolve);

        assert_eq!(get_within_reason(&cache).await, None);
        assert_eq!(get_within_reason(&cache).await, None);

        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "it is held off like any other failure, and nobody is left waiting"
        );
    }

    // -- warming up --------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn warming_up_starts_the_lookup_before_anyone_asks() {
        let (_dir, node) = installed_node();
        let (resolve, lookups) = resolver(Duration::from_secs(1), Some(node.clone()));
        let cache = NodeCache::new(resolve);

        cache.warm_up();
        cache.warm_up();
        // Nobody is waiting; the lookup finishes by itself.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(started(&lookups), 1, "warming up twice starts one lookup");

        let before = Instant::now();
        assert_eq!(
            cache.get().await,
            Some(node),
            "the first caller finds the runtime ready"
        );
        assert_eq!(before.elapsed(), Duration::ZERO);
        assert_eq!(started(&lookups), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn warming_up_respects_the_cooldown_after_a_failure() {
        let (resolve, lookups) = resolver(Duration::ZERO, None);
        let cache = NodeCache::new(resolve);
        assert_eq!(get_within_reason(&cache).await, None);

        cache.warm_up();
        tokio::task::yield_now().await;
        assert_eq!(started(&lookups), 1, "inside the cooldown");

        tokio::time::advance(RETRY_AFTER_FAILURE).await;
        cache.warm_up();
        tokio::task::yield_now().await;
        assert_eq!(started(&lookups), 2, "after the cooldown");
    }

    // -- logging -----------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_failed_lookup_is_reported_once_not_by_every_call() {
        let (warnings, _guard) = count_warnings();
        let (resolve, _lookups) = resolver(Duration::ZERO, None);
        let cache = NodeCache::new(resolve);

        for _ in 0..6 {
            assert_eq!(get_within_reason(&cache).await, None);
        }

        assert_eq!(
            warnings.load(Ordering::SeqCst),
            1,
            "one warning when the lookup failed, none for the calls it held off"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_lookup_is_reported_once_not_by_every_waiting_caller() {
        let (warnings, _guard) = count_warnings();
        let (resolve, _lookups) = never_finishing();
        let cache = NodeCache::new(resolve);

        for _ in 0..4 {
            assert_eq!(get_within_reason(&cache).await, None);
        }

        assert_eq!(warnings.load(Ordering::SeqCst), 1);
    }
}
