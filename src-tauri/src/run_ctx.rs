//! Everything a turn needs from the process driving it, with no window in it.
//!
//! M1 proved a gateway can read the store. M2 drives real turns from a process
//! that has no window, so the run path cannot take `tauri::AppHandle`: it takes a
//! [`RunContext`] instead. The context carries the four things a turn actually
//! needs from its host — where its events go, where its caches live, the UI
//! request registry, and the surface that builds host tools — and the desktop
//! keeps its app handle inside [`TurnSurface`] rather than in the run path
//! (PAN-102).
//!
//! Two rules keep this honest:
//!
//! - **Emitting never needs a window.** [`RunContext::emit`] writes to the sink
//!   the process installed; the desktop installs its own app handle as that sink
//!   (`lib.rs`, `run()`), so a desktop emit is byte-for-byte what it was.
//! - **A missing sink is an error, not silence.** [`RunContext::headless`] refuses
//!   to build without one, because a run whose events go nowhere looks exactly
//!   like a turn that hangs.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::{
    architect::Admission,
    events::{self, Emitter},
    providers::{AgentEvent, ToolHost},
    RunningAgent,
};

/// How a turn ended, in the terms the desktop needs to route it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnOutcome {
    pub successful: bool,
    pub stopped: bool,
}

/// What only the driving process can do for a turn.
///
/// The desktop implements this with an app handle: it builds the window-scoped
/// host tools (browser, VM, simulator, bots, deploy), forwards approvals to the
/// team scheduler and routes finished turns to bot channels. A headless process
/// implements it with [`NoSurface`] until it has hosts of its own — which is why
/// the run path can be shared before every tool exists remotely.
pub trait TurnSurface: Send + Sync {
    /// Whether anything that can answer a question is attached right now.
    ///
    /// A desktop window is always a client. A headless process is only a client
    /// while something is listening, which decides whether an app action fails
    /// fast instead of waiting for a picker nobody can open (PAN-106).
    ///
    /// The first caller is a remote app action (tasks 7 and 10); until then only
    /// the tests exercise it, which is why it is allowed to be unused.
    #[allow(dead_code)]
    fn client_attached(&self) -> bool;

    /// The host tools that need a window: the team or agent tool stack, before
    /// per-turn wrappers are added.
    fn base_host(
        &self,
        provider: &str,
        session_id: &str,
        stopped: Arc<AtomicBool>,
        team: Option<&Admission>,
    ) -> Option<ToolHost>;

    /// The window-scoped wrappers around a host (Jev, bots, simulator, VM,
    /// deploy, preview). `base` is what `base_host` returned.
    fn wrap_hosts(
        &self,
        base: Option<ToolHost>,
        session_id: &str,
        stopped: Arc<AtomicBool>,
    ) -> Option<ToolHost>;

    /// An approval or question arrived during the turn.
    fn approval(&self, event: &AgentEvent, team: Option<&Admission>);

    /// An approval asked during the run reached a bot conversation.
    fn bot_approval(&self, session_id: &str, request_id: &str, text: &str);

    /// A turn finished; bot conversations reply to their channel here.
    fn run_finished(&self, session_id: &str, successful: bool, stopped: bool);

    /// The registry has released the run; team scheduling continues from here.
    fn turn_completed(
        &self,
        session_id: &str,
        outcome: TurnOutcome,
        team: Option<&Admission>,
        agents: &HashMap<String, RunningAgent>,
    );
}

/// A surface for a process with no window and no desktop tools yet.
///
/// M2 task 2 gives the gateway its own surface; until then this is the honest
/// answer for a headless run, and the unit tests below construct it.
#[allow(dead_code)]
pub struct NoSurface;

impl TurnSurface for NoSurface {
    fn client_attached(&self) -> bool {
        false
    }

    fn base_host(
        &self,
        _provider: &str,
        _session_id: &str,
        _stopped: Arc<AtomicBool>,
        _team: Option<&Admission>,
    ) -> Option<ToolHost> {
        None
    }

    fn wrap_hosts(
        &self,
        _base: Option<ToolHost>,
        _session_id: &str,
        _stopped: Arc<AtomicBool>,
    ) -> Option<ToolHost> {
        None
    }

    fn approval(&self, _event: &AgentEvent, _team: Option<&Admission>) {}

    fn bot_approval(&self, _session_id: &str, _request_id: &str, _text: &str) {}

    fn run_finished(&self, _session_id: &str, _successful: bool, _stopped: bool) {}

    fn turn_completed(
        &self,
        _session_id: &str,
        _outcome: TurnOutcome,
        _team: Option<&Admission>,
        _agents: &HashMap<String, RunningAgent>,
    ) {
    }
}

/// An app action waiting for a client to answer it.
struct PendingUi {
    reply: oneshot::Sender<crate::Result<Value>>,
    stopped: Arc<AtomicBool>,
}

/// The app actions the agent asked for, waiting for a client.
///
/// One per process and owned by `AppState`, not a separate Tauri-managed state:
/// the run path reaches it through the context, and the command that answers it
/// reaches it through the app state. A remote client will use the same registry.
#[derive(Default)]
pub struct UiRequests(Mutex<HashMap<String, PendingUi>>);

/// More than this many unanswered app actions is a stuck client, not a queue.
const MAX_PENDING_UI: usize = 16;

impl UiRequests {
    /// Registers a request and returns the receiver its answer will arrive on.
    pub(crate) fn register(
        &self,
        id: String,
        stopped: Arc<AtomicBool>,
    ) -> crate::Result<oneshot::Receiver<crate::Result<Value>>> {
        let (reply, rx) = oneshot::channel();
        let mut pending = self.0.lock().map_err(|e| e.to_string())?;
        if pending.len() >= MAX_PENDING_UI {
            return Err("Too many pending app actions.".into());
        }
        pending.insert(id, PendingUi { reply, stopped });
        Ok(rx)
    }

    /// Answers a pending request. Both the removal and the stopped check happen
    /// here, in that order, so an answer for a finished task fails rather than
    /// waking a request whose turn is gone.
    pub(crate) fn answer(&self, id: &str, result: crate::Result<Value>) -> crate::Result<()> {
        let pending = self
            .0
            .lock()
            .map_err(|e| e.to_string())?
            .remove(id)
            .ok_or("App action expired.")?;
        if pending.stopped.load(Ordering::SeqCst) {
            return Err("Task has stopped.".into());
        }
        pending
            .reply
            .send(result)
            .map_err(|_| "App action expired.".to_string())
    }

    /// Drops a request whose asker is gone.
    pub(crate) fn forget(&self, id: &str) {
        if let Ok(mut pending) = self.0.lock() {
            pending.remove(id);
        }
    }

    /// Whether a request is still waiting and its run is alive.
    pub(crate) fn active(&self, id: &str) -> bool {
        self.0
            .lock()
            .ok()
            .and_then(|pending| {
                pending
                    .get(id)
                    .map(|PendingUi { stopped, .. }| !stopped.load(Ordering::SeqCst))
            })
            .unwrap_or(false)
    }
}

/// What a turn needs from the process driving it: no window, no runtime handle.
#[derive(Clone)]
pub struct RunContext {
    emitter: Arc<dyn Emitter>,
    cache_dir: PathBuf,
    data_dir: PathBuf,
    ui: Arc<UiRequests>,
    surface: Arc<dyn TurnSurface>,
}

impl RunContext {
    /// A context over an explicit emitter. The desktop passes its own sink here.
    pub fn new(
        emitter: Arc<dyn Emitter>,
        cache_dir: PathBuf,
        data_dir: PathBuf,
        ui: Arc<UiRequests>,
        surface: Arc<dyn TurnSurface>,
    ) -> Self {
        Self {
            emitter,
            cache_dir,
            data_dir,
            ui,
            surface,
        }
    }

    /// A context for a process with no window: the emitter is the sink the process
    /// installed, and no app handle is involved anywhere.
    ///
    /// A missing sink is refused rather than defaulted, because a turn whose
    /// events reach nobody is indistinguishable from one that hung.
    ///
    /// The gateway constructs its context here in M2 task 2; the tests below are
    /// the only caller until then.
    #[allow(dead_code)]
    pub fn headless(
        cache_dir: PathBuf,
        data_dir: PathBuf,
        ui: Arc<UiRequests>,
        surface: Arc<dyn TurnSurface>,
    ) -> crate::Result<Self> {
        let emitter = events::sink().ok_or_else(|| "no_event_sink: no event sink is installed".to_string())?;
        Ok(Self::new(emitter, cache_dir, data_dir, ui, surface))
    }

    /// The desktop's context: its own app handle is the emitter when nothing else
    /// was installed, exactly as `events::emit(&app, …)` did before this type.
    pub fn for_app(
        app: &tauri::AppHandle,
        cache_dir: PathBuf,
        data_dir: PathBuf,
        ui: Arc<UiRequests>,
        surface: Arc<dyn TurnSurface>,
    ) -> Self {
        let emitter = events::sink()
            .unwrap_or_else(|| Arc::new(app.clone()) as Arc<dyn Emitter>);
        Self::new(emitter, cache_dir, data_dir, ui, surface)
    }

    /// Emits to every surface this process serves.
    pub fn emit<T: Serialize>(&self, name: &str, payload: T) -> crate::Result<()> {
        self.emitter.emit(name, events::payload_value(payload))
    }

    /// Emits to one named target on every surface this process serves.
    ///
    /// Window-targeted events (`agent-ui-action`) go through this once a remote
    /// client is the thing being targeted (M2 task 5).
    #[allow(dead_code)]
    pub fn emit_to<T: Serialize>(
        &self,
        target: &str,
        name: &str,
        payload: T,
    ) -> crate::Result<()> {
        self.emitter
            .emit_to(target, name, events::payload_value(payload))
    }

    /// Where captures and per-turn scratch files live for this run.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// The process's own data directory, for anything that must outlive a turn
    /// (uploads, the audit log; M2 tasks 7 and 11).
    #[allow(dead_code)]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The shared app-action registry, for a headless host that answers UI requests
    /// through the context rather than through an app handle (M2 task 2).
    #[allow(dead_code)]
    pub fn ui(&self) -> &Arc<UiRequests> {
        &self.ui
    }

    /// What this process can do for a turn.
    pub fn surface(&self) -> &Arc<dyn TurnSurface> {
        &self.surface
    }

    /// The host tools that need a window, before per-turn wrappers.
    pub fn tool_host(
        &self,
        provider: &str,
        session_id: &str,
        stopped: Arc<AtomicBool>,
        team: Option<&Admission>,
    ) -> Option<ToolHost> {
        self.surface.base_host(provider, session_id, stopped, team)
    }

    /// The window-scoped wrappers around a host, from this process's surface.
    pub fn wrap_tool_hosts(
        &self,
        base: Option<ToolHost>,
        session_id: &str,
        stopped: Arc<AtomicBool>,
    ) -> Option<ToolHost> {
        self.surface.wrap_hosts(base, session_id, stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Records what reaches it, so a test can assert on the bytes a surface sees.
    #[derive(Default)]
    struct Recorder(StdMutex<Vec<(String, Value)>>);

    impl Emitter for Recorder {
        fn emit(&self, name: &str, payload: Value) -> crate::Result<()> {
            self.0.lock().unwrap().push((name.to_owned(), payload));
            Ok(())
        }
        fn emit_to(&self, target: &str, name: &str, payload: Value) -> crate::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push((format!("{target}/{name}"), payload));
            Ok(())
        }
    }

    fn context(emitter: Arc<Recorder>) -> RunContext {
        RunContext::new(
            emitter,
            PathBuf::from("/tmp/cache"),
            PathBuf::from("/tmp/data"),
            Arc::new(UiRequests::default()),
            Arc::new(NoSurface),
        )
    }

    #[test]
    fn a_context_emit_reaches_the_sink_with_the_same_payload_shape() {
        let recorder = Arc::new(Recorder::default());
        let ctx = context(recorder.clone());
        ctx.emit("sessions-changed", ()).unwrap();
        ctx.emit_to("main", "agent-ui-action", serde_json::json!({"requestId":"r"}))
            .unwrap();
        let seen = recorder.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        // The same payload the app has always received: `()` is null, not `{}`.
        assert_eq!(seen[0], ("sessions-changed".into(), Value::Null));
        assert_eq!(seen[1].0, "main/agent-ui-action");
        assert_eq!(seen[1].1["requestId"], "r");
    }

    #[test]
    fn a_context_reports_where_a_run_writes_and_what_it_can_do() {
        let recorder = Arc::new(Recorder::default());
        let ctx = context(recorder);
        assert_eq!(ctx.cache_dir(), Path::new("/tmp/cache"));
        assert_eq!(ctx.data_dir(), Path::new("/tmp/data"));
        assert!(ctx
            .tool_host("claude", "s1", Arc::new(AtomicBool::new(false)), None)
            .is_none());
    }

    #[test]
    fn answering_an_app_action_twice_fails_the_second_time() {
        let ui = UiRequests::default();
        let rx = ui
            .register("r1".into(), Arc::new(AtomicBool::new(false)))
            .unwrap();
        ui.answer("r1", Ok(Value::Null)).unwrap();
        assert!(rx.blocking_recv().unwrap().is_ok());
        assert_eq!(ui.answer("r1", Ok(Value::Null)), Err("App action expired.".to_string()));
    }

    #[test]
    fn a_stopped_run_cannot_be_answered() {
        let ui = UiRequests::default();
        let stopped = Arc::new(AtomicBool::new(true));
        let _rx = ui.register("r1".into(), stopped).unwrap();
        assert!(!ui.active("r1"), "a stopped run's request is not answerable");
        assert_eq!(ui.answer("r1", Ok(Value::Null)), Err("Task has stopped.".to_string()));
    }

    #[test]
    fn a_headless_context_uses_the_installed_sink_and_refuses_without_one() {
        // The M2 entry point: no window anywhere in this test.
        let _serial = events::TEST_SINK_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events::reset_sink();
        let refused = RunContext::headless(
            PathBuf::from("/tmp/cache"),
            PathBuf::from("/tmp/data"),
            Arc::new(UiRequests::default()),
            Arc::new(NoSurface),
        )
        .err()
        .expect("a context with no sink must be refused");
        assert_eq!(refused, "no_event_sink: no event sink is installed");

        let recorder = Arc::new(Recorder::default());
        assert!(events::set_sink(recorder.clone()), "the sink was free");
        let ctx = RunContext::headless(
            PathBuf::from("/tmp/cache"),
            PathBuf::from("/tmp/data"),
            Arc::new(UiRequests::default()),
            Arc::new(NoSurface),
        )
        .expect("a context over an installed sink");
        ctx.emit("agent-event", ()).unwrap();
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
        events::reset_sink();
    }
}
