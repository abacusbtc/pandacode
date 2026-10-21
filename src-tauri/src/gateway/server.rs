//! Router, bearer-token auth and the browser origin allow-list.

use std::time::Duration;

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::{dispatch, rpc};
use serde_json::json;

/// What a request needs to be authorised and answered.
#[derive(Clone)]
pub(crate) struct RouterState {
    pub(crate) token: String,
    pub(crate) allowed_origins: Vec<String>,
    pub(crate) ctx: dispatch::Ctx,
}

pub(crate) fn router(state: RouterState) -> Router {
    let cors = cors(&state.allowed_origins);
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route("/capabilities", axum::routing::get(capabilities))
        .route("/rpc", axum::routing::post(rpc::handle))
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

async fn authorize(State(state): State<RouterState>, request: Request, next: Next) -> Response {
    // Health is public, and a preflight carries no Authorization header by design.
    if request.method() == Method::OPTIONS || request.uri().path() == "/healthz" {
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
