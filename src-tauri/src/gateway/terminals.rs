//! The terminals a gateway is holding open, and the rule that ends one nobody is
//! watching (PAN-110).
//!
//! A gateway terminal is a shell on the owner's machine with nobody sitting in front
//! of it. Left alone it is a process, a PTY and a file descriptor per tab that was
//! ever opened, so the gateway closes one when **no client is attached and it has
//! printed nothing for `--terminal-idle`** (default 30 minutes, `0` = never).
//!
//! What counts as attached is a socket, not a person: the `/events` socket that
//! filtered for this terminal, for as long as it is open. A tab with a client
//! watching is never idle-closed, however quiet the shell is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// What the gateway knows about one terminal it started.
struct Session {
    /// When it was opened: the clock an idle shell with no output is measured from.
    opened_at: i64,
    /// How many sockets are filtered to it right now.
    attached: usize,
}

pub(crate) struct Watch {
    idle: Duration,
    sessions: Mutex<HashMap<String, Session>>,
}

impl Watch {
    pub(crate) fn new(idle: Duration) -> Arc<Self> {
        Arc::new(Self {
            idle,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// The window this watch closes on. The sweeper reads it here so the rule has
    /// one home.
    pub(crate) fn window(&self) -> Duration {
        self.idle
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        // A panic while holding this must not take the terminal registry with it.
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A terminal this process started, from now.
    pub(crate) fn watch(&self, id: &str) {
        self.lock().insert(
            id.to_owned(),
            Session {
                opened_at: crate::storage::now(),
                attached: 0,
            },
        );
    }

    /// A terminal that is gone: closed by a client, or ended by its own shell.
    pub(crate) fn unwatch(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Marks one socket as watching this terminal until the guard is dropped.
    pub(crate) fn attach(self: &Arc<Self>, id: &str) -> Attached {
        if let Some(session) = self.lock().get_mut(id) {
            session.attached += 1;
        }
        Attached {
            watch: self.clone(),
            id: id.to_owned(),
        }
    }

    /// Terminals with no client attached, as `(id, opened_at)`. The caller supplies
    /// the last output, because that is the event stream's own fact.
    pub(crate) fn unattached(&self) -> Vec<(String, i64)> {
        self.lock()
            .iter()
            .filter(|(_, session)| session.attached == 0)
            .map(|(id, session)| (id.clone(), session.opened_at))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn attached(&self, id: &str) -> usize {
        self.lock().get(id).map(|session| session.attached).unwrap_or(0)
    }
}

/// One socket's claim on a terminal, released however the socket ends.
pub(crate) struct Attached {
    watch: Arc<Watch>,
    id: String,
}

impl Drop for Attached {
    fn drop(&mut self) {
        if let Some(session) = self.watch.lock().get_mut(&self.id) {
            session.attached = session.attached.saturating_sub(1);
        }
    }
}

/// Whether a terminal is idle at `now`, given the last moment it printed.
///
/// `last_output` is `None` for a shell that has printed nothing yet, which is
/// measured from when it was opened: an empty prompt is still an idle shell.
pub(crate) fn is_idle(idle: Duration, opened_at: i64, last_output: Option<i64>, now: i64) -> bool {
    if idle.is_zero() {
        return false;
    }
    let last = last_output.unwrap_or(opened_at).max(opened_at);
    now.saturating_sub(last) >= idle.as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: Duration = Duration::from_secs(60);

    fn watch() -> Arc<Watch> {
        Watch::new(MINUTE)
    }

    #[test]
    fn an_unattached_shell_is_idle_after_the_window_and_a_watched_one_never_is() {
        let watch = watch();
        watch.watch("t1");
        let opened = watch.lock().get("t1").map(|s| s.opened_at).unwrap();
        assert_eq!(watch.unattached().len(), 1);

        assert!(!is_idle(MINUTE, opened, None, opened + 59_000), "not yet");
        assert!(is_idle(MINUTE, opened, None, opened + 60_000), "no output, no client");

        // A client watching keeps it alive even past the window.
        let attached = watch.attach("t1");
        assert!(watch.unattached().is_empty(), "a watched terminal is not a candidate");
        assert!(watch.attached("t1") == 1);
        drop(attached);
        assert_eq!(watch.attached("t1"), 0);
        assert_eq!(watch.unattached().len(), 1);
        assert!(is_idle(MINUTE, opened, None, opened + 3_600_000));
    }

    #[test]
    fn output_alone_keeps_it_alive_and_zero_disables_the_rule() {
        let watch = watch();
        watch.watch("t1");
        let opened = watch.lock().get("t1").map(|s| s.opened_at).unwrap();
        // An hour old, but printed a moment ago.
        assert!(!is_idle(MINUTE, opened, Some(opened + 3_600_000), opened + 3_600_000 + 1_000));
        assert!(is_idle(MINUTE, opened, Some(opened + 60_000), opened + 3_600_000));
        assert!(!is_idle(Duration::ZERO, opened, None, opened + 86_400_000), "0 means never");
    }

    #[test]
    fn a_forgotten_terminal_stops_being_a_candidate() {
        let watch = watch();
        watch.watch("t1");
        watch.unwatch("t1");
        assert!(watch.unattached().is_empty());
        // Dropping a claim for a terminal that is gone is not a panic.
        let stale = watch.attach("t1");
        drop(stale);
        assert_eq!(watch.attached("t1"), 0);
    }
}
