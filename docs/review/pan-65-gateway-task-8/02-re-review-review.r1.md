# Re-Review Report: PAN-65 Gateway Task 8

**Reviewer:** `review.r1` (Independent Reviewer)  
**Target:** PAN-65 'gateway task 8: client transport in the frontend bridge (remote mode)'  
**Branch:** `abacusaus/pan-65-gateway-task-8-client-transport-in-the-frontend-bridge`  
**Commits Reviewed:**
- `2b5f669` fix(gateway-client): cancel pending reconnect and prune empty listener sets
- `270b658` fix(bridge): close a replaced remote client and route preview by mode
- `d55e51b` fix(gateway): remove the old token only after saving, and serialize its tests
- `4c4b2a1` chore(review): record the pan-65 review.r1 report  
**Base:** `d9e9d26`  
**Date:** 2026-10-22  
**Status:** **APPROVED**

---

## 1. Verification of Prior Findings

| Finding | Severity | Status | Verification & Resolution |
| :--- | :---: | :---: | :--- |
| **1. Flaky Rust tests** | HIGH | **VERIFIED RESOLVED** | Added `static TEST_LOCK: std::sync::Mutex<()>` with poison unwinding in `gateway_client::tests`. Verified by running `cargo test gateway_client` under default multi-threading 5x in a loop with 0 failures (previously failed intermittently). |
| **2. WebSocket leak during reconnect backoff** | HIGH | **VERIFIED RESOLVED** | In `src/gateway-client.ts`, `connect()` now clears and nulls `reconnect`. Verified by new test `subscribe during the reconnect backoff does not open a second socket`. |
| **3. Empty Sets in listener Map causing reconnects** | MEDIUM | **VERIFIED RESOLVED** | In `src/gateway-client.ts`, unsubscribe callback deletes the key from `listeners` when `set.size === 0`. Verified by new test `unsubscribing every listener stops the dropped socket from reconnecting`. |
| **4. `setMode` abandoning client without `close()`** | MEDIUM | **VERIFIED RESOLVED** | In `src/bridge.ts`, `setMode` now invokes `if (remote && remote !== client) remote.close();`. |
| **5. Bare `preview` checks instead of `mode === 'preview'`** | LOW | **VERIFIED RESOLVED** | In `src/bridge.ts`, `subscribe`, `chooseFolder`, `chooseAttachments`, `chooseVsix`, and `chooseAttachmentFolder` all now check `if (mode === 'preview')`. |
| **6. Premature token deletion in `connect`** | LOW | **VERIFIED RESOLVED** | In `src-tauri/src/gateway_client.rs`, `connect()` stores the new secret and updates settings before removing the `previous` token. |

---

## 2. Test & Build Verification

- **Node Test Suite (`npm test`):** **PASS** (115/115 passed, including both new gateway client reconnect tests, in 2.68s).
- **TypeScript Typecheck (`npx tsc --noEmit`):** **PASS** (zero diagnostics).
- **Rust Tests (`cargo test gateway_client`):** **PASS** (6/6 passed across 5 consecutive parallel test runs with zero flake).
- **Dependencies:** Confirmed no new npm dependencies introduced.

---

## 3. Final Recommendation

**Verdict:** **APPROVED**

All six findings from the initial review have been addressed cleanly and verified with dedicated automated tests and multi-threaded test execution. The task branch is clean and ready for PR.
