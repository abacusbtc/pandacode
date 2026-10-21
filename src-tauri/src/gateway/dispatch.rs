//! The command table: the subset the gateway serves, and nothing else.
//!
//! Everything absent from `handle` returns `unsupported_command`, so the client
//! can hide a surface instead of discovering it hangs.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::AppState;

/// What a command runs against. The gateway owns the only writer on this data
/// directory (see `config::Lock`), so the empty agent registry is the truth.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) state: Arc<AppState>,
}

pub fn handle(command: &str, args: Value, ctx: &Ctx) -> Result<Value, String> {
    match command {
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
        other => Err(format!("unsupported_command: {other}")),
    }
}

fn from_value<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|error| error.to_string())
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}
