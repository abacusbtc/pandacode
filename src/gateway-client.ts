// The remote transport: the same command surface the app already calls, over the
// gateway's JSON-RPC endpoint and events socket instead of Tauri IPC.
//
// `fetch` and `WebSocket` are injectable so this runs under `node --test` with a
// stub, and so the app can pass the platform's own implementations unchanged.
// Constructing a client performs no I/O: nothing is fetched or opened until the
// first `call` or `subscribe`.
//
// Frames are numbered (PAN-105): every one is `{seq, event, payload}`, and the number
// is the whole guarantee that nothing was missed. This client therefore remembers the
// last number it saw, asks a reopened socket to resume after it (`since`), and passes
// an announced hole (`lagged`) on to whoever can refetch what it lost.

/** The smallest socket surface this needs, so a stub can stand in for WebSocket. */
export type GatewaySocket = {
  onmessage: ((event: any) => void) | null;
  onclose: ((event: any) => void) | null;
  close(): void;
};
export type GatewaySocketImpl = new (url: string) => GatewaySocket;

export type GatewayOptions = {
  url: string;
  token: string;
  /**
   * This client's identity on the wire. Defaults to the app instance's own id, which is
   * what the app wants; a test may pass its own to run two clients in one process.
   */
  clientId?: string;
  fetchImpl?: typeof fetch;
  WebSocketImpl?: GatewaySocketImpl;
  /** Uploads report progress, which needs `XMLHttpRequest`; tests inject a stub. */
  XMLHttpRequestImpl?: UploadRequestImpl;
  /** Delay before reopening a dropped events socket. Tests set it to a few ms. */
  reconnectDelayMs?: number;
};

/** What `/upload` answers: the id a message names, never a path. */
export type UploadReceipt = {
  id: string;
  name: string;
  kind: 'image' | 'document';
  bytes: number;
  sha256: string;
  createdAt: number;
};

/** The smallest upload surface this needs, so a stub can stand in for XHR. */
export type UploadRequest = {
  upload: { onprogress: ((event: { lengthComputable: boolean; loaded: number; total: number }) => void) | null };
  onload: (() => void) | null;
  onerror: (() => void) | null;
  status: number;
  responseText: string;
  open(method: string, url: string): void;
  setRequestHeader(name: string, value: string): void;
  send(body: unknown): void;
};
export type UploadRequestImpl = new () => UploadRequest;

/** The range a socket missed: frames `fromSeq`..`toSeq` are not coming. */
export type MissedFrames = { fromSeq: number; toSeq: number };

export type GatewayClient = {
  call<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  subscribe(name: string, callback: (payload: any) => void): Promise<() => void>;
  /**
   * One terminal's frames from a socket filtered to it (`?terminal=`), replayed from
   * the start of its ring so a console that opens late still gets its scrollback.
   * `onReset` is the server saying it could not replay that far back.
   */
  watchTerminal(terminalId: string, onFrame: (event: any) => void, onReset: (missed: MissedFrames) => void): () => void;
  /** The highest frame number seen; the value a reconnect resumes after. */
  lastSeq(): number;
  /** Streams one file to the gateway; `onProgress` gets bytes sent of the total. */
  upload(file: Blob, name: string, kind: 'image' | 'document', onProgress?: (sent: number, total: number) => void): Promise<UploadReceipt>;
  close(): void;
};

/** One frame as it arrives, or `null` for something that is not a frame at all. */
type Frame = { seq?: number; event: string; payload: any };

/**
 * This app instance's identity on the wire (PAN-142).
 *
 * One per app instance, not per connection and never derived from the gateway URL: two
 * windows against one gateway are two clients, and a reconnect must not look like a new
 * client, or an app action the running turn is waiting on becomes unanswerable because
 * the only socket that could carry it now claims a different name.
 *
 * `crypto.randomUUID` where the webview has it, sixteen random bytes hex otherwise. The
 * gateway accepts letters, digits, `-`, `_`, `.` and `:` in an identity, and both shapes
 * fit inside its 64-character bound.
 */
function instanceIdentity(): string {
  const crypto = (globalThis as { crypto?: { randomUUID?: () => string; getRandomValues?: (bytes: Uint8Array) => void } }).crypto;
  const uuid = crypto?.randomUUID?.();
  if (uuid) return uuid;
  const bytes = new Uint8Array(16);
  if (crypto?.getRandomValues) crypto.getRandomValues(bytes);
  else for (let index = 0; index < bytes.length; index += 1) bytes[index] = Math.floor(Math.random() * 256);
  return Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
}

const INSTANCE_ID = instanceIdentity();

/** The identity every client in this app instance sends; a test asserts a reconnect keeps it. */
export function clientIdentity(): string {
  return INSTANCE_ID;
}

export function createGatewayClient(options: GatewayOptions): GatewayClient {
  // A saved URL may carry a trailing slash; every path below is joined onto it.
  const base = options.url.replace(/\/+$/, '');
  const { token } = options;
  const doFetch = options.fetchImpl ?? fetch;
  const delay = options.reconnectDelayMs ?? 1000;
  const listeners = new Map<string, Set<(payload: any) => void>>();
  let socket: GatewaySocket | null = null;
  let reconnect: ReturnType<typeof setTimeout> | null = null;
  let closed = false;
  let seq = 0;
  /** The highest frame number this client has seen, across every socket it opened. */
  let lastSeq = 0;
  /** Console sockets, so close() stops their reconnect timers too. */
  const consoleSockets = new Set<() => void>();

  // The identity rides on every socket: the gateway addresses an app action to a client,
  // and the socket that has to carry it is whichever this window has open (PAN-142).
  const client = options.clientId ?? INSTANCE_ID;
  const socketUrl = (query: string): string =>
    `${base.replace(/^http/, 'ws')}/events?token=${encodeURIComponent(token)}&client=${encodeURIComponent(client)}${query}`;
  const socketImpl = (): GatewaySocketImpl => options.WebSocketImpl ?? (WebSocket as unknown as GatewaySocketImpl);

  /**
   * Reads one frame and reports its number to `seen`, which owns that socket's resume
   * point. A frame that is not JSON, or that names no event, is not a reason to tear
   * the socket down.
   *
   * The two sockets keep separate counters on purpose. Numbers come from one gateway
   * counter, but if a console socket advanced the main socket's resume point, a main
   * socket that was behind would ask to resume past frames it never saw — a silent
   * hole, which is the one thing numbering exists to prevent.
   */
  const readFrame = (data: unknown, seen: (seq: number) => void): Frame | null => {
    let frame: { seq?: unknown; event?: unknown; payload?: unknown };
    try {
      frame = JSON.parse(String(data));
    } catch {
      return null;
    }
    if (typeof frame.event !== 'string') return null;
    const payload = frame.payload ?? null;
    // A hole and a terminal reset carry the range instead of a number of their own;
    // the stream continues after `toSeq`, so that is where the next resume starts. Only
    // these two: any other payload's fields are the server's business, not numbering.
    if (typeof frame.seq === 'number') seen(frame.seq);
    else if ((frame.event === 'lagged' || frame.event === 'terminal-reset') && payload && typeof payload === 'object') {
      const to = Number((payload as MissedFrames).toSeq);
      if (Number.isFinite(to)) seen(to);
    }
    return { seq: typeof frame.seq === 'number' ? frame.seq : undefined, event: frame.event, payload };
  };
  const advance = (to: number) => { if (to > lastSeq) lastSeq = to; };

  const connect = () => {
    // A subscribe during the backoff window connects right away; the pending
    // timer must not fire afterwards and open a second, orphaned socket.
    if (reconnect) {
      clearTimeout(reconnect);
      reconnect = null;
    }
    // Resume after the last frame this client saw. Without a `since` the socket is a
    // live feed only, and a reconnect would silently start at "now" (PAN-105).
    socket = new (socketImpl())(socketUrl(lastSeq ? `&since=${lastSeq}` : ''));
    socket.onmessage = event => {
      const frame = readFrame(event.data, advance);
      if (!frame) return;
      for (const listener of listeners.get(frame.event) ?? []) listener(frame.payload);
    };
    socket.onclose = () => {
      socket = null;
      // Nothing is listening any more, or the caller closed: do not reopen.
      if (closed || listeners.size === 0) return;
      reconnect = setTimeout(() => {
        reconnect = null;
        if (!closed && listeners.size > 0) connect();
      }, delay);
    };
  };

  return {
    async call<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
      let response: Response;
      try {
        response = await doFetch(`${base}/rpc`, {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json',
            Authorization: `Bearer ${token}`,
            // Which client is asking: the gateway ties a turn to the client that started
            // it, so this header is what makes that turn's app actions addressable
            // (PAN-142).
            'x-pandacode-client': client,
          },
          body: JSON.stringify({ id: String(++seq), command, args }),
        });
      } catch (error) {
        throw `gateway_unreachable: ${error instanceof Error ? error.message : String(error)}`;
      }
      if (response.status === 401) throw 'unauthorized: the gateway rejected the token';
      let body: { ok?: boolean; result?: unknown; error?: string };
      try {
        body = await response.json();
      } catch {
        throw `gateway_error: ${response.status}`;
      }
      if (!response.ok || !body.ok) {
        // The server's own string, the same one the Tauri commands return.
        throw body.error ?? `gateway_error: ${response.status}`;
      }
      return body.result as T;
    },
    async subscribe(name: string, callback: (payload: any) => void) {
      if (!listeners.has(name)) listeners.set(name, new Set());
      listeners.get(name)!.add(callback);
      if (!socket && !closed) connect();
      return () => {
        const set = listeners.get(name);
        if (!set) return;
        set.delete(callback);
        // Prune empty event names so the reconnect guard in socket.onclose
        // sees zero listeners once the last one goes.
        if (set.size === 0) listeners.delete(name);
      };
    },
    watchTerminal(terminalId: string, onFrame: (event: any) => void, onReset: (missed: MissedFrames) => void) {
      // Its own socket, because the filter *is* the subscription: `?terminal=` is how
      // the gateway knows someone is watching, and `since=0` is how it replays the
      // ring a console that has just opened has never seen. Its own frame number, so
      // a dropped console socket resumes after what it already drew.
      let own: GatewaySocket | null = null;
      let timer: ReturnType<typeof setTimeout> | null = null;
      let stopped = false;
      let resumedFrom = 0;
      const advanceOwn = (to: number) => { if (to > resumedFrom) resumedFrom = to; };
      const open = () => {
        if (timer) { clearTimeout(timer); timer = null; }
        own = new (socketImpl())(socketUrl(`&terminal=${encodeURIComponent(terminalId)}&since=${resumedFrom}`));
        own.onmessage = event => {
          const frame = readFrame(event.data, advanceOwn);
          if (!frame) return;
          if (frame.event === 'terminal-event') onFrame(frame.payload);
          else if (frame.event === 'terminal-reset') onReset({ fromSeq: Number(frame.payload?.fromSeq ?? 0), toSeq: Number(frame.payload?.toSeq ?? 0) });
        };
        own.onclose = () => {
          own = null;
          if (stopped) return;
          timer = setTimeout(() => { timer = null; if (!stopped) open(); }, delay);
        };
      };
      open();
      const stop = () => {
        consoleSockets.delete(stop);
        stopped = true;
        if (timer) clearTimeout(timer);
        timer = null;
        own?.close();
        own = null;
      };
      consoleSockets.add(stop);
      return stop;
    },
    lastSeq() {
      return lastSeq;
    },
    upload(file: Blob, name: string, kind: 'image' | 'document', onProgress?: (sent: number, total: number) => void): Promise<UploadReceipt> {
      // The request body is the file itself, so the server streams it rather than
      // parsing anything; the name is display metadata in the query, and `call`
      // cannot carry bytes without base64.
      const Impl = options.XMLHttpRequestImpl ?? (XMLHttpRequest as unknown as UploadRequestImpl);
      return new Promise<UploadReceipt>((resolve, reject) => {
        const request = new Impl();
        request.open('POST', `${base}/upload?name=${encodeURIComponent(name)}&kind=${kind}`);
        request.setRequestHeader('Authorization', `Bearer ${token}`);
        request.upload.onprogress = event => {
          if (event.lengthComputable && onProgress) onProgress(event.loaded, event.total);
        };
        request.onerror = () => reject(`gateway_unreachable: the upload to ${base} failed`);
        request.onload = () => {
          if (request.status === 401) return reject('unauthorized: the gateway rejected the token');
          let body: Partial<UploadReceipt> & { error?: string } = {};
          try { body = JSON.parse(request.responseText); } catch { /* a non-JSON reply is reported below */ }
          if (request.status !== 200 || typeof body.id !== 'string') {
            return reject(body.error || `gateway_error: ${request.status}`);
          }
          resolve(body as UploadReceipt);
        };
        request.send(file);
      });
    },
    close() {
      closed = true;
      if (reconnect) clearTimeout(reconnect);
      reconnect = null;
      listeners.clear();
      // A console socket is this client's too: leaving one reconnecting after a
      // disconnect would knock on a gateway the person just left.
      for (const stop of [...consoleSockets]) stop();
      socket?.close();
      socket = null;
    },
  };
}
