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

use super::{audit, dispatch, server::Peer, server::RouterState};
use std::time::Instant;

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

/// The task a command names, when it names one. Turn and queue commands all carry
/// `sessionId`, and it is the one argument the audit line needs to tie a command to
/// the work it touched.
fn session_of(args: &Value) -> Option<String> {
    args.get("sessionId")
        .and_then(Value::as_str)
        .filter(|session| !session.is_empty())
        .map(str::to_owned)
}

/// The decision a `respond_approval` carried, as the word the journal should use.
fn approval_decision(command: &str, args: &Value) -> Option<&'static str> {
    if command != "respond_approval" {
        return None;
    }
    match args.get("approved").and_then(Value::as_bool) {
        Some(true) => Some("approved"),
        Some(false) => Some("denied"),
        // A decision this gateway cannot read is still a decision it was handed.
        None => Some("unreadable"),
    }
}

pub async fn handle(
    State(state): State<RouterState>,
    Peer(client): Peer,
    body: Result<Json<Request>, JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        // The rejection carries the status: a malformed body is a client error
        // and an oversized one is 413, which must not be flattened to 400.
        Err(rejection) => return (rejection.status(), rejection.body_text()).into_response(),
    };
    let started = Instant::now();
    let command = request.command;
    let args = request.args;
    let session = session_of(&args);
    let decision = approval_decision(&command, &args);
    let request_id = args
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let ctx = state.ctx.clone();
    // Store work is blocking; keep it off the request's own worker. A turn command
    // hands back a future instead, which is awaited here rather than in that worker.
    // The command string travels a copy, because the audit line needs it too.
    let dispatched = command.clone();
    let outcome =
        tokio::task::spawn_blocking(move || dispatch::handle(&dispatched, args, &ctx)).await;
    let result = match outcome {
        Ok(dispatch::Answer::Ready(result)) => result,
        Ok(dispatch::Answer::Pending(future)) => future.await,
        Err(error) => Err(format!("the command did not finish: {error}")),
    };
    // One line per command, whatever it did: what came in, from where, how long it
    // took, and whether it answered. The error *text* stays out — it can quote the
    // thing the person was working on.
    let mut line = audit::Line::new("command")
        .field("client", &client)
        .field("command", &command)
        .field("outcome", if result.is_ok() { "ok" } else { "failed" })
        .field("duration_ms", started.elapsed().as_millis());
    if let Some(session) = &session {
        line = line.field("session", session);
    }
    line.emit();
    // An approval is a decision, and a decision is what an audit exists to record:
    // it gets its own line with the word the person would use.
    if let Some(decision) = decision {
        let mut line = audit::Line::new("approval")
            .field("client", &client)
            .field("decision", decision)
            .field("duration_ms", started.elapsed().as_millis());
        if let Some(session) = &session {
            line = line.field("session", session);
        }
        if let Some(request_id) = &request_id {
            line = line.field("request", request_id);
        }
        line.emit();
    }
    let body = match result {
        Ok(result) => ResponseBody {
            id: request.id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => ResponseBody {
            id: request.id,
            ok: false,
            result: None,
            error: Some(error),
        },
    };
    Json(body).into_response()
}
