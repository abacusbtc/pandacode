//! Which tasks this gateway is driving, in this process, and the one thing a
//! remote turn needs from its surface: its end.
//!
//! A row in the store can claim to be running when no process is behind it — the
//! desktop settles those at boot and on read, but between the two nothing in the
//! database is proof of life. This registry is that proof for a gateway: it is
//! written when a turn is launched here and cleared when it ends, so "what is
//! live" is answered from the process that runs the turns rather than inferred
//! from a row.
//!
//! Task 4 adds sequence numbers on top of these entries and task 5 adds
//! approvals, which is why the entries carry the session id rather than only a
//! count.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::Serialize;

use crate::{
    architect::Admission,
    providers::{AgentEvent, ToolHost},
    run_ctx::{TurnOutcome, TurnSurface},
    RunningAgent,
};

/// One live turn, as a client sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveTask {
    pub session_id: String,
    pub provider: String,
    /// Epoch milliseconds, the same clock the store stamps rows with.
    pub started_at: i64,
}

/// The runs this process is driving, one instance per gateway.
#[derive(Default)]
pub struct Live(Mutex<HashMap<String, LiveTask>>);

impl Live {
    /// Records a turn this gateway is about to launch.
    ///
    /// Called before the launch, not after, so a turn that finishes immediately
    /// cannot finish before it was ever recorded: its own end then clears it.
    ///
    /// Returns whether this call was the one that recorded it. A second send on a
    /// session that is already live must not restart the clock, and its eventual
    /// refusal must not clear the turn that is actually running.
    pub fn start(&self, session_id: &str, provider: &str) -> crate::Result<bool> {
        let mut live = self.0.lock().map_err(|e| e.to_string())?;
        if live.contains_key(session_id) {
            return Ok(false);
        }
        live.insert(
            session_id.to_owned(),
            LiveTask {
                session_id: session_id.to_owned(),
                provider: provider.to_owned(),
                started_at: crate::storage::now(),
            },
        );
        Ok(true)
    }

    /// Forgets a turn that has ended, for whatever reason. Ending is the whole
    /// condition: successful, failed and stopped are all "not live".
    pub fn finish(&self, session_id: &str) {
        if let Ok(mut live) = self.0.lock() {
            live.remove(session_id);
        }
    }

    /// What is live now, oldest first so a client polling the list sees a stable
    /// order rather than a hash order.
    pub fn tasks(&self) -> Vec<LiveTask> {
        let mut tasks: Vec<LiveTask> = self
            .0
            .lock()
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        tasks.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        tasks
    }
}

/// The gateway's turn surface.
///
/// It offers no desktop hosts, and it is the seam where a remote turn's end is
/// observed: `turn_completed` runs after the registry has released the run and
/// before the next turn can start, which is exactly when an entry stops being
/// true. Task 5 fills in the approval half here.
pub struct GatewaySurface {
    live: Arc<Live>,
}

impl GatewaySurface {
    pub fn new(live: Arc<Live>) -> Self {
        Self { live }
    }
}

impl TurnSurface for GatewaySurface {
    fn base_host(
        &self,
        _provider: &str,
        _session_id: &str,
        _stopped: Arc<std::sync::atomic::AtomicBool>,
        _team: Option<&Admission>,
    ) -> Option<ToolHost> {
        None
    }

    fn wrap_hosts(
        &self,
        _base: Option<ToolHost>,
        _session_id: &str,
        _stopped: Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<ToolHost> {
        None
    }

    fn approval(&self, _event: &AgentEvent, _team: Option<&Admission>) {}

    fn bot_approval(&self, _session_id: &str, _request_id: &str, _text: &str) {}

    fn run_finished(&self, _session_id: &str, _successful: bool, _stopped: bool) {}

    fn turn_completed(
        &self,
        session_id: &str,
        _outcome: TurnOutcome,
        _team: Option<&Admission>,
        _agents: &HashMap<String, RunningAgent>,
    ) {
        self.live.finish(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn surface(live: &Arc<Live>) -> GatewaySurface {
        GatewaySurface::new(live.clone())
    }

    #[test]
    fn a_launched_turn_is_listed_and_a_finished_one_is_not() {
        let live = Arc::new(Live::default());
        assert!(live.tasks().is_empty(), "a gateway with nothing running lists nothing");
        live.start("s1", "claude").unwrap();
        live.start("s2", "codex").unwrap();
        let listed = live.tasks();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].session_id, "s1", "oldest first");
        assert_eq!(listed[0].provider, "claude");
        assert!(listed[0].started_at > 0, "a start time the client can show");

        // Ending removes it whatever the outcome was: the surface only knows that
        // the registry released the run.
        let agents = HashMap::new();
        for outcome in [
            TurnOutcome {
                successful: true,
                stopped: false,
            },
            TurnOutcome {
                successful: false,
                stopped: false,
            },
            TurnOutcome {
                successful: false,
                stopped: true,
            },
        ] {
            live.start("s1", "claude").unwrap();
            surface(&live).turn_completed("s1", outcome, None, &agents);
            assert!(
                !live.tasks().iter().any(|task| task.session_id == "s1"),
                "a turn that ended must not stay listed"
            );
        }
        assert_eq!(live.tasks().len(), 1, "the other task is still running");
        surface(&live).turn_completed(
            "s2",
            TurnOutcome {
                successful: true,
                stopped: false,
            },
            None,
            &agents,
        );
        assert!(live.tasks().is_empty());
    }

    #[test]
    fn the_surface_offers_no_desktop_tools() {
        let live = Arc::new(Live::default());
        let stopped = Arc::new(AtomicBool::new(false));
        assert!(surface(&live).base_host("codex", "s1", stopped.clone(), None).is_none());
        assert!(surface(&live).wrap_hosts(None, "s1", stopped).is_none());
    }

    #[test]
    fn the_list_serialises_the_shape_the_client_reads() {
        let live = Arc::new(Live::default());
        live.start("s1", "claude").unwrap();
        let json = serde_json::to_value(live.tasks()).unwrap();
        assert_eq!(json[0]["sessionId"], "s1");
        assert_eq!(json[0]["provider"], "claude");
        assert!(json[0]["startedAt"].is_i64(), "camelCase, like every other payload");
    }
}
