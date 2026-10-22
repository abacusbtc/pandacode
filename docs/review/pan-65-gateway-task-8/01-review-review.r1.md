# Independent Code Review: PAN-65 Gateway Task 8

**Reviewer:** `review.r1` (Independent Reviewer)  
**Target:** PAN-65 'gateway task 8: client transport in the frontend bridge (remote mode)'  
**Branch:** `abacusaus/pan-65-gateway-task-8-client-transport-in-the-frontend-bridge`  
**Commits:** `853332a` and `02e03f0` (Base: `d9e9d26`)  
**Date:** 2026-10-22  
**Status:** In Review (Changes Requested)

---

## 1. Executive Summary

Task 8 implements the frontend client transport and bridge seam for connecting the PandaCode app to a remote gateway, along with the native Tauri commands to persist connection credentials in the OS keychain via `secret_store`.

- **Acceptance Verification:**
  - Node tests (`tests/gateway-client.test.mjs`): **PASS** (9/9 pass).
  - TypeScript typecheck (`npx tsc --noEmit`): **PASS** (zero diagnostics).
  - Existing regression test suite (`npm test`): **PASS** (all 113 tests green).
  - Dependency check: **PASS** (no new npm dependencies added).
  - Rust tests (`cargo test gateway_client`): **PASS** with `--test-threads=1`, but **FLAKY/FAILING** under default multi-threaded test execution.

Four actionable defects were identified across the transport layer, bridge lifecycle, and native test synchronization.

---

## 2. Findings & Defects

### Finding 1: Flaky Rust tests due to concurrent access to shared `secret_store`
- **File & Lines:** `src-tauri/src/gateway_client.rs:90-145`
- **Severity:** **HIGH** (CI reliability / test suite integrity)
- **Problem:**
  Running `cargo test gateway_client` intermittently fails when tests execute in parallel (the default in Cargo). We observed test failures across multiple runs:
  ```
  failures:
      gateway_client::tests::a_connection_without_a_token_is_not_config
      gateway_client::tests::switching_gateways_drops_the_previous_token
  ```
- **Root Cause:**
  `secret_store` under `#[cfg(test)]` (`src-tauri/src/secret_store.rs:4-8`) uses a single in-memory `OnceLock<Mutex<HashMap<String, String>>>`. All unit tests in `gateway_client.rs` concurrently mutate and clear the exact same keys (`SERVICE = "app.pandacode.gateway"`, `SETTINGS = "settings"`, and `"http://gw.test"`). When tests run simultaneously on multiple threads, `disconnect()` in one test wipes the settings of another test mid-execution.
- **Empirical Proof:**
  - `cargo test gateway_client` failed in Task-55 and on iteration 3 of 5.
  - `cargo test gateway_client -- --test-threads=1` passes 100% reliably.
- **Recommendation:**
  Add a test mutex inside `src-tauri/src/gateway_client.rs`:
  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

      #[test]
      fn connect_then_config_round_trips_through_the_secure_store() {
          let _guard = TEST_LOCK.lock().unwrap();
          disconnect();
          ...
  ```

---

### Finding 2: WebSocket connection multiplication / leak during pending reconnect
- **File & Lines:** `src/gateway-client.ts:44-67, 97`
- **Severity:** **HIGH** (Resource leak / orphaned background WebSockets)
- **Problem:**
  When a connected socket closes, `socket.onclose` sets `socket = null` and schedules a reconnect via `reconnect = setTimeout(..., delay)`. If any caller invokes `client.subscribe()` while `reconnect` is pending (e.g., during the 1000ms backoff window):
  ```typescript
  if (!socket && !closed) connect();
  ```
  sees `socket === null` and immediately opens a new socket. Crucially, `connect()` **does not cancel** the pending `reconnect` timer. When the timer fires, it calls `connect()` again, opening a second socket and overwriting `socket`. The earlier socket is orphaned and never closed by `client.close()`.
- **Empirical Proof:**
  Tested with a mock socket in Node:
  ```
  instances after 1st subscribe: 1
  instances after 2nd subscribe: 2
  instances after timer fires: 3
  ```
- **Recommendation:**
  Cancel any pending timer in `connect()`:
  ```typescript
  const connect = () => {
    if (reconnect) {
      clearTimeout(reconnect);
      reconnect = null;
    }
    const Impl = options.WebSocketImpl ?? (WebSocket as unknown as GatewaySocketImpl);
    ...
  ```

---

### Finding 3: Unsubscribing listeners leaves empty Sets in Map, causing indefinite reconnects with 0 active listeners
- **File & Lines:** `src/gateway-client.ts:58-66, 94-98`
- **Severity:** **MEDIUM** (Unnecessary background network traffic)
- **Problem:**
  `subscribe` returns an unsubscribe callback:
  ```typescript
  return () => listeners.get(name)?.delete(callback);
  ```
  This removes the callback from the `Set`, but never deletes the key from the `listeners` `Map`.
  In `socket.onclose`, the reconnect guard is:
  ```typescript
  if (closed || listeners.size === 0) return;
  ```
  Because `listeners` is a `Map`, `listeners.size` reflects the number of event names ever subscribed to, not active listeners. If all callbacks unsubscribe, `listeners.size` remains > 0. When the socket drops, it repeatedly reconnects to the gateway despite zero active listeners.
- **Empirical Proof:**
  Tested in Node:
  ```
  instances initially: 1
  [off() invoked, socket closed]
  instances after socket closed with 0 active listeners: 2
  ```
- **Recommendation:**
  Prune empty Sets from the Map:
  ```typescript
  return () => {
    const set = listeners.get(name);
    if (set) {
      set.delete(callback);
      if (set.size === 0) listeners.delete(name);
    }
  };
  ```

---

### Finding 4: `setMode` does not close previous `GatewayClient`
- **File & Lines:** `src/bridge.ts:12-15`
- **Severity:** **MEDIUM** (Resource leak on mode switch)
- **Problem:**
  ```typescript
  export function setMode(next: Mode, client?: GatewayClient | null): void {
    mode = next;
    remote = client ?? null;
  }
  ```
  When switching from `remote` to `native` or `preview`, or replacing a client with a different gateway connection, `remote.close()` is never called. The discarded client's WebSocket and reconnect timer continue running in the background.
- **Recommendation:**
  Close the previous client when changing instances:
  ```typescript
  export function setMode(next: Mode, client?: GatewayClient | null): void {
    if (remote && remote !== client) remote.close();
    mode = next;
    remote = client ?? null;
  }
  ```

---

### Finding 5: `subscribe` in `src/bridge.ts` checks global `preview` instead of `mode === 'preview'`
- **File & Lines:** `src/bridge.ts:55`
- **Severity:** **LOW** (Pattern divergence)
- **Problem:**
  In `call`:
  ```typescript
  if (mode === 'preview') return (await import('./preview')).invoke<T>(command, args);
  ```
  In `subscribe`:
  ```typescript
  if (preview) return (await import('./preview')).listen(name, callback);
  ```
  Checking the module-level boolean constant `preview` rather than `mode === 'preview'` creates an inconsistency with `call` if `mode` is explicitly manipulated.
- **Recommendation:**
  Use `if (mode === 'preview')` for consistency.

---

### Finding 6: Premature token deletion in `connect` before save succeeds
- **File & Lines:** `src-tauri/src/gateway_client.rs:72-78`
- **Severity:** **LOW** (Robustness / failure recovery)
- **Problem:**
  In `connect(url, token)`:
  ```rust
  if let Some(previous) = settings().url {
      if previous != url {
          secret_store::remove(SERVICE, &previous);
      }
  }
  secret_store::set(SERVICE, &url, token)?;
  save_settings(&Settings { url: Some(url.clone()) })?;
  ```
  If `secret_store::set` or `save_settings` fails, the previous URL's token has already been deleted from the secure store while `settings().url` still points to `previous`.
- **Recommendation:**
  Remove `previous` only after the new secret and settings have been successfully saved.

---

## 3. Verdict

**Verdict:** **CHANGES REQUESTED**

The core transport implementation is compact and follows the specification in `plan/2026-10-22-pandacode-gateway.md`. However, Findings 1 and 2 represent concrete correctness issues (flaky Cargo tests and WebSocket connection multiplication) that should be addressed before landing.
