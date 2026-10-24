//! One line per thing the gateway did, for the operator to read back later.
//!
//! With the gateway running as the owner (spec §2.2), a log file it writes is a file
//! the agent it runs can rewrite, and an audit the audited process can edit is not an
//! audit. So lines go to **stdout** and the unit sets `StandardOutput=journal`:
//! systemd copies each line into the journal, where the owner can read it and the
//! gateway's own processes cannot change it.
//!
//! What a line never carries is message text or file content — writing the person's
//! work into the journal is the failure this avoids. A line names what happened to
//! which ids and how long it took. The one piece of prose is the approval question an
//! auto-deny answered, because that question *is* the decision being recorded.
//!
//! Shape, one line per event, `key=value` so a journal query can filter on it:
//!
//! ```text
//! audit event=command client=127.0.0.1:54321 command=send_message session=9f2c outcome=ok duration_ms=41
//! audit event=approval client=127.0.0.1:54321 session=9f2c request=9f2c:3 decision=approved duration_ms=3
//! audit event=approval_denied session=9f2c request=9f2c:3 ttl_ms=3600000 action="Run `git push`?"
//! audit event=upload client=127.0.0.1:54321 upload=1b7e kind=document bytes=20481 sha256=… duration_ms=12
//! audit event=auth client=203.0.113.9 path=/rpc failures=11 window_s=60 status=429
//! ```

use std::{fmt::Display, sync::RwLock};

/// The longest value one line carries, in characters. The approval question is the
/// only free text here, and a single prompt must not become a kilobyte of journal per
/// auto-deny.
const VALUE_MAX: usize = 240;

/// Lines written while a recorder is installed. A running gateway never installs one:
/// its lines go to stdout and nowhere else. `RwLock<Option<…>>` rather than
/// `OnceLock`, following the event sink, so a test process can read what its own
/// gateways wrote without a second global per test.
static RECORDED: RwLock<Option<Vec<String>>> = RwLock::new(None);

/// A handle on the lines this process has written, for a test.
///
/// Tests in one binary run in parallel threads against one process, so this records
/// for everybody: assert on your own unique ids rather than on a line count.
pub struct Recording {
    _private: (),
}

/// Starts recording audit lines in this process. Idempotent — a second caller keeps
/// the lines already captured instead of resetting another test's.
pub fn record() -> Recording {
    if let Ok(mut recorded) = RECORDED.write() {
        if recorded.is_none() {
            *recorded = Some(Vec::new());
        }
    }
    Recording { _private: () }
}

impl Recording {
    /// Every line recorded so far, oldest first.
    pub fn lines(&self) -> Vec<String> {
        RECORDED
            .read()
            .ok()
            .and_then(|recorded| recorded.clone())
            .unwrap_or_default()
    }

    /// The lines that name `id` — the way a test in a shared process isolates its own.
    pub fn for_id(&self, id: &str) -> Vec<String> {
        let needle = format!("={id}");
        self.lines()
            .into_iter()
            .filter(|line| line.contains(&needle))
            .collect()
    }
}

/// One line, built field by field so a caller cannot forget the `event` name.
pub struct Line {
    text: String,
}

impl Line {
    /// Starts a line for `event`: `command`, `approval`, `approval_denied`, `upload`,
    /// `auth`, `terminal`.
    pub fn new(event: &str) -> Self {
        Self {
            text: format!("audit event={}", atom(event)),
        }
    }

    /// Adds one field. Numbers go in bare; a value with a space, a quote or a control
    /// character is quoted and escaped so the line stays one line. Every value is
    /// bounded: a prompt that happens to have no spaces must not become a kilobyte.
    pub fn field(mut self, key: &str, value: impl Display) -> Self {
        let full = value.to_string();
        let cut = full.chars().count() > VALUE_MAX;
        let value: String = if cut {
            full.chars().take(VALUE_MAX).collect()
        } else {
            full
        };
        self.text.push(' ');
        self.text.push_str(&atom(key));
        self.text.push('=');
        let quoted = cut
            || value
                .chars()
                .any(|c| c.is_whitespace() || c == '"' || c == '\\' || c.is_control());
        if quoted {
            self.text.push('"');
            for character in value.chars() {
                match character {
                    '"' => self.text.push_str("\\\""),
                    '\\' => self.text.push_str("\\\\"),
                    '\n' => self.text.push_str("\\n"),
                    '\r' => self.text.push_str("\\r"),
                    '\t' => self.text.push_str("\\t"),
                    c if c.is_control() => self.text.push(' '),
                    c => self.text.push(c),
                }
            }
            if cut {
                self.text.push_str("...");
            }
            self.text.push('"');
        } else {
            self.text.push_str(&atom(&value));
        }
        self
    }

    /// Writes the line: stdout (journald) and, in a test process, the recorder.
    pub fn emit(self) {
        println!("{}", self.text);
        if let Ok(mut recorded) = RECORDED.write() {
            if let Some(lines) = recorded.as_mut() {
                // A long-lived test process must not grow this without a bound.
                if lines.len() > 4_000 {
                    lines.remove(0);
                }
                lines.push(self.text);
            }
        }
    }
}

/// A `key` or a value that needs no quoting: runs of characters a journal query can
/// match. Anything else is replaced rather than escaped, because these are names this
/// code chose (`event`, field keys), never operator text.
fn atom(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':' | '/') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_key_value_and_quotes_what_would_break_it() {
        let recording = record();
        Line::new("approval_denied")
            .field("session", "s1")
            .field("ttl_ms", 3_600_000)
            .field("action", "Run `git push`?\nsecond line")
            .emit();
        let line = recording
            .lines()
            .into_iter()
            .find(|line| line.contains("session=s1"))
            .expect("the line was recorded");
        assert!(line.starts_with("audit event=approval_denied "), "{line}");
        assert!(line.contains("ttl_ms=3600000"), "a number goes in bare: {line}");
        assert!(line.contains(r#"action="Run `git push`?\nsecond line""#), "{line}");
        assert_eq!(line.lines().count(), 1, "one line per event, never two: {line}");
    }

    #[test]
    fn a_very_long_value_is_cut_rather_than_logged_whole() {
        let recording = record();
        let long = "x".repeat(500);
        Line::new("test").field("action", &long).emit();
        let line = recording
            .lines()
            .into_iter()
            .find(|line| line.starts_with("audit event=test "))
            .expect("the line was recorded");
        assert!(line.len() < VALUE_MAX + 60, "bounded: {} chars", line.len());
        assert!(line.ends_with("...\""), "and marked as cut: {line}");
    }
}
