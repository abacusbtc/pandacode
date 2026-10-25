//! Router, bearer-token auth and the browser origin allow-list.

use std::time::Duration;

use axum::{
    body::Body,
    extract::{
        connect_info::ConnectInfo,
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Request, State,
    },
    http::{header, request::Parts, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use futures_util::StreamExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::{audit, dispatch, events::BroadcastSink, rpc, static_files};
use serde_json::json;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// The only paths a token protects. The SPA's own files must load before a
/// client can present one.
const GUARDED: &[&str] = &["/rpc", "/capabilities", "/events", "/upload"];

/// Failed authorisations one source may make in the window before it is answered
/// `429` (spec §2.3). Ten is small enough to make guessing a 32-byte token pointless
/// and large enough that a person pasting the wrong token twice is not punished.
const AUTH_FAILURES: u32 = 10;
/// The window those failures are counted in. It rolls: a source that goes quiet
/// starts again from zero.
const AUTH_WINDOW: Duration = Duration::from_secs(60);

/// 64 MB for `/rpc`: base64 attachments are the largest thing that surface carries,
/// and the limit is chosen here rather than inherited as a framework default nobody
/// picked.
///
/// `/upload` is the exception, and carries its own per-route limit: a file is
/// streamed, so a 200 MiB file costs a chunk of memory, not a body. The reverse
/// proxy's `client_max_body_size` therefore follows the upload cap (docs/gateway.md),
/// not this number.
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
    /// Failed authorisations per source, for the limit that answers `429`.
    pub(crate) auth_failures: Arc<AuthFailures>,
}

/// Failed authorisations per source, counted over a rolling window.
///
/// The unit of the limit is a *failure*, never a request: a client holding the right
/// token is never limited, so a brute force from an address cannot lock the owner out
/// of the gateway they reach from the same address (spec §2.3, review focus 7).
#[derive(Default)]
pub(crate) struct AuthFailures(RwLock<HashMap<IpAddr, (Instant, u32)>>);

impl AuthFailures {
    /// Records one failed authorisation from `source` and answers how many it has made
    /// in the current window.
    fn record(&self, source: IpAddr, now: Instant) -> u32 {
        let mut failures = self.0.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Sources whose window has passed are dropped, so the map tracks a current
        // brute force rather than every client that ever mistyped a token.
        failures.retain(|_, (started, _)| now.saturating_duration_since(*started) < AUTH_WINDOW);
        let entry = failures.entry(source).or_insert((now, 0));
        entry.1 = entry.1.saturating_add(1);
        entry.1
    }
}

/// The header a client names itself in on `/rpc`, so the gateway knows which client
/// started a turn without guessing from a socket (PAN-123).
pub(crate) const CLIENT_HEADER: &str = "x-pandacode-client";

/// The name a client gives itself, from `?client=` or [`CLIENT_HEADER`].
///
/// It is a routing identity, not a secret: it decides which addressed frames reach
/// a socket. Bounded and refused rather than repaired, because it travels on every
/// frame it addresses (PAN-123).
pub(crate) fn client_identity(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || value.len() > 64 {
        return Err("A client identity must be 1 to 64 characters.");
    }
    let ok = value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'));
    if !ok {
        return Err("A client identity may only use letters, digits, '-', '_', '.' and ':'.");
    }
    Ok(value.to_owned())
}

/// The connection a request came from, for the audit line and the auth limit.
///
/// Inserted into the request by `authorize`, and read by handlers through the
/// extractor below, so a handler does not have to know how the server was built. A
/// caller without connect info (a unit test driving the router directly) reads
/// `local`, which is honest rather than inventing an address.
#[derive(Clone, Debug)]
pub(crate) struct Peer(pub String);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<Peer>()
            .cloned()
            .unwrap_or_else(|| Peer("local".to_string())))
    }
}

/// The address a request came from, or `None` when this server was not built with
/// connect info.
fn peer_of(request: &Request) -> Option<String> {
    request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|connect| connect.0.to_string())
}

/// The source an address belongs to: failures are counted per host, not per
/// connection, because a client opens a new port for every request.
fn source_of(peer: &str) -> Option<IpAddr> {
    peer.parse::<std::net::SocketAddr>().ok().map(|address| address.ip())
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
    Peer(client): Peer,
    headers: HeaderMap,
    uri: Uri,
    body: Body,
) -> Response {
    let started = Instant::now();
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
        Ok(upload) => {
            // The upload is what the operator needs to find later: which file, how
            // big, and the digest to prove it is the one they think. The name the
            // client sent is display metadata and stays out of the journal.
            audit::Line::new("upload")
                .field("client", &client)
                .field("upload", &upload.meta.id)
                .field(
                    "kind",
                    match upload.meta.kind {
                        crate::uploads::Kind::Image => "image",
                        crate::uploads::Kind::Document => "document",
                    },
                )
                .field("bytes", upload.meta.bytes)
                .field("sha256", &upload.meta.sha256)
                .field("duration_ms", started.elapsed().as_millis())
                .emit();
            axum::Json(upload.meta).into_response()
        }
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
            "target" if !value.is_empty() => filter.target = Some(value.to_owned()),
            // This socket's own identity: what makes a client-addressed frame reach
            // it, without hiding the session frames it also needs (PAN-123).
            "client" if !value.is_empty() => {
                filter.client = Some(client_identity(value).map_err(|error| error.to_string())?)
            }
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
    // The connection this request arrived on: the audit line's `client` and the key
    // the auth-failure limit counts against. Read before anything can consume it.
    let peer = peer_of(&request).unwrap_or_else(|| "local".to_string());
    // CORS is outermost, so a real preflight never reaches this point: what this
    // middleware sees are the app's own requests. Guard by the canonical path,
    // because /rpc/ and //rpc are not what axum routes to the handler, and an
    // auth decision must not hang on how a client spelled the path.
    let path = canonical_path(request.uri().path());
    let mut request = request;
    request.extensions_mut().insert(Peer(peer.clone()));
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
        // A wrong token is the only thing this limit counts, so the token's owner is
        // never locked out of their own gateway, and the window rolls.
        let failures = match source_of(&peer) {
            Some(source) => state.auth_failures.record(source, Instant::now()),
            // Without connect info there is no source to count, and refusing on a
            // guess would be worse than allowing the attempt to be refused as wrong.
            None => 1,
        };
        let status = if failures > AUTH_FAILURES {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::UNAUTHORIZED
        };
        audit::Line::new("auth")
            .field("client", &peer)
            .field("path", &path)
            .field("failures", failures)
            .field("window_s", AUTH_WINDOW.as_secs())
            .field("status", status.as_u16())
            .emit();
        return (
            status,
            if status == StatusCode::TOO_MANY_REQUESTS {
                "too many failed attempts; wait a minute and try the token again"
            } else {
                "unauthorized"
            },
        )
            .into_response();
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
