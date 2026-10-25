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

use crate::{
    agent_tools::{request_ui_on, AppAction},
    run_ctx::UiRequests,
};

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
            AppAction {
                // Unaddressed: the desktop shape, which is what PAN-106 proved.
                client: None,
                session: "s1",
                action: "open_picker",
                args: json!({}),
                stopped: Arc::new(AtomicBool::new(false)),
            },
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
        answerer.answer(None, &id, Ok(json!("done"))).unwrap();
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
        AppAction {
            // Unaddressed, like the desktop's own call.
            client: None,
            session: "s1",
            action: "open_picker",
            args: json!({}),
            stopped: Arc::new(AtomicBool::new(false)),
        },
    ));
    answered.join().unwrap();

    assert_eq!(result.unwrap(), json!("done"));
}

/// PAN-123: where an app action is addressed is the run context's decision, and the
/// desktop's answer — an unaddressed turn — is unchanged: it still broadcasts.
#[test]
fn an_addressed_app_action_goes_to_one_client_while_an_unaddressed_one_broadcasts() {
    use crate::{
        events::Emitter,
        run_ctx::{NoSurface, RunContext},
    };
    use serde_json::Value;
    use std::sync::Mutex;

    /// Records how it was addressed and which request went that way.
    #[derive(Default)]
    struct Addressing(Mutex<Vec<(String, String)>>);
    impl Addressing {
        fn seen(&self) -> Vec<(String, String)> {
            self.0.lock().unwrap().clone()
        }
        fn record(&self, address: String, payload: &Value) {
            let id = payload["requestId"].as_str().unwrap_or_default().to_owned();
            self.0.lock().unwrap().push((address, id));
        }
    }
    impl Emitter for Addressing {
        fn emit(&self, name: &str, payload: Value) -> Result<(), String> {
            self.record(format!("broadcast/{name}"), &payload);
            Ok(())
        }
        fn emit_to(&self, target: &str, name: &str, payload: Value) -> Result<(), String> {
            self.record(format!("{target}/{name}"), &payload);
            Ok(())
        }
        fn emit_to_client(&self, client: &str, name: &str, payload: Value) -> Result<(), String> {
            self.record(format!("client:{client}/{name}"), &payload);
            Ok(())
        }
    }

    /// Runs one app action as a turn would, answers it as `answerer`, and returns how
    /// the emitter was addressed.
    fn run_action(client: Option<&str>, answerer: Option<&str>) -> Vec<String> {
        let recorder = Arc::new(Addressing::default());
        let ui = Arc::new(UiRequests::default());
        let context = RunContext::new(
            recorder.clone(),
            "/tmp/pan123-cache".into(),
            "/tmp/pan123-data".into(),
            ui.clone(),
            Arc::new(NoSurface),
        );
        let run = context.for_client(client);
        let asker = std::thread::spawn(move || {
            runtime()
                .block_on(run.request_app_action(
                    "s1",
                    "open_picker",
                    json!({}),
                    Arc::new(AtomicBool::new(false)),
                ))
                .unwrap()
        });
        // The request is registered before it is emitted, so a recorded frame always
        // has an id that can be answered.
        let mut answered = false;
        for _ in 0..100 {
            if let Some((_, id)) = recorder.seen().first() {
                ui.answer(answerer, id, Ok(json!("done"))).unwrap();
                answered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(answered, "the app action was never emitted");
        let value = asker.join().unwrap();
        assert_eq!(value, json!("done"));
        recorder.seen().into_iter().map(|(address, _)| address).collect()
    }

    assert_eq!(
        run_action(Some("alpha"), Some("alpha")),
        ["client:alpha/agent-ui-action"],
        "an addressed action reaches that client's sink and nothing else"
    );
    assert_eq!(
        run_action(None, None),
        ["broadcast/agent-ui-action"],
        "a desktop turn addresses nothing, so its action broadcasts as it always did"
    );
}
