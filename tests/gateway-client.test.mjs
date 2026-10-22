// The remote transport in front of the gateway's JSON-RPC and events socket.
// `fetch` and `WebSocket` are injected, so these run in Node with no browser.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createGatewayClient } from '../src/gateway-client.ts';

const ok = (result, id = '1') => ({ ok: true, json: async () => ({ id, ok: true, result }) });
const fail = (error, id = '1') => ({ ok: true, json: async () => ({ id, ok: false, error }) });

class FakeSocket {
  static instances = [];
  constructor(url) {
    this.url = url;
    this.sent = [];
    FakeSocket.instances.push(this);
  }
  message(frame) { this.onmessage?.({ data: JSON.stringify(frame) }); }
  closed() { this.onclose?.({}); }
  close() { this.closedByClient = true; this.onclose?.({}); }
}

const sockets = () => {
  FakeSocket.instances = [];
  return FakeSocket;
};

test('call posts the envelope and unwraps the result', async () => {
  const seen = [];
  const fetchImpl = async (url, init) => {
    seen.push({ url, body: JSON.parse(init.body), auth: init.headers.Authorization, method: init.method });
    return ok([{ id: 'p1', name: 'repo' }]);
  };
  const client = createGatewayClient({ url: 'http://gw.test', token: 't', fetchImpl, WebSocketImpl: sockets() });
  const projects = await client.call('list_projects', {});
  assert.equal(projects[0].name, 'repo');
  assert.equal(seen[0].method, 'POST');
  assert.equal(seen[0].url, 'http://gw.test/rpc');
  assert.equal(seen[0].body.command, 'list_projects');
  assert.equal(seen[0].body.args.deep, undefined);
  assert.equal(typeof seen[0].body.id, 'string');
  assert.equal(seen[0].auth, 'Bearer t');
});

test('a failed envelope rejects with the server error string', async () => {
  const fetchImpl = async () => fail('unsupported_command: vm_start');
  const client = createGatewayClient({ url: 'http://gw.test', token: 't', fetchImpl, WebSocketImpl: sockets() });
  await assert.rejects(() => client.call('vm_start', {}), /unsupported_command: vm_start/);
});

test('a 401 is its own message, not the server body', async () => {
  const fetchImpl = async () => ({ status: 401, ok: false, json: async () => ({}) });
  const client = createGatewayClient({ url: 'http://gw.test', token: 'stale', fetchImpl, WebSocketImpl: sockets() });
  await assert.rejects(() => client.call('list_projects', {}), /unauthorized/i);
});

test('an HTTP failure that is not an envelope is reported with its status', async () => {
  const fetchImpl = async () => ({ status: 502, ok: false, json: async () => { throw new Error('not json'); } });
  const client = createGatewayClient({ url: 'http://gw.test', token: 't', fetchImpl, WebSocketImpl: sockets() });
  await assert.rejects(() => client.call('list_projects', {}), /502/);
});

test('a network failure says the gateway is unreachable', async () => {
  const fetchImpl = async () => { throw new Error('connect ECONNREFUSED'); };
  const client = createGatewayClient({ url: 'http://gw.test', token: 't', fetchImpl, WebSocketImpl: sockets() });
  await assert.rejects(() => client.call('list_projects', {}), /ECONNREFUSED|unreachable/i);
});

test('building a client touches nothing until it is used', async () => {
  // The app must not phone home at import or construction time.
  let fetched = 0;
  const WebSocketImpl = sockets();
  const client = createGatewayClient({ url: 'http://gw.test', token: 't', fetchImpl: async () => { fetched++; return ok(null); }, WebSocketImpl });
  assert.equal(fetched, 0);
  assert.equal(WebSocketImpl.instances.length, 0);
  assert.equal(typeof client.close, 'function');
  client.close();
});

test('subscribe opens one socket, dispatches by event name, and unsubscribes', async () => {
  const WebSocketImpl = sockets();
  const client = createGatewayClient({ url: 'http://gw.test', token: 't', fetchImpl: async () => ok(null), WebSocketImpl });
  const seen = [];
  const off = await client.subscribe('sessions-changed', payload => seen.push(payload));
  const other = [];
  await client.subscribe('agent-event', payload => other.push(payload));

  assert.equal(WebSocketImpl.instances.length, 1);
  const socket = WebSocketImpl.instances[0];
  assert.equal(socket.url, 'ws://gw.test/events?token=t');

  socket.message({ event: 'sessions-changed', payload: null });
  socket.message({ event: 'agent-event', payload: { kind: 'text' } });
  assert.deepEqual(seen, [null]);
  assert.deepEqual(other, [{ kind: 'text' }]);

  off();
  socket.message({ event: 'sessions-changed', payload: null });
  assert.deepEqual(seen, [null], 'an unsubscribed listener stops receiving');
  client.close();
});

test('an https url becomes wss for the events socket', async () => {
  const WebSocketImpl = sockets();
  const client = createGatewayClient({ url: 'https://gw.test/', token: 'a b', fetchImpl: async () => ok(null), WebSocketImpl });
  await client.subscribe('sessions-changed', () => {});
  assert.equal(WebSocketImpl.instances[0].url, 'wss://gw.test/events?token=a%20b');
  client.close();
});

test('a dropped socket reconnects, and close() stops it', async () => {
  const WebSocketImpl = sockets();
  const client = createGatewayClient({ url: 'http://gw.test', token: 't', fetchImpl: async () => ok(null), WebSocketImpl, reconnectDelayMs: 1 });
  await client.subscribe('sessions-changed', () => {});
  assert.equal(WebSocketImpl.instances.length, 1);

  WebSocketImpl.instances[0].closed();
  await new Promise(resolve => setTimeout(resolve, 25));
  assert.equal(WebSocketImpl.instances.length, 2, 'a dropped socket is reopened');

  client.close();
  WebSocketImpl.instances[1].closed();
  await new Promise(resolve => setTimeout(resolve, 25));
  assert.equal(WebSocketImpl.instances.length, 2, 'close() cancels reconnecting');
});
