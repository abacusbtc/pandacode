//! The command table: the subset the gateway serves, and nothing else.
//!
//! Everything absent from `handle` returns `unsupported_command`, so the client
//! can hide a surface instead of discovering it hangs.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::AppState;

/// The M1 commands. `handle` and `/capabilities` both read this list, so the
/// endpoint can never advertise something the table does not answer.
pub(crate) const COMMANDS: &[&str] = &[
    "add_project",
    "list_projects",
    "remove_project",
    "list_sessions",
    "create_session",
    "get_messages",
    "gateway_status",
];

/// What the client may rely on beyond the raw command list.
pub(crate) const FEATURES: &[&str] = &["projects", "sessions"];

/// What a command runs against. The gateway owns the only writer on this data
/// directory (see `config::Lock`), so the empty agent registry is the truth.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) state: Arc<AppState>,
}

pub fn handle(command: &str, args: Value, ctx: &Ctx) -> Result<Value, String> {
    match command {
        "gateway_status" => Ok(json!({ "commands": COMMANDS, "features": FEATURES })),
        "add_project" => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                path: String,
            }
            let args: Args = from_value(args)?;
            to_value(crate::add_project_inner(&ctx.state, args.path)?)
        }
        "list_projects" => to_value(crate::list_projects_inner(&ctx.state)?),
        "remove_project" => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args {
                project_id: String,
            }
            let args: Args = from_value(args)?;
            crate::remove_project_inner(&ctx.state, args.project_id)?;
            Ok(json!(null))
        }
        "list_sessions" => {
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
        }
        "create_session" => {
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
        }
        "get_messages" => {
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
        }
        other => Err(format!("unsupported_command: {other}")),
    }
}

fn from_value<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|error| error.to_string())
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}
