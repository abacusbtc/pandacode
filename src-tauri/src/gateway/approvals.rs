//! Auto-deny: a question nobody answers must not park a turn for ever.
//!
//! A turn that asks a permission question waits for a client to answer it. On the
//! desktop a window is always there; a gateway may have nobody connected, and the
//! question would park until the process dies. This watchdog gives every pending
//! approval its own deadline, counted from the moment it was raised, and at the
//! deadline sends the run the same denial a client would send, so the agent sees a
//! refusal it can plan around and its turn continues.
//!
//! The desktop keeps today's behaviour by construction: this module is started by
//! the gateway's `spawn` and nowhere else, because an attached window is an
//! attached client.

use std::{sync::Arc, time::Duration};

use crate::{
    events::{payload_value, Emitter},
    providers::RunInput,
    AppState,
};

/// How often pending approvals are looked at. A second keeps the sentence a client
/// reads accurate without walking the registry more often than it needs.
const TICK: Duration = Duration::from_secs(1);

/// Starts the watchdog. `None` means auto-deny is off (`--approval-ttl 0`), and no
/// task is started at all rather than one that would do nothing every second.
pub fn watch(
    state: Arc<AppState>,
    emitter: Arc<dyn Emitter>,
    ttl: Option<Duration>,
) -> Option<tokio::task::JoinHandle<()>> {
    let ttl = ttl?;
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TICK);
        loop {
            ticker.tick().await;
            if let Err(error) = deny_expired(&state, &emitter, ttl) {
                // A sweep that cannot read the registry must not take the gateway
                // down; the next tick tries again.
                eprintln!("approval sweep failed: {error}");
            }
        }
    }))
}

/// One pass over the live registry: deny every approval whose own deadline has
/// passed, and report how many were denied.
pub(crate) fn deny_expired(
    state: &AppState,
    emitter: &Arc<dyn Emitter>,
    ttl: Duration,
) -> Result<usize, String> {
    let ttl_ms = ttl.as_millis().min(i64::MAX as u128) as i64;
    let now = crate::storage::now();
    // Lock order, as everywhere else in this crate: registry -> approvals -> store.
    let agents = state.agents.lock().map_err(|e| e.to_string())?;
    let mut denied = 0;
    for (session_id, agent) in agents.iter() {
        // Each question has its own deadline, so one raised late is not denied
        // because the turn has been waiting a long time.
        let expired: Vec<(String, crate::PendingApproval)> = {
            let pending = agent.approvals.lock().map_err(|e| e.to_string())?;
            pending
                .iter()
                .filter(|(_, question)| now.saturating_sub(question.raised_at) >= ttl_ms)
                .map(|(id, question)| (id.clone(), question.clone()))
                .collect()
        };
        for (request_id, question) in expired {
            if deny(state, emitter, session_id, agent, &request_id, &question, ttl)? {
                denied += 1;
            }
        }
    }
    Ok(denied)
}

/// Denies one request. Returns whether this call was the one that took it out of
/// the pending set, which is what makes a deny and a client answer mutually
/// exclusive rather than merely fast.
fn deny(
    state: &AppState,
    emitter: &Arc<dyn Emitter>,
    session_id: &str,
    agent: &crate::RunningAgent,
    request_id: &str,
    question: &crate::PendingApproval,
    ttl: Duration,
) -> Result<bool, String> {
    if agent.stopped.load(std::sync::atomic::Ordering::SeqCst) {
        // The turn is over; finishing it owns the status, and there is nobody to
        // tell. The entry goes away with the run.
        return Ok(false);
    }
    // Remove first: whoever takes the id out of the pending set owns the answer,
    // so a client answering at this moment wins or is refused as no longer
    // pending — never both.
    let taken = {
        let mut pending = agent.approvals.lock().map_err(|e| e.to_string())?;
        pending.remove(request_id)
    };
    if taken.is_none() {
        return Ok(false);
    }

    let sentence = format!(
        "No client was connected for {}; the request was denied.",
        spell(ttl)
    );
    // The run hears exactly what a client's "no" would have told it.
    let original = request_id
        .strip_prefix(&format!("{}:", agent.run_id))
        .unwrap_or(request_id)
        .to_owned();
    let delivered = agent.send(RunInput::Approval {
        request_id: original,
        approved: false,
    });
    let still_waiting = agent
        .approvals
        .lock()
        .map(|pending| !pending.is_empty())
        .unwrap_or(false);
    {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        // Loud in the transcript: one row naming the request, so a client that
        // reconnects reads what happened instead of an unanswered question.
        store
            .message(
                session_id,
                "approval_denied",
                &format!("{sentence}\n\nRequest: {request_id}"),
            )
            .map_err(|e| e.to_string())?;
        store
            .status(session_id, if still_waiting { "waiting" } else { "running" })
            .map_err(|e| e.to_string())?;
    }
    // Loud on the ring: a client that connects afterwards and replays from before
    // this frame reads the denial, in the same shape an approval arrives in.
    let mut event = crate::event(session_id, "approval_denied", &sentence);
    event.request_id = Some(request_id.to_owned());
    let _ = emitter.emit("agent-event", payload_value(event));
    // One audit line: the gateway answered a question with nobody there, and the
    // journal is where that decision has to survive. The question itself is logged
    // because it *is* the decision — nothing else here is prose.
    super::audit::Line::new("approval_denied")
        .field("session", session_id)
        .field("request", request_id)
        .field("ttl_ms", ttl.as_millis())
        .field("action", &question.text)
        .emit();
    Ok(delivered.is_ok())
}

/// "1 h", "30 m", "45 s" — the unit a person reads in the sentence.
fn spell(ttl: Duration) -> String {
    let seconds = ttl.as_secs();
    if seconds >= 3600 && seconds.is_multiple_of(3600) {
        format!("{} h", seconds / 3600)
    } else if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{} m", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        gateway::events::{BroadcastSink, Filter},
        providers::RunInput,
        storage::Store,
        PendingApproval, RunningAgent,
    };
    use std::{
        collections::HashMap,
        sync::{atomic::AtomicBool, Mutex},
    };

    /// A gateway-shaped process: an in-memory store with one task mid-turn, and one
    /// live run whose input channel the test owns.
    struct Fixture {
        state: Arc<AppState>,
        sink: Arc<BroadcastSink>,
        /// The observer socket, when the test wants to read frames. A test that
        /// asserts "no client is attached" builds one without it.
        frames: Option<crate::gateway::events::Subscription>,
        rx: tokio::sync::mpsc::UnboundedReceiver<RunInput>,
        session: String,
        run_id: String,
    }

    impl Fixture {
        fn new() -> Self {
            Self::build(true)
        }

        fn unwatched() -> Self {
            Self::build(false)
        }

        fn build(watched: bool) -> Self {
            let store = Store::open(std::path::Path::new(":memory:")).unwrap();
            let project = store.add_project(&std::env::temp_dir()).unwrap();
            let task = store
                .create_session(&project.id, "claude", "sonnet", "full")
                .unwrap();
            store.begin_task_turn(&task.id, "do the thing").unwrap();
            let state = AppState {
                store: Arc::new(Mutex::new(store)),
                agents: Arc::new(Mutex::new(HashMap::new())),
                terminals: Arc::new(Mutex::new(HashMap::new())),
                shutdown: std::sync::atomic::AtomicU8::new(0),
                ui: Default::default(),
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let agent = RunningAgent::new(tx, Arc::new(AtomicBool::new(false)));
            let run_id = agent.run_id.clone();
            state
                .agents
                .lock()
                .unwrap()
                .insert(task.id.clone(), agent);
            let sink = BroadcastSink::new();
            let frames = watched.then(|| sink.subscribe(Filter::default(), None));
            Self {
                state: Arc::new(state),
                sink,
                frames,
                rx,
                session: task.id,
                run_id,
            }
        }

        /// Raises a question that was asked `age_ms` ago.
        fn ask(&self, id: &str, age_ms: i64) -> String {
            let scoped = format!("{}:{id}", self.run_id);
            let agents = self.state.agents.lock().unwrap();
            agents[&self.session].approvals.lock().unwrap().insert(
                scoped.clone(),
                PendingApproval::at(crate::storage::now() - age_ms, format!("Allow {id}?")),
            );
            scoped
        }

        fn pending(&self) -> usize {
            let agents = self.state.agents.lock().unwrap();
            let count = agents[&self.session].approvals.lock().unwrap().len();
            drop(agents);
            count
        }

        fn status(&self) -> String {
            self.state
                .store
                .lock()
                .unwrap()
                .session(&self.session)
                .unwrap()
                .status
        }

        fn denied_rows(&self) -> Vec<String> {
            let store = self.state.store.lock().unwrap();
            let mut statement = store
                .db
                .prepare("SELECT content FROM messages WHERE session_id=?1 AND role='approval_denied' ORDER BY id")
                .unwrap();
            statement
                .query_map([&self.session], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        }

        fn take_frames(&mut self) -> Vec<serde_json::Value> {
            let Some(subscription) = &mut self.frames else {
                return Vec::new();
            };
            let mut frames = Vec::new();
            while let Ok(frame) = subscription.frames.try_recv() {
                frames.push(serde_json::from_str(&frame.to_json()).unwrap());
            }
            frames
        }

        fn sweep(&self, ttl: Duration) -> usize {
            let emitter: Arc<dyn Emitter> = self.sink.clone();
            deny_expired(&self.state, &emitter, ttl).unwrap()
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn an_unanswered_question_is_denied_at_its_own_deadline() {
        let mut fixture = Fixture::new();
        let id = fixture.ask("req-1", 10_000);

        assert_eq!(fixture.sweep(Duration::from_secs(5)), 1);

        // The run hears a denial, so its turn can continue.
        match fixture.rx.try_recv() {
            Ok(RunInput::Approval { request_id, approved }) => {
                assert_eq!(request_id, "req-1");
                assert!(!approved, "auto-deny must deny");
            }
            other => match other {
                Ok(_) => panic!("the run received something other than the denial"),
                Err(_) => panic!("the run received nothing"),
            },        }
        assert_eq!(fixture.pending(), 0, "the request left the pending set");
        assert_eq!(fixture.status(), "running", "the task is not left waiting");

        // Loud in the transcript, with the request named.
        let rows = fixture.denied_rows();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains(&id), "the row names the request: {}", rows[0]);

        // Loud on the ring, with the same request id a client would have answered:
        // an agent-event whose kind is the denial, exactly like every other one.
        let frames = fixture.take_frames();
        let denial = frames
            .iter()
            .find(|frame| frame["payload"]["kind"] == "approval_denied")
            .unwrap_or_else(|| panic!("the denial is on the ring; saw {frames:?}"));
        assert_eq!(denial["event"], "agent-event");
        assert_eq!(denial["payload"]["requestId"], id);
        assert_eq!(denial["payload"]["sessionId"], fixture.session);
    }

    #[test]
    fn the_deadline_is_each_questions_own_and_not_the_turns_age() {
        let mut fixture = Fixture::new();
        let old = fixture.ask("old", 10_000);
        let fresh = fixture.ask("fresh", 0);

        assert_eq!(fixture.sweep(Duration::from_secs(5)), 1, "only the old one is due");
        assert_eq!(fixture.status(), "waiting", "one question is still open");
        let denied = fixture.take_frames();
        assert!(denied.iter().all(|frame| frame["payload"]["requestId"] != fresh));
        assert!(denied.iter().any(|frame| frame["payload"]["requestId"] == old));

        // Unrelated activity on the turn does not move the fresh one's deadline,
        // and does not save the old one either.
        let agents = fixture.state.agents.lock().unwrap();
        let agent = &agents[&fixture.session];
        agent
            .last_event
            .store(crate::storage::now(), std::sync::atomic::Ordering::SeqCst);
        drop(agents);
        assert_eq!(fixture.sweep(Duration::from_secs(5)), 0);
        assert_eq!(fixture.pending(), 1);
    }

    #[test]
    fn a_client_answer_before_the_deadline_wins_and_no_deny_follows() {
        let mut fixture = Fixture::new();
        let id = fixture.ask("req-1", 1_000);
        runtime()
            .block_on(crate::respond_approval_inner(
                &fixture.state,
                &fixture.session,
                id.clone(),
                true,
            ))
            .unwrap();

        assert_eq!(
            fixture.sweep(Duration::from_secs(5)),
            0,
            "an answered question is not denied"
        );
        // The only thing the run heard is the client's answer.
        match fixture.rx.try_recv() {
            Ok(RunInput::Approval { approved, .. }) => assert!(approved),
            Ok(_) => panic!("expected the client's answer, got something else"),
            Err(_) => panic!("expected the client's answer, got nothing"),
        }
        assert!(fixture.rx.try_recv().is_err(), "no second answer reached the run");
        assert!(fixture.denied_rows().is_empty());

        // And a late answer for that id is refused, not delivered twice.
        assert_eq!(
            runtime()
                .block_on(crate::respond_approval_inner(
                    &fixture.state,
                    &fixture.session,
                    id,
                    false
                ))
                .unwrap_err(),
            "This approval is no longer pending."
        );
    }

    #[test]
    fn a_late_answer_for_a_denied_request_is_refused() {
        let fixture = Fixture::new();
        let id = fixture.ask("req-1", 10_000);
        assert_eq!(fixture.sweep(Duration::from_secs(5)), 1);
        assert_eq!(
            runtime()
                .block_on(crate::respond_approval_inner(
                    &fixture.state,
                    &fixture.session,
                    id,
                    true
                ))
                .unwrap_err(),
            "This approval is no longer pending."
        );
    }

    #[tokio::test]
    async fn the_watchdog_denies_on_its_own_when_no_client_is_attached() {
        let recording = crate::gateway::audit::record();
        let mut fixture = Fixture::unwatched();
        assert_eq!(fixture.sink.clients(), 0, "no socket is attached, as the spec says");
        fixture.ask("req-1", 0);

        // The real task, not just one sweep: a short TTL and one tick later the
        // question is denied.
        let watchdog = watch(
            fixture.state.clone(),
            fixture.sink.clone(),
            Some(Duration::from_millis(1)),
        )
        .expect("a watchdog was started");
        let mut heard = false;
        for _ in 0..25 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if fixture.rx.try_recv().is_ok() {
                heard = true;
                break;
            }
        }
        watchdog.abort();
        assert!(heard, "an unanswered question was denied without a client");
        assert_eq!(fixture.pending(), 0);

        // The denial is a decision, so it has to survive in the journal, with the
        // question it answered. A turn that reads the transcript can see the row; an
        // operator asking who denied what reads this.
        let line = recording
            .lines()
            .into_iter()
            .find(|line| {
                line.contains("event=approval_denied") && line.contains(&format!("session={}", fixture.session))
            })
            .unwrap_or_else(|| panic!("no auto-deny audit line: {:#?}", recording.lines()));
        assert!(line.contains("request="), "{line}");
        assert!(line.contains("ttl_ms=1"), "{line}");
        assert!(
            line.contains("action=\""),
            "the question it answered is the decision, and it is quoted: {line}"
        );
    }

    #[test]
    fn ttl_zero_starts_no_watchdog() {
        let fixture = Fixture::new();
        assert!(
            watch(fixture.state.clone(), fixture.sink.clone(), None).is_none(),
            "--approval-ttl 0 must not start a task that denies anything"
        );
    }
}
