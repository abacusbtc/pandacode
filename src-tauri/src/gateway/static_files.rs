//! The built SPA: a real file when it exists, `index.html` for everything else.
//!
//! Requests are matched literally on the raw URI path. Build assets are hashed
//! ASCII names, and any other path is a client route that the shell answers.

use std::path::{Path, PathBuf};

use axum::{
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
};

/// Extension to content type; anything else is served as bytes.
const TYPES: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("json", "application/json"),
    ("wasm", "application/wasm"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("woff2", "font/woff2"),
];

/// Serves `request_path` from `root`, falling back to the shell. `None` means
/// there is no build here at all, which the router turns into a 404.
pub async fn serve(root: &Path, request_path: &str) -> Option<Response> {
    let file = inside(root, request_path).or_else(|| inside(root, "index.html"))?;
    let bytes = tokio::fs::read(&file).await.ok()?;
    Some((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(&file)),
        )],
        bytes,
    )
        .into_response())
}

/// A regular file under `root`. `..`, absolute paths and symlinks that leave the
/// build directory never resolve, so the shell answers instead.
fn inside(root: &Path, request_path: &str) -> Option<PathBuf> {
    let relative = request_path.trim_start_matches('/');
    if relative.is_empty() || relative.split('/').any(|part| part == "..") {
        return None;
    }
    let candidate = root.join(relative);
    if !candidate.is_file() {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let candidate = candidate.canonicalize().ok()?;
    candidate.starts_with(&root).then_some(candidate)
}

fn content_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    TYPES
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(extension))
        .map(|(_, value)| *value)
        .unwrap_or("application/octet-stream")
}
