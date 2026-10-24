//! Boot recovery, proved by effect rather than by restating the rule (PAN-104).
//!
//! A gateway cannot serve a row that claims to be running when no process is
//! behind it. The rule that settles such a row is not new: `Store::open` does it
//! for every process that opens the database, which is why the desktop and the
//! gateway agree by construction. These tests show that rule firing through the
//! gateway's own start path, and through a real `SIGKILL` of a process that was
//! mid-write.

use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use super::*;
use crate::storage::Store;

/// A store with one task that claims to be mid-turn, plus a running subagent,
/// exactly what a process leaves behind when it dies between two writes.
fn seed_dying_process(dir: &Path) -> String {
    let store = Store::open(&dir.join("kingcode.sqlite3")).unwrap();
    let project = store.add_project(dir).unwrap();
    let task = store
        .create_session(&project.id, "claude", "sonnet", "full")
        .unwrap();
    store.begin_task_turn(&task.id, "Build a game").unwrap();
    store
        .db
        .execute(
            "INSERT INTO subagents(session_id,id,content,updated_at) VALUES (?1,'worker-a',?2,0)",
            rusqlite::params![
                task.id,
                serde_json::json!({"id":"worker-a","status":"running","tools":[{"name":"t","status":"running"}],"updatedAt":0})
                    .to_string()
            ],
        )
        .unwrap();
    task.id
}

/// Reads the row the way a person would, without opening the store — so opening
/// the database is not what settles it.
fn row(dir: &Path, session_id: &str) -> (String, String, Option<i64>) {
    let db = rusqlite::Connection::open(dir.join("kingcode.sqlite3")).unwrap();
    db.query_row(
        "SELECT status, task_stage, completed_at FROM sessions WHERE id=?1",
        [session_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

fn subagent_status(dir: &Path, session_id: &str) -> (String, String) {
    let db = rusqlite::Connection::open(dir.join("kingcode.sqlite3")).unwrap();
    let content: String = db
        .query_row(
            "SELECT content FROM subagents WHERE session_id=?1",
            [session_id],
            |r| r.get(0),
        )
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&content).unwrap();
    (
        value["status"].as_str().unwrap().to_string(),
        value["tools"][0]["status"].as_str().unwrap().to_string(),
    )
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn gateway_config(dir: &Path) -> GatewayConfig {
    GatewayConfig {
        data_dir: dir.to_path_buf(),
        bind: "127.0.0.1:0".parse().unwrap(),
        token: "test-token".into(),
        static_dir: None,
        allowed_origins: config::default_origins(),
        force_owner: false,
    }
}

#[test]
fn a_gateway_start_settles_a_row_that_claims_to_run_before_it_serves() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = seed_dying_process(dir.path());

    // Open the gateway exactly as the binary does: this is the start path, not a
    // store call in a test.
    let runtime = runtime();
    let handle = runtime
        .block_on(spawn(gateway_config(dir.path())))
        .expect("the gateway starts on a directory whose previous process died");

    // Read the file directly: no `Store::open` here, so nothing but the gateway's
    // own start can have settled this row.
    let (status, stage, completed_at) = row(dir.path(), &session_id);
    assert_eq!(status, "interrupted", "a dead process must not read as running");
    assert_eq!(stage, "in_progress");
    assert_eq!(completed_at, None);
    let (subagent, tool) = subagent_status(dir.path(), &session_id);
    assert_eq!(subagent, "stopped", "the subagent stops with its turn");
    assert_eq!(tool, "failed", "an unfinished tool is settled too");
    assert_ne!(status, "running");

    runtime.block_on(handle.stop());
}

#[test]
fn a_second_gateway_start_finds_nothing_to_settle() {
    // The recovery is idempotent: once settled, a row stays settled.
    let dir = tempfile::tempdir().unwrap();
    let session_id = seed_dying_process(dir.path());
    let runtime = runtime();
    let first = runtime.block_on(spawn(gateway_config(dir.path()))).unwrap();
    runtime.block_on(first.stop());
    let second = runtime.block_on(spawn(gateway_config(dir.path()))).unwrap();
    let (status, _, _) = row(dir.path(), &session_id);
    assert_eq!(status, "interrupted");
    runtime.block_on(second.stop());
}

/// The child half of the SIGKILL proof. It does nothing unless the parent asked
/// for it, so a normal test run just passes.
#[test]
fn sigkill_child_writer() {
    let Ok(dir) = std::env::var("PAN104_STUB_DATA") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    // The lock the binary takes: the kernel must release it when this process is
    // killed, or the parent could never reopen the directory.
    let _lock = match crate::data_lock::claim(&dir, false) {
        crate::data_lock::Claim::Held(lock) => lock,
        crate::data_lock::Claim::Busy(message) | crate::data_lock::Claim::Unavailable(message) => {
            println!("PAN104_LOCK_REFUSED {message}");
            return;
        }
    };
    seed_dying_process(&dir);
    println!("PAN104_READY");
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    // Wait to be killed, as a real turn would be mid-work.
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
fn sigkill_leaves_no_task_claiming_to_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["sigkill_child_writer", "--nocapture"])
        .env("PAN104_STUB_DATA", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the test binary can run itself");

    // Wait for the child to say it has written a running row, without hanging the
    // suite if it never does.
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.trim() == "PAN104_READY" {
                let _ = tx.send(());
                return;
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(60))
        .expect("the child wrote its row before the deadline");

    // The row is really there and really claims to run.
    let db = rusqlite::Connection::open(dir.path().join("kingcode.sqlite3")).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE status='running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "the child was mid-turn when it was killed");
    drop(db);

    // SIGKILL: nothing in the child gets to run.
    child.kill().expect("the child can be killed");
    let status = child.wait().unwrap();
    assert!(!status.success(), "the child died by signal, not by exit");

    // The kernel released its lock, so this process is now the writer.
    let claim = crate::data_lock::claim(dir.path(), false);
    assert!(
        matches!(claim, crate::data_lock::Claim::Held(_)),
        "a SIGKILLed holder must not keep the directory locked"
    );

    // The recovery path: the next process to open the store settles the row.
    let store = Store::open(&dir.path().join("kingcode.sqlite3")).unwrap();
    let task = store.sessions().unwrap();
    assert_eq!(task.len(), 1, "the killed turn's task is still listed");
    assert_eq!(task[0].status, "interrupted");
    assert_eq!(task[0].task_stage, "in_progress");
    assert_eq!(task[0].completed_at, None);
}
