//! Router, bearer-token auth and the browser origin allow-list.

use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Request, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::{dispatch, events::BroadcastSink, rpc, static_files};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// The only paths a token protects. The SPA's own files must load before a
/// client can present one.
const GUARDED: &[&str] = &["/rpc", "/capabilities", "/events"];

/// 64 MB, the same number docs/gateway.md tells the reverse proxy to use for
/// `client_max_body_size`, so the proxy and the gateway refuse the same body
/// instead of disagreeing. Base64 attachments are the largest thing this surface
/// will carry; the limit is ours, not a framework default nobody chose.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// What a request needs to be authorised and answered.
#[derive(Clone)]
pub(crate) struct RouterState {
    pub(crate) token: String,
    pub(crate) allowed_origins: Vec<String>,
    pub(crate) static_dir: Option<PathBuf>,
    pub(crate) events: Arc<BroadcastSink>,
    pub(crate) ctx: dispatch::Ctx,
}

pub(crate) fn router(state: RouterState) -> Router {
    let cors = cors(&state.allowed_origins);
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route("/capabilities", axum::routing::get(capabilities))
        .route("/rpc", axum::routing::post(rpc::handle))
        .route("/events", axum::routing::get(events_socket))
        .fallback(spa)
        // Layer order is load-bearing: the last layer is outermost, so CORS
        // answers the browser's token-less preflight and auth guards the rest.
        // Swapping these two breaks the app's client with an opaque browser error.
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .layer(cors)
        // Outermost, so no handler buffers a body before the cap is applied.
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

/// The same list the dispatcher matches on, so a client can hide what is absent.
async fn capabilities() -> Response {
    axum::Json(json!({
        "commands": dispatch::COMMANDS,
        "features": dispatch::FEATURES,
    }))
    .into_response()
}

/// The SPA, or a plain 404 when this gateway was started without a build.
async fn spa(State(state): State<RouterState>, method: Method, uri: Uri) -> Response {
    // The fallback serves files, so a write to a client route is a wrong method
    // rather than a missing route: 405 with Allow says that, where 200 with the
    // shell would quietly tell a POST its body was accepted.
    if method != Method::GET && method != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, HeaderValue::from_static("GET, HEAD"))],
            "method not allowed",
        )
            .into_response();
    }
    match &state.static_dir {
        Some(root) => static_files::serve(root, uri.path())
            .await
            .unwrap_or_else(not_found),
        None => not_found(),
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// One socket per client, receiving the frames it asked for.
///
/// `?session=<id>` and `?terminal=<id>` choose what reaches this socket, and
/// `?since=<seq>` asks for what it missed. The token is read by `authorize`, so a
/// missing or wrong one is a 401 before this runs.
async fn events_socket(
    State(state): State<RouterState>,
    upgrade: WebSocketUpgrade,
    uri: Uri,
) -> Response {
    let (filter, since) = match events_query(&uri) {
        Ok(query) => query,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    let subscription = state.events.subscribe(filter, since);
    upgrade.on_upgrade(move |socket| forward(socket, subscription))
}

/// The `/events` query. Unknown parameters are ignored so an older client keeps
/// working; a `since` that is not a number is a client error, not a silent
/// full replay.
fn events_query(uri: &Uri) -> Result<(super::events::Filter, Option<u64>), String> {
    let mut filter = super::events::Filter::default();
    let mut since = None;
    for pair in uri.query().unwrap_or_default().split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            // Already checked by `authorize`, which needs it before this point.
            "token" => {}
            "session" if !value.is_empty() => filter.session = Some(value.to_owned()),
            "terminal" if !value.is_empty() => filter.terminal = Some(value.to_owned()),
            "since" => {
                since = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("since must be a frame number, not {value:?}"))?,
                )
            }
            _ => {}
        }
    }
    Ok((filter, since))
}

async fn forward(mut socket: WebSocket, mut subscription: super::events::Subscription) {
    // A resume the ring could not serve is announced before anything else, so the
    // client knows to refetch instead of trusting a stream with a hole in it.
    if let Some((from, to)) = subscription.hole {
        if send_lagged(&mut socket, from, to).await.is_err() {
            return;
        }
    }
    for frame in std::mem::take(&mut subscription.replay) {
        if socket
            .send(Message::Text(frame.to_json().into()))
            .await
            .is_err()
        {
            return;
        }
    }

    // A hole is flushed before the frame after it, so the client never sees a frame
    // from beyond a gap without being told about the gap first. The tick covers the
    // case where nothing further arrives.
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
            frame = subscription.frames.recv() => match frame {
                Some(frame) => {
                    if announce_hole(&mut socket, &subscription.dropped).await.is_err() {
                        break;
                    }
                    if socket.send(Message::Text(frame.to_json().into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            _ = ticker.tick() => {
                if announce_hole(&mut socket, &subscription.dropped).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Sends the `lagged` frame for whatever this socket missed, if anything.
async fn announce_hole(
    socket: &mut WebSocket,
    dropped: &std::sync::Mutex<Option<(u64, u64)>>,
) -> Result<(), ()> {
    let hole = dropped
        .lock()
        .ok()
        .and_then(|mut hole| hole.take());
    match hole {
        Some((from, to)) => send_lagged(socket, from, to).await,
        None => Ok(()),
    }
}

async fn send_lagged(socket: &mut WebSocket, from: u64, to: u64) -> Result<(), ()> {
    socket
        .send(Message::Text(super::events::lagged_frame(from, to).into()))
        .await
        .map_err(|_| ())
}

async fn authorize(State(state): State<RouterState>, request: Request, next: Next) -> Response {
    // CORS is outermost, so a real preflight never reaches this point: what this
    // middleware sees are the app's own requests. Guard by the canonical path,
    // because /rpc/ and //rpc are not what axum routes to the handler, and an
    // auth decision must not hang on how a client spelled the path.
    let path = canonical_path(request.uri().path());
    if !GUARDED.contains(&path.as_str()) {
        return next.run(request).await;
    }
    let presented = bearer(request.headers()).or_else(|| {
        // A browser cannot set headers on a WebSocket handshake, so the socket
        // accepts the token as a query parameter too. Nothing else does: a token
        // in a URL is more likely to end up in a log.
        (path == "/events")
            .then(|| query_token(request.uri()))
            .flatten()
    });
    if presented.as_deref() != Some(state.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(request).await
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// The gateway's own token is hex, so it needs no percent-decoding.
fn query_token(uri: &Uri) -> Option<String> {
    uri.query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .map(str::to_string)
}

/// Collapses the spellings axum does not route to a handler, so `/rpc/` and
/// `//rpc` are judged as the API they name.
fn canonical_path(path: &str) -> String {
    let mut canonical = String::with_capacity(path.len());
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        canonical.push('/');
        canonical.push_str(segment);
    }
    if canonical.is_empty() {
        canonical.push('/');
    }
    canonical
}

/// Reflects only the origins named on the command line; everything else is dropped.
fn cors(origins: &[String]) -> CorsLayer {
    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin).ok())
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .max_age(Duration::from_secs(600))
}
