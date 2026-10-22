// The remote transport: the same command surface the app already calls, over the
// gateway's JSON-RPC endpoint and events socket instead of Tauri IPC.
//
// `fetch` and `WebSocket` are injectable so this runs under `node --test` with a
// stub, and so the app can pass the platform's own implementations unchanged.
// Constructing a client performs no I/O: nothing is fetched or opened until the
// first `call` or `subscribe`.

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
  fetchImpl?: typeof fetch;
  WebSocketImpl?: GatewaySocketImpl;
  /** Delay before reopening a dropped events socket. Tests set it to a few ms. */
  reconnectDelayMs?: number;
};

export type GatewayClient = {
  call<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  subscribe(name: string, callback: (payload: any) => void): Promise<() => void>;
  close(): void;
};

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

  const connect = () => {
    // A subscribe during the backoff window connects right away; the pending
    // timer must not fire afterwards and open a second, orphaned socket.
    if (reconnect) {
      clearTimeout(reconnect);
      reconnect = null;
    }
    const Impl = options.WebSocketImpl ?? (WebSocket as unknown as GatewaySocketImpl);
    socket = new Impl(`${base.replace(/^http/, 'ws')}/events?token=${encodeURIComponent(token)}`);
    socket.onmessage = event => {
      let frame: { event?: string; payload?: unknown };
      try {
        frame = JSON.parse(String(event.data));
      } catch {
        // A frame that is not JSON is not a reason to tear the socket down.
        return;
      }
      if (typeof frame.event !== 'string') return;
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
          headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
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
    close() {
      closed = true;
      if (reconnect) clearTimeout(reconnect);
      reconnect = null;
      listeners.clear();
      socket?.close();
      socket = null;
    },
  };
}
