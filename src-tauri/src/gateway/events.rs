//! The gateway's sink: every emitted event becomes one numbered frame, is kept in
//! a per-session ring so a client can resume, and is pushed to each socket that
//! asked for it.
//!
//! Nothing here awaits. The emitter takes a short lock that appends the frame to a
//! ring and tries to enqueue it for each subscriber; a socket that is behind is not
//! waited for. Its gap is recorded and announced as a `lagged` frame, which is the
//! difference between a slow phone and a stalled turn.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::events::Emitter;

/// How far back a client may resume: 4 000 frames, or anything younger than ten
/// minutes — whichever keeps more.
const RING_FRAMES: usize = 4_000;
const RING_AGE_MS: i64 = 10 * 60 * 1000;

/// A terminal's ring is bounded by bytes as well as frames. A shell's output is
/// high-volume and a frame count says nothing about how much scrollback that is, so
/// a terminal keeps 256 KiB of what it printed and no more (PAN-110).
const TERMINAL_RING_BYTES: usize = 256 * 1024;

/// How far a socket may fall behind before it is told it lagged. Deep enough for a
/// burst of tool output, shallow enough that the hole it has to ask about is short.
const SUBSCRIBER_QUEUE: usize = 512;

/// The ring that holds frames with no routing key: a sidebar or project change
/// belongs to no session, and every socket needs it.
const GLOBAL: &str = "*";

/// What a socket asked to see.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub session: Option<String>,
    pub terminal: Option<String>,
    /// An explicit client target, set by `emit_to`. A subscriber that names one
    /// sees only frames addressed to it (PAN-106: an app action belongs to a
    /// client, never to a window label).
    pub target: Option<String>,
}

impl Filter {
    /// A frame with no routing key is a process-wide signal, so it reaches every
    /// socket. Everything else must match every filter that was given.
    fn matches(&self, frame: &Frame) -> bool {
        if frame.session.is_none() && frame.terminal.is_none() && frame.target.is_none() {
            return true;
        }
        if let Some(session) = &self.session {
            if frame.session.as_ref() != Some(session) {
                return false;
            }
        }
        if let Some(terminal) = &self.terminal {
            if frame.terminal.as_ref() != Some(terminal) {
                return false;
            }
        }
        if let Some(target) = &self.target {
            if frame.target.as_ref() != Some(target) {
                return false;
            }
        }
        true
    }

    /// Whether a ring can hold a frame this filter would accept.
    fn reads(&self, ring: &str) -> bool {
        if ring == GLOBAL {
            return true;
        }
        if self.session.is_none() && self.terminal.is_none() {
            return true;
        }
        if let Some(session) = &self.session {
            if ring == format!("s:{session}") {
                return true;
            }
        }
        if let Some(terminal) = &self.terminal {
            if ring == format!("t:{terminal}") {
                return true;
            }
        }
        false
    }
}

/// One event on the wire, numbered once.
#[derive(Clone, Debug)]
pub struct Frame {
    pub seq: u64,
    pub event: String,
    pub payload: Value,
    /// Routing keys, read out of the payload rather than added to it.
    session: Option<String>,
    terminal: Option<String>,
    /// An explicit client target, from `emit_to`. On the frame, never inside the
    /// payload.
    target: Option<String>,
    /// When it was published, for the ring's age bound. Never serialised.
    at: i64,
    /// What this frame costs in a byte-bounded ring: its payload as it goes on the
    /// wire. Counted once, when the frame is made.
    bytes: usize,
}

impl Frame {
    fn from_event(seq: u64, event: &str, payload: Value) -> Self {
        let key = |name: &str| {
            payload
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let bytes = serde_json::to_string(&payload)
            .map(|encoded| encoded.len())
            .unwrap_or(0);
        Self {
            seq,
            event: event.to_owned(),
            session: key("sessionId"),
            terminal: key("terminalId"),
            payload,
            target: None,
            at: crate::storage::now(),
            bytes,
        }
    }

    /// The same frame, addressed to one client. A window label is not a client
    /// target; `emit_to` on the desktop never reaches this sink.
    fn addressed(mut self, target: &str) -> Self {
        self.target = Some(target.to_owned());
        self
    }

    /// The bytes a client reads: the number and the name on the frame, and the
    /// payload exactly as the emitting code serialised it. Nothing is re-nested.
    /// `target` appears only on a frame that has one, so an ordinary event's shape
    /// is unchanged from task 4.
    pub fn to_json(&self) -> String {
        let mut frame = serde_json::json!({ "seq": self.seq, "event": self.event, "payload": self.payload });
        if let Some(target) = &self.target {
            frame["target"] = serde_json::Value::String(target.clone());
        }
        frame.to_string()
    }

    fn ring(&self) -> String {
        if let Some(session) = &self.session {
            format!("s:{session}")
        } else if let Some(terminal) = &self.terminal {
            format!("t:{terminal}")
        } else {
            GLOBAL.to_owned()
        }
    }
}

/// The frames one session (or terminal) may resume from.
#[derive(Default)]
struct Ring {
    frames: VecDeque<Frame>,
    /// The newest number this ring threw away, so a resume can name what it lost.
    evicted_through: u64,
    /// What the frames in the ring weigh, for a byte-bounded terminal ring.
    bytes: usize,
}

impl Ring {
    fn push(&mut self, frame: Frame, now: i64, max_bytes: Option<usize>) {
        self.bytes += frame.bytes;
        self.frames.push_back(frame);
        // Both bounds, and the longer one wins: a frame stays if it is among the
        // newest RING_FRAMES or younger than RING_AGE_MS.
        while self.frames.len() > RING_FRAMES
            && self
                .frames
                .front()
                .is_some_and(|oldest| now.saturating_sub(oldest.at) > RING_AGE_MS)
        {
            self.evict_one();
        }
        // A terminal additionally holds at most its byte budget, but never drops
        // its newest frame: the thing the client came back for is the tail.
        if let Some(max) = max_bytes {
            while self.bytes > max && self.frames.len() > 1 {
                self.evict_one();
            }
        }
    }

    fn evict_one(&mut self) {
        if let Some(dropped) = self.frames.pop_front() {
            self.bytes = self.bytes.saturating_sub(dropped.bytes);
            self.evicted_through = self.evicted_through.max(dropped.seq);
        }
    }

    /// When this ring last carried a frame: a terminal's last output.
    fn newest_at(&self) -> Option<i64> {
        self.frames.back().map(|frame| frame.at)
    }
}

struct Subscriber {
    filter: Filter,
    tx: mpsc::Sender<Frame>,
    /// Numbers this socket missed while its queue was full, merged into one range.
    hole: Arc<Mutex<Option<(u64, u64)>>>,
}

#[derive(Default)]
struct Inner {
    rings: HashMap<String, Ring>,
    subscribers: Vec<Subscriber>,
}

/// Fans one event out to every socket that asked for it.
pub struct BroadcastSink {
    /// The number this process has handed out. Monotonic, and never reused, so a
    /// client can order what it receives and ask for what it missed.
    next: AtomicU64,
    inner: Mutex<Inner>,
}

/// What one socket gets when it connects.
pub struct Subscription {
    /// Frames between `since` and now, oldest first: what the client missed while
    /// it was away, when the ring still had it.
    pub replay: Vec<Frame>,
    /// Numbers the ring could not serve, as `(from, to)`: the client is told to
    /// refetch rather than handed a stream that quietly skips.
    pub hole: Option<(u64, u64)>,
    /// The same situation for a terminal, where there is nothing to refetch: the
    /// scrollback the client missed is gone, so it is told to clear and redraw what
    /// follows instead of resuming into a hole (PAN-110).
    pub reset: Option<(u64, u64)>,
    pub frames: mpsc::Receiver<Frame>,
    /// Set while this socket is behind; the socket sends a `lagged` frame for it
    /// before it sends anything newer.
    pub dropped: Arc<Mutex<Option<(u64, u64)>>>,
}

impl BroadcastSink {
    pub fn new() -> Arc<BroadcastSink> {
        Arc::new(BroadcastSink {
            next: AtomicU64::new(0),
            inner: Mutex::new(Inner::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic in an emitter must not turn into a panic on every later event,
        // which would take the event stream down with the turn.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One frame to every socket that wants it. Never awaits and never waits for a
    /// socket: the only wait is this process's own short critical section.
    pub fn publish(&self, name: &str, payload: Value) {
        self.deliver(name, payload, None);
    }

    /// Publishes a frame addressed to one client. The desktop's `emit_to` targets a
    /// window and never comes through here; this is the remote equivalent.
    pub fn publish_to(&self, target: &str, name: &str, payload: Value) {
        self.deliver(name, payload, Some(target));
    }

    fn deliver(&self, name: &str, payload: Value, target: Option<&str>) {
        let seq = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let mut frame = Frame::from_event(seq, name, payload);
        if let Some(target) = target {
            frame = frame.addressed(target);
        }
        let now = crate::storage::now();
        let mut inner = self.lock();
        let key = frame.ring();
        let budget = frame.terminal.is_some().then_some(TERMINAL_RING_BYTES);
        inner.rings.entry(key).or_default().push(frame.clone(), now, budget);
        inner.deliver(frame);
    }

    /// How many sockets are attached right now. Dead ones are dropped here rather
    /// than waiting for the next publish, so "is a client attached?" answers about
    /// the present.
    pub fn clients(&self) -> usize {
        let mut inner = self.lock();
        inner.subscribers.retain(|subscriber| !subscriber.tx.is_closed());
        inner.subscribers.len()
    }

    /// When this terminal last printed, for the idle rule. `None` while it has
    /// printed nothing: a shell sitting at an empty prompt is still idle.
    pub fn last_terminal_output(&self, terminal_id: &str) -> Option<i64> {
        self.lock()
            .rings
            .get(&format!("t:{terminal_id}"))
            .and_then(Ring::newest_at)
    }

    /// Registers a socket and takes its replay in one step, so a frame published
    /// during the handover is either in the replay or on the queue, never both and
    /// never neither.
    pub fn subscribe(&self, filter: Filter, since: Option<u64>) -> Subscription {
        let (tx, frames) = mpsc::channel(SUBSCRIBER_QUEUE);
        let dropped = Arc::new(Mutex::new(None));
        let mut inner = self.lock();

        let mut replay: Vec<Frame> = inner
            .rings
            .iter()
            .filter(|(ring, _)| filter.reads(ring))
            .flat_map(|(_, ring)| ring.frames.iter().filter(|f| filter.matches(f)).cloned())
            .collect();
        replay.sort_by_key(|frame| frame.seq);

        let mut hole = None;
        let mut reset = None;
        if let Some(since) = since {
            let evicted = inner
                .rings
                .iter()
                .filter(|(ring, _)| filter.reads(ring))
                .map(|(_, ring)| ring.evicted_through)
                .max()
                .unwrap_or(0);
            if evicted > since && filter.terminal.is_some() {
                // A terminal cannot refetch: the shell's own scrollback ended at
                // the ring's start. The client clears and redraws from what the
                // replay carries, and is told the numbers it will never see.
                reset = Some((since, evicted));
            } else {
                replay.retain(|frame| frame.seq > since);
                if evicted > since {
                    hole = Some((since + 1, evicted));
                    replay.retain(|frame| frame.seq > evicted);
                }
            }
        }

        inner.subscribers.push(Subscriber {
            filter,
            tx,
            hole: dropped.clone(),
        });
        Subscription {
            replay,
            hole,
            reset,
            frames,
            dropped,
        }
    }
}

impl Inner {
    fn deliver(&mut self, frame: Frame) {
        let seq = frame.seq;
        self.subscribers.retain(|subscriber| {
            if !subscriber.filter.matches(&frame) {
                return true;
            }
            match subscriber.tx.try_send(frame.clone()) {
                Ok(()) => true,
                // Full is the slow-client case: record the number and move on. The
                // turn must not wait for a phone on a train.
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if let Ok(mut hole) = subscriber.hole.lock() {
                        *hole = Some(match *hole {
                            Some((from, _)) => (from, seq),
                            None => (seq, seq),
                        });
                    }
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }
}

impl Emitter for BroadcastSink {
    fn emit(&self, name: &str, payload: Value) -> Result<(), String> {
        self.publish(name, payload);
        // Broadcasting has no per-client failure: a socket that is gone is gone, and
        // the frames it missed are announced to it rather than reported here.
        Ok(())
    }

    /// An event addressed to one client. On the desktop a target is a window label
    /// and goes through the app handle instead; here it is a client target carried
    /// on the frame, so a subscriber that asked for it receives it.
    fn emit_to(&self, target: &str, name: &str, payload: Value) -> Result<(), String> {
        self.publish_to(target, name, payload);
        Ok(())
    }
}

/// The hole, as a frame a client reads: it names the range it missed and then the
/// stream continues from `toSeq + 1`.
pub fn lagged_frame(from: u64, to: u64) -> String {
    serde_json::json!({ "event": "lagged", "payload": { "fromSeq": from, "toSeq": to } })
        .to_string()
}

/// A terminal's scrollback loss, as a frame a client reads: clear what you drew and
/// start again from the frames that follow (PAN-110).
pub fn terminal_reset_frame(from: u64, to: u64) -> String {
    serde_json::json!({ "event": "terminal-reset", "payload": { "fromSeq": from, "toSeq": to } })
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(session: &str) -> Value {
        json!({ "sessionId": session, "kind": "text", "text": "hi" })
    }

    fn drain(subscription: &mut Subscription) -> Vec<Frame> {
        let mut received = Vec::new();
        while let Ok(frame) = subscription.frames.try_recv() {
            received.push(frame);
        }
        received
    }

    #[test]
    fn every_frame_is_numbered_once_and_the_payload_is_untouched() {
        let sink = BroadcastSink::new();
        let mut subscription = sink.subscribe(Filter::default(), None);
        let payload = event("s1");
        sink.publish("agent-event", payload.clone());
        sink.publish("sessions-changed", Value::Null);

        let received = drain(&mut subscription);
        assert_eq!(received.iter().map(|f| f.seq).collect::<Vec<_>>(), [1, 2]);
        let wire: Value = serde_json::from_str(&received[0].to_json()).unwrap();
        assert_eq!(wire["seq"], 1);
        assert_eq!(wire["event"], "agent-event");
        assert_eq!(wire["payload"], payload, "the payload was rewritten in transit");
        let keys: Vec<&String> = wire.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["event", "payload", "seq"], "the frame adds no fields to the payload");
    }

    #[test]
    fn a_subscriber_sees_only_its_own_session_and_the_global_frames() {
        let sink = BroadcastSink::new();
        let mut mine = sink.subscribe(
            Filter {
                session: Some("a".into()),
                terminal: None,
                target: None,
            },
            None,
        );
        sink.publish("agent-event", event("a"));
        sink.publish("agent-event", event("b"));
        sink.publish("sessions-changed", Value::Null);
        let received = drain(&mut mine);
        let sessions: Vec<&str> = received
            .iter()
            .map(|f| f.payload["sessionId"].as_str().unwrap_or("(global)"))
            .collect();
        assert_eq!(sessions, ["a", "(global)"]);
    }

    #[test]
    fn a_terminal_filter_isolates_one_shell() {
        let sink = BroadcastSink::new();
        let mut one = sink.subscribe(
            Filter {
                session: None,
                terminal: Some("t1".into()),
                target: None,
            },
            None,
        );
        sink.publish("terminal-event", json!({ "terminalId": "t1", "data": "a" }));
        sink.publish("terminal-event", json!({ "terminalId": "t2", "data": "b" }));
        sink.publish("agent-event", event("a"));
        let received = drain(&mut one);
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].payload["terminalId"], "t1");
    }

    #[test]
    fn a_socket_that_stops_reading_is_told_what_it_missed_and_the_ring_keeps_it() {
        let sink = BroadcastSink::new();
        // Never drained: the queue fills, and every later frame is a hole.
        let subscription = sink.subscribe(Filter::default(), None);
        let total = SUBSCRIBER_QUEUE + 40;
        for _ in 0..total {
            sink.publish("agent-event", event("s1"));
        }
        let hole = *subscription.dropped.lock().unwrap();
        assert_eq!(
            hole,
            Some(((SUBSCRIBER_QUEUE + 1) as u64, total as u64)),
            "the hole is exactly the frames the queue could not take"
        );
        // The ring kept every one of them, so the client can ask for the hole.
        let mut replay = sink.subscribe(
            Filter::default(),
            Some(SUBSCRIBER_QUEUE as u64),
        );
        let recovered = std::mem::take(&mut replay.replay);
        assert_eq!(
            recovered.iter().map(|f| f.seq).collect::<Vec<_>>(),
            ((SUBSCRIBER_QUEUE + 1) as u64..=total as u64).collect::<Vec<_>>(),
            "the missed range is recoverable in order, with no gap"
        );
        assert_eq!(replay.hole, None, "the ring still had everything");
    }

    #[test]
    fn a_resume_from_before_the_ring_gets_a_lagged_range_not_a_quiet_jump() {
        let sink = BroadcastSink::new();
        for _ in 0..RING_FRAMES {
            sink.publish("agent-event", event("s1"));
        }
        // Age every frame so the next pushes fall outside both bounds and evict,
        // without making the test wait ten minutes.
        {
            let mut inner = sink.lock();
            for ring in inner.rings.values_mut() {
                for frame in ring.frames.iter_mut() {
                    frame.at = 0;
                }
            }
        }
        for _ in 0..5 {
            sink.publish("agent-event", event("s1"));
        }

        let subscription = sink.subscribe(Filter::default(), Some(2));
        assert_eq!(
            subscription.hole,
            Some((3, 5)),
            "the range the ring dropped, named rather than skipped"
        );
        assert!(
            subscription.replay.iter().all(|frame| frame.seq > 5),
            "the replay continues after the announced range"
        );
        assert_eq!(subscription.replay[0].seq, 6);
    }

    #[test]
    fn a_targeted_event_carries_its_target_and_reaches_only_that_client() {
        let sink = BroadcastSink::new();
        let mut mine = sink.subscribe(
            Filter {
                target: Some("client-a".into()),
                ..Default::default()
            },
            None,
        );
        sink.publish_to("client-a", "agent-ui-action", json!({"sessionId": "s", "requestId": "r1"}));
        sink.publish_to("client-b", "agent-ui-action", json!({"sessionId": "s", "requestId": "r2"}));
        // An ordinary event has no target, and is not this subscriber's business.
        sink.publish("agent-event", event("s"));

        let received = drain(&mut mine);
        assert_eq!(received.len(), 1, "only the frame addressed to this client");
        let wire: Value = serde_json::from_str(&received[0].to_json()).unwrap();
        assert_eq!(wire["target"], "client-a");
        assert_eq!(wire["payload"]["requestId"], "r1");

        // An untargeted frame keeps the task-4 shape: no `target` key at all.
        let sink = BroadcastSink::new();
        let mut plain = sink.subscribe(Filter::default(), None);
        sink.publish("agent-event", event("s"));
        let received = drain(&mut plain);
        let wire: Value = serde_json::from_str(&received[0].to_json()).unwrap();
        assert!(wire.get("target").is_none());
    }

    #[test]
    fn a_terminal_resume_past_its_ring_asks_for_a_reset_and_a_session_does_not() {
        let sink = BroadcastSink::new();
        // 300 KiB of terminal output, in ordinary chunks: more than the 256 KiB a
        // terminal keeps, and far fewer than RING_FRAMES, so the byte bound is what
        // evicts here.
        let chunk = "x".repeat(4 * 1024);
        for _ in 0..80 {
            sink.publish(
                "terminal-event",
                json!({ "terminalId": "t1", "data": chunk, "exit": false }),
            );
        }
        let subscription = sink.subscribe(
            Filter {
                session: None,
                terminal: Some("t1".into()),
                target: None,
            },
            Some(1),
        );
        assert!(
            subscription.reset.is_some(),
            "a terminal cannot refetch scrollback: it is told to reset"
        );
        assert_eq!(subscription.hole, None, "and not handed a refetch instruction");
        let (_, lost_through) = subscription.reset.unwrap();
        assert!(lost_through > 1, "the reset names what was lost");
        assert!(
            subscription.replay.iter().all(|frame| frame.seq > lost_through),
            "the redraw starts after the loss"
        );
        assert!(
            subscription.replay.len() > 1,
            "there is something to redraw from: {}",
            subscription.replay.len()
        );

        // A session filter is never told to reset: a client can refetch a turn, and
        // `a_resume_from_before_the_ring_gets_a_lagged_range_not_a_quiet_jump` proves
        // that range is named rather than skipped.
        for _ in 0..10 {
            sink.publish("agent-event", json!({ "sessionId": "s1", "kind": "text" }));
        }
        let session = sink.subscribe(
            Filter {
                session: Some("s1".into()),
                terminal: None,
                target: None,
            },
            Some(1),
        );
        assert_eq!(session.reset, None, "only a terminal is told to clear and redraw");
    }

    #[test]
    fn a_terminal_ring_keeps_its_last_bytes_and_never_drops_what_it_just_got() {
        let sink = BroadcastSink::new();
        let chunk = "y".repeat(8 * 1024);
        for index in 0..40 {
            sink.publish(
                "terminal-event",
                json!({ "terminalId": "t1", "data": chunk, "exit": false, "n": index }),
            );
        }
        let mut all = sink.subscribe(
            Filter {
                session: None,
                terminal: Some("t1".into()),
                target: None,
            },
            Some(0),
        );
        let kept = std::mem::take(&mut all.replay);
        let bytes: usize = kept
            .iter()
            .map(|frame| frame.bytes)
            .sum();
        assert!(
            bytes <= TERMINAL_RING_BYTES + 8 * 1024,
            "the ring holds its budget plus the frame that crossed it: {bytes}"
        );
        assert_eq!(
            kept.last().unwrap().payload["n"], 39,
            "the newest frame is never evicted"
        );
        assert!(
            kept.first().unwrap().seq > 1,
            "older output went to make room: {:?}",
            kept.first().map(|frame| frame.seq)
        );
    }

    #[test]
    fn a_slow_socket_cannot_stop_the_emitter() {
        let sink = BroadcastSink::new();
        let _stalled = sink.subscribe(Filter::default(), None);
        let started = std::time::Instant::now();
        // Synchronous by construction: publishing cannot await a socket. Ten thousand
        // frames into a stalled subscriber must still return, and quickly.
        for _ in 0..10_000 {
            sink.publish("agent-event", event("s1"));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the emitter waited for a socket: {:?}",
            started.elapsed()
        );
    }
}
