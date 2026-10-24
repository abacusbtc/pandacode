//! Router, bearer-token auth and the browser origin allow-list.

use std::time::Duration;

use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Request, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use futures_util::StreamExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::{dispatch, events::BroadcastSink, rpc, static_files};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// The only paths a token protects. The SPA's own files must load before a
/// client can present one.
const GUARDED: &[&str] = &["/rpc", "/capabilities", "/events", "/upload"];

/// 64 MB, the same number docs/gateway.md tells the reverse proxy to use for
/// `client_max_body_size`, so the proxy and the gateway refuse the same body
/// instead of disagreeing. Base64 attachments are the largest thing this surface
/// will carry; the limit is ours, not a framework default nobody chose.
///
/// `/upload` is the exception, and carries its own per-route limit: a file is
/// streamed, so a 200 MiB file costs a chunk of memory, not a body.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// What a request needs to be authorised and answered.
#[derive(Clone)]
pub(crate) struct RouterState {
    pub(crate) token: String,
    pub(crate) allowed_origins: Vec<String>,
    pub(crate) static_dir: Option<PathBuf>,
    pub(crate) events: Arc<BroadcastSink>,
    pub(crate) ctx: dispatch::Ctx,
    /// `<data-dir>/uploads`, the directory this route writes into.
    pub(crate) uploads: PathBuf,
    /// The largest single file `/upload` accepts (`--upload-max`).
    pub(crate) upload_max: usize,
    /// Who is watching which terminal, so an idle shell can be closed (PAN-110).
    pub(crate) watch: Arc<super::terminals::Watch>,
}

pub(crate) fn router(state: RouterState) -> Router {
    let cors = cors(&state.allowed_origins);
    // The per-route limit must be read before the state is moved into the router.
    let upload_max = state.upload_max;
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route("/capabilities", axum::routing::get(capabilities))
        .route("/rpc", axum::routing::post(rpc::handle))
        .route("/events", axum::routing::get(events_socket))
        // A file is the route's own size, not `/rpc`'s: the router-wide cap stays
        // for JSON, and this one layer replaces it for this route alone.
        .route(
            "/upload",
            axum::routing::post(upload).layer(DefaultBodyLimit::max(upload_max)),
        )
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

fn failure(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

/// A byte count as the sentence a person reads, with the same arithmetic the
/// limits use (1024-based) so the number matches `--upload-max`.
fn size_label(bytes: usize) -> String {
    const MIB: usize = 1024 * 1024;
    if bytes.is_multiple_of(MIB) {
        format!("{} MB", bytes / MIB)
    } else {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    }
}

struct UploadQuery {
    name: Option<String>,
    kind: Option<String>,
}

/// `?name=` is percent-decoded and `+` means space, as a query string does. The
/// name is display metadata wherever it goes from here; nothing below builds a
/// path from it.
fn upload_query(uri: &Uri) -> UploadQuery {
    let mut query = UploadQuery {
        name: None,
        kind: None,
    };
    for pair in uri.query().unwrap_or_default().split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_decode(value);
        match key {
            "name" if !value.is_empty() => query.name = Some(value),
            "kind" if !value.is_empty() => query.kind = Some(value),
            _ => {}
        }
    }
    query
}

fn percent_decode(value: &str) -> String {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            // A literal `+` in a filename must be sent as `%2B`; this is a query
            // string, where `+` is the space a form encoder writes.
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                match (digit(bytes[index + 1]), digit(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        decoded.push(high * 16 + low);
                        index += 3;
                        continue;
                    }
                    _ => decoded.push(b'%'),
                }
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// `POST /upload?name=<urlencoded>&kind=image|document`: the request body is the
/// file, streamed into the data directory as it arrives, and the answer is the
/// metadata a client needs to name it in a message later.
///
/// The cap is checked twice, and both checks matter. A declared `Content-Length`
/// is refused before a byte is read, which is cheap and catches the honest client;
/// the running count then aborts a body that lied about its length (chunked, or a
/// header smaller than what follows). Nothing is buffered: `Body::into_data_stream`
/// hands over the chunks hyper already read, and the sha256 is computed from those
/// same chunks rather than by reading the file back.
async fn upload(
    State(state): State<RouterState>,
    headers: HeaderMap,
    uri: Uri,
    body: Body,
) -> Response {
    let query = upload_query(&uri);
    let kind = match query.kind.as_deref().and_then(crate::uploads::Kind::parse) {
        Some(kind) => kind,
        None => return failure(StatusCode::BAD_REQUEST, "kind must be image or document"),
    };
    let name = match query.name {
        Some(name) if crate::uploads::name_is_sane(&name) => name,
        Some(_) => return failure(StatusCode::BAD_REQUEST, "That filename is too long."),
        None => return failure(StatusCode::BAD_REQUEST, "name is required"),
    };
    let refused = format!(
        "This file is larger than {}.",
        size_label(state.upload_max)
    );
    // Before the first byte: a declared length above the cap is an answer, not a
    // transfer to interrupt half a gigabyte into.
    if let Some(length) = declared_length(&headers) {
        if length > state.upload_max as u64 {
            return failure(StatusCode::PAYLOAD_TOO_LARGE, &refused);
        }
    }
    let mut staged = match crate::uploads::Staged::begin(&state.uploads) {
        Ok(staged) => staged,
        Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let mut stream = body.into_data_stream();
    let mut received: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            // The client went away mid-body; dropping `staged` removes the part.
            Err(_) => return failure(StatusCode::BAD_REQUEST, "The upload was interrupted."),
        };
        received += chunk.len() as u64;
        if received > state.upload_max as u64 {
            return failure(StatusCode::PAYLOAD_TOO_LARGE, &refused);
        }
        if let Err(error) = staged.write(&chunk).await {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, &error);
        }
    }
    match staged.finish(name, kind, crate::storage::now()).await {
        // `{id, name, kind, bytes, sha256, createdAt}` — the id is the only thing a
        // later message needs, and the digest is the one computed while streaming.
        Ok(upload) => axum::Json(upload.meta).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
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
    let subscription = state.events.subscribe(filter.clone(), since);
    // A socket filtered to one terminal is the client watching it. Held for the life
    // of the socket: the idle rule closes a shell only when nobody is attached.
    let attached = filter
        .terminal
        .as_deref()
        .map(|terminal| state.watch.attach(terminal));
    upgrade.on_upgrade(move |socket| forward(socket, subscription, attached))
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

async fn forward(
    mut socket: WebSocket,
    mut subscription: super::events::Subscription,
    // Dropped with this future: a socket that closes detaches its terminal.
    _attached: Option<super::terminals::Attached>,
) {
    // A resume the ring could not serve is announced before anything else, so the
    // client knows to refetch instead of trusting a stream with a hole in it. A
    // terminal cannot refetch, so it is told to clear and redraw from the replay.
    if let Some((from, to)) = subscription.reset {
        if socket
            .send(Message::Text(super::events::terminal_reset_frame(from, to).into()))
            .await
            .is_err()
        {
            return;
        }
    }
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
