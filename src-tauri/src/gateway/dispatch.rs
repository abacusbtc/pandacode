//! The command table: the subset the gateway serves, and nothing else.
//!
//! Everything absent from `handle` returns `unsupported_command`, so the client
//! can hide a surface instead of discovering it hangs.

use std::sync::Arc;

use serde_json::{json, Value};

use super::commands;
use super::live::Live;
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
    // M3: a remote client can pick the model a session runs, which the picker has been
    // offering since PAN-121 (PAN-159). A live turn is refused rather than re-pointed.
    "configure_session",
    // M3 task 3: a remote client can delete a task, and a live turn is refused with a
    // reason instead of being orphaned (PAN-124).
    "delete_session",
    // M3: the daily loop — open a task, pin it, reorder the list — each the desktop's own
    // inner, so a gateway row and a desktop row cannot disagree (PAN-161).
    "get_task",
    "pin_session",
    "reorder_sessions",
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
    // M3 task 1: a client answers the app action it was shown (PAN-123).
    "resolve_agent_ui_action",
    "steer_task",
    "queue_list",
    "queue_add",
    "queue_update",
    "queue_remove",
    "queue_reorder",
    "queue_take",
    "queue_restore",
    // M2 task 3: which tasks this gateway is driving (PAN-104).
    "gateway_live_tasks",
    // M3: the usage panel's two reads, from the gateway's own credentials and data
    // directory (PAN-138).
    "agent_usage",
    "usage_ledger",
    // M2 task 6: the catalog a client picks from (PAN-107).
    "list_providers",
    "list_provider_models",
    // M2 task 9: a real shell, over the socket the events already travel on
    // (PAN-110). `start_run_script_terminal` is deliberately absent: it is the
    // desktop's Run button, driven by an app action, not a client's own shell.
    "start_terminal",
    "write_terminal",
    "resize_terminal",
    "close_terminal",
    "get_terminal_activity",
    // M3 task 3: the owner's own terminal appearance, which the client renders with
    // (PAN-125).
    "get_terminal_preferences",
    "start_harness_terminal",
];

/// What the client may rely on beyond the raw command list.
///
/// `turns` is the one that changes what a client offers: a gateway that answers the
/// turn commands advertises it, and a client that does not see it must not show a
/// remote turn surface at all (PAN-107).
pub(crate) const FEATURES: &[&str] = &["projects", "sessions", "turns", "terminal", "usage"];

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
    /// directories, and (until task 5 gives it approvals over the socket) its own
    /// surface.
    pub(crate) run: crate::run_ctx::RunContext,
    /// The runs this process is actually driving, which no row can answer (§5).
    pub(crate) live: Arc<Live>,
    /// The terminals this process is holding open, and which sockets watch them:
    /// the idle rule is the gateway's, so the client never has to send a heartbeat
    /// (PAN-110).
    pub(crate) watch: Arc<super::terminals::Watch>,
}

impl Ctx {
    /// The same gateway, handling one request from a named client (PAN-123).
    ///
    /// One process serves many clients, so the identity is per request: the run
    /// context this hands a turn is where the turn learns where its app actions go.
    pub(crate) fn for_client(&self, client: Option<&str>) -> Ctx {
        Ctx {
            state: self.state.clone(),
            run: self.run.for_client(client),
            live: self.live.clone(),
            watch: self.watch.clone(),
        }
    }
}

/// The provider a task would run, for the live entry.///
/// One store read on the gateway's own send path. It is deliberately not folded
/// into `resolve_send`/`resolve_start`: those are shared with the desktop
/// wrappers, which have no use for it and should not pay for it.
fn provider_of(state: &AppState, session_id: &str) -> Result<String, String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    Ok(store.session(session_id)?.provider)
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
        "configure_session" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                session_id: String,
                provider: String,
                model: String,
                project_id: Option<String>,
            }
            let args: Args = from_value(args)?;
            // A turn already in flight keeps the model it started with: the desktop
            // refuses here for the same reason, asking the agent map instead of the live
            // registry (PAN-159).
            if ctx.live.running(&args.session_id) {
                return Err("Stop the agent before changing its project, provider, or model.".into());
            }
            // The picker's own catalogue is the client's list; a model this host cannot
            // negotiate is a sentence here rather than a turn that dies at negotiation.
            let narrowed = crate::provider_settings::opencode_model_ids().unwrap_or_default();
            if let Some(refusal) = crate::providers::model_refusal(&args.provider, &args.model, &narrowed) {
                return Err(refusal);
            }
            to_value(crate::configure_session_inner(
                &ctx.state,
                &args.session_id,
                &args.provider,
                &args.model,
                args.project_id.as_deref(),
            )?)
        }),
        "get_task" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                session_id: String,
            }
            let args: Args = from_value(args)?;
            to_value(crate::get_task_inner(&ctx.state, &args.session_id)?)
        }),
        "pin_session" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                session_id: String,
                pinned: bool,
            }
            let args: Args = from_value(args)?;
            let saved = crate::pin_session_inner(&ctx.state, &args.session_id, args.pinned)?;
            let _ = ctx.run.emit("sessions-changed", ());
            to_value(saved)
        }),
        "reorder_sessions" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                session_ids: Vec<String>,
            }
            let args: Args = from_value(args)?;
            let saved = crate::reorder_sessions_inner(&ctx.state.store, &args.session_ids)?;
            let _ = ctx.run.emit("sessions-changed", ());
            to_value(saved)
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
                    /// Files a remote client uploaded first (PAN-108). Unknown or
                    /// expired ids fail this send before a turn starts.
                    upload_ids: Option<Vec<String>>,
                }
                let Args {
                    session_id,
                    mut text,
                    image_ids,
                    moodboard_ids,
                    upload_ids,
                } = from_value(args)?;
                let target =
                    crate::resolve_send(&ctx.state, &session_id, &mut text, moodboard_ids.as_deref())?;
                if target.team {
                    return Err(TEAMS_NOT_SERVED.to_owned());
                }
                let provider = provider_of(&ctx.state, &session_id)?;
                // Recorded before the launch so a turn that ends at once cannot end
                // before it was listed; the surface clears it when the run ends. A
                // second send on a live session leaves the running turn's entry
                // alone, including when its own launch is then refused.
                let recorded = ctx.live.start(&session_id, &provider, ctx.run.client())?;
                if let Err(error) = crate::send_message_inner(
                    &ctx.state,
                    ctx.run.clone(),
                    session_id.clone(),
                    text,
                    image_ids,
                    upload_ids,
                    target.project_id,
                )
                .await
                {
                    if recorded {
                        ctx.live.finish(&session_id);
                    }
                    return Err(error);
                }
                Ok(json!(null))
            })
        }
        "delete_session" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    session_id: String,
                }
                let args: Args = from_value(args)?;
                // A turn this gateway is driving owns a harness process; deleting its row
                // would leave that process writing into nothing, so the request is
                // refused with the one instruction that makes it possible (PAN-124).
                if ctx
                    .live
                    .tasks()
                    .iter()
                    .any(|task| task.session_id == args.session_id)
                {
                    return Err("This task is running on the gateway. Stop it before deleting it.".to_owned());
                }
                // A deleted task never sends a follow-up; free its waiting agent.
                crate::providers::discard_warm_agent(&args.session_id);
                let state = ctx.state.clone();
                // The gateway keeps uploads under the data directory both processes
                // share, so the delete can remove them before it answers rather than
                // waiting for the next sweep.
                let root = crate::uploads::root(ctx.run.data_dir());
                let session_id = args.session_id.clone();
                let deleted = tokio::task::spawn_blocking(move || {
                    crate::task_deletion::delete_state_at(&state, &session_id, Some(&root))
                })
                .await
                .map_err(|error| format!("the delete did not finish: {error}"))??;
                let _ = ctx.run.emit(
                    "sessions-deleted",
                    json!({ "sessionIds": &deleted.deleted_session_ids }),
                );
                let _ = ctx.run.emit("sessions-changed", ());
                to_value(deleted)
            })
        }
        "get_terminal_preferences" => ready(|| to_value(crate::terminal_preferences::discover())),
        "start_task" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    session_id: String,
                    upload_ids: Option<Vec<String>>,
                }
                let args: Args = from_value(args)?;
                if crate::resolve_start(&ctx.state, &args.session_id)?.is_some() {
                    return Err(TEAMS_NOT_SERVED.to_owned());
                }
                let provider = provider_of(&ctx.state, &args.session_id)?;
                let recorded = ctx.live.start(&args.session_id, &provider, ctx.run.client())?;
                if let Err(error) = crate::start_task_inner(
                    &ctx.state,
                    ctx.run.clone(),
                    args.session_id.clone(),
                    args.upload_ids,
                )
                .await
                {
                    if recorded {
                        ctx.live.finish(&args.session_id);
                    }
                    return Err(error);
                }
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
        "resolve_agent_ui_action" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    request_id: String,
                    result: Option<Value>,
                    error: Option<String>,
                }
                let args: Args = from_value(args)?;
                // The caller answers as itself: an action addressed to one client is
                // refused when another one presents it (PAN-123).
                crate::agent_tools::resolve_ui_action(
                    &ctx.state,
                    ctx.run.client(),
                    args.request_id,
                    args.result,
                    args.error,
                )?;
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

        "gateway_live_tasks" => ready(|| to_value(ctx.live.tasks())),

        // ---- terminals (M2 task 9) -----------------------------------------
        // A shell is a PTY write and a process spawn: the work is blocking, so it
        // runs on the blocking pool and the answer comes back as a future, exactly
        // like the turn commands.
        "agent_usage" => pending(async move {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                provider: String,
                #[serde(default)]
                refresh: bool,
            }
            let args: Args = from_value(args)?;
            to_value(crate::agent_usage::usage(args.provider, args.refresh).await?)
        }),
        "usage_ledger" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    #[serde(default)]
                    refresh: bool,
                }
                let args: Args = from_value(args)?;
                to_value(crate::usage_ledger::ledger(ctx.run.data_dir().to_path_buf(), args.refresh).await?)
            })
        }
        "start_terminal" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    project_id: String,
                    session_id: Option<String>,
                    cols: u16,
                    rows: u16,
                }
                let args: Args = from_value(args)?;
                let state = ctx.state.clone();
                let events = ctx.run.emitter();
                let id = tokio::task::spawn_blocking(move || {
                    crate::start_terminal_inner(
                        &state,
                        events,
                        &args.project_id,
                        args.session_id.as_deref(),
                        args.cols,
                        args.rows,
                    )
                })
                .await
                .map_err(|error| format!("the terminal did not start: {error}"))??;
                // Registered before the id reaches the client, so an idle shell
                // cannot outlive the rule that closes it.
                ctx.watch.watch(&id);
                Ok(json!(id))
            })
        }
        "write_terminal" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    terminal_id: String,
                    data: String,
                }
                let args: Args = from_value(args)?;
                let state = ctx.state.clone();
                tokio::task::spawn_blocking(move || {
                    crate::write_terminal_inner(&state, &args.terminal_id, &args.data)
                })
                .await
                .map_err(|error| format!("the write did not finish: {error}"))??;
                Ok(json!(null))
            })
        }
        "resize_terminal" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                terminal_id: String,
                cols: u16,
                rows: u16,
            }
            let args: Args = from_value(args)?;
            crate::resize_terminal_inner(&ctx.state, &args.terminal_id, args.cols, args.rows)?;
            Ok(json!(null))
        }),
        "close_terminal" => ready(|| {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                terminal_id: String,
            }
            let args: Args = from_value(args)?;
            crate::close_terminal_inner(&ctx.state, &args.terminal_id)?;
            // A closed shell is not a candidate for the idle rule.
            ctx.watch.unwatch(&args.terminal_id);
            Ok(json!(null))
        }),
        "get_terminal_activity" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    terminal_id: String,
                }
                let args: Args = from_value(args)?;
                let state = ctx.state.clone();
                // Probing what runs in the shell means reading the process table,
                // which is blocking work like the rest of the terminal surface.
                let activity = tokio::task::spawn_blocking(move || {
                    crate::terminal_activity_inner(&state, &args.terminal_id)
                })
                .await
                .map_err(|error| format!("the activity check did not finish: {error}"))??;
                to_value(activity)
            })
        }
        "start_harness_terminal" => {
            let ctx = ctx.clone();
            pending(async move {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Args {
                    session_id: String,
                    cols: u16,
                    rows: u16,
                }
                let args: Args = from_value(args)?;
                let events = ctx.run.emitter();
                let id = crate::task_controls::start_harness_terminal_inner(
                    &ctx.state,
                    events,
                    &args.session_id,
                    args.cols,
                    args.rows,
                )
                .await?;
                ctx.watch.watch(&id);
                Ok(json!(id))
            })
        }

        // A catalog read runs the provider CLIs, so it answers with a future like the
        // other work that is not a store read (PAN-107).
        "list_providers" => pending(async move {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                force: Option<bool>,
            }
            let args: Args = from_value(args)?;
            to_value(crate::list_providers_inner(args.force).await)
        }),
        "list_provider_models" => pending(async move {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                provider: String,
                force: Option<bool>,
            }
            let args: Args = from_value(args)?;
            to_value(crate::list_provider_models_inner(args.provider, args.force).await?)
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
            live: Arc::new(Live::default()),
            watch: crate::gateway::terminals::Watch::new(std::time::Duration::from_secs(1800)),
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// PAN-125: the answer is the owner's own terminal config, read by the same code the
    /// desktop command uses. A gateway-side default would answer the same all-null shape
    /// on this host, so the test supplies a config it can recognise.
    #[test]
    fn terminal_preferences_are_the_owners_own_values_not_a_gateway_default() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config.path().join("ghostty")).unwrap();
        std::fs::write(
            config.path().join("ghostty/config"),
            "font-family = Iosevka Term\nadjust-cell-height = 25%\ncursor-style = block\n",
        )
        .unwrap();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", config.path());

        let answer = handle("get_terminal_preferences", json!({}), &ctx(data.path()));
        let desktop = serde_json::to_value(crate::terminal_preferences::discover()).unwrap();
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        let Answer::Ready(served) = answer else {
            panic!("get_terminal_preferences must answer inside the worker");
        };
        let served = served.unwrap();
        assert_eq!(served, desktop, "the wire answer is the desktop reader's answer");
        assert_eq!(served["fontFamily"], "Iosevka Term");
        assert_eq!(served["source"], "Ghostty");
        assert_eq!(served["lineHeight"], 1.25);
        assert_eq!(served["cursorStyle"], "block");
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
    fn a_row_that_claims_to_run_is_not_live_and_a_failed_launch_leaves_nothing_behind() {
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
        // A row claiming to run, with no process behind it. The registry is the
        // proof of life, so the row alone must not appear.
        ctx.state
            .store
            .lock()
            .unwrap()
            .begin_task_turn(&session.id, "hello")
            .unwrap();
        let listed = match handle("gateway_live_tasks", json!({}), &ctx) {
            Answer::Ready(result) => result.unwrap(),
            Answer::Pending(_) => panic!("gateway_live_tasks answers immediately"),
        };
        assert_eq!(listed, json!([]), "a stale row is not a live task");

        // A launch that fails must not leave the entry behind. The guard is the
        // failure this suite can drive without a harness: a turn is already
        // running, so the launch is refused after the entry was recorded.
        let other = ctx
            .state
            .store
            .lock()
            .unwrap()
            .create_session(&project.id, "claude", "sonnet", "full")
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.state.agents.lock().unwrap().insert(
            other.id.clone(),
            crate::RunningAgent::new(tx, Arc::new(std::sync::atomic::AtomicBool::new(false))),
        );
        let error = match handle(
            "send_message",
            json!({"sessionId": other.id, "text": "hi"}),
            &ctx,
        ) {
            Answer::Pending(future) => runtime().block_on(future).unwrap_err(),
            Answer::Ready(_) => panic!("send_message answers later"),
        };
        assert_eq!(
            error, "This task is already running in an agent or harness console.",
            "the guard is the failure path this test drives"
        );
        assert!(
            ctx.live.tasks().is_empty(),
            "a launch that failed must not stay listed: {:?}",
            ctx.live.tasks()
        );
    }

    #[test]
    fn a_refused_second_send_leaves_the_running_turn_listed() {
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

        // A turn is running: the registry has it and the agent registry agrees.
        ctx.live.start(&session.id, "claude", None).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.state.agents.lock().unwrap().insert(
            session.id.clone(),
            crate::RunningAgent::new(tx, Arc::new(std::sync::atomic::AtomicBool::new(false))),
        );

        let error = match handle(
            "send_message",
            json!({"sessionId": session.id, "text": "again"}),
            &ctx,
        ) {
            Answer::Pending(future) => runtime().block_on(future).unwrap_err(),
            Answer::Ready(_) => panic!("send_message answers later"),
        };
        assert_eq!(
            error,
            "This task is already running in an agent or harness console."
        );
        let listed = match handle("gateway_live_tasks", json!({}), &ctx) {
            Answer::Ready(result) => result.unwrap(),
            Answer::Pending(_) => panic!("gateway_live_tasks answers immediately"),
        };
        assert_eq!(
            listed[0]["sessionId"], session.id,
            "the refusal must not clear the turn that is still running"
        );
    }

    #[test]
    fn four_images_is_the_cap_and_an_upload_counts_with_a_screenshot() {
        // A valid one-pixel PNG: the normalizer keeps it as it is, so this test is
        // about the count, not about encoding.
        use base64::{engine::general_purpose::STANDARD, Engine};
        let png = STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC")
            .unwrap();
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
        let root = crate::uploads::root(dir.path());
        let runtime = runtime();
        let mut ids = Vec::new();
        for index in 0..5 {
            let mut staged = crate::uploads::Staged::begin(&root).unwrap();
            runtime.block_on(staged.write(&png)).unwrap();
            let upload = runtime
                .block_on(staged.finish(
                    format!("shot-{index}.png"),
                    crate::uploads::Kind::Image,
                    0,
                ))
                .unwrap();
            ids.push(upload.meta.id);
        }
        let send = |image_ids: Value, upload_ids: Vec<String>| match handle(
            "send_message",
            json!({
                "sessionId": session.id,
                "text": "look",
                "imageIds": image_ids,
                "uploadIds": upload_ids,
            }),
            &ctx,
        ) {
            Answer::Pending(future) => runtime.block_on(future),
            Answer::Ready(_) => panic!("send_message answers later"),
        };

        assert_eq!(
            send(json!([]), ids.clone()).unwrap_err(),
            "Attach up to four images per message.",
            "five uploads is one too many"
        );
        assert_eq!(
            send(json!([uuid::Uuid::new_v4().to_string()]), ids[..4].to_vec()).unwrap_err(),
            "Attach up to four images per message.",
            "an upload and a browser screenshot draw on the same four"
        );

        // A turn is running, so a send that passes the cap is refused by the guard
        // rather than spawning a harness: that the guard is reached is the proof the
        // cap let four through.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.state.agents.lock().unwrap().insert(
            session.id.clone(),
            crate::RunningAgent::new(tx, Arc::new(std::sync::atomic::AtomicBool::new(false))),
        );
        assert_eq!(
            send(json!([]), ids[..4].to_vec()).unwrap_err(),
            "This task is already running in an agent or harness console."
        );
    }

    #[test]
    fn an_unknown_upload_id_fails_the_send_before_a_turn_starts() {
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
        let missing = uuid::Uuid::new_v4().to_string();
        let error = match handle(
            "send_message",
            json!({"sessionId": session.id, "text": "here", "uploadIds": [missing]}),
            &ctx,
        ) {
            Answer::Pending(future) => runtime().block_on(future).unwrap_err(),
            Answer::Ready(_) => panic!("send_message answers later"),
        };
        assert_eq!(error, format!("unknown_upload: {missing}"));
        assert!(
            ctx.live.tasks().is_empty(),
            "a refused send is not a live task: {:?}",
            ctx.live.tasks()
        );
    }

    #[test]
    fn the_live_list_carries_what_a_client_needs_to_address_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        ctx.live.start("s1", "claude", None).unwrap();
        let listed = match handle("gateway_live_tasks", json!({}), &ctx) {
            Answer::Ready(result) => result.unwrap(),
            Answer::Pending(_) => panic!("gateway_live_tasks answers immediately"),
        };
        assert_eq!(listed[0]["sessionId"], "s1");
        assert_eq!(listed[0]["provider"], "claude");
        assert!(listed[0]["startedAt"].is_i64());
        ctx.live.finish("s1");
        let empty = match handle("gateway_live_tasks", json!({}), &ctx) {
            Answer::Ready(result) => result.unwrap(),
            Answer::Pending(_) => panic!("gateway_live_tasks answers immediately"),
        };
        assert_eq!(empty, json!([]));
    }

    #[test]
    fn the_catalog_over_rpc_is_the_desktop_commands_own_answer() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let runtime = runtime();

        let over_rpc = match handle("list_providers", json!({}), &ctx) {
            Answer::Pending(future) => runtime.block_on(future).unwrap(),
            Answer::Ready(_) => panic!("list_providers answers later"),
        };
        let desktop = runtime.block_on(crate::list_providers(None));
        assert_eq!(
            over_rpc,
            serde_json::to_value(&desktop).unwrap(),
            "the wire carries the command's own answer, not a re-shaping"
        );

        // A provider's models: the same answer success or failure, because it is the
        // same call underneath.
        if let Some(provider) = desktop.iter().find(|p| p.installed).or_else(|| desktop.first()) {
            let rpc = match handle(
                "list_provider_models",
                json!({"provider": provider.id}),
                &ctx,
            ) {
                Answer::Pending(future) => runtime.block_on(future),
                Answer::Ready(_) => panic!("list_provider_models answers later"),
            };
            match runtime.block_on(crate::list_provider_models(provider.id.clone(), None)) {
                Ok(models) => assert_eq!(rpc.unwrap(), serde_json::to_value(&models).unwrap()),
                Err(error) => assert_eq!(rpc.unwrap_err(), error, "a failure is the same failure"),
            }
        }
    }

    #[test]
    fn capabilities_advertise_turns_and_the_catalog_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let value = match handle("gateway_status", json!({}), &ctx) {
            Answer::Ready(result) => result.unwrap(),
            Answer::Pending(_) => panic!("gateway_status answers immediately"),
        };
        let features: Vec<&str> = value["features"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|feature| feature.as_str())
            .collect();
        assert!(
            features.contains(&"turns"),
            "a client decides whether to offer remote turns from this: {features:?}"
        );
        // And the rule from task 3 still holds: a command a client is promised is in
        // the same list the dispatcher answers from.
        let advertised: Vec<&str> = value["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|command| command.as_str())
            .collect();
        for command in ["list_providers", "list_provider_models"] {
            assert!(advertised.contains(&command), "{command} is served but not advertised");
        }
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
