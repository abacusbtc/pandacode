# Gateway

`pandacode-gateway` serves PandaCode's own backend over HTTP, so a browser on
another computer can use the same projects and tasks the desktop app holds. It is
the app's Rust library with a different entry point: same `AppState`, same
`Store` methods, same command names and the same error strings. The desktop build
never compiles or starts it — everything behind the `gateway` Cargo feature
(axum, tower-http, the `/events` socket) stays out of the app bundle.

This is milestone 2. It serves projects, sessions, agent turns, uploads and
terminals. Files, git, the browser, VMs and the simulator are not in it, and a
client is told so rather than discovering it by hanging: the command table below is
the whole surface, and `/capabilities` can never advertise a command the dispatcher
does not answer.

It runs **as you**, on your machine, with your credentials and your projects — the
owner's decision for M2, and the reason a remote turn is indistinguishable from one
started at the desk. The consequences are in "What you are trusting" below; the
short version is that the token is as good as your SSH key.

## Run it

```sh
npm run build                       # the app the gateway will serve, into dist/
npm run gateway:dev                 # cargo run --features gateway --bin pandacode-gateway -- --static-dir dist --port 4175
```

Then open `http://127.0.0.1:4175/`, and connect from Settings → General → Remote
gateway with that address and the token from the data directory:

```sh
cat ~/.local/share/ai.pandacode.desktop/gateway-token
```

Documents on a gateway host need one external reader: install **poppler-utils**
for PDFs (`pdftotext`). DOCX, ODT and EPUB are read in-process; without
poppler-utils a PDF is refused with a message naming the package rather than
failing vaguely.

Which formats a Linux gateway reads, so nobody has to guess: plain text and
Markdown; PDF through `pdftotext`; DOCX, ODT and EPUB in-process. Legacy binary
formats — `.doc`, `.rtf`, `.pages`, `.pptx`, `.xlsx` — are refused on any host that
has no system reader for them, with a message asking for a text export; that
message is the same sentence a Mac user sees, and on Linux it means "export it
first", not "this gateway cannot read documents".

The binary is a normal cargo target; `--help` prints the flags:

```sh
cargo run --manifest-path src-tauri/Cargo.toml --features gateway --bin pandacode-gateway -- --help
usage: pandacode-gateway [--data-dir <dir>] [--host <host>] [--port <port>] [--token <token>] [--token-file <file>] [--static-dir <dir>] [--allow-origin <origin>]… [--approval-ttl <duration>] [--upload-max <size>] [--terminal-idle <duration>] [--force-owner]
```

`--host` defaults to `127.0.0.1` and `--port` to `47399`. `--static-dir` is
optional: without it the gateway is API-only and every page request is a 404,
which is the right mode behind a proxy that already serves the files. `--help` and
`Ctrl-C` work as they should; the lock and the listener are both released on
SIGINT.

`--upload-max` is the largest single file `POST /upload` accepts: `200MiB` by
default, written as plain bytes or with a unit (`200MiB`, `512KB`, `1G`). It is an
accident guard — a phone photo of a document fits, a mistaken video does not — not
a security control. The RPC surface keeps its own 64 MiB cap regardless.

`--terminal-idle` is how long a terminal is kept when **no client is attached to it
and it has printed nothing**: `30m` by default, written as `90s`, `30m`, `2h` or
`1h30m`, and `0` to keep them forever. A terminal with a client watching is never
closed, however quiet it is, and a closed one's process group is killed and the
client is told it was closed for being idle rather than losing the shell silently.

`--approval-ttl` is how long an approval stays pending with **no client attached**
before the gateway denies it on the person's behalf: `1h` by default, in the same
units, and `0` turns auto-deny off (logged once at start — an approval nobody
answers then waits until the turn ends). The clock starts when the question was
raised, not when the turn last did anything, so a busy turn cannot silence it. The
denial reaches the run as a refusal it can plan around, appears in the transcript,
and is one of the audited events below.

## The data directory

`--data-dir` is the folder holding `kingcode.sqlite3` — the **same file** the
desktop app uses. Pointing the gateway at the app's data directory is how a
browser sees the projects that are already there; pointing it at a fresh
directory gives an empty database and a new token.

The default is the desktop app's own directory, built without a Tauri handle:
`ai.pandacode.desktop` under the platform data base — `$XDG_DATA_HOME` (or
`~/.local/share`) on Linux, `~/Library/Application Support` on macOS, `%APPDATA%`
on Windows. A test pins that identifier against `tauri.conf.json`, so the two
directories cannot drift apart by renaming the bundle.

**Run as a service it is your directory, not a system one.** The unit in
`scripts/gateway/` runs as `User=abacus` with no `--data-dir`, so on Linux that is
`~/.local/share/ai.pandacode.desktop` — the same folder the desktop app uses, with
the same `kingcode.sqlite3`, created on first start by the gateway itself. Nothing
in `/var/lib` is involved any more, and nothing needs `sudo` to read the token.

Three files live there, all created with `0600` where they are secret:

- `kingcode.sqlite3` — projects, sessions, messages and settings.
- `gateway-token` — the bearer token, 64 hex characters, generated on first run.
- `kingcode.lock` — the writer lock, and the holder's PID for a human reading the
  directory. Nothing reads the number back; see below.

A database whose `user_version` is newer than this binary understands is refused
before any statement touches it. A second binary sharing one file must not be the
thing that reveals it is too old.

### One writer

The desktop app and the gateway open the same SQLite file, so both take
`kingcode.lock` before opening the store. On unix it is a kernel advisory lock
(`flock`) held by an open descriptor: the kernel releases it when the process
dies, including on `SIGKILL`, so holding the lock *is* the state and there is no
stale-lock recovery to get wrong. A second process finds it held and refuses:

```
/home/abacus/.local/share/ai.pandacode.desktop is in use by another process (pid 4711): a second PandaCode or
the pandacode-gateway holds the database lock.
Stop that process and try again.
```

So: **stop the desktop app before starting the gateway against its data
directory, and vice versa.** There is no queueing and no takeover.
`--force-owner` does not take a live lock — a flag that can silently steal a live
writer is worse than no flag — and says so when it is passed. On Windows there is
no portable kernel lock, so this build records the PID and reports the lock as
held; two writers remain possible there (PAN-55), and the gateway's served target
is unix. A filesystem that cannot lock at all is a refusal, never a warning: a
gateway that cannot promise it is the only writer is not a gateway.

## The token

Every request except `/healthz` and the SPA's own files needs the token:

```
Authorization: Bearer <token>
```

`--token` takes an explicit value and wins over everything. `--token-file` names a
file that **must already exist** — a typo there must not silently rotate the
token — and is never created. With neither flag the gateway reads
`<data-dir>/gateway-token`, creating it on first run with mode `0600` before a
single byte is written.

The `/events` socket accepts the token as `?token=` as well, because a browser
cannot set headers on a WebSocket handshake. Nothing else takes a token in a URL:
a token in a query string is more likely to end up in a log. Keep the token out
of shell history and proxy logs; `cat` the file, paste it into Settings once, and
the app keeps it in the OS keychain.

Binding anything other than loopback requires a token you chose — `--token` or
`--token-file`. A token this process generated for its own data directory is not
enough to expose the database to a network, so `--host 0.0.0.0` without one is
refused before the token file is even resolved.

### Rotating it

The token has no expiry and no second copy, so rotation is a restart:

1. Stop the gateway (`sudo systemctl stop pandacode-gateway`, or Ctrl-C).
2. Delete `<data-dir>/gateway-token` — on a service install that is
   `~/.local/share/ai.pandacode.desktop/gateway-token`, and no `sudo` is needed.
3. Start it again. The next start writes a new 32-byte token, `0600`, in the same
   place.
4. `cat` the file and paste it into Settings → General → Remote gateway. The app
   keeps it in the OS keychain; every other client, and anything you gave the old
   token to, is now locked out.

`--token`/`--token-file` installs ignore the data directory's file entirely, so
rotating one of those means changing what the flag points at and restarting.

### Guessing it

Ten failed authorisations from one address in a rolling minute is what a source
gets before the gateway answers `429` for the rest of that minute. The limit counts
**failures**, never requests: a client holding the right token is never limited, so
a brute force from an address cannot lock you out of the gateway you reach from the
same address. Every failure is audited (see "Audit" below), and the window rolls —
a source that stops trying starts again from zero.

## The HTTP surface

`GET /healthz` answers `ok` with no token, so a supervisor, a proxy or a test can
ask whether the process is up without holding a secret.

`POST /rpc` is the transport. One command per request, one result or one error,
in the same envelope the client sends:

```json
{"id": "7", "command": "add_project", "args": {"path": "/srv/repos/notes"}}
{"id": "7", "ok": true, "result": {"id": "…", "name": "notes", "path": "/srv/repos/notes"}}
{"id": "7", "ok": false, "error": "unsupported_command: git_status"}
```

Command failures keep HTTP 200 and travel in `error`, because they are the same
strings the Tauri commands already return and the UI handles them the same way.
The status codes that are not 200 mean the request itself was wrong: `401`
without a usable token, `429` after ten failed tokens from one address in a
minute, `413` for a body over 64 MiB (or an upload over `--upload-max`), `405` for
a write to a client route, `400` for a body that is not JSON.

`POST /rpc` may name the caller in an `x-pandacode-client` header (same identity
rules as `?client=`; a malformed one is `400`). The identity is what makes a turn
belong to a client: an app action the turn raises is addressed to that client and
to no other socket, `gateway_live_tasks` reports it in `client`, and
`resolve_agent_ui_action` refuses an answer from a different client — including a
caller that named none. A request without the header still works and owns nothing,
which is how every M2 client behaves today.

`POST /rpc` is capped at 64 MiB, chosen here rather than inherited: base64
attachments are the largest thing this surface carries. `/upload` has its own cap
(`--upload-max`, 200 MiB by default) because a file streams instead of being
buffered. The proxy must allow the larger of the two (see below) or it will refuse
a body the gateway would have accepted.

`GET /capabilities` and the `gateway_status` command answer the same list, from
one constant in `src-tauri/src/gateway/dispatch.rs` that the dispatcher also
matches on. A test pins both directions — advertised implies answered, and
answered implies advertised.

The table below lists the groups. `gateway_status` is the authority for the exact
list a given binary serves; anything absent from it answers `unsupported_command`,
which is how a client hides a surface instead of discovering it by hanging.

| Group | Commands |
| --- | --- |
| Projects and sessions | `list_projects`, `add_project`, `remove_project`, `list_sessions`, `create_session`, `delete_session`, `get_messages` |
| Remote folder picker | `gateway_list_dirs` |
| Catalog | `gateway_status`, `list_providers`, `list_provider_models` |
| Turns | `send_message`, `start_task`, `stop_session`, `respond_approval`, `respond_user_input`, `steer_task`, `resolve_agent_ui_action` |
| The queue | `queue_list`, `queue_add`, `queue_update`, `queue_remove`, `queue_reorder`, `queue_take`, `queue_restore` |
| Which turns this gateway runs | `gateway_live_tasks` |
| Terminal | `start_terminal`, `start_harness_terminal`, `write_terminal`, `resize_terminal`, `close_terminal`, `get_terminal_activity`, `get_terminal_preferences` |

### `delete_session` and a task the gateway is running

`delete_session` removes the task, its owned worker conversations, and the uploads it
owned — sent ones, and ones only a queued message still referred to — before it
answers, rather than waiting for the hourly sweep. The seven-day upload cap still
applies to everything else.

**A task this gateway is currently driving is refused**, with
`This task is running on the gateway. Stop it before deleting it.` The alternative —
stopping the turn as part of a delete — would kill work the person did not ask to kill,
on a single destructive request, and the reason would be visible only in a process the
row no longer names. A refusal is reversible and names the command that unblocks it:
`stop_session`, then delete. The same rule covers the other live holders the store
already knows about (a waiting agent, an open harness console, an active Jev run); each
refusal names what to stop.

### `get_terminal_preferences`

The gateway runs as the owner, so this reads the same terminal configs the desktop's
command does (`ghostty`, `kitty`, `alacritty`, `wezterm`, Windows Terminal, most
recently modified wins) — there is no gateway-side default and no second store. Only
the terminal's name leaves native code (`source`), never a path or a file's contents;
the values are appearance only (font, line height, cursor, theme colours), and the
client renders its own terminal with them. Nothing else carries them implicitly: a
client that does not ask for them never receives a preference payload.

`features` is `["projects", "sessions", "turns", "terminal"]`. The app asks a
connected gateway only for what this list names, and hides what it does not serve —
the browser toggle, the VM and simulator tabs, the quick launcher, the GitHub view,
the Run button, the `.vsix` import, the usage panel, and (without `turns`) the
composer itself — then shows them again on disconnect. The inspector's Changes and
Files tabs say the backend does not read them rather than claiming a clean tree or
an empty folder, and a window connected to a gateway never calls itself the
browser preview.

`GET /events` upgrades to a WebSocket and sends one JSON text frame per event,
with the same event names the desktop emits. Every frame is numbered:
`{"seq": 41, "event": "sessions-changed", "payload": null}`, and the number is the
whole guarantee that nothing was missed. A browser cannot send a header on the
handshake, so this is the one place the token rides in the query string.

Three query parameters choose what a socket receives and where it starts:

- `?session=<id>` and `?terminal=<id>` filter to one task or one shell. A frame
  with no routing key is a process-wide signal and reaches every socket.
- `?client=<id>` names **this socket's own client identity** (1–64 characters:
  letters, digits, `-`, `_`, `.`, `:`). It is not a filter: it decides which
  frames addressed to that client reach this socket, and it never hides the
  session or terminal frames the same socket also needs. A socket that names
  neither sees only unaddressed frames and anything the process broadcasts.
- `?since=<seq>` replays what the socket missed, exclusively after that number. A
  client that reconnects therefore resumes exactly where it stopped instead of
  starting at "now".

`?target=<name>` still exists as a **filter** and is not how a client identifies
itself: a socket that names a target receives only frames carrying that target,
so it loses the unaddressed frames for the session it is watching. Prefer
`?client=` for identity.

A socket that falls so far behind that its frames have left the ring is **told**,
never skipped: it receives `{"event": "lagged", "payload": {"fromSeq": 3, "toSeq": 9}}`
and then the stream continues from 10. A turn can be refetched from `get_messages`,
so that is what a client does. A terminal cannot, so its socket is told to clear
and redraw instead: `{"event": "terminal-reset", ...}`, followed by whatever the
terminal's 256 KiB ring still holds. Frame numbers come from one process-wide
counter; the payload is exactly what the emitting code serialised, so a desktop and
a gateway event are the same shape.

A frame addressed to one client carries that identity beside the payload —
`{"seq": 42, "event": "agent-ui-action", "client": "alpha", "payload": {...}}` —
and reaches only the socket that named `/events?client=alpha`. An unaddressed frame
has no `client` field at all, so an ordinary event's shape is unchanged.

Anything else is the SPA: a real file from `--static-dir` when it exists,
`index.html` for every other path, and a plain 404 when the gateway was started
without a build. Paths are canonicalised and resolved inside the build directory,
so `..` and symlinks cannot leave it.

### Who can read what

`gateway_list_dirs` lists folders below any absolute path on the **server** — by
default starting at the server's home directory, hidden entries excluded unless
asked — so a browser whose native folder dialog would answer with a path on the
*client's* machine can pick a folder that actually exists on the gateway.

This is deliberate (decision D6) and it is the sharp edge of the surface: **any
holder of the token can enumerate any directory the gateway process can read**,
its names and structure, whether or not a project points at it. It lists folders
only, never file contents and never files, and it does not follow a path outside
what the OS already permits for that user. Run the gateway as an account whose
read access you are willing to hand to every token holder, and treat the token as
an administrative credential rather than a convenience.

### What you are trusting

**The gateway runs as you, on your machine; anyone with its token can do anything
you can do.** Not "can open your projects" — can run a turn, and a turn is a shell
with your credentials: your git config, your provider CLI logins, your SSH agent,
your files. So:

- Keep it on your LAN, behind the proxy, with TLS at the proxy.
- Treat the token like an SSH key: never in a URL, a chat message or a screenshot,
  and rotate it (above) if it may have leaked.
- Prefer the standard approval mode. It is the brake that turns "the agent can run
  anything" into "the agent asks before it runs anything"; `full` exists for when
  you mean it.
- The audit lines below are a record, not a sandbox. They tell you what happened;
  they do not stop it happening. Nothing here isolates the agent from your
  account — decision 2 of M2 says exactly that.

## Behind a reverse proxy

The gateway speaks plain HTTP and binds loopback by default. Terminate TLS at the
proxy; do not expose the HTTP port itself.

- **WebSocket upgrade on `/events`.** Forward `Upgrade` and `Connection`, and
  allow a long read timeout — nginx's default 60 s drops an idle socket. The app
  reconnects, so a drop is a hiccup rather than a failure, but the event stream is
  only as good as the proxy's timeout.
- **Body limit.** `client_max_body_size 220m` in nginx. Two limits are in play: an
  RPC call is capped at 64 MiB, and `/upload` takes one file of up to 200 MiB
  (`--upload-max`). Set the proxy to the largest of them plus headroom — a proxy
  limit below the upload cap refuses a file the gateway would have taken, and the
  client sees the proxy's HTML error page instead of the gateway's sentence. If you
  change `--upload-max`, change this line with it.
- **Uploads.** `POST /upload?name=<urlencoded>&kind=image|document` with the file as
  the raw request body; the answer is `{id, name, kind, bytes, sha256, createdAt}`,
  and a message names those ids in `uploadIds`. Raise `client_body_timeout` (and
  nginx's `proxy_request_buffering off`) if you upload over a slow link: the
  gateway streams, but a proxy that buffers the whole body first turns a 200 MiB
  upload into 200 MiB of proxy memory. Uploaded files live in
  `<data-dir>/uploads/<uuid>/`, are cleaned up after 24 hours if no message refers
  to them, and at most 7 days even when one does.
- **Paths.** Serve everything under one host: the SPA, `/rpc`, `/capabilities`,
  `/events`, `/upload` and `/healthz`. Client routes are answered by `index.html`,
  so do not rewrite unknown paths to a 404.
- **Origin.** When the app is served *by* the gateway it is same-origin and needs
  no flag. A page on another origin calling the API must be named with
  `--allow-origin https://…`, once per origin; the default list is the app's own
  webview origins and nothing else. The CORS layer answers the browser's
  token-less preflight, which is why it sits outside the auth middleware.
- **Do not cache.** Hash-named build assets may be cached forever; `/rpc`,
  `/capabilities`, `/events` and `index.html` may not.

## As a service

`scripts/gateway/pandacode-gateway.service` is a systemd unit for a host that
serves one data directory **as its owner**:

```sh
cargo build --release --manifest-path src-tauri/Cargo.toml --features gateway --bin pandacode-gateway
sudo install -m 0755 src-tauri/target/release/pandacode-gateway /usr/local/bin/pandacode-gateway
sudo install -d /srv/pandacode/dist && sudo cp -r dist/. /srv/pandacode/dist/
sudo install -m 0644 scripts/gateway/pandacode-gateway.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now pandacode-gateway
curl -s http://127.0.0.1:47399/healthz
cat ~/.local/share/ai.pandacode.desktop/gateway-token
```

The unit is `User=abacus` / `Group=abacus` and nothing else: no `DynamicUser`, no
`StateDirectory`, no `--data-dir`. That is the M2 decision in one place — the
gateway runs as the person, so a remote turn has their credentials, their git
config, their provider CLIs and their `~/projects`, exactly as if they had typed it
at the desk. The data directory is therefore **their** app data directory,
`~/.local/share/ai.pandacode.desktop` on Linux (the same folder the desktop app
opens), created on first start by the gateway itself. There is no `/var/lib` in the
picture, nothing to pre-create, and nothing needs `sudo` to read the token.

Two settings are deliberately **not** in the unit, and the second is load-bearing:

- `ProtectSystem=` and `PrivateTmp=` are gone *by decision*. A strict system view
  and a private `/tmp` make a build unable to write `~/.cargo` or its own
  `target/`; "as if I were on the host machine" is incompatible with them. Do not
  re-tighten them and then wonder why a build fails.
- `ProtectHome=` is absent, and now load-bearing rather than incidental:
  credentials, projects and the data directory all live under `/home`. Hiding it
  would leave the remote folder picker listing an empty root and every turn unable
  to read its own configuration.

`NoNewPrivileges=yes` stays, `UMask=0077` stays, and `StandardOutput=journal` is
what makes the audit lines below survive: systemd copies them into the journal as
they are written, where the gateway's own processes cannot edit them.

The unit binds loopback, so the proxy must run on the same host or share its
network namespace; a network bind needs `--host` **and** an explicit
`--token-file` (see above). A system unit with `User=` needs no
`loginctl enable-linger` — that is only for `--user` units.

Starting and stopping the service is an operational step, not something this
document automates: stop the desktop app first, because only one process may hold
the data directory's lock (above).

## Audit

Every RPC command, approval decision, auto-deny, upload and failed authorisation
writes **one line** to stdout — which the unit sends to the journal (`journalctl
-u pandacode-gateway --grep 'audit event='`). Lines are `key=value` so a journal
query can filter on them:

```text
audit event=command client=127.0.0.1:54321 command=send_message session=9f2c outcome=ok duration_ms=41
audit event=approval client=127.0.0.1:54321 decision=approved session=9f2c request=9f2c:3 duration_ms=3
audit event=approval_denied session=9f2c request=9f2c:3 ttl_ms=3600000 action="Run `git push`?"
audit event=upload client=127.0.0.1:54321 upload=1b7e kind=document bytes=20481 sha256=9c1f… duration_ms=12
audit event=auth client=203.0.113.9 path=/rpc failures=11 window_s=60 status=429
```

| Field | Meaning |
| --- | --- |
| `event` | `command`, `approval`, `approval_denied`, `upload`, `auth` |
| `client` | The connection the request arrived on. For a command or an upload it is that request's peer; the audit names the connection, not a person — the token is the identity |
| `command`, `outcome` | What was asked and whether it answered (`ok` / `failed`). The error *text* is deliberately not logged: it can quote the work |
| `session`, `request`, `decision` | The task a command names; the approval request and the word the client answered (`approved` / `denied`) |
| `ttl_ms`, `action` | The auto-deny's configured TTL and the question it answered. The question is the one piece of prose here, because it *is* the decision |
| `upload`, `bytes`, `sha256`, `kind` | The stored file: its id, size, digest and kind. **Not** its name, and never its content |
| `failures`, `window_s`, `status` | An auth failure, how many that source has made in the window, and what it was answered (`401`, or `429` once over the limit) |
| `duration_ms` | Wall time the gateway spent, for commands, approvals and uploads |

What is never in an audit line: message text, prompt text, document or image
content, file paths from a project. A line names ids and sizes so the operator can
find the thing; it does not copy the thing. That is the rule that keeps the
journal from becoming a second, unaudited copy of the person's work.

## Tests

`tests/gateway.spec.ts` drives the app's connect flow against a stub gateway, and
`tests/gateway-e2e.spec.ts` is the one test that proves the real thing: it starts
this binary on a free port with a temporary `--data-dir`, polls `/healthz`, reads
the token the gateway wrote, loads the built app from the gateway itself, adds a
project through the app's own Open project form, and asserts both that the
project is in the gateway's database and that none of the browser preview's
fixture projects is anywhere in the document. It needs `dist/` (the build) and a
compiled gateway binary:

```sh
npm run gateway:test                # npm run build, then the spec with one worker
```

Its own run is serial because it owns a port and a data directory, and it kills
the child process in a `finally`. Running it by hand is
`npx playwright test tests/gateway-e2e.spec.ts --workers=1`.

It is **not** part of a plain `npx playwright test`: that suite runs against the
Vite dev server and has to stay usable on a clean checkout, where this spec would
fail without `dist/` and fight three browser workers for four cores while cargo
compiles. `playwright.config.ts` adds it to `testMatch` when it is named on the
command line (both commands above do that) or when `PANDACODE_GATEWAY_E2E=1` is
set; otherwise the suite skips it.

`npm run gateway:test` launches Chrome from the path in
`PANDACODE_TEST_CHROME`, which defaults to macOS Chrome
(`/Applications/Google Chrome.app/…`). On Linux, name the browser:

```sh
PANDACODE_TEST_CHROME=/usr/bin/google-chrome npm run gateway:test
```

The Rust side is `src-tauri/tests/gateway_http.rs`, gated on the feature:
`cargo test --manifest-path src-tauri/Cargo.toml --features gateway`.

## Dependency note

Under `--features gateway` the tree carries two major versions of tungstenite:
0.28 as the app's own (non-optional) copy and 0.29 through axum's `ws` feature.
The desktop build has 0.28 alone. Do not downgrade the app-level version to
deduplicate: the axum copy is reachable only from the gateway's `/events` socket,
and the app's copy is the one the desktop's bot channels use.

See [ipc.md](ipc.md) for the command contract the gateway mirrors.
