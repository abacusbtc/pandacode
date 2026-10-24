//! The command table: the subset the gateway serves, and nothing else.
//!
//! Everything absent from `handle` returns `unsupported_command`, so the client
//! can hide a surface instead of discovering it hangs.

use std::sync::Arc;

use serde_json::{json, Value};

use super::commands;
use crate::AppState;

/// The commands the gateway serves. `handle` and `/capabilities` both read this
/// list, so the endpoint can never advertise something the table does not answer.
pub(crate) const COMMANDS: &[&str] = &[
    // M1
    "add_project",
    "list_projects",
    "remove_project",
    "list_sessions",
    "create_session",
    "get_messages",
    "gateway_list_dirs",
    "gateway_status",
    // M2 task 2: the turn commands, each the same `_inner` the desktop command
    // calls (PAN-103). Task 6 adds the `turns` capability; until then these are
    // advertised as commands only.
    "send_message",
    "start_task",
    "stop_session",
    "respond_approval",
    "respond_user_input",
    "steer_task",
    "queue_list",
    "queue_add",
    "queue_update",
    "queue_remove",
    "queue_reorder",
    "queue_take",
    "queue_restore",
];

/// What the client may rely on beyond the raw command list.
pub(crate) const FEATURES: &[&str] = &["projects", "sessions"];

/// A team is N runs driven by the architect, which a window owns; M2 does not
/// serve teams (spec section 8, rows 2 and 6). Saying so is better than starting
/// one turn of a team and leaving the rest of it unrouted.
const TEAMS_NOT_SERVED: &str = "Teams are not served over the gateway; this task runs one.";

/// What a command runs against. The gateway owns the only writer on this data
/// directory (see `config::Lock`), so the empty agent registry is the truth.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) state: Arc<AppState>,
    /// What a turn runs with in this process: the gateway's own sink, its own
    /// directories, and (until task 5 gives it approvals over the socket) a
    /// surface that offers no desktop hosts.
    pub(crate) run: crate::run_ctx::RunContext,
}

/// A command's answer.
///
/// M1's commands answer inside the blocking worker, which is where their store
/// work belongs. A turn command cannot: starting, steering or stopping a turn is
/// async, so it hands back a future that owns the state it needs and runs on the
/// request worker instead of blocking one.
pub(crate) enum Answer {
    Ready(Result<Value, String>),
    Pending(PendingAnswer),
}

type PendingAnswer =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>>;

/// Runs blocking work now; `?` inside the closure is the command's error.
fn ready(work: impl FnOnce() -> Result<Value, String>) -> Answer {
    Answer::Ready(work())
}

/// Hands back work that will finish later, owning everything it touches.
fn pending<T>(work: T) -> Answer
where
    T: std::future::Future<Output = Result<Value, String>> + Send + 'static,
{
    Answer::Pending(Box::pin(work))
}

pub fn handle(command: &str, args: Value, ctx: &Ctx) -> Answer {
    match command {
        "gateway_status" => ready(|| Ok(json!({ "commands": COMMANDS, "features": FEATURES }))),
        "add_project" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                path: String,
            }
            let args: Args = from_value(args)?;
            to_value(crate::add_project_inner(&ctx.state, args.path)?)
        }),
        "list_projects" => ready(|| to_value(crate::list_projects_inner(&ctx.state)?)),
        "remove_project" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                project_id: String,
            }
            let args: Args = from_value(args)?;
            crate::remove_project_inner(&ctx.state, args.project_id)?;
            Ok(json!(null))
        }),
        "list_sessions" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                project_id: Option<String>,
            }
            let args: Args = from_value(args)?;
            let mut sessions = crate::list_sessions_inner(&ctx.state)?;
            if let Some(project_id) = args.project_id {
                sessions.retain(|session| session.project_id == project_id);
            }
            to_value(sessions)
        }),
        "create_session" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                project_id: String,
                provider: String,
                model: String,
                mode: Option<String>,
            }
            let args: Args = from_value(args)?;
            // The desktop's sixth argument is a draft flag for New tasks; a task
            // created over the gateway is a real one from its first moment.
            to_value(crate::create_session_inner(
                &ctx.state,
                args.project_id,
                args.provider,
                args.model,
                args.mode,
                None,
            )?)
        }),
        "get_messages" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                session_id: String,
                before_id: Option<i64>,
            }
            let args: Args = from_value(args)?;
            to_value(crate::get_messages_inner(
                &ctx.state,
                args.session_id,
                args.before_id,
            )?)
        }),
        "gateway_list_dirs" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                path: Option<String>,
                #[serde(default)]
                show_hidden: bool,
            }
            let args: Args = from_value(args)?;
            commands::list_dirs(args.path, args.show_hidden)
        }),

        // ---- turn commands (M2 task 2) -------------------------------------
        "send_message" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    session_id: String,
                    text: String,
                    image_ids: Option<Vec<String>>,
                    moodboard_ids: Option<Vec<String>>,
                }
                let Args {
                    session_id,
                    mut text,
                    image_ids,
                    moodboard_ids,
                } = from_value(args)?;
                let target =
                    crate::resolve_send(&ctx.state, &session_id, &mut text, moodboard_ids.as_deref())?;
                if target.team {
                    return Err(TEAMS_NOT_SERVED.to_owned());
                }
                crate::send_message_inner(
                    &ctx.state,
                    ctx.run.clone(),
                    session_id,
                    text,
                    image_ids,
                    target.project_id,
                )
                .await?;
                Ok(json!(null))
            })
        }
        "start_task" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    session_id: String,
                }
                let args: Args = from_value(args)?;
                if crate::resolve_start(&ctx.state, &args.session_id)?.is_some() {
                    return Err(TEAMS_NOT_SERVED.to_owned());
                }
                crate::start_task_inner(&ctx.state, ctx.run.clone(), args.session_id).await?;
                Ok(json!(null))
            })
        }
        "stop_session" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    session_id: String,
                }
                let args: Args = from_value(args)?;
                // A team's stop goes through the architect, which a gateway does
                // not run; stopping an ordinary task is the shared inner.
                if crate::is_team(&ctx.state, &args.session_id)? {
                    return Err(TEAMS_NOT_SERVED.to_owned());
                }
                crate::stop_session_inner(&ctx.state, &args.session_id).await?;
                Ok(json!(null))
            })
        }
        "respond_approval" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    session_id: String,
                    request_id: String,
                    approved: bool,
                }
                let args: Args = from_value(args)?;
                crate::respond_approval_inner(
                    &ctx.state,
                    &args.session_id,
                    args.request_id,
                    args.approved,
                )
                .await?;
                Ok(json!(null))
            })
        }
        "respond_user_input" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    session_id: String,
                    request_id: String,
                    response: Value,
                }
                let args: Args = from_value(args)?;
                crate::respond_user_input_inner(
                    &ctx.state,
                    &args.session_id,
                    args.request_id,
                    args.response,
                )
                .await?;
                Ok(json!(null))
            })
        }
        "steer_task" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    session_id: String,
                    text: String,
                }
                let args: Args = from_value(args)?;
                crate::message_queue::steer_task_inner(&ctx.state, &args.session_id, args.text)
                    .await?;
                Ok(json!(null))
            })
        }
        "queue_list" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                session_id: Option<String>,
            }
            let args: Args = from_value(args)?;
            to_value(crate::message_queue::queue_list_inner(
                &ctx.state,
                args.session_id,
            )?)
        }),
        "queue_add" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                session_id: String,
                text: String,
                attachments: Option<Value>,
            }
            let args: Args = from_value(args)?;
            to_value(crate::message_queue::queue_add_inner(
                &ctx.state,
                args.session_id,
                args.text,
                args.attachments,
            )?)
        }),
        "queue_update" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                id: String,
                text: String,
                attachments: Option<Value>,
            }
            let args: Args = from_value(args)?;
            to_value(crate::message_queue::queue_update_inner(
                &ctx.state,
                args.id,
                args.text,
                args.attachments,
            )?)
        }),
        "queue_remove" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                id: String,
            }
            let args: Args = from_value(args)?;
            crate::message_queue::queue_remove_inner(&ctx.state, args.id)?;
            Ok(json!(null))
        }),
        "queue_reorder" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                session_id: String,
                ids: Vec<String>,
            }
            let args: Args = from_value(args)?;
            to_value(crate::message_queue::queue_reorder_inner(
                &ctx.state,
                args.session_id,
                args.ids,
            )?)
        }),
        "queue_take" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                id: String,
            }
            let args: Args = from_value(args)?;
            to_value(crate::message_queue::queue_take_inner(&ctx.state, args.id)?)
        }),
        "queue_restore" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                entry: crate::message_queue::QueuedMessage,
            }
            let args: Args = from_value(args)?;
            to_value(crate::message_queue::queue_restore_inner(
                &ctx.state,
                args.entry,
            )?)
        }),

        other => ready(|| Err(format!("unsupported_command: {other}"))),
    }
}

fn from_value<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|error| error.to_string())
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_ctx::NoSurface;

    /// A sink that keeps nothing: these tests are about the command surface, not
    /// about what a turn emits.
    struct NullSink;
    impl crate::events::Emitter for NullSink {
        fn emit(&self, _name: &str, _payload: Value) -> Result<(), String> {
            Ok(())
        }
        fn emit_to(&self, _target: &str, _name: &str, _payload: Value) -> Result<(), String> {
            Ok(())
        }
    }

    /// A gateway context over a temp data directory: the state a turn command
    /// gets here is the gateway's own, which holds no desktop app.
    fn ctx(dir: &std::path::Path) -> Ctx {
        let state = crate::gateway_state(dir).unwrap();
        let run = crate::run_ctx::RunContext::new(
            Arc::new(NullSink),
            dir.join("cache"),
            dir.to_path_buf(),
            state.ui.clone(),
            Arc::new(NoSurface),
        );
        Ctx {
            state: Arc::new(state),
            run,
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn every_turn_command_is_advertised() {
        for command in [
            "send_message",
            "start_task",
            "stop_session",
            "respond_approval",
            "respond_user_input",
            "steer_task",
            "queue_list",
            "queue_add",
            "queue_update",
            "queue_remove",
            "queue_reorder",
            "queue_take",
            "queue_restore",
        ] {
            assert!(
                COMMANDS.contains(&command),
                "{command} is answered but not advertised"
            );
        }
    }

    #[test]
    fn a_running_task_refuses_a_second_send_with_the_desktops_message() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let project = ctx.state.store.lock().unwrap().add_project(dir.path()).unwrap();
        // A real provider: the guard is reached after `prepare_command`, so this
        // proves the refusal on the path a desktop send takes. Nothing is spawned
        // while the guard holds, which is exactly what the test asserts.
        let session = ctx
            .state
            .store
            .lock()
            .unwrap()
            .create_session(&project.id, "claude", "sonnet", "full")
            .unwrap();
        // A turn is running on this task: the registry holds it, exactly as a
        // launched turn would.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.state.agents.lock().unwrap().insert(
            session.id.clone(),
            crate::RunningAgent::new(tx, Arc::new(std::sync::atomic::AtomicBool::new(false))),
        );

        let answer = handle(
            "send_message",
            json!({"sessionId": session.id, "text": "again"}),
            &ctx,
        );
        let error = match answer {
            Answer::Pending(future) => runtime().block_on(future).unwrap_err(),
            Answer::Ready(_) => panic!("send_message must answer later"),
        };
        assert_eq!(
            error,
            "This task is already running in an agent or harness console."
        );
    }

    #[test]
    fn stopping_an_idle_task_answers_null_and_approvals_report_the_honest_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let project = ctx.state.store.lock().unwrap().add_project(dir.path()).unwrap();
        let session = ctx
            .state
            .store
            .lock()
            .unwrap()
            .create_session(&project.id, "claude", "sonnet", "full")
            .unwrap();

        let runtime = runtime();
        let stopped = match handle(
            "stop_session",
            json!({"sessionId": session.id}),
            &ctx,
        ) {
            Answer::Pending(future) => runtime.block_on(future).unwrap(),
            Answer::Ready(_) => panic!("stop_session must answer later"),
        };
        assert_eq!(stopped, json!(null), "a stop with no running turn is not an error");

        let refused = match handle(
            "respond_approval",
            json!({"sessionId": session.id, "requestId": "r1", "approved": true}),
            &ctx,
        ) {
            Answer::Pending(future) => runtime.block_on(future).unwrap_err(),
            Answer::Ready(_) => panic!("respond_approval must answer later"),
        };
        assert_eq!(refused, "This agent is no longer running.");
    }
}
