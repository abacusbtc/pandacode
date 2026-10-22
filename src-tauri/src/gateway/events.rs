//! The gateway's sink: every emitted event becomes one JSON text frame on every
//! connected `/events` socket.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::events::Emitter;

/// Fans one event out to every connected socket.
pub struct BroadcastSink {
    sender: broadcast::Sender<String>,
}

impl BroadcastSink {
    /// The buffer holds a burst of tool events while a client is slow or
    /// reconnecting; a client that falls further behind is told it lagged rather
    /// than stalling the emitter.
    pub fn new() -> Arc<BroadcastSink> {
        let (sender, _) = broadcast::channel(1024);
        Arc::new(BroadcastSink { sender })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.sender.subscribe()
    }

    /// One frame to every socket. No receivers is the normal state before a
    /// client connects.
    pub fn publish(&self, name: &str, payload: serde_json::Value) {
        let _ = self.sender.send(frame(name, payload));
    }
}

impl Emitter for BroadcastSink {
    fn emit(&self, name: &str, payload: serde_json::Value) -> Result<(), String> {
        self.publish(name, payload);
        // Broadcasting has no per-client failure: a socket that is gone is gone,
        // and the sender cannot report it.
        Ok(())
    }

    /// A remote client has no windows to target, so a targeted event reaches every
    /// connected client: the closest honest equivalent, and the only one that does
    /// not silently drop the event.
    fn emit_to(&self, _target: &str, name: &str, payload: serde_json::Value) -> Result<(), String> {
        self.emit(name, payload)
    }
}

fn frame(name: &str, payload: serde_json::Value) -> String {
    serde_json::json!({ "event": name, "payload": payload }).to_string()
}
