//! Router, bearer-token auth and the browser origin allow-list.

use std::time::Duration;

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::{dispatch, rpc, static_files};
use serde_json::json;
use std::path::PathBuf;

/// The only paths a token protects. The SPA's own files must load before a
/// client can present one, and `/events` joins this list in Task 6.
const GUARDED: &[&str] = &["/rpc", "/capabilities"];

/// What a request needs to be authorised and answered.
#[derive(Clone)]
pub(crate) struct RouterState {
    pub(crate) token: String,
    pub(crate) allowed_origins: Vec<String>,
    pub(crate) static_dir: Option<PathBuf>,
    pub(crate) ctx: dispatch::Ctx,
}

pub(crate) fn router(state: RouterState) -> Router {
    let cors = cors(&state.allowed_origins);
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route("/capabilities", axum::routing::get(capabilities))
        .route("/rpc", axum::routing::post(rpc::handle))
        .fallback(spa)
        // Layer order is load-bearing: the last layer is outermost, so CORS
        // answers the browser's token-less preflight and auth guards the rest.
        // Swapping these two breaks the app's client with an opaque browser error.
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .layer(cors)
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
async fn spa(State(state): State<RouterState>, uri: Uri) -> Response {
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

async fn authorize(State(state): State<RouterState>, request: Request, next: Next) -> Response {
    // Health, the SPA's files and a browser preflight are public; the API is not.
    if request.method() == Method::OPTIONS || !GUARDED.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if presented != Some(state.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(request).await
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
