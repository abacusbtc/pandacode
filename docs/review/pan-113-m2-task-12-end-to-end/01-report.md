# M2 task 12 — end-to-end and negative proofs (PAN-113)

Written for the owner. Every claim here is an assertion in a test you can run or an
observed output quoted below. Where a proof is weaker than the spec's ideal, the
weakening is stated in the same sentence as the claim.

Branch `abacusaus/pan-113-m2-task-12-end-to-end`, stacked on task 11's head `5a2b181`.
Everything below was run on this host, with `XDG_*` under `/tmp/opencode/pan113` and
temporary data directories only — never `~/.local/share`.

## 1. The real turn — ran, on `opencode`, and it works

**One defect stands between the owner's chosen model id and this turn. Filed as PAN-121.**

The owner released the held step on `opencode` (DeepSeek V4.1 Flash). The gateway's own
catalog serves `opencode-go/deepseek-v4.1-flash`, so the first attempt used exactly that
id, and it failed before a single token was generated:

```
status: "Opening session"
status: "Applying task mode"
ERROR EVENT: "The harness does not advertise the selected setting. Refresh its models or choose the harness default."
```

Reproduced without spending anything, using the repository's own metadata-only smoke test
(negotiation only, prompt never sent, 2.3 s):

```sh
PANDACODE_OPENCODE_SMOKE_MODEL=opencode-go/deepseek-v4.1-flash \
  cargo test --manifest-path src-tauri/Cargo.toml --lib \
  installed_opencode_negotiates_selected_model_without_prompt -- --ignored
# panicked: "The harness does not advertise the selected setting. …"
```

**Cause (measured, not guessed):** the catalog and the live session disagree. The
gateway's `list_provider_models` for `opencode` answers **431** models, including every
`opencode-go/*` id; the session's own `configOptions` model select advertises **nine**,
all free tier:

```
opencode/beam-free, opencode/big-pickle, opencode/deepseek-v4.1-flash-free,
opencode/flint-40b-free, opencode/glm-5.3-flash-free, opencode/mimo-v2.6-flash-free,
opencode/nemotron-3-ultra-free, opencode/nemotron-3.5-lightning-free, …
```

`providers/acp.rs` (`choice_request`, reached from `acp_model`) refuses any id the session
did not advertise, so every non-free model the picker offers dies at negotiation. That is
**PAN-121**, with this reproduction. It is not fixed here: a proof task that also changes
the thing it proves is worth nothing, and the lead ruled the fix is its own row.

**So the end-to-end evidence ran on `opencode/deepseek-v4.1-flash-free`** — the free id of
the same model family, the only DeepSeek V4.1 Flash id the session accepts:

> **The owner's chosen id (`opencode-go/deepseek-v4.1-flash`) is not selectable through
> the gateway today (PAN-121). The end-to-end evidence below ran on
> `opencode/deepseek-v4.1-flash-free` instead.**

```sh
PANDACODE_E2E_MODEL=opencode/deepseek-v4.1-flash-free \
  cargo test --manifest-path src-tauri/Cargo.toml --features gateway \
  --test gateway_http real_turn_end_to_end -- --ignored --nocapture
```

Observed (verbatim from the run):

```
provider opencode serves 431 models
status: "Opening session" / "Applying task mode" / "Applying model settings" / "Waiting for response"
event kinds: ["status","status","status","session","status","commands_changed","status",
              "status","message_start","text","usage","turn_changes","done"]
streamed text: "The gateway turn works."
usage the harness reported: {"contextWindow":200000,"usedTokens":13091}
saved assistant rows: ["The gateway turn works."]
```

The prompt asked for one fixed sentence and nothing else, so the turn wrote no code and
read nothing of the owner's. It cost one short turn; the harness surfaced **context usage
(13,091 of 200,000 tokens), not a cost figure** — there is no dollar number to report, and
none is estimated here.

**What the free-tier run does prove:** the whole pipeline — a real provider CLI spawned by
the real gateway, a session created through the wire, streaming over a real `/events`
socket, the usage event, the turn ending, the transcript read back through `get_messages`,
and `gateway_live_tasks` empty afterwards. **What it does not prove:** the *paid* path.
The paid id is refused at negotiation today (PAN-121), so nothing here says a paid turn
would run, bill, or behave the same once the id is accepted. The test itself takes the
model id from `PANDACODE_E2E_MODEL` and checks it against the gateway's catalog, so when
PAN-121 is fixed it is the same command with the owner's id.

## 2. Client killed mid-turn

`a_client_killed_mid_turn_does_not_stop_the_turn_and_a_reconnect_resumes`. Instrument: the
stand-in harness (below), so the *turn* is not a model, but everything else is real — real
binary, real socket, real registry, real ring.

- The client closes its socket after the first text frame. 1.2 s later
  `gateway_live_tasks` still lists the turn: **nothing is bound to a client**.
- A client that reconnects with `since=<last frame it saw>` gets a replay whose first
  frame is exactly `last + 1` and whose sequence numbers increment by one with no repeat.
- The transcript contains all ten deltas (`[10]` present): the turn finished with nobody
  watching it.

**Weakened:** the harness is a stub, so this proves the lifecycle and the transport, not
provider behaviour. Section 1 is the provider half.

## 3. An approval with no client

`an_approval_with_no_client_is_denied_at_its_deadline_and_survives`. A real one-second
`--approval-ttl`, no socket open at any point during the wait.

- The stand-in harness appends the control response it received to a file: the run is told
  `{"behavior": "deny"}` — the refusal reaches the run, it does not hang.
- `get_messages` contains an `approval_denied` row, and an assistant row after it
  ("Declined as asked."): the turn **continued** past the denial.
- A client that connects afterwards with `since=0` replays both the `approval` and
  `approval_denied` frames from the ring.

**Not a test hook:** the deadline is the real watchdog with a one-second TTL.

## 4. A lagged reader

`a_lagged_reader_is_told_what_it_missed_and_resuming_yields_exactly_that_range`. A real
socket, a real ring, a reader that stops reading.

- **What the frame rate had to be:** 2 000 frames of ~8 KiB, about 16 MB, emitted with no
  delay while the reader was paused for 3 s. Two smaller attempts did **not** lag
  (700 frames of ~100 B, and 700 of 4 KiB): loopback autotunes its buffers to megabytes and
  swallowed them whole, which is the difference between a real hole and a synthetic one.
  A deliberately tiny `SO_RCVBUF` was tried and rejected as an instrument: it throttles the
  child's own pipe instead of the subscriber queue, so nothing drops.
- The slow reader receives `{"event":"lagged","payload":{"fromSeq":…,"toSeq":…}}`; every
  frame it did receive is outside that range, and the frame before it is present.
- Reconnecting with `since=fromSeq-1` replays the hole and the rest, contiguous, once each.
- The union of what the slow reader got and what the resume gave has **no gap and no
  duplicate** — checked by sequence number, not by counting.

## 5. Gateway SIGKILL mid-turn

`the_real_gateway_sigkilled_mid_turn_leaves_a_resumable_row_and_releases_the_lock`. The
**real binary** is spawned as a child, with a turn genuinely in flight (the stand-in
harness holds it open on stdin), a subagent and its tool seeded into the database, and
then `SIGKILL`ed — no shutdown, no flush.

- The lock is released: a second gateway on the same data directory starts.
- Read back with the **`sqlite3` CLI**, not the app's code:
  `sessions.status='interrupted'`, `task_stage='in_progress'`, `completed_at` NULL,
  `SELECT COUNT(*) FROM sessions WHERE status='running'` = 0, the subagent `stopped`, its
  unfinished tool `failed`.
- `gateway_live_tasks` on the new process is `[]`.

**Weakened:** the turn is the stand-in harness (so no provider process was orphaned mid
call); the subagent and tool rows are seeded straight into the database, which is what a
SIGKILLed process would leave behind but is not itself produced by a real model. Section 1
covers a real turn; this covers the crash.

## 6. Upload expiry

`an_unreferenced_upload_is_swept_a_queued_one_survives_and_the_cap_still_wins`. Over the
wire: two uploads, one named by a queued message, and the clock seam task 7 built
(`GatewayHandle::sweep_uploads(now)`), so nothing waits a day.

- +25 h: `(removed, kept) == (1, 1)` — the loose upload's directory is gone, the queued
  one's is there.
- +8 days: the hard cap wins over the reference, and the queued one goes too.

**Which path was exercised, and what was not:** expiry and queued-survival are proved over
the wire. "Deleting a session removes its uploads immediately" is `task_deletion`'s job and
is already proved by
`task_deletion::tests::deleting_a_task_removes_the_uploads_it_owned_and_leaves_the_rest`;
it **cannot be reached over the gateway at all**, because `delete_session` is not in the
command table. That is a finding, below.

## 7. An app action with no client

**Not proved over the real transport, and this is the honest gap of this task.** The only
trigger for `request_ui` is an agent calling the app's MCP tool, which reaches
`request_ui_on` inside the gateway. The stand-in harness cannot speak MCP, and a real
harness that calls it is exactly the spend this task avoids. What *is* proved, in process,
by PAN-106's own tests: with `attached == false` the wait is 5 s (measured 4.8–9 s
end-to-end, answer honoured at 5.5 s) and the message names the cause; with a client
attached it is 12 s. What is **not** proved is the one line the real transport adds —
`GatewaySurface::client_attached()`, i.e. that `sink.clients()` is non-zero exactly when a
socket is open.

Two consequences of that, both live today and both worth a row:

1. A targeted frame needs a subscribing client (NOTES, 2026-10-24). `BroadcastSink::emit_to`
   addresses one client, and the shipped frontend subscribes with no `target`, so an app
   action would reach **nobody** even on a real turn.
2. `agent-ui-action` is the only targeted event, so this has no other symptom and no test
   today.

## Two negative proofs (broken on purpose, restored byte-identically)

1. **The resume point.** `events.rs`'s replay filter changed from `frame.seq > since` to
   `>= since`: `a_client_killed_mid_turn…` fails with
   `the replay starts at the next frame…: [5, 6, 7, …]` / `left: 5, right: 6` — a duplicate,
   which is the failure a reconnect is supposed to be immune to.
2. **The denial reaching the run.** `approvals.rs`'s `deny` returns `Ok(())` instead of
   sending `RunInput::Approval{approved:false}`: `an_approval_with_no_client…` fails with
   `the run must receive a denial, not silence: ""` after 20 s — recorded, audited, and
   never delivered, which is the policy rotting quietly.

Restoration: `sha256sum` of both files matches the pre-break value and the whole `git diff`
is identical before and after (`diff` of the two saved patches is empty).

## Review Focus 1–8 — the state at the end of M2

| # | Item | State |
| --- | --- | --- |
| 1 | Auto-deny correctness | **Proved.** Right id, removed from the pending set before acting (so a client answer at the deadline is refused, never both), denial reaches the run and it continues (§3), durable row and ring frame, TTL from the raise (PAN-106's own negative proof). One gap: a restart still leaves nothing to mark, by design (§9.3). |
| 2 | Uploads | **Proved** across tasks 7 and 12: `/upload` in `GUARDED`, no client path becomes a filesystem path, cap enforced before and during the stream, partials removed on abort, the sweep never eats a queued message's upload (§6), `client_max_body_size 220m` in the docs. |
| 3 | Terminal zombie shells | **Proved by PAN-110's suite**: closed tab, dropped socket and idle timeout all end the group; idle close does not kill a watched shell; resize storms do not leak PTYs (descriptor-count test). Not re-proved here. |
| 4 | Unit as the owner user | **Proved statically** by PAN-112: `User=abacus`, no `DynamicUser`/`StateDirectory`, no `--data-dir`, `ProtectSystem` off *by decision* with the comment, token under the owner's home. Never started as a service — that is the owner's machine. |
| 5 | Payload drift | **Guarded, not eliminated.** A malformed event is named once instead of swallowed (PAN-111); no test drives a one-field-off gateway frame end to end. |
| 6 | Silent event loss | **Proved.** `lagged` announced rather than skipped (§4), `since` replays exactly the missed range, a ring that cannot serve a resume says so, and a reconnect never duplicates the boundary frame (negative proof 1). |
| 7 | Auth bypass on the new surface | **Proved.** `/upload` and every `/events` parameter go through `authorize` (existing tests); the failure limit is per source and never limits a valid token (PAN-112). |
| 8 | Double writer / double turn | **Proved for the writer** (lock, cold-start settling, §5). **Proved for turns**: a second send on a running session is refused before anything spawns, and a terminal harness and an agent turn for one session are both refused (`lib.rs:1223`). Not exercised with two *live* clients on one session — the refusal is the same code either way. |

## Findings (not fixed here — each is its own row or a note for the lead)

1. **PAN-121 — the catalog advertises what the session refuses.** 431 ids offered, 9
   accepted by `opencode`. The owner's chosen paid id is unusable until this is fixed. This
   is the defect that matters most in M2: a picker that offers a model and a turn that dies
   at the first sentence.
2. **A remote app action reaches nobody.** `emit_to` is single-target; the shipped client
   subscribes without a `target`. No symptom today only because nothing on the gateway path
   emits a targeted frame.
3. **`delete_session` is not served.** A remote client cannot delete a task, so the
   immediate upload cleanup path of §6 is unreachable over the wire (it is correct, and
   tested, at the store level). The frontend does not gate the delete action, so it fails
   with whatever `unsupported_command` produces.
4. **`get_terminal_preferences` is not served** while the terminal over the gateway is; the
   client swallows the failure and uses defaults, so remote consoles ignore the owner's
   appearance settings silently.

## What this task did not verify

- The paid model path through `opencode` (PAN-121), and therefore any billing behaviour.
- A real harness raising an approval or an app action (§7): the approval path is proved
  with the stand-in harness and the app-action path only in process.
- A real provider process killed mid-call (the SIGKILL proof kills the gateway while a
  stub holds the turn).
- Anything on the owner's machine as a service: no install, no `systemctl`, no sudo.
