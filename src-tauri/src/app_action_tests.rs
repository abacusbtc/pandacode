//! An app action needs a client. With none attached it must fail fast and say
//! why, instead of waiting out the desktop's patience (PAN-106).

use std::{
    sync::{
        atomic::AtomicBool,
        mpsc,
        Arc,
    },
    time::{Duration, Instant},
};

use serde_json::json;

use crate::{agent_tools::request_ui_on, run_ctx::UiRequests};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn an_app_action_with_no_client_fails_fast_and_names_the_cause() {
    let ui = Arc::new(UiRequests::default());
    let started = Instant::now();
    let error = runtime()
        .block_on(request_ui_on(
            &ui,
            |_name, _payload| Ok(()),
            // The run's surface answering "nothing is attached".
            false,
            "s1",
            "open_picker",
            json!({}),
            Arc::new(AtomicBool::new(false)),
        ))
        .unwrap_err();
    let waited = started.elapsed();

    assert_eq!(error, "No client is attached to answer this app action.");
    assert!(
        waited >= Duration::from_millis(4_800) && waited < Duration::from_secs(9),
        "the no-client path is five seconds, not the desktop's twelve: {waited:?}"
    );
}

#[test]
fn with_a_client_attached_the_desktop_wait_is_unchanged() {
    let ui = Arc::new(UiRequests::default());
    let (id_tx, id_rx) = mpsc::channel::<String>();

    // The answer arrives after the five-second no-client deadline, so only the
    // longer attached window can still receive it.
    let answerer = ui.clone();
    let answered = std::thread::spawn(move || {
        let id = id_rx.recv().expect("the request was emitted");
        std::thread::sleep(Duration::from_millis(5_500));
        answerer.answer(&id, Ok(json!("done"))).unwrap();
    });

    let result = runtime().block_on(request_ui_on(
        &ui,
        move |_name, payload| {
            id_tx
                .send(payload["requestId"].as_str().unwrap().to_string())
                .unwrap();
            Ok(())
        },
        // A window is an attached client.
        true,
        "s1",
        "open_picker",
        json!({}),
        Arc::new(AtomicBool::new(false)),
    ));
    answered.join().unwrap();

    assert_eq!(result.unwrap(), json!("done"));
}
