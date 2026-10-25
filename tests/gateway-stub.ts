// A stub gateway that answers the same wire the real one does, so a browser proof can
// drive the surfaces a person uses without compiling Rust and booting a binary.
//
// It speaks the three things the app depends on: the JSON-RPC envelope on /rpc, a
// numbered event stream on /events (`{seq, event, payload}`, PAN-105), and the /upload
// route a message names by id (PAN-108). Extracted from tests/gateway.spec.ts so more
// than one spec can share it.

import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import type { Socket } from 'node:net';

/** A command's answer: a value, a promise, or a throw (which becomes an error envelope). */
export type Reply = unknown | ((args: any) => unknown);
export type Replies = Record<string, Reply>;

/** What `/upload` answers, per test. */
export type UploadAnswer = { status: number; body: unknown };

export type Stub = {
  url: string;
  /** Every command asked for, in order. */
  calls: string[];
  /** The same, with the arguments — what a send actually carried. */
  requests: { command: string; args: Record<string, unknown> }[];
  /** Open event sockets. Zero means the app never subscribed to this gateway. */
  sockets: () => number;
  /** The query string of every /events socket opened, oldest first. */
  socketUrls: string[];
  /**
   * The `x-pandacode-client` header of every /rpc request, in order — `null` when the
   * client named itself not at all (PAN-142).
   */
  rpcClients: (string | null)[];
  /** Pushes one numbered `{seq, event, payload}` frame to every open socket, as the real gateway does. */
  emit: (event: string, payload?: unknown) => void;
  /** The same, addressed to one terminal: only its filtered socket (and unfiltered ones) receive it. */
  emitTerminal: (terminalId: string, event: string, payload?: unknown) => void;
  /**
   * Announces a hole the way the gateway's ring does: frames `fromSeq`..`toSeq` are
   * never sent, the stream resumes after `toSeq`, and a client that repairs nothing
   * keeps a permanently wrong view.
   */
  announceHole: (fromSeq: number, toSeq: number) => void;
  /** Closes every socket without closing the server: a dropped connection. */
  drop: () => void;
  /** The /upload requests received, so a test can see the bytes arrive. */
  uploads: { name: string; kind: string; bytes: number }[];
  close: () => Promise<void>;
};

/** RFC 6455 appendix A: the accept key is SHA-1 of key + this GUID, base64. */
const WS_GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

/** One unmasked text frame; the server-to-client direction is never masked. */
function textFrame(text: string): Buffer {
  const payload = Buffer.from(text);
  if (payload.length < 126) return Buffer.concat([Buffer.from([0x81, payload.length]), payload]);
  if (payload.length < 65536) {
    const header = Buffer.alloc(4);
    header[0] = 0x81; header[1] = 126; header.writeUInt16BE(payload.length, 2);
    return Buffer.concat([header, payload]);
  }
  const header = Buffer.alloc(10);
  header[0] = 0x81; header[1] = 127; header.writeBigUInt64BE(BigInt(payload.length), 2);
  return Buffer.concat([header, payload]);
}

type Open = { socket: Socket; terminal: string | null };
/** A frame already sent, kept so a resume can replay what a client missed. */
type Sent = { seq: number; text: string; terminal: string | null };

export type StubOptions = {
  /** Answers `/upload`. Defaults to accepting the file and naming it; may be held open. */
  upload?: (name: string, kind: string, body: Buffer) => UploadAnswer | Promise<UploadAnswer>;
};

export async function startGateway(replies: Replies, options: StubOptions = {}): Promise<Stub> {
  const calls: string[] = [];
  const requests: { command: string; args: Record<string, unknown> }[] = [];
  const opens: Open[] = [];
  const socketUrls: string[] = [];
  const rpcClients: (string | null)[] = [];
  const uploads: { name: string; kind: string; bytes: number }[] = [];
  /** The ring, so a reconnect can be answered with the frames it missed (PAN-105). */
  const sent: Sent[] = [];
  /** The gateway numbers every frame from one counter, per process. */
  let seq = 0;
  let accepted = 0;

  const server = createServer((request, response) => {
    // The real gateway answers the browser's token-less preflight and names the
    // headers a client may send (src-tauri/src/gateway/server.rs, `cors`). Without
    // this the browser fails the fetch before /rpc is ever reached.
    const allow: Record<string, string> = {
      'Access-Control-Allow-Origin': request.headers.origin ?? '*',
      'Access-Control-Allow-Methods': 'GET, POST',
      // `x-pandacode-client` is on the list for the same reason the real gateway
      // allows it: the app names its client on every command (PAN-142).
      'Access-Control-Allow-Headers': 'authorization, content-type, x-pandacode-client',
      'Access-Control-Max-Age': '600',
      Vary: 'Origin',
    };
    if (request.method === 'OPTIONS') {
      response.writeHead(204, allow);
      response.end();
      return;
    }
    const chunks: Buffer[] = [];
    request.on('data', chunk => { chunks.push(chunk as Buffer); });
    request.on('end', async () => {
      const url = new URL(request.url ?? '/', 'http://127.0.0.1');
      if (request.method === 'POST' && url.pathname === '/upload') {
        const body = Buffer.concat(chunks);
        const name = url.searchParams.get('name') ?? '';
        const kind = url.searchParams.get('kind') ?? 'document';
        uploads.push({ name, kind, bytes: body.length });
        // A test may refuse the file, which is how the app's own error path is proved.
        const answer = await (options.upload?.(name, kind, body) ?? {
          status: 200,
          body: { id: `upload-${++accepted}`, name, kind, bytes: body.length, sha256: 'stub', createdAt: Date.now() },
        });
        response.writeHead(answer.status, { ...allow, 'Content-Type': 'application/json' });
        response.end(JSON.stringify(answer.body));
        return;
      }
      if (request.method !== 'POST' || !url.pathname.startsWith('/rpc')) {
        response.writeHead(404, allow);
        response.end('not found');
        return;
      }
      const frame = JSON.parse(Buffer.concat(chunks).toString() || '{}');
      calls.push(frame.command);
      const named = request.headers['x-pandacode-client'];
      rpcClients.push(typeof named === 'string' && named ? named : null);
      requests.push({ command: frame.command, args: frame.args ?? {} });
      const reply = replies[frame.command];
      const footer = { ...allow, 'Content-Type': 'application/json' };
      if (reply === undefined) {
        response.writeHead(200, footer);
        response.end(JSON.stringify({ id: frame.id, ok: false, error: `unsupported_command: ${frame.command}` }));
        return;
      }
      // A reply may be a promise, so a test can hold a request open and watch a loading
      // state. A command that fails is an error envelope, exactly what the real server sends.
      let result: { ok: true; value: unknown } | { ok: false; value: unknown };
      try {
        const raw = typeof reply === 'function' ? (reply as (args: unknown) => unknown)(frame.args) : reply;
        result = { ok: true, value: await raw };
      } catch (error) {
        result = { ok: false, value: error instanceof Error ? error.message : String(error) };
      }
      if (response.writableEnded) return;
      response.writeHead(200, footer);
      response.end(result.ok
        ? JSON.stringify({ id: frame.id, ok: true, result: result.value })
        : JSON.stringify({ id: frame.id, ok: false, error: result.value }));
    });
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = (server.address() as { port: number }).port;
  // The gateway's /events socket (src-tauri/src/gateway/server.rs). Only the handshake
  // and one-way text frames are needed: the client sends nothing on it.
  server.on('upgrade', (request, socket) => {
    const key = request.headers['sec-websocket-key'];
    if (typeof key !== 'string') { socket.destroy(); return; }
    const url = new URL(request.url ?? '/', 'http://127.0.0.1');
    const query = url.searchParams;
    const accept = createHash('sha1').update(key + WS_GUID).digest('base64');
    socket.on('error', () => undefined);
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    socketUrls.push(request.url ?? '');
    const terminal = query.get('terminal');
    opens.push({ socket, terminal });
    // `since=N` is the whole point of numbering: replay exactly what this socket missed
    // and nothing it already saw, which is how a client can prove it drew only the delta.
    const since = Number(query.get('since') ?? '0');
    if (Number.isFinite(since) && since > 0) {
      for (const frame of sent) {
        if (frame.seq <= since) continue;
        if (terminal && frame.terminal !== terminal) continue;
        socket.write(frame.text);
      }
    }
  });
  const frame = (event: string, payload: unknown, number: number | null): string =>
    textFrame(JSON.stringify(number === null ? { event, payload } : { seq: number, event, payload }));
  /** Sends to every socket that asked for this event: a terminal filter excludes the rest. */
  const send = (event: string, payload: unknown, terminal: string | null = null) => {
    const text = frame(event, payload, ++seq);
    sent.push({ seq, text, terminal });
    if (sent.length > 400) sent.shift();
    for (const open of opens) {
      if (open.terminal && open.terminal !== terminal) continue;
      open.socket.write(text);
    }
  };
  return {
    url: `http://127.0.0.1:${port}`,
    calls,
    requests,
    sockets: () => opens.length,
    socketUrls,
    rpcClients,
    emit: (event, payload = null) => send(event, payload),
    emitTerminal: (terminalId, event, payload = null) => send(event, payload, terminalId),
    announceHole: (fromSeq, toSeq) => {
      // The number moves to the far side of the hole: the stream continues after it.
      seq = Math.max(seq, toSeq);
      const text = textFrame(JSON.stringify({ event: 'lagged', payload: { fromSeq, toSeq } }));
      for (const open of opens) if (!open.terminal) open.socket.write(text);
    },
    drop: () => { for (const open of opens) open.socket.destroy(); opens.length = 0; },
    uploads,
    close: () => new Promise<void>(resolve => {
      // fetch keeps its connections alive and the event socket is long-lived;
      // closing both keeps teardown instant.
      for (const open of opens) open.socket.destroy();
      opens.length = 0;
      server.closeAllConnections?.();
      server.close(() => resolve());
    }),
  };
}
