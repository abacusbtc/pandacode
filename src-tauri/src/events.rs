//! One event sink for every surface: the desktop's webviews and the gateway's
//! WebSocket clients receive the same names and the same payload JSON.
//!
//! Call sites use `events::emit(&app, name, payload)` instead of
//! `app.emit(name, payload)`. With no sink installed — the desktop today — that
//! falls through to exactly the call it replaced, so payloads and timing are
//! unchanged. A process that serves other surfaces installs a sink once, and the
//! same payload then travels to both.

use std::sync::{Arc, OnceLock};

use serde::Serialize;

/// A destination for emitted events. Both methods take already-serialized JSON so
/// every surface sees the same bytes.
pub trait Emitter: Send + Sync {
    fn emit(&self, name: &str, payload: serde_json::Value);
    fn emit_to(&self, target: &str, name: &str, payload: serde_json::Value);
}

/// The desktop's own sink: the same calls the app made before this module existed.
impl Emitter for tauri::AppHandle {
    fn emit(&self, name: &str, payload: serde_json::Value) {
        let _ = tauri::Emitter::emit(self, name, payload);
    }

    fn emit_to(&self, target: &str, name: &str, payload: serde_json::Value) {
        // Mirrors the call this module replaced. Resolving a window by label first
        // would quietly narrow what a target can be.
        let _ = tauri::Emitter::emit_to(self, target, name, payload);
    }
}

static SINK: OnceLock<Arc<dyn Emitter>> = OnceLock::new();

/// Installs the process-wide sink. First call wins, because a second writer would
/// split one event stream across two destinations; returns whether this call was
/// the one that took effect.
pub fn set_sink(sink: Arc<dyn Emitter>) -> bool {
    SINK.set(sink).is_ok()
}

/// Emits to every listener on every surface this process serves.
///
/// A sink takes the event without an app, so delivery there cannot fail; without
/// one this is exactly the Tauri call it replaced, error included, because a site
/// that waits for a UI reply must be able to fail fast when nothing will answer.
pub fn emit<T: Serialize>(app: &tauri::AppHandle, name: &str, payload: T) -> Result<(), String> {
    let value = payload_value(payload);
    match SINK.get() {
        Some(sink) => {
            sink.emit(name, value);
            Ok(())
        }
        None => tauri::Emitter::emit(app, name, value).map_err(|error| error.to_string()),
    }
}

/// Emits to one named target on every surface, and to the app when none is set.
pub fn emit_to<T: Serialize>(
    app: &tauri::AppHandle,
    target: &str,
    name: &str,
    payload: T,
) -> Result<(), String> {
    let value = payload_value(payload);
    match SINK.get() {
        Some(sink) => {
            sink.emit_to(target, name, value);
            Ok(())
        }
        None => tauri::Emitter::emit_to(app, target, name, value)
            .map_err(|error| error.to_string()),
    }
}

/// Emits to one specific window object rather than by name.
///
/// Window-scoped UI events stay local: a sink has no window identity, so there is
/// nothing for another surface to receive. This is the same call the site made
/// before, with the payload serialized once.
pub fn emit_window<W>(window: &W, name: &str, payload: impl Serialize) -> Result<(), String>
where
    W: tauri::Emitter<tauri::Wry>,
{
    tauri::Emitter::emit(window, name, payload_value(payload)).map_err(|error| error.to_string())
}

/// Serializes once, the way every emit path needs it. A payload that cannot
/// serialize is sent as null rather than dropped, which matches what the previous
/// `let _ = app.emit(…)` sites did with the error.
fn payload_value<T: Serialize>(payload: T) -> serde_json::Value {
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
}
