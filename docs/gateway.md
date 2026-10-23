# Gateway

`pandacode-gateway` serves PandaCode's own backend over HTTP, so a browser on
another computer can use the same projects and tasks the desktop app holds. It is
the app's Rust library with a different entry point: same `AppState`, same
`Store` methods, same command names and the same error strings. The desktop build
never compiles or starts it — everything behind the `gateway` Cargo feature
(axum, tower-http, the `/events` socket) stays out of the app bundle.

This is milestone 1. It serves projects and sessions. Agent turns, files, git,
terminals, the browser, VMs and the simulator are not in it, and a client is told
so rather than discovering it by hanging: the command table below is the whole
surface, and `/capabilities` can never advertise a command the dispatcher does
not answer.

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

The binary is a normal cargo target; `--help` prints the flags:

```sh
cargo run --manifest-path src-tauri/Cargo.toml --features gateway --bin pandacode-gateway -- --help
usage: pandacode-gateway [--data-dir <dir>] [--host <host>] [--port <port>] [--token <token>] [--token-file <file>] [--static-dir <dir>] [--allow-origin <origin>]… [--force-owner]
```

`--host` defaults to `127.0.0.1` and `--port` to `47399`. `--static-dir` is
optional: without it the gateway is API-only and every page request is a 404,
which is the right mode behind a proxy that already serves the files. `--help` and
`Ctrl-C` work as they should; the lock and the listener are both released on
SIGINT.

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
/var/lib/pandacode is in use by another process (pid 4711): a second PandaCode or
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
without a usable token, `413` for a body over 64 MiB, `405` for a write to a
client route, `400` for a body that is not JSON.

The body cap is 64 MiB, chosen here rather than inherited: base64 attachments are
the largest thing this surface will carry. The proxy must allow the same number
(see below) or it will refuse a body the gateway would have accepted.

`GET /capabilities` and the `gateway_status` command answer the same list, from
one constant in `src-tauri/src/gateway/dispatch.rs` that the dispatcher also
matches on. A test pins both directions — advertised implies answered, and
answered implies advertised.

| Command | Args | Result |
| --- | --- | --- |
| `list_projects` | — | `Project[]` |
| `add_project` | `{path}` | `Project` |
| `remove_project` | `{projectId}` | `null` |
| `list_sessions` | `{projectId?}` | `Session[]` |
| `create_session` | `{projectId, provider, model, mode?}` | `Session` |
| `get_messages` | `{sessionId, beforeId?}` | message page |
| `gateway_list_dirs` | `{path?, showHidden?}` | `{path, parent, entries}` |
| `gateway_status` | — | `{commands, features}` |

`features` is `["projects", "sessions"]`. The app hides what a gateway does not
serve — the browser toggle, the VM and simulator tabs, the quick launcher — and
shows them again on disconnect.

`GET /events` upgrades to a WebSocket and sends one JSON text frame per event,
`{"event": "sessions-changed", "payload": null}`, with the same event names the
desktop emits. A browser cannot send a header on the handshake, so this is the one
place the token rides in the query string. A client that falls behind skips ahead
rather than stalling the emitter — the socket is a refresh hint, not a log — and
the app reloads its state on reconnect. None of the milestone-1 commands emits an
event today — they are reads and single writes — so a milestone-1 client sees an
empty socket; it exists so a later command's event has somewhere to go.

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

## Behind a reverse proxy

The gateway speaks plain HTTP and binds loopback by default. Terminate TLS at the
proxy; do not expose the HTTP port itself.

- **WebSocket upgrade on `/events`.** Forward `Upgrade` and `Connection`, and
  allow a long read timeout — nginx's default 60 s drops an idle socket. The app
  reconnects, so a drop is a hiccup rather than a failure, but the event stream is
  only as good as the proxy's timeout.
- **Body limit.** `client_max_body_size 64m` in nginx, the same number as the
  gateway's own cap. A smaller proxy limit refuses bodies the gateway would have
  taken, and the client sees the proxy's HTML error page instead of an RPC error.
- **Paths.** Serve everything under one host: the SPA, `/rpc`, `/capabilities`,
  `/events` and `/healthz`. Client routes are answered by `index.html`, so do not
  rewrite unknown paths to a 404.
- **Origin.** When the app is served *by* the gateway it is same-origin and needs
  no flag. A page on another origin calling the API must be named with
  `--allow-origin https://…`, once per origin; the default list is the app's own
  webview origins and nothing else. The CORS layer answers the browser's
  token-less preflight, which is why it sits outside the auth middleware.
- **Do not cache.** Hash-named build assets may be cached forever; `/rpc`,
  `/capabilities`, `/events` and `index.html` may not.

## As a service

`scripts/gateway/pandacode-gateway.service` is a systemd unit for a host that
serves one data directory:

```sh
sudo install -m 0755 src-tauri/target/release/pandacode-gateway /usr/local/bin/pandacode-gateway
sudo install -d /var/lib/pandacode /srv/pandacode
sudo install -d /srv/pandacode/dist && sudo cp -r dist/. /srv/pandacode/dist/
sudo install -m 0644 scripts/gateway/pandacode-gateway.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now pandacode-gateway
curl -s http://127.0.0.1:47399/healthz
```

Build the release binary with `cargo build --release --manifest-path
src-tauri/Cargo.toml --features gateway --bin pandacode-gateway`. The token is
created on first start at `/var/lib/pandacode/gateway-token`; read it with
`sudo cat` and keep it out of shell history. The unit binds loopback, so the
proxy must run on the same host or share its network namespace; a network bind
needs `--host` **and** an explicit `--token-file` (see above).

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

The Rust side is `src-tauri/tests/gateway_http.rs`, gated on the feature:
`cargo test --manifest-path src-tauri/Cargo.toml --features gateway`.

## Dependency note

Under `--features gateway` the tree carries two major versions of tungstenite:
0.28 as the app's own (non-optional) copy and 0.29 through axum's `ws` feature.
The desktop build has 0.28 alone. Do not downgrade the app-level version to
deduplicate: the axum copy is reachable only from the gateway's `/events` socket,
and the app's copy is the one the desktop's bot channels use.

See [ipc.md](ipc.md) for the command contract the gateway mirrors.
