# PandaCode Gateway + Remote App Client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make PandaCode's real backend reachable over HTTP/WebSocket from a browser, and let the desktop app on Windows/macOS/Linux connect to that backend instead of its own.

**Architecture:** A second binary (`pandacode-gateway`) boots the real `Store` against the same `kingcode.sqlite3` the desktop app uses, exposes commands as JSON-RPC over `POST /rpc` with a bearer token, streams events over `WS /events`, and serves the built SPA. The desktop app keeps its native shell but gains a `remote` transport mode in `src/bridge.ts`, so the same UI code runs against either backend. Command bodies are split into `*_inner(&AppState, …)` functions so Tauri and the gateway share them without a Tauri runtime.

**Tech Stack:** Rust 2021, `axum` 0.8 (new, optional dependency behind a `gateway` feature), `tokio`, `rusqlite`/`Store`, `serde_json`. Frontend: TypeScript, existing `src/bridge.ts` seam, Node `--test` unit tests, Playwright for browser tests.

**Spec:** the Design Decisions section below is the spec for this plan; it was derived from the code as it exists at `0.3.16`.

## Global Constraints

- Default desktop build must not change: `axum` is `optional = true`, the gateway binary declares `required-features = ["gateway"]`, and `cargo build` without the feature must produce byte-identical dependency sets.
- `cargo test --lib`, `npm test`, `npx tsc --noEmit` and `npx playwright test` (preview mode) stay green after every task.
- IPC arg names are camelCase on the wire (`docs/ipc.md`); RPC arg structs use `#[serde(rename_all = "camelCase")]`.
- The gateway binds `127.0.0.1` by default and **refuses to start** on any other address without an explicit token.
- No telemetry, no phone-home, no account check — same rule as the app.
- The gateway never writes a second database: it opens `kingcode.sqlite3` in the platform app-data dir (`~/.local/share/ai.pandacode.desktop` on Linux) unless `--data-dir` says otherwise, and holds an exclusive lock file.
- All user-facing strings are plain sentence case, matching existing UI copy.

## Review Focus

1. **Two processes on one SQLite file.** Starting the gateway while the desktop app is open on the same machine must fail loudly (lock file), not silently produce two writers.
2. **Event payload drift.** A WebSocket event whose JSON differs from the old `emit` payload makes the UI stop updating with no error. Every event emitter keeps its exact shape.
3. **Cross-origin fetch from the Tauri webview.** The app's UI origin (`http://tauri.localhost`, `tauri://localhost`) is not the gateway's origin; without an explicit allow-list the client mode fails with an opaque CORS error.
4. **Remote mode calling native-only commands.** Dialogs, Tart VMs, the simulator and the updater do not exist server-side; they must fail fast with `unsupported_command`, never hang.
5. **Large payloads over JSON-RPC.** Images and file previews as base64 can exceed memory for a 30 MB attachment; the RPC layer caps body size and returns 413 rather than buffering without limit.

---

## Design Decisions

**D1 — One crate, one optional feature, a second binary.** `src-tauri/Cargo.toml` gains `[features] gateway = ["dep:axum"]` and `[[bin]] name = "pandacode-gateway"`. The binary links the existing `pandacode_lib`, so it inherits the desktop build prerequisites (on Linux: `libwebkit2gtk-4.1-dev` et al.) but never initialises GTK. A workspace split into `core`/`desktop`/`gateway` crates is deliberately deferred: 79k lines of `AppHandle`-coupled code is not a prerequisite for M1, and the split would block this plan on a rewrite.

**D2 — Transport.** `POST /rpc` with `{ "id": string, "command": string, "args": object }` → `{ "id": string, "ok": true, "result": any }` or `{ "id": string, "ok": false, "error": string }`. `WS /events` carries `{ "event": string, "payload": any }`. `GET /healthz` → `ok`. `GET /capabilities` → `{ "commands": string[], "features": string[] }`. Errors are the same strings the Tauri commands already return, so UI error handling is unchanged.

**D3 — Events through a process-wide sink.** New `src-tauri/src/events.rs` defines `pub trait Emitter { fn emit(&self, name: &str, payload: serde_json::Value); fn emit_to(&self, target: &str, name: &str, payload: serde_json::Value); }`, implements it for `tauri::AppHandle` (delegating to the existing `emit`/`emit_to`), and holds `static SINK: OnceLock<Arc<dyn Emitter + Send + Sync>>`. Call sites change from `app.emit("agent-event", payload)` to `events::emit(&app, "agent-event", payload)`. Payload JSON is untouched, which is what keeps Review Focus 2 satisfied.

**D4 — Explicit dispatch subset.** `gateway::dispatch::handle(command, args, ctx)` matches on a string and calls `*_inner` functions. M1 implements: `add_project`, `list_projects`, `remove_project`, `list_sessions`, `create_session`, `get_messages`, `gateway_list_dirs`, plus `gateway_status`. Anything else returns `unsupported_command: <name>` and is absent from `/capabilities`, so the UI can hide it. The other ~159 commands keep their existing Tauri-only bodies.

**D5 — Client mode in the app.** `src/bridge.ts` gains `'remote'` as a third mode. Boot order: `entry.ts` → `main.ts` calls `gateway_config` (Tauri command) → if `{url, token}` is returned, `setMode('remote')` before the first `call`. Remote mode uses `src/gateway-client.ts`; native-only surfaces (VM, simulator, overlay launcher, updater UI) are hidden via `/capabilities`.

**D6 — Remote paths are server paths.** In remote mode `chooseFolder`/`chooseAttachments` route to `gateway_list_dirs` and render an in-app picker, because a local Windows file dialog produces a path that does not exist on the Linux server. This is the same defect the browser UI shows today.

**D7 — Token and storage.** Gateway: 32 random bytes, hex, created on first run at `<data-dir>/gateway-token` with mode `0600`, or supplied with `--token-file`. Client: URL in the existing settings row, token in the OS keychain via `secret_store::set("app.pandacode.gateway", url, token)`.

**D8 — Non-goals for this plan.** Live agent turns (`send_message`, streaming, approvals, queues), terminal/Pty over the socket, attachments, VM/simulator/browser-preview parity, multi-user auth, and TLS (terminated by the homelab reverse proxy). M2 = agent execution, planned separately once this lands.

---

## File Structure

**Create**
- `src-tauri/src/bin/pandacode-gateway.rs` — argv parsing, constructs `GatewayConfig`, calls `gateway::run`.
- `src-tauri/src/gateway/mod.rs` — `run`, `spawn`, `GatewayConfig`, `GatewayHandle`.
- `src-tauri/src/gateway/server.rs` — axum router, auth middleware, CORS allow-list, body limit.
- `src-tauri/src/gateway/rpc.rs` — request/response types, JSON error mapping.
- `src-tauri/src/gateway/dispatch.rs` — the command table from D4.
- `src-tauri/src/gateway/commands.rs` — `*_inner` bodies shared with Tauri.
- `src-tauri/src/gateway/events.rs` — broadcast sink + `/events` socket.
- `src-tauri/src/gateway/static_files.rs` — SPA serving with `index.html` fallback.
- `src-tauri/src/gateway/config.rs` — data dir defaults, token file, lock file.
- `src-tauri/tests/gateway_http.rs` — end-to-end HTTP tests against a temp database.
- `src/gateway-client.ts` — fetch + WebSocket transport with reconnect.
- `src/gateway-settings.ts` — connect/disconnect UI.
- `src/remote-files.ts` — remote folder picker.
- `tests/gateway-client.test.mjs` — Node tests against a stub server.
- `tests/gateway-e2e.spec.ts` — Playwright against the real gateway binary.
- `docs/gateway.md` — operator docs (start, token, reverse proxy, systemd).
- `scripts/gateway/pandacode-gateway.service` — systemd sample.

**Modify**
- `src-tauri/Cargo.toml` — feature, binary, optional `axum`.
- `src-tauri/src/lib.rs` — `pub mod gateway;`, `pub mod events;`, `*_inner` extraction for the D4 subset, `gateway_config`/`gateway_connect`/`gateway_disconnect` commands, `manage(AppState)` unchanged.
- `src-tauri/src/events_refactor` sites — the 29 files with `.emit(`/`.emit_to(` call sites.
- `src/bridge.ts` — mode selection and remote transport, `subscribe` over WS.
- `src/main.ts` — boot-time mode detection, capability gating, connect badge.
- `src/entry.ts` — unchanged; mode detection happens after `main` loads.
- `package.json` — `gateway:dev`, `gateway:test` scripts.
- `docs/ipc.md` — one paragraph pointing at `docs/gateway.md`.

---

## Task 1: Gateway feature, binary, health endpoint

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Create: `src-tauri/src/gateway/mod.rs`, `src-tauri/src/gateway/config.rs`, `src-tauri/src/bin/pandacode-gateway.rs`
- Test: `src-tauri/tests/gateway_http.rs`

**Interfaces:**
- Produces: `pandacode_lib::gateway::{GatewayConfig, spawn, GatewayHandle}` where `spawn(config: GatewayConfig) -> std::io::Result<GatewayHandle>`, `GatewayHandle { addr: SocketAddr, shutdown: tokio::sync::oneshot::Sender<()>, join: tokio::task::JoinHandle<()> }`, and `GatewayConfig { data_dir: PathBuf, bind: SocketAddr, token: String, static_dir: Option<PathBuf> }`.

- [ ] **Step 1: Write the failing test**

```rust
// src-tauri/tests/gateway_http.rs
use pandacode_lib::gateway::{spawn, GatewayConfig};

#[tokio::test]
async fn healthz_returns_ok() {
    let dir = tempfile::tempdir().unwrap();
    let config = GatewayConfig {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().unwrap(),
        token: "test-token".into(),
        static_dir: None,
    };
    let handle = spawn(config).await.unwrap();
    let url = format!("http://{}/healthz", handle.addr);
    let response = reqwest::get(url).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
    handle.stop().await;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features gateway --test gateway_http healthz_returns_ok`
Expected: FAIL — `no gateway in pandacode_lib`, `tempfile` unresolved.

- [ ] **Step 3: Implement the minimum**

```toml
# src-tauri/Cargo.toml
[features]
gateway = ["dep:axum", "dep:tempfile"]

[dependencies]
axum = { version = "0.8", optional = true }
tempfile = { version = "3", optional = true }

[[bin]]
name = "pandacode-gateway"
path = "src/bin/pandacode-gateway.rs"
required-features = ["gateway"]
```

```rust
// src-tauri/src/gateway/mod.rs
pub mod config;
use std::net::SocketAddr;
use std::path::PathBuf;

pub struct GatewayConfig {
    pub data_dir: PathBuf,
    pub bind: SocketAddr,
    pub token: String,
    pub static_dir: Option<PathBuf>,
}

pub struct GatewayHandle {
    pub addr: SocketAddr,
    pub shutdown: tokio::sync::oneshot::Sender<()>,
    pub join: tokio::task::JoinHandle<()>,
}

impl GatewayHandle {
    pub async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.join.await;
    }
}

pub async fn spawn(config: GatewayConfig) -> std::io::Result<GatewayHandle> {
    let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let addr = listener.local_addr()?;
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async { let _ = shutdown_rx.await; })
            .await;
    });
    Ok(GatewayHandle { addr, shutdown, join })
}
```

```rust
// src-tauri/src/bin/pandacode-gateway.rs
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let config = match pandacode_lib::gateway::config::from_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(error) => { eprintln!("{error}"); return std::process::ExitCode::FAILURE; }
    };
    match pandacode_lib::gateway::spawn(config).await {
        Ok(handle) => { println!("pandacode gateway on http://{}", handle.addr); let _ = tokio::signal::ctrl_c().await; handle.stop().await; std::process::ExitCode::SUCCESS }
        Err(error) => { eprintln!("cannot start gateway: {error}"); std::process::ExitCode::FAILURE }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --features gateway --test gateway_http healthz_returns_ok`
Expected: PASS. Also run `cargo build` (no feature) and confirm it compiles without `axum`.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/src/gateway src-tauri/src/bin/pandacode-gateway.rs src-tauri/tests/gateway_http.rs src-tauri/src/lib.rs
git commit -m "feat(gateway): add optional gateway binary with health endpoint"
```

---

## Task 2: Token auth, CORS allow-list, public-bind guard

**Files:**
- Create: `src-tauri/src/gateway/server.rs`
- Modify: `src-tauri/src/gateway/mod.rs`
- Test: `src-tauri/tests/gateway_http.rs`

**Interfaces:**
- Consumes: `GatewayConfig`, `spawn` from Task 1.
- Produces: `server::router(state: RouterState) -> axum::Router`; `RouterState { token: String, allowed_origins: Vec<String> }`; auth rejects with `401` and body `unauthorized`; `config::from_args` errors with `refusing to bind <addr> without a token` when `--host` is non-loopback and no token file is supplied.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn rpc_requires_bearer_token() {
    let handle = spawn(test_config()).await.unwrap();
    let client = reqwest::Client::new();
    let url = format!("http://{}/rpc", handle.addr);
    let denied = client.post(&url).json(&serde_json::json!({"id":"1","command":"list_projects","args":{}})).send().await.unwrap();
    assert_eq!(denied.status(), 401);
    let allowed = client.post(&url).header("Authorization", "Bearer test-token")
        .json(&serde_json::json!({"id":"2","command":"noop","args":{}})).send().await.unwrap();
    assert_ne!(allowed.status(), 401);
    handle.stop().await;
}

#[test]
fn public_bind_without_token_is_refused() {
    let error = pandacode_lib::gateway::config::validate("0.0.0.0:8443".parse().unwrap(), None, false).unwrap_err();
    assert!(error.contains("refusing to bind"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --features gateway --test gateway_http`
Expected: FAIL — `rpc` route missing, `config::validate` missing.

- [ ] **Step 3: Implement**

```rust
// src-tauri/src/gateway/server.rs
use axum::{extract::State, http::{HeaderMap, Method, StatusCode, header}, response::{IntoResponse, Response}, routing::post, Router};

pub struct RouterState { pub token: String, pub allowed_origins: Vec<String> }

pub fn router(state: RouterState) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .route("/rpc", post(super::rpc::handle))
        .layer(axum::middleware::from_fn_with_state(state.clone(), authorize))
        .layer(tower_http_cors(&state.allowed_origins))
        .with_state(state)
}

async fn authorize(State(state): State<RouterState>, headers: HeaderMap, method: Method, request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if request.uri().path() == "/healthz" { return next.run(request).await; }
    let presented = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    if presented != Some(state.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let _ = method;
    next.run(request).await
}
```

`tower_http_cors` is a small local helper that reflects an allowed `Origin` into `Access-Control-Allow-Origin` and answers `OPTIONS` with `204`; it reads the origin list passed on the CLI (`--allow-origin`), which defaults to `http://tauri.localhost`, `https://tauri.localhost`, `tauri://localhost`.

- [ ] **Step 4: Verify**

Run: `cargo test --features gateway --test gateway_http`
Expected: PASS (both tests). Run `cargo clippy --features gateway --all-targets -- -D warnings`; expect no warnings.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/gateway src-tauri/tests/gateway_http.rs
git commit -m "feat(gateway): require bearer token and pin allowed origins"
```

---

## Task 3: Real store boot and the first RPC commands

**Files:**
- Create: `src-tauri/src/gateway/commands.rs`, `src-tauri/src/gateway/rpc.rs`, `src-tauri/src/gateway/dispatch.rs`
- Modify: `src-tauri/src/lib.rs` (extract `*_inner` for `add_project`, `list_projects`, `remove_project`), `src-tauri/src/gateway/config.rs` (lock file)
- Test: `src-tauri/tests/gateway_http.rs`

**Interfaces:**
- Consumes: `spawn`, `RouterState`.
- Produces: `dispatch::handle(command: &str, args: serde_json::Value, ctx: &Ctx) -> Result<serde_json::Value, String>`; `Ctx { state: Arc<AppState>, data_dir: PathBuf, events: Arc<dyn Emitter> }`; RPC envelope `{id, command, args}` / `{id, ok, result|error}`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn add_and_list_projects_round_trip_through_the_real_database() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let handle = spawn(test_config_in(data.path())).await.unwrap();
    let client = rpc_client(&handle);
    let created = client.call("add_project", serde_json::json!({"path": project.path()})).await.unwrap();
    assert_eq!(created["path"].as_str().unwrap(), project.path().to_str().unwrap());
    let listed = client.call("list_projects", serde_json::json!({})).await.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert!(data.path().join("kingcode.sqlite3").exists());
    handle.stop().await;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features gateway --test gateway_http add_and_list`
Expected: FAIL — `unsupported_command: add_project` (dispatch stub returns it).

- [ ] **Step 3: Implement**

Extract the bodies in `lib.rs` so both transports share them:

```rust
// src-tauri/src/lib.rs
pub(crate) fn add_project_inner(state: &AppState, path: String) -> Result<Project> { /* body of the existing command */ }
#[tauri::command]
async fn add_project(state: State<'_, AppState>, path: String) -> Result<Project> { add_project_inner(&state, path) }
```

Open the store in the gateway exactly as `setup` does:

```rust
// src-tauri/src/gateway/config.rs
pub fn open_store(data_dir: &Path) -> crate::Result<crate::storage::Store> {
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    crate::storage::Store::open(&data_dir.join("kingcode.sqlite3"))
}
```

```rust
// src-tauri/src/gateway/dispatch.rs
pub fn handle(command: &str, args: serde_json::Value, ctx: &Ctx) -> Result<serde_json::Value, String> {
    use serde_json::json;
    match command {
        "add_project" => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args { path: String }
            let args: Args = serde_json::from_value(args).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(crate::add_project_inner(&ctx.state, args.path)?).map_err(|e| e.to_string())?)
        }
        "list_projects" => Ok(serde_json::to_value(crate::list_projects_inner(&ctx.state)?).map_err(|e| e.to_string())?),
        "remove_project" => {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Args { project_id: String }
            let args: Args = serde_json::from_value(args).map_err(|e| e.to_string())?;
            crate::remove_project_inner(&ctx.state, args.project_id)?;
            Ok(json!(null))
        }
        other => Err(format!("unsupported_command: {other}")),
    }
}
```

A lock file (`<data-dir>/gateway.lock`, `OpenOptions::new().create_new(true)`) is created at startup and removed on shutdown; a second start fails with `another gateway is already running on this data directory`.

> **Superseded by `src-tauri/src/data_lock.rs`:** a PID file cannot be made correct by patching its checks, so the lock is now a kernel `flock` on `<data-dir>/kingcode.lock`, shared with the desktop app, and the file is never removed.

- [ ] **Step 4: Verify**

Run: `cargo test --features gateway --test gateway_http` — all tests PASS, including the Task 1 and 2 tests.
Run: `cargo test --lib` — desktop command wrappers still pass.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/gateway src-tauri/tests/gateway_http.rs
git commit -m "feat(gateway): serve real projects from the shared database"
```

---

## Task 4: Sessions read path and capabilities

**Files:**
- Modify: `src-tauri/src/lib.rs` (`list_sessions_inner`, `create_session_inner`, `get_messages_inner`), `src-tauri/src/gateway/dispatch.rs`, `src-tauri/src/gateway/server.rs`
- Test: `src-tauri/tests/gateway_http.rs`

**Interfaces:**
- Produces: `/capabilities` returning `{"commands": ["add_project","list_projects",…], "features": ["projects","sessions"]}`; RPC commands `list_sessions {projectId?}`, `create_session {projectId, provider, model, mode?}`, `get_messages {sessionId, beforeId?}`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn capabilities_and_sessions_come_from_the_real_schema() {
    let (handle, data) = seeded_gateway().await; // seeds one project + one session via RPC
    let caps = rpc_client(&handle).call("gateway_status", serde_json::json!({})).await.unwrap();
    assert!(caps["commands"].as_array().unwrap().iter().any(|c| c == "create_session"));
    let sessions = rpc_client(&handle).call("list_sessions", serde_json::json!({})).await.unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    let messages = rpc_client(&handle).call("get_messages", serde_json::json!({"sessionId": sessions[0]["id"]})).await.unwrap();
    assert!(messages["messages"].as_array().unwrap().is_empty());
    let _ = data;
    handle.stop().await;
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --features gateway --test gateway_http capabilities_and_sessions`
Expected: FAIL — `unsupported_command: create_session`.

- [ ] **Step 3: Implement**

Add the three `*_inner` extractions mirroring the existing command bodies (they already only take `&AppState` once the `State` wrapper is removed), then three dispatch arms deserialising camelCase args (`projectId`, `provider`, `model`, `mode`, `sessionId`, `beforeId`). `gateway_status` returns the static capability list from D4.

- [ ] **Step 4: Verify**

Run: `cargo test --features gateway --test gateway_http`
Expected: PASS. Confirm `npx tsc --noEmit` still passes (frontend untouched in this task).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/gateway src-tauri/tests/gateway_http.rs
git commit -m "feat(gateway): expose sessions and capabilities over rpc"
```

---

## Task 5: Serve the built SPA

**Files:**
- Create: `src-tauri/src/gateway/static_files.rs`
- Modify: `src-tauri/src/gateway/server.rs`
- Test: `src-tauri/tests/gateway_http.rs`

**Interfaces:**
- Produces: `static_files::serve(root: PathBuf, path: &str) -> Option<Response>`; unknown paths resolve to `index.html`; extension→content-type map covers `html js css wasm svg png woff2 json`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn serves_index_for_unknown_routes_and_types_assets() {
    let dist = tempfile::tempdir().unwrap();
    std::fs::write(dist.path().join("index.html"), "<html>pandacode</html>").unwrap();
    std::fs::write(dist.path().join("app.js"), "console.log(1)").unwrap();
    let handle = spawn(GatewayConfig { static_dir: Some(dist.path().to_path_buf()), ..test_config() }).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let deep = reqwest::get(format!("{base}/threads/42")).await.unwrap();
    assert_eq!(deep.status(), 200);
    assert!(deep.text().await.unwrap().contains("pandacode"));
    let js = reqwest::get(format!("{base}/app.js")).await.unwrap();
    assert_eq!(js.headers()["content-type"], "text/javascript; charset=utf-8");
    handle.stop().await;
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --features gateway --test gateway_http serves_index`
Expected: FAIL — 404 from the router.

- [ ] **Step 3: Implement**

`Router::fallback` calls `static_files::serve`, which rejects `..` segments, canonicalises against the root, and falls back to `index.html`. Statics are served without auth (the SPA needs to load before it can present a token); `/rpc` and `/events` remain token-gated.

- [ ] **Step 4: Verify**

Run: `cargo test --features gateway --test gateway_http`
Expected: PASS. Manual check: `npm run build && cargo run --features gateway --bin pandacode-gateway -- --static-dir dist --port 4174`, then `curl -s localhost:4174/ | head -3` shows the real `index.html`.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/gateway src-tauri/tests/gateway_http.rs
git commit -m "feat(gateway): serve the built spa with index fallback"
```

---

## Task 6: Event sink refactor and the events socket

**Files:**
- Create: `src-tauri/src/events.rs`, `src-tauri/src/gateway/events.rs`
- Modify: the 29 files with `emit`/`emit_to` call sites, `src-tauri/src/lib.rs` (`pub mod events;`, sink installation in `run()`), `src-tauri/src/gateway/server.rs`
- Test: `src-tauri/tests/gateway_http.rs`

**Interfaces:**
- Produces: `events::set_sink(Arc<dyn Emitter + Send + Sync>)`, `events::emit<T: serde::Serialize>(app: &tauri::AppHandle, name: &str, payload: T)`, `events::emit_to<T: Serialize>(app: &tauri::AppHandle, target: &str, name: &str, payload: T)`; WS `/events` frames `{"event": string, "payload": any}`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn websocket_clients_receive_emitted_events() {
    let handle = spawn(test_config()).await.unwrap();
    let url = format!("ws://{}/events?token=test-token", handle.addr);
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    handle.emit("agent-event", serde_json::json!({"sessionId": "s1", "kind": "text", "text": "hi"})).await;
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next()).await.unwrap().unwrap().unwrap();
    let frame: serde_json::Value = serde_json::from_str(&message.to_text().unwrap()).unwrap();
    assert_eq!(frame["event"], "agent-event");
    assert_eq!(frame["payload"]["sessionId"], "s1");
    handle.stop().await;
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --features gateway --test gateway_http websocket_clients`
Expected: FAIL — no `/events` route, no `GatewayHandle::emit`.

- [ ] **Step 3: Implement the trait, then the mechanical call-site change**

```rust
// src-tauri/src/events.rs
use std::sync::Arc;
use std::sync::OnceLock;

pub trait Emitter: Send + Sync {
    fn emit(&self, name: &str, payload: serde_json::Value);
    fn emit_to(&self, target: &str, name: &str, payload: serde_json::Value);
}

impl Emitter for tauri::AppHandle {
    fn emit(&self, name: &str, payload: serde_json::Value) { let _ = tauri::Emitter::emit(self, name, payload); }
    fn emit_to(&self, target: &str, name: &str, payload: serde_json::Value) {
        if let Some(window) = self.get_webview_window(target) { let _ = tauri::Emitter::emit(&window, name, payload); }
    }
}

static SINK: OnceLock<Arc<dyn Emitter>> = OnceLock::new();
pub fn set_sink(sink: Arc<dyn Emitter>) { let _ = SINK.set(sink); }

pub fn emit<T: serde::Serialize>(app: &tauri::AppHandle, name: &str, payload: T) {
    let value = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    match SINK.get() { Some(sink) => sink.emit(name, value), None => { let _ = tauri::Emitter::emit(app, name, value); } }
}
```

Replace every call site mechanically, keeping payloads byte-identical:

```bash
# 29 files; review the diff for emit_to sites by hand
perl -pi -e 's/\bapp\.emit\(/crate::events::emit(&app, /g; s/\bapp\.emit_to\(/crate::events::emit_to(&app, /g' \
  $(grep -rl '\.emit(' src-tauri/src --include='*.rs')
grep -rn '\.emit(\|\.emit_to(' src-tauri/src --include='*.rs'   # expect only events.rs hits
```

Gateway side: a `tokio::sync::broadcast::Sender<serde_json::Value>` in `Ctx`, an `Emitter` impl that serialises `{"event","payload"}` and sends it, a `/events` handler that subscribes per socket and forwards, and `GatewayHandle::emit` for tests.

- [ ] **Step 4: Verify**

Run: `cargo test --features gateway --test gateway_http` and `cargo test --lib` and `npx playwright test -g "streaming"` (desktop preview still shows streamed text).
Expected: all PASS. This is the task where a wrong payload shape shows up as a dead UI, so run one real desktop turn via `npx tauri dev` and confirm messages still appear.

- [ ] **Step 5: Commit**

```bash
git add -A src-tauri/src
git commit -m "refactor(events): route emits through a process-wide sink"
```

---

## Task 7: Remote directory listing

**Files:**
- Create: `src-tauri/src/gateway/commands.rs` (append `list_dirs`)
- Modify: `src-tauri/src/gateway/dispatch.rs`
- Test: `src-tauri/tests/gateway_http.rs`

**Interfaces:**
- Produces: `gateway_list_dirs {path?: string|null, showHidden?: bool}` → `{path: string, parent: string|null, entries: [{name: string, isDir: boolean}]}`; non-absolute or non-existent paths return `not_a_directory: <path>`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn lists_directories_on_the_server_filesystem() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("alpha")).unwrap();
    std::fs::write(root.path().join("file.txt"), "x").unwrap();
    let handle = spawn(test_config()).await.unwrap();
    let listing = rpc_client(&handle).call("gateway_list_dirs", serde_json::json!({"path": root.path()})).await.unwrap();
    let names: Vec<&str> = listing["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["alpha"]);
    handle.stop().await;
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --features gateway --test gateway_http lists_directories`
Expected: FAIL — `unsupported_command: gateway_list_dirs`.

- [ ] **Step 3: Implement**

```rust
pub fn list_dirs(path: Option<String>, show_hidden: bool) -> Result<serde_json::Value, String> {
    let resolved = match path { Some(ref p) if p.starts_with('/') => std::path::PathBuf::from(p), _ => std::env::var("HOME").map(std::path::PathBuf::from).map_err(|e| e.to_string())? };
    if !resolved.is_dir() { return Err(format!("not_a_directory: {}", resolved.display())); }
    let mut entries: Vec<serde_json::Value> = std::fs::read_dir(&resolved).map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .filter(|e| show_hidden || !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| { let is_dir = e.file_type().ok()?.is_dir(); if !is_dir { return None; } Some(serde_json::json!({"name": e.file_name().to_string_lossy(), "isDir": true})) })
        .collect();
    entries.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
    Ok(serde_json::json!({"path": resolved.display().to_string(), "parent": resolved.parent().map(|p| p.display().to_string()), "entries": entries}))
}
```

- [ ] **Step 4: Verify**

Run: `cargo test --features gateway --test gateway_http`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/gateway src-tauri/tests/gateway_http.rs
git commit -m "feat(gateway): list server directories for remote project picking"
```

---

## Task 8: Client transport in the frontend bridge

**Files:**
- Create: `src/gateway-client.ts`, `tests/gateway-client.test.mjs`
- Modify: `src/bridge.ts`, `src-tauri/src/lib.rs` (`gateway_config`, `gateway_connect`, `gateway_disconnect`)
- Test: `tests/gateway-client.test.mjs`

**Interfaces:**
- Produces: `createGatewayClient({url, token, fetchImpl, WebSocketImpl})` → `{call<T>(command, args): Promise<T>, subscribe(name, cb): Promise<() => void>, close(): void}`; `bridge.setMode('remote' | 'native' | 'preview')`; Tauri command `gateway_config() -> Option<{url: string, token: string}>` reading settings + keychain.

- [ ] **Step 1: Write the failing test**

```javascript
// tests/gateway-client.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { createGatewayClient } from '../src/gateway-client.ts';

test('call posts the envelope and unwraps the result', async () => {
  const seen = [];
  const fetchImpl = async (url, init) => {
    seen.push({ url, body: JSON.parse(init.body), auth: init.headers.Authorization });
    return { ok: true, json: async () => ({ id: '1', ok: true, result: [{ id: 'p1', name: 'repo' }] }) };
  };
  const client = createGatewayClient({ url: 'http://gw.test', token: 't', fetchImpl, WebSocketImpl: class {} });
  const projects = await client.call('list_projects', {});
  assert.equal(projects[0].name, 'repo');
  assert.equal(seen[0].body.command, 'list_projects');
  assert.equal(seen[0].auth, 'Bearer t');
});

test('a failed envelope rejects with the server error string', async () => {
  const fetchImpl = async () => ({ ok: true, json: async () => ({ id: '1', ok: false, error: 'unsupported_command: vm_start' }) });
  const client = createGatewayClient({ url: 'http://gw.test', token: 't', fetchImpl, WebSocketImpl: class {} });
  await assert.rejects(() => client.call('vm_start', {}), /unsupported_command: vm_start/);
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npm test -- --test-name-pattern "gateway"` (or `node --test tests/gateway-client.test.mjs`)
Expected: FAIL — cannot resolve `src/gateway-client.ts` exports.

- [ ] **Step 3: Implement the client and wire the bridge**

```typescript
// src/gateway-client.ts
export type GatewayOptions = { url: string; token: string; fetchImpl?: typeof fetch; WebSocketImpl?: typeof WebSocket };
export function createGatewayClient(options: GatewayOptions) {
  const { url, token } = options;
  const doFetch = options.fetchImpl ?? fetch;
  const listeners = new Map<string, Set<(payload: unknown) => void>>();
  let socket: WebSocket | null = null;
  let closed = false;
  let seq = 0;

  const connect = () => {
    const Impl = options.WebSocketImpl ?? WebSocket;
    socket = new Impl(`${url.replace(/^http/, 'ws')}/events?token=${encodeURIComponent(token)}`);
    socket.onmessage = event => {
      const frame = JSON.parse(String(event.data));
      for (const listener of listeners.get(frame.event) ?? []) listener(frame.payload);
    };
    socket.onclose = () => { if (!closed) setTimeout(connect, 1000); };
  };

  return {
    async call<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
      const response = await doFetch(`${url}/rpc`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
        body: JSON.stringify({ id: String(++seq), command, args }),
      });
      if (response.status === 401) throw 'unauthorized: the gateway rejected the token';
      const body = await response.json();
      if (!body.ok) throw body.error;
      return body.result as T;
    },
    async subscribe(name: string, callback: (payload: unknown) => void) {
      if (!listeners.has(name)) listeners.set(name, new Set());
      listeners.get(name)!.add(callback);
      if (!socket) connect();
      return () => listeners.get(name)?.delete(callback);
    },
    close() { closed = true; socket?.close(); },
  };
}
```

`src/bridge.ts` adds:

```typescript
type Mode = 'native' | 'preview' | 'remote';
let mode: Mode = preview ? 'preview' : 'native';
let remote: ReturnType<typeof import('./gateway-client').createGatewayClient> | null = null;
export function setMode(next: Mode, client?: typeof remote) { mode = next; remote = client ?? null; }
export function currentMode() { return mode; }

export async function call<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  if (mode === 'remote') return remote!.call<T>(command, args);
  if (mode === 'preview') return (await import('./preview')).invoke<T>(command, args);
  return (await import('@tauri-apps/api/core')).invoke<T>(command, args);
}
```

`subscribe` gains the same three-way branch. `chooseFolder` returns `null` in remote mode and callers fall through to `remote-files.ts` (Task 10).

- [ ] **Step 4: Verify**

Run: `node --test tests/gateway-client.test.mjs` → PASS.
Run: `npx tsc --noEmit` → PASS.
Run: `npx playwright test -g "new task"` → PASS (preview mode untouched).

- [ ] **Step 5: Commit**

```bash
git add src/gateway-client.ts src/bridge.ts tests/gateway-client.test.mjs src-tauri/src/lib.rs
git commit -m "feat(bridge): add remote gateway transport to the app"
```

---

## Task 9: Connect UI, boot wiring, capability gating

**Files:**
- Create: `src/gateway-settings.ts`
- Modify: `src/main.ts` (boot sequence, header badge, gating), `src/styles.css` (badge styles), `src-tauri/src/lib.rs`
- Test: `tests/gateway.spec.ts`

**Interfaces:**
- Consumes: `createGatewayClient`, `setMode`, `gateway_config`, `gateway_connect`, `gateway_disconnect`.
- Produces: `gatewayStatus(): {url: string, mode: Mode} | null` exported from `gateway-settings.ts`; header badge `#gateway-badge` with text `Connected to <host>`; `applyCapabilities(commands: string[])` that disables VM, simulator, browser-preview and overlay entries when absent.

- [ ] **Step 1: Write the failing test**

```typescript
// tests/gateway.spec.ts
import { test, expect } from '@playwright/test';
import { createServer } from 'node:http';

test('connects to a gateway and hides unsupported surfaces', async ({ page }) => {
  const server = createServer((request, response) => {
    if (request.url?.startsWith('/rpc')) {
      response.setHeader('Content-Type', 'application/json');
      response.end(JSON.stringify({ id: '1', ok: true, result: JSON.stringify({ commands: ['list_projects'], features: [] }) }));
      return;
    }
    response.statusCode = 404; response.end('nope');
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = (server.address() as { port: number }).port;
  await page.goto('/');
  await page.evaluate((url) => (window as any).__connectGatewayForTest(url), `http://127.0.0.1:${port}`);
  await expect(page.locator('#gateway-badge')).toContainText('Connected to');
  await expect(page.locator('#vm-button')).toHaveCount(0);
  server.close();
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx playwright test tests/gateway.spec.ts`
Expected: FAIL — `__connectGatewayForTest` undefined, no `#gateway-badge`.

- [ ] **Step 3: Implement**

- `gateway-settings.ts` renders a Settings row (URL field, token field, Test, Connect, Disconnect) and exports `connect(url, token)` which calls `gateway_connect` (persists URL in settings plus token in the keychain), creates the client, calls `gateway_status`, then `setMode('remote', client)` and reloads the workspace.
- `main.ts` boot: `const config = await call('gateway_config'); if (config) { setMode('remote', createGatewayClient(config)); }` before the first session fetch; expose `window.__connectGatewayForTest` only when `preview` is true, so the Playwright suite can drive it without a native shell.
- `applyCapabilities` hides `#vm-button`, `#simulator-button`, `#browser-preview-button` and the launcher entry when the command list lacks `vm_start`, `simulator_start`, `browser_open`, `overlay_toggle`.

- [ ] **Step 4: Verify**

Run: `npx playwright test tests/gateway.spec.ts` → PASS.
Run: `npx playwright test` → PASS (existing suites unaffected).

- [ ] **Step 5: Commit**

```bash
git add src/gateway-settings.ts src/main.ts src/styles.css tests/gateway.spec.ts src-tauri/src/lib.rs
git commit -m "feat(app): connect the desktop app to a remote gateway"
```

---

## Task 10: Remote folder picker

**Files:**
- Create: `src/remote-files.ts`
- Modify: `src/main.ts` (open-project and attach flows), `src/attachments.ts` (remote branch)
- Test: `tests/gateway.spec.ts`

**Interfaces:**
- Consumes: `call('gateway_list_dirs', { path, showHidden })`, `gateway_status` (command list).
- Produces: `pickRemoteFolder(): Promise<string | null>` rendering a dialog with breadcrumb path, directory list, Up, and Use this folder.

- [ ] **Step 1: Write the failing test**

```typescript
test('browses server folders in remote mode', async ({ page }) => {
  await page.evaluate(() => (window as any).__stubGateway([
    { match: 'gateway_list_dirs', reply: { path: '/srv/repos', parent: '/srv', entries: [{ name: 'pandacode', isDir: true }] } },
  ]));
  await page.click('#open-project');
  await expect(page.locator('.remote-files .entry')).toContainText('pandacode');
  await page.click('.remote-files .use-folder');
  await expect(page.locator('#project-title')).toContainText('pandacode');
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx playwright test tests/gateway.spec.ts -g "server folders"`
Expected: FAIL — `#open-project` opens the native dialog path and `.remote-files` never appears.

- [ ] **Step 3: Implement**

`main.ts` branches: `if (currentMode() === 'remote') { const path = await pickRemoteFolder(); … } else { const path = await chooseFolder(); … }`. The dialog lists directories only, keeps a `path` state, and disables "Use this folder" while loading. Keyboard: Enter opens the highlighted entry, Backspace goes up, Esc closes.

- [ ] **Step 4: Verify**

Run: `npx playwright test tests/gateway.spec.ts` → PASS. Also verify the empty-folder case shows `No folders here` and the error case shows the server string, not a stack trace.

- [ ] **Step 5: Commit**

```bash
git add src/remote-files.ts src/main.ts src/attachments.ts tests/gateway.spec.ts
git commit -m "feat(app): pick server-side folders when connected to a gateway"
```

---

## Task 11: End-to-end proof, docs, service file

**Files:**
- Create: `docs/gateway.md`, `scripts/gateway/pandacode-gateway.service`, `tests/gateway-e2e.spec.ts`
- Modify: `package.json`, `docs/ipc.md`

**Interfaces:**
- Produces: `npm run gateway:dev` = `cargo run --features gateway --bin pandacode-gateway -- --static-dir dist --port 4175`; the e2e suite asserting a seeded project from a real database appears in a browser with no fixtures.

- [ ] **Step 1: Write the failing test**

```typescript
// tests/gateway-e2e.spec.ts
import { test, expect } from '@playwright/test';
import { execFileSync, spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

test('a seeded project from the real database is visible, fixtures are not', async ({ page }) => {
  const data = mkdtempSync(join(tmpdir(), 'pc-gw-'));
  const project = mkdtempSync(join(tmpdir(), 'pc-repo-'));
  const child = spawn('cargo', ['run', '--features', 'gateway', '--bin', 'pandacode-gateway', '--',
    '--data-dir', data, '--static-dir', 'dist', '--port', '4175', '--token-file', join(data, 'token')], { cwd: process.cwd() });
  await page.waitForTimeout(5000); // gateway boot; replaced by a healthz poll below
  const token = execFileSync('cat', [join(data, 'token')]).toString().trim();
  await page.goto('http://127.0.0.1:4175/');
  await page.evaluate(([url, t]) => (window as any).__connectGatewayForTest(url, t), ['http://127.0.0.1:4175', token]);
  await page.evaluate((p) => (window as any).__addProjectForTest(p), project);
  await expect(page.locator('#project-list')).toContainText(project.split('/').pop()!);
  await expect(page.locator('body')).not.toContainText('orbit');
  child.kill();
});
```

Replace the fixed `waitForTimeout` with a `healthz` poll loop before committing — a flaky wait is not acceptable.

- [ ] **Step 2: Run to verify it fails**

Run: `npx playwright test tests/gateway-e2e.spec.ts`
Expected: FAIL — no `__addProjectForTest`, or the seed never appears.

- [ ] **Step 3: Implement docs and service**

`docs/gateway.md` documents: start command, `--data-dir` semantics (same `kingcode.sqlite3` as the desktop app), token file, reverse-proxy expectations (WebSocket upgrade, `client_max_body_size`, TLS at the proxy), the capability list, and the "one writer" rule. `scripts/gateway/pandacode-gateway.service` is a systemd unit with `ExecStart=/usr/local/bin/pandacode-gateway --data-dir /var/lib/pandacode --static-dir /srv/pandacode/dist --port 47399`, `Restart=always`, `DynamicUser=yes`, `ProtectSystem=strict`, `ReadWritePaths=/var/lib/pandacode`. `package.json` scripts add `gateway:dev` and `gateway:test`. `docs/ipc.md` gets one paragraph: the gateway mirrors the same command names; the supported subset is what `/capabilities` reports.

- [ ] **Step 4: Verify**

Run: `npx playwright test tests/gateway-e2e.spec.ts` → PASS.
Run: `npm test`, `npx tsc --noEmit`, `cargo test --lib`, `cargo clippy --features gateway --all-targets -- -D warnings` → all PASS.

- [ ] **Step 5: Commit**

```bash
git add docs/gateway.md docs/ipc.md scripts/gateway tests/gateway-e2e.spec.ts package.json
git commit -m "docs(gateway): document running the gateway and prove real data end to end"
```

---

## Task 12: Operator wiring on the homelab (out of repo)

**Files:** none in this repository; records what the operator does after Task 11.

- [ ] **Step 1:** `npm run build` in `/home/abacus/agents/pandacode`, then install the unit:

```bash
sudo install -m 0755 target/release/pandacode-gateway /usr/local/bin/pandacode-gateway
# Do NOT create /var/lib/pandacode: the unit's StateDirectory=pandacode creates and owns it
# for the DynamicUser (a pre-created root- or abacus-owned directory breaks the lock/database).
sudo cp scripts/gateway/pandacode-gateway.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now pandacode-gateway
```

- [ ] **Step 2:** Verify locally (the state directory is 0700 and owned by the ephemeral user, so the token needs sudo): `curl -s -H "Authorization: Bearer $(sudo cat /var/lib/pandacode/gateway-token)" http://127.0.0.1:47399/capabilities | jq .commands` lists the M1 commands.

- [ ] **Step 3:** Point the existing Nginx Proxy Manager host at it: Forward Hostname `pandacode-gateway`, port `47399`, Websockets on, `client_max_body_size 64m`, certificate `*.lan`. No new certificate, no new DNS record.

- [ ] **Step 4:** From Windows, open `https://panda.lan`, paste the token, and confirm the project list is the real one from `/var/lib/pandacode/kingcode.sqlite3`.

- [ ] **Step 5:** No commit — this is environment state.

---

## Plan self-check

- **Coverage:** D1→Tasks 1,2; D2→Tasks 3,4,5,6,7; D3→Task 6; D4→Tasks 3,4; D5→Tasks 8,9; D6→Task 10; D7→Tasks 2,9; D8 stated as non-goals. Operator path→Task 12.
- **Placeholders:** none; every code step carries the code, every test step carries the assertion.
- **Type consistency:** `GatewayConfig`, `GatewayHandle`, `Emitter`, `Ctx`, `createGatewayClient`, `setMode`, `currentMode`, `pickRemoteFolder`, `gateway_list_dirs` are used with the same signatures in every task that touches them.
- **Review Focus mapping:** (1) lock file → Task 3; (2) payload shape → Task 6 verification; (3) CORS → Task 2 allow-list, exercised by Task 8's client tests; (4) unsupported commands → Task 3's `unsupported_command` and Task 9's gating; (5) payload size → Task 2 body limit, asserted in Task 11's proxy config.
