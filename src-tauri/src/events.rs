//! One event sink for every surface: the desktop's webviews and the gateway's
//! WebSocket clients receive the same names and the same payload JSON.
//!
//! Call sites use `events::emit(&app, name, payload)` instead of
//! `app.emit(name, payload)`. With no sink installed — the desktop today — that
//! falls through to exactly the call it replaced, so payloads and timing are
//! unchanged. A process that serves other surfaces installs a sink once, and the
//! same payload then travels to both.

use std::sync::{Arc, RwLock};

use serde::Serialize;

/// A destination for emitted events. Both methods take already-serialized JSON so
/// every surface sees the same bytes.
///
/// Both return the delivery error. A sink that cannot deliver is the difference
/// between a caller failing fast and one waiting out a timeout for a reply that
/// will never come, so this must not be swallowed one layer down.
pub trait Emitter: Send + Sync {
    fn emit(&self, name: &str, payload: serde_json::Value) -> Result<(), String>;
    fn emit_to(&self, target: &str, name: &str, payload: serde_json::Value) -> Result<(), String>;
    /// An event addressed to one remote client identity (PAN-123).
    ///
    /// A process with no remote clients — the desktop — has no identity to address,
    /// so this fails explicitly rather than quietly broadcasting. A native turn
    /// carries no client, so it never reaches this default.
    #[allow(dead_code)]
    fn emit_to_client(
        &self,
        _client: &str,
        _name: &str,
        _payload: serde_json::Value,
    ) -> Result<(), String> {
        Err("This process has no client to address events to.".into())
    }
}

/// The desktop's own sink: the same calls the app made before this module existed.
impl<R: tauri::Runtime> Emitter for tauri::AppHandle<R> {
    fn emit(&self, name: &str, payload: serde_json::Value) -> Result<(), String> {
        tauri::Emitter::emit(self, name, payload).map_err(|error| error.to_string())
    }

    fn emit_to(&self, target: &str, name: &str, payload: serde_json::Value) -> Result<(), String> {
        // Mirrors the call this module replaced. Resolving a window by label first
        // would quietly narrow what a target can be.
        tauri::Emitter::emit_to(self, target, name, payload).map_err(|error| error.to_string())
    }
}

/// Settable once, and reset only in tests. A `RwLock` rather than a `OnceLock`
/// because a test process must be able to drop a sink it installed instead of
/// inheriting the first test's dead one; reads stay a read lock on a path that
/// fires a few times a second at most.
static SINK: RwLock<Option<Arc<dyn Emitter>>> = RwLock::new(None);

/// Serializes tests that install or drop the process-wide sink.
///
/// The sink is global, so a test that installs one would otherwise be visible to
/// an unrelated test running beside it — including the ones in `run_ctx`.
#[cfg(test)]
pub(crate) static TEST_SINK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Installs the process-wide sink. First call wins, because a second writer would
/// split one event stream across two destinations; returns whether this call was
/// the one that took effect.
pub fn set_sink(sink: Arc<dyn Emitter>) -> bool {
    let mut guard = lock();
    if guard.is_some() {
        return false;
    }
    *guard = Some(sink);
    true
}

/// Forgets the installed sink. Test-only: production installs one for the life of
/// the process, and a reset there would be a way to lose events silently.
#[cfg(test)]
pub(crate) fn reset_sink() {
    *lock() = None;
}

fn lock() -> std::sync::RwLockWriteGuard<'static, Option<Arc<dyn Emitter>>> {
    // A sink that panicked mid-emit must not turn into a panic on every later
    // event, which would take the app's event stream down with it.
    SINK.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn installed() -> Option<Arc<dyn Emitter>> {
    SINK.read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The installed sink, for a caller that has no app handle to fall back to.
///
/// A process that serves another surface (the gateway, a test) reads this to build
/// its `RunContext`; `None` means this process has not installed one.
pub(crate) fn sink() -> Option<Arc<dyn Emitter>> {
    installed()
}

/// Emits to every listener on every surface this process serves.
///
/// A sink takes the event without an app, so delivery there cannot fail; without
/// one this is exactly the Tauri call it replaced, error included, because a site
/// that waits for a UI reply must be able to fail fast when nothing will answer.
pub fn emit<R: tauri::Runtime, T: Serialize>(
    app: &tauri::AppHandle<R>,
    name: &str,
    payload: T,
) -> Result<(), String> {
    let value = payload_value(payload);
    match installed() {
        Some(sink) => sink.emit(name, value),
        None => tauri::Emitter::emit(app, name, value).map_err(|error| error.to_string()),
    }
}

/// Emits to one named target on every surface, and to the app when none is set.
pub fn emit_to<R: tauri::Runtime, T: Serialize>(
    app: &tauri::AppHandle<R>,
    target: &str,
    name: &str,
    payload: T,
) -> Result<(), String> {
    let value = payload_value(payload);
    match installed() {
        Some(sink) => sink.emit_to(target, name, value),
        None => tauri::Emitter::emit_to(app, target, name, value)
            .map_err(|error| error.to_string()),
    }
}

/// Emits to one specific window object rather than by name.
///
/// Window-scoped UI events stay local: a sink has no window identity, so there is
/// nothing for another surface to receive. This is the same call the site made
/// before, with the payload serialized once.
pub fn emit_window<R: tauri::Runtime, W: tauri::Emitter<R>>(
    window: &W,
    name: &str,
    payload: impl Serialize,
) -> Result<(), String> {
    tauri::Emitter::emit(window, name, payload_value(payload)).map_err(|error| error.to_string())
}

/// Serializes once, the way every emit path needs it. A payload that cannot
/// serialize is sent as null rather than dropped, which matches what the previous
/// `let _ = app.emit(…)` sites did with the error.
pub(crate) fn payload_value<T: Serialize>(payload: T) -> serde_json::Value {
    serde_json::to_value(payload).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    //! These pin the two payloads the frontend actually reads, in the shape that
    //! reaches it. The failure this guards against is silent: a renamed or
    //! re-nested field compiles, emits and simply stops updating the UI. They run
    //! in `cargo test --lib`, which CI does run without the gateway feature.

    use super::*;
    use crate::providers::{AgentEvent, ToolActivity};

    /// A streaming text delta, the event whose loss freezes the transcript.
    fn text_delta() -> AgentEvent {
        AgentEvent {
            session_id: "s1".into(),
            kind: "text".into(),
            text: "hello".into(),
            request_id: None,
            data: None,
            tool: None,
        }
    }

    #[test]
    fn agent_event_text_delta_shape_is_pinned() {
        let json = payload_value(text_delta());
        assert_eq!(
            json,
            serde_json::json!({
                "sessionId": "s1",
                "kind": "text",
                "text": "hello",
                "requestId": null,
                "data": null,
            }),
            "agent-event shape changed; the transcript reads exactly these fields"
        );
        // tool is skipped while absent rather than arriving as null.
        assert!(json.get("tool").is_none());
    }

    #[test]
    fn agent_event_tool_activity_shape_is_pinned() {
        let mut event = text_delta();
        event.kind = "tool".into();
        event.tool = Some(ToolActivity {
            id: "t1".into(),
            name: "Read".into(),
            status: "running".into(),
            input: String::new(),
            output: String::new(),
            write_paths: None,
        });
        let json = payload_value(event);
        let tool = &json["tool"];
        assert_eq!(tool["id"], "t1");
        assert_eq!(tool["name"], "Read");
        assert_eq!(tool["status"], "running");
        assert_eq!(tool["input"], "");
        assert_eq!(tool["output"], "");
        assert!(json["sessionId"] == "s1");
    }

    #[test]
    fn sessions_changed_payload_is_null_not_an_object() {
        // Every sessions-changed call site passes `()`. The UI listens for the name
        // alone, and a payload that became `{}` or a struct would still "work"
        // until some client started reading fields from it.
        assert_eq!(payload_value(()), serde_json::Value::Null);
    }

    /// Delivers nothing, the way a window that is gone does.
    struct FailingSink;

    impl Emitter for FailingSink {
        fn emit(&self, _name: &str, _payload: serde_json::Value) -> Result<(), String> {
            Err("the window is gone".into())
        }
        fn emit_to(
            &self,
            _target: &str,
            _name: &str,
            _payload: serde_json::Value,
        ) -> Result<(), String> {
            Err("the window is gone".into())
        }
    }

    #[test]
    fn a_sink_error_reaches_the_caller_instead_of_being_swallowed() {
        let _serial = TEST_SINK_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // The regression this guards: with the app handle as the sink, an emit
        // failure used to be discarded in the trait impl, so `emit_to(...)?` could
        // never fire and request_ui waited out its 12-second timeout for a reply no
        // window would ever send.
        reset_sink();
        assert!(set_sink(Arc::new(FailingSink)), "the sink was free");
        assert!(
            !set_sink(Arc::new(FailingSink)),
            "a second sink must not replace the first"
        );

        let app = tauri::test::mock_app();
        let handle = app.handle().clone();
        assert_eq!(
            emit_to(&handle, "main", "agent-ui-action", ()),
            Err("the window is gone".to_string()),
            "emit_to swallowed the sink's error"
        );
        assert_eq!(
            emit(&handle, "sessions-changed", ()),
            Err("the window is gone".to_string()),
            "emit swallowed the sink's error"
        );
        reset_sink();
    }
}
