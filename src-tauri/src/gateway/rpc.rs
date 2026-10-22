//! The JSON-RPC envelope: one command per request, one result or one error.
//!
//! Command failures keep the HTTP status at 200 and travel in `error`, because
//! they are the same strings the Tauri commands already return and the UI
//! handles them the same way.

use axum::{
    extract::{rejection::JsonRejection, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{dispatch, server::RouterState};

#[derive(Deserialize)]
pub struct Request {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Serialize)]
struct ResponseBody {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn handle(
    State(state): State<RouterState>,
    body: Result<Json<Request>, JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        // The rejection carries the status: a malformed body is a client error
        // and an oversized one is 413, which must not be flattened to 400.
        Err(rejection) => return (rejection.status(), rejection.body_text()).into_response(),
    };
    let command = request.command;
    let args = request.args;
    let ctx = state.ctx.clone();
    // Store work is blocking; keep it off the request's own worker.
    let outcome = tokio::task::spawn_blocking(move || dispatch::handle(&command, args, &ctx)).await;
    let body = match outcome {
        Ok(Ok(result)) => ResponseBody {
            id: request.id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Ok(Err(error)) => ResponseBody {
            id: request.id,
            ok: false,
            result: None,
            error: Some(error),
        },
        Err(error) => ResponseBody {
            id: request.id,
            ok: false,
            result: None,
            error: Some(format!("the command did not finish: {error}")),
        },
    };
    Json(body).into_response()
}
