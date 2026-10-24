//! The turn commands take the plain state they need, so a process with no window
//! calls the same code the desktop command does (PAN-103).
//!
//! The three commands that need an `AppHandle` for a team (`send_message`,
//! `start_task`, `stop_session`) keep that routing in the desktop command, because
//! an architect team is N runs a window owns. What the gateway reaches is the
//! non-team inner, and that is what these tests hold to account: `respond_approval`
//! and the queue wrappers prove the wrapper→inner hop, since they need no handle at
//! all and a mock runtime cannot produce a real one.

use super::*;

/// A state whose store lives only in this process, with no app around it.
fn state() -> AppState {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    AppState {
        store: Arc::new(Mutex::new(store)),
        agents: Arc::new(Mutex::new(HashMap::new())),
        terminals: Arc::new(Mutex::new(HashMap::new())),
        shutdown: AtomicU8::new(0),
        ui: Default::default(),
    }
}

fn session(state: &AppState) -> Session {
    let store = state.store.lock().unwrap();
    let project = store.add_project(&std::env::temp_dir()).unwrap();
    store
        .create_session(&project.id, "claude", "sonnet", "full")
        .unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn turn_command_inners_answer_on_state_alone() {
    let state = state();
    let session = session(&state);
    let runtime = runtime();

    runtime.block_on(async {
        // A stop with no running turn is a no-op, exactly as the command behaves.
        stop_session_inner(&state, &session.id).await.unwrap();
        assert_eq!(
            respond_approval_inner(&state, &session.id, "r1".into(), true)
                .await
                .unwrap_err(),
            "This agent is no longer running."
        );
        assert_eq!(
            respond_user_input_inner(&state, &session.id, "r1".into(), serde_json::json!({}))
                .await
                .unwrap_err(),
            "This agent is no longer running."
        );
        // The size limit is checked before the registry, so it answers without one.
        let huge = serde_json::json!("x".repeat(32 * 1024 + 1));
        assert_eq!(
            respond_user_input_inner(&state, &session.id, "r1".into(), huge)
                .await
                .unwrap_err(),
            "Response is too large."
        );
        // A steer with no turn running leaves the message queued, as the desktop says.
        assert_eq!(
            message_queue::steer_task_inner(&state, &session.id, " hi ".into())
                .await
                .unwrap_err(),
            "The agent is not running; the message stays queued."
        );
    });

    // The queue commands are plain store work on the same state.
    let entry =
        message_queue::queue_add_inner(&state, session.id.clone(), "next".into(), None).unwrap();
    assert_eq!(entry.text, "next");
    assert_eq!(
        message_queue::queue_list_inner(&state, Some(session.id.clone()))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        message_queue::queue_take_inner(&state, entry.id).unwrap().text,
        "next"
    );
    assert!(
        message_queue::queue_list_inner(&state, Some(session.id)).unwrap().is_empty(),
        "a taken entry is delivered once"
    );
}

#[test]
fn a_second_send_is_refused_at_the_guard_before_anything_is_prepared() {
    let state = state();
    let session = session(&state);
    // A turn already runs on this task, as the registry records it.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    state.agents.lock().unwrap().insert(
        session.id.clone(),
        RunningAgent::new(tx, Arc::new(AtomicBool::new(false))),
    );
    let (run_tx, _run_rx) = tokio::sync::mpsc::unbounded_channel();
    let refused = match prepare_task_run_in_project(
        &state,
        session.id.clone(),
        Some("again".into()),
        Vec::new(),
        run_tx,
        Arc::new(AtomicBool::new(false)),
        RunAdmission::default(),
    ) {
        Ok(_) => panic!("a running task must refuse a second send before preparing it"),
        Err(error) => error,
    };
    assert_eq!(
        refused,
        "This task is already running in an agent or harness console."
    );
}

#[test]
fn the_command_wrapper_hands_the_same_work_to_the_inner() {
    let app = tauri::test::mock_app();
    app.manage(state());
    let state = app.state::<AppState>();
    let session = session(&state);
    let runtime = runtime();

    runtime.block_on(async {
        let added = message_queue::queue_add(
            state.clone(),
            session.id.clone(),
            "queued".into(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(added.text, "queued");
        assert_eq!(
            message_queue::queue_list(state.clone(), Some(session.id.clone()))
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            respond_approval(state.clone(), session.id.clone(), "r1".into(), true)
                .await
                .unwrap_err(),
            "This agent is no longer running.",
            "the command answers with the inner's message, not its own"
        );
        message_queue::queue_remove(state.clone(), added.id).await.unwrap();
        assert!(message_queue::queue_list(state.clone(), None).await.unwrap().is_empty());
    });
}
