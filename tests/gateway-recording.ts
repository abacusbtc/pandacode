// A real gateway for the boot-recording spec (PAN-101).
//
// The recording is the gateway's **own** audit of the commands it answered
// (`audit event=command … command=<name>`, PAN-112), not a proxy in front of it and not
// a stub: nothing sits between the client and the server that could reorder, drop or
// invent an entry. The app is served by that same gateway, so the browser loads it from
// the gateway's origin — same-origin `/rpc` and `/events`, no CORS, no fixture.
//
// The binary needs `--features gateway`. When it is neither built nor runnable the spec
// skips with that reason rather than pretending the gate was tested.

import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { createServer } from 'node:net';
import { homedir, tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

// Playwright loads this as an ES module, so there is no `__dirname`: the runner's
// working directory is the repository root (playwright.config.ts is read from there).
const ROOT = process.cwd();
const MANIFEST = join(ROOT, 'src-tauri', 'Cargo.toml');
/** The gateway serves the built app; `npm run build` writes it. */
export const STATIC_DIR = join(ROOT, 'dist');
const BINARY = join(ROOT, 'src-tauri', 'target', 'debug', 'pandacode-gateway');

/** Whether the spec can run: the built app and something to run the server with. */
export function gatewayRunnable(): boolean {
  return existsSync(join(STATIC_DIR, 'index.html'));
}

function cargoCommand(): string {
  if (process.env.CARGO) return process.env.CARGO;
  const local = join(homedir(), '.cargo', 'bin', 'cargo');
  return existsSync(local) ? local : 'cargo';
}

/** A port the kernel just handed out, so a parallel run cannot take the gateway's. */
function freePort(): Promise<number> {
  return new Promise((ok, fail) => {
    const probe = createServer();
    probe.once('error', fail);
    probe.listen(0, '127.0.0.1', () => {
      const address = probe.address();
      const port = typeof address === 'object' && address ? address.port : 0;
      probe.close(() => (port ? ok(port) : fail(new Error('no port was assigned'))));
    });
  });
}

export type Recording = {
  base: string;
  token: string;
  /** Every command the gateway journalled, in the order it answered them. */
  commands(): string[];
  /** Forgets what has been recorded so far: what a relaunch measurement needs. */
  clear(): void;
  /** What the gateway advertises, read the way a client reads it. */
  advertised(): Promise<{ commands: string[]; features: string[] }>;
  stop(): Promise<void>;
};

/**
 * Starts the real gateway on a free port over a fresh database, serving the built app,
 * with its audit lines captured.
 */
export async function recordGateway(): Promise<Recording> {
  const data = mkdtempSync(join(tmpdir(), 'pc-gw-record-'));
  const port = await freePort();
  const base = `http://127.0.0.1:${port}`;
  const commands: string[] = [];
  const collect = (chunk: unknown): void => {
    for (const line of String(chunk).split('\n')) {
      if (!line.startsWith('audit event=command')) continue;
      const command = /(?:^|\s)command=(\S+)/.exec(line)?.[1];
      if (command) commands.push(command);
    }
  };

  // A built binary starts instantly; `cargo run` is the fallback for a checkout that
  // has not built the gateway yet, and CI's cargo job may have warmed either.
  const args = [
    '--data-dir', data, '--static-dir', STATIC_DIR,
    '--host', '127.0.0.1', '--port', String(port),
  ];
  const child: ChildProcess = existsSync(BINARY)
    ? spawn(BINARY, args, { cwd: ROOT, stdio: ['ignore', 'pipe', 'pipe'] })
    : spawn(cargoCommand(), [
        'run', '--quiet', '--manifest-path', MANIFEST, '--features', 'gateway', '--bin', 'pandacode-gateway', '--',
        ...args,
      ], { cwd: ROOT, stdio: ['ignore', 'pipe', 'pipe'] });

  let output = '';
  child.stdout?.on('data', chunk => { output += String(chunk); collect(chunk); });
  child.stderr?.on('data', chunk => { output += String(chunk); });

  const stop = async (): Promise<void> => {
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited = new Promise<void>(ok => child.once('exit', () => ok()));
    child.kill('SIGTERM');
    const forced = setTimeout(() => child.kill('SIGKILL'), 10_000);
    await exited;
    clearTimeout(forced);
    try { rmSync(data, { recursive: true, force: true }); } catch { /* Left for the OS to reap. */ }
  };

  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`the gateway exited with ${child.exitCode} before answering /healthz:\n${output}`);
    }
    try {
      const response = await fetch(`${base}/healthz`);
      if (response.ok && (await response.text()).trim() === 'ok') break;
    } catch {
      // Not listening yet; the next attempt is the answer.
    }
    await new Promise(ok => setTimeout(ok, 100));
  }

  const token = readFileSync(join(data, 'gateway-token'), 'utf8').trim();
  return {
    base,
    token,
    commands: () => [...commands],
    clear: () => { commands.length = 0; },
    async advertised() {
      const response = await fetch(`${base}/rpc`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
        body: JSON.stringify({ id: 'record', command: 'gateway_status', args: {} }),
      });
      const body = await response.json() as { result?: { commands?: string[]; features?: string[] } };
      return { commands: body.result?.commands ?? [], features: body.result?.features ?? [] };
    },
    stop,
  };
}
