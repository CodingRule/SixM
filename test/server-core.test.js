import { test } from 'node:test';
import assert from 'node:assert/strict';
import WebSocket from 'ws';
import { SixMServer } from '../src/server/core.js';
import { silentLogger } from '../src/server/logger.js';
import {
  PROTOCOL_VERSION,
  Msg,
  encodeMsg,
  encodeState,
  decodeSnapshot,
  emptyState,
} from '../src/shared/protocol.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(fn, what = 'condition', timeout = 2000) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const value = fn();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await sleep(4);
  }
}

/** Start a server on an ephemeral loopback port and stop it when the test ends. */
async function startServer(t, options = {}) {
  const server = new SixMServer({ port: 0, host: '127.0.0.1', logger: silentLogger, ...options });
  t.after(() => server.stop('test over'));
  const port = await server.start();
  return { server, port, url: `http://127.0.0.1:${port}` };
}

/** A raw protocol client that records everything it receives. */
function connect(t, port, wsOptions) {
  const ws = new WebSocket(`ws://127.0.0.1:${port}/`, wsOptions);
  const c = { ws, msgs: [], bins: [], closed: false, id: 0, welcome: null };
  ws.on('message', (data, isBinary) => {
    if (isBinary) c.bins.push(new Uint8Array(data));
    else c.msgs.push(JSON.parse(data.toString()));
  });
  ws.on('close', () => { c.closed = true; });
  ws.on('error', () => {});
  t.after(() => ws.terminate());
  c.opened = new Promise((resolve, reject) => {
    ws.once('open', resolve);
    ws.once('error', reject);
  });
  /** Wait for (and consume) the next control message of a type. */
  c.next = (type, timeout) => until(() => {
    const i = c.msgs.findIndex((m) => m.t === type);
    return i === -1 ? null : c.msgs.splice(i, 1)[0];
  }, `"${type}" message`, timeout);
  c.send = (type, fields) => ws.send(encodeMsg(type, fields));
  c.hello = (name, extra = {}) => c.send(Msg.HELLO, { proto: PROTOCOL_VERSION, name, build: 'test', ...extra });
  c.state = (seq, partial) => ws.send(encodeState(seq, { ...emptyState(), ...partial }));
  c.snapshots = () => c.bins.map((b) => decodeSnapshot(b));
  c.untilClosed = () => until(() => c.closed, 'socket close');
  return c;
}

async function join(t, port, name, wsOptions) {
  const c = connect(t, port, wsOptions);
  await c.opened;
  c.hello(name);
  c.welcome = await c.next(Msg.WELCOME);
  c.id = c.welcome.id;
  return c;
}

/** Connect, send a hello, expect a REJECT followed by the socket closing. */
async function expectReject(t, port, helloFields) {
  const c = connect(t, port);
  await c.opened;
  if (helloFields) c.send(Msg.HELLO, helloFields);
  const reject = await c.next(Msg.REJECT);
  await c.untilClosed();
  return reject;
}

test('handshake: WELCOME carries id, server info, existing players and resources', async (t) => {
  const { server, port } = await startServer(t, { name: 'Test Box', maxPlayers: 8, tickRate: 30 });
  server.setClientResources((player) => [{ name: 'hud', forPlayer: player.id }]);
  let joined = null;
  server.on('playerJoined', (p) => { joined = p; });

  const a = await join(t, port, '  Alice\u0007  ');
  assert.equal(a.welcome.id, 1);
  assert.equal(a.welcome.name, 'Alice');
  assert.equal(a.welcome.serverName, 'Test Box');
  assert.equal(a.welcome.tickRate, 30);
  assert.equal(a.welcome.maxPlayers, 8);
  assert.deepEqual(a.welcome.players, []);
  assert.deepEqual(a.welcome.resources, [{ name: 'hud', forPlayer: 1 }]);

  assert.equal(joined, server.getPlayer(1));
  assert.equal(joined.name, 'Alice');
  assert.match(joined.endpoint, /^127\.0\.0\.1:\d+$/);
  assert.deepEqual(joined.identifiers, ['ip:127.0.0.1']);
  assert.equal(joined.lastSeq, -1);
  assert.deepEqual(joined.state, emptyState());
  assert.ok(joined.joinedAt <= Date.now());
  assert.equal(server.players.size, 1);

  const b = await join(t, port, 'Bob');
  assert.equal(b.id, 2);
  assert.deepEqual(b.welcome.players, [{ id: 1, name: 'Alice' }]);

  // The ws-level ping sent at join yields a measured round trip.
  await until(() => typeof joined.ping === 'number' && joined._pingSentAt === 0, 'ping measurement');
  assert.ok(joined.ping >= 0 && joined.ping < 1000);
});

test('handshake: setClientResources accepts a plain array; /ws path works', async (t) => {
  const { server, port } = await startServer(t);
  server.setClientResources(['a', 'b']);
  const viaWs = new WebSocket(`ws://127.0.0.1:${port}/ws`);
  t.after(() => viaWs.terminate());
  const welcome = await new Promise((resolve, reject) => {
    viaWs.on('error', reject);
    viaWs.on('open', () => viaWs.send(encodeMsg(Msg.HELLO, { proto: PROTOCOL_VERSION, name: 'P' })));
    viaWs.on('message', (d) => resolve(JSON.parse(d.toString())));
  });
  assert.equal(welcome.t, Msg.WELCOME);
  assert.deepEqual(welcome.resources, ['a', 'b']);

  // Any other upgrade path is refused before a WebSocket exists.
  const bad = new WebSocket(`ws://127.0.0.1:${port}/nope`);
  const err = await new Promise((resolve) => bad.on('error', resolve));
  assert.match(String(err.message), /404/);
});

test('handshake: rejects wrong protocol, empty name, non-hello first frame', async (t) => {
  const { server, port } = await startServer(t);
  let connecting = 0;
  server.on('playerConnecting', () => { connecting++; });

  const proto = await expectReject(t, port, { proto: PROTOCOL_VERSION + 1, name: 'X' });
  assert.match(proto.reason, /protocol version/i);

  const empty = await expectReject(t, port, { proto: PROTOCOL_VERSION, name: ' \u0000\t ' });
  assert.match(empty.reason, /name/i);

  const missing = await expectReject(t, port, { proto: PROTOCOL_VERSION });
  assert.match(missing.reason, /name/i);

  const notHello = connect(t, port);
  await notHello.opened;
  notHello.send(Msg.EVENT, { name: 'x', args: [] });
  assert.match((await notHello.next(Msg.REJECT)).reason, /hello/i);
  await notHello.untilClosed();

  const binaryFirst = connect(t, port);
  await binaryFirst.opened;
  binaryFirst.state(1, {});
  assert.match((await binaryFirst.next(Msg.REJECT)).reason, /hello/i);

  assert.equal(connecting, 0, 'playerConnecting must not fire for invalid handshakes');
  assert.equal(server.players.size, 0);
});

test('handshake: rejects when the server is full', async (t) => {
  const { server, port } = await startServer(t, { maxPlayers: 1 });
  await join(t, port, 'Only');
  const reject = await expectReject(t, port, { proto: PROTOCOL_VERSION, name: 'Late' });
  assert.match(reject.reason, /full/i);
  assert.equal(server.players.size, 1);
});

test('handshake: no HELLO within the timeout is rejected', async (t) => {
  const { port } = await startServer(t, { helloTimeoutMs: 60 });
  const started = Date.now();
  const reject = await expectReject(t, port, null);
  assert.match(reject.reason, /timed out/i);
  assert.ok(Date.now() - started >= 50);
});

test('player ids: lowest free id is reused', async (t) => {
  const { server, port } = await startServer(t);
  const a = await join(t, port, 'A');
  const b = await join(t, port, 'B');
  const c = await join(t, port, 'C');
  assert.deepEqual([a.id, b.id, c.id], [1, 2, 3]);

  b.ws.close();
  await until(() => !server.players.has(2), 'player 2 to drop');
  const d = await join(t, port, 'D');
  assert.equal(d.id, 2);
  const e = await join(t, port, 'E');
  assert.equal(e.id, 4);
});

test('JOIN and LEAVE are broadcast to the other players', async (t) => {
  const { server, port } = await startServer(t);
  const dropped = [];
  server.on('playerDropped', (p, reason) => dropped.push([p.id, reason]));

  const a = await join(t, port, 'A');
  const b = await join(t, port, 'B');
  assert.deepEqual(await a.next(Msg.JOIN), { t: Msg.JOIN, id: 2, name: 'B' });
  assert.equal(b.msgs.some((m) => m.t === Msg.JOIN), false, 'no JOIN for yourself');

  b.ws.close();
  assert.deepEqual(await a.next(Msg.LEAVE), { t: Msg.LEAVE, id: 2, reason: 'disconnected' });
  assert.deepEqual(dropped, [[2, 'disconnected']]);
  assert.equal(server.getPlayer(2), undefined);
});

test('netEvent is emitted for client events; args default to []', async (t) => {
  const { server, port } = await startServer(t);
  const events = [];
  server.on('netEvent', (player, name, args) => events.push([player.id, name, args]));

  const a = await join(t, port, 'A');
  a.send(Msg.EVENT, { name: 'chat:say', args: ['hi', 2, { k: true }] });
  a.send(Msg.EVENT, { name: 'noargs' });
  await until(() => events.length === 2, 'two net events');
  assert.deepEqual(events, [
    [1, 'chat:say', ['hi', 2, { k: true }]],
    [1, 'noargs', []],
  ]);
});

test('emitNet delivers to one player or to everyone; broadcast honours exceptId', async (t) => {
  const { server, port } = await startServer(t);
  const a = await join(t, port, 'A');
  const b = await join(t, port, 'B');

  assert.equal(server.emitNet(a.id, 'only:a', [1]), true);
  assert.equal(server.emitNet(-1, 'all', ['x']), true);
  assert.equal(server.emitNet(99, 'nobody', []), false);

  assert.deepEqual(await a.next(Msg.EVENT), { t: Msg.EVENT, name: 'only:a', args: [1], src: 0 });
  assert.deepEqual(await a.next(Msg.EVENT), { t: Msg.EVENT, name: 'all', args: ['x'], src: 0 });
  assert.deepEqual(await b.next(Msg.EVENT), { t: Msg.EVENT, name: 'all', args: ['x'], src: 0 });

  const circular = [];
  circular.push(circular);
  assert.equal(server.emitNet(-1, 'bad', circular), false);

  server.broadcast(Msg.EVENT, { name: 'not:b', args: [], src: 0 }, b.id);
  server.getPlayer(b.id).send(Msg.EVENT, { name: 'marker', args: [], src: 0 });
  assert.equal((await a.next(Msg.EVENT)).name, 'not:b');
  assert.equal((await b.next(Msg.EVENT)).name, 'marker');
});

test('client PING is answered with PONG', async (t) => {
  const { port } = await startServer(t);
  const a = await join(t, port, 'A');
  a.send(Msg.PING, { ts: 12345 });
  assert.deepEqual(await a.next(Msg.PONG), { t: Msg.PONG, ts: 12345 });
});

test('state sync: each client receives the other in snapshots, never itself', async (t) => {
  const { server, port } = await startServer(t, { tickRate: 50 });
  const ticks = [];
  server.on('tick', (n, dt) => ticks.push([n, dt]));

  const a = await join(t, port, 'A');
  const b = await join(t, port, 'B');
  a.state(1, { x: 1, y: 2, z: 3, heading: 90, model: 0xdeadbeef, flags: 5 });
  b.state(1, { x: 10, y: 20, z: 30 });

  const forA = await until(() => a.snapshots().find((s) => s.entities.length === 1), 'snapshot for A');
  const forB = await until(() => b.snapshots().find((s) => s.entities.length === 1), 'snapshot for B');
  assert.equal(forA.entities[0].id, b.id);
  assert.deepEqual(forA.entities[0].state, { ...emptyState(), x: 10, y: 20, z: 30 });
  assert.equal(forB.entities[0].id, a.id);
  assert.deepEqual(forB.entities[0].state, {
    ...emptyState(), x: 1, y: 2, z: 3, heading: 90, model: 0xdeadbeef, flags: 5,
  });
  assert.ok(forA.tick >= 1);
  for (const s of [...a.snapshots(), ...b.snapshots()]) assert.ok(s, 'every binary frame decodes');
  assert.equal(a.snapshots().some((s) => s.entities.some((e) => e.id === a.id)), false);

  assert.ok(ticks.length > 0);
  assert.equal(ticks[0][0], 1);
  assert.equal(typeof ticks[0][1], 'number');
  assert.equal(server.tick, ticks[ticks.length - 1][0]);
});

test('state sync: players that never sent a state are not included', async (t) => {
  const { port } = await startServer(t, { tickRate: 100, streamDistance: 0 });
  const a = await join(t, port, 'A');
  await join(t, port, 'Silent');
  a.state(1, { x: 1 });
  await sleep(80);
  assert.equal(a.bins.length, 0, 'nothing to report: no snapshot at all');
});

test('tick loop keeps pace with the configured rate', async (t) => {
  const { server } = await startServer(t, { tickRate: 100 });
  const before = server.tick;
  await sleep(300);
  const ran = server.tick - before;
  assert.ok(ran >= 20 && ran <= 34, `expected about 30 ticks in 300 ms, got ${ran}`);
});

test('streamDistance culls far players and sends exactly one empty snapshot on leaving range', async (t) => {
  const { port } = await startServer(t, { tickRate: 100, streamDistance: 100 });
  const a = await join(t, port, 'A');
  const b = await join(t, port, 'B');
  a.state(1, { x: 0, y: 0, z: 0 });
  b.state(1, { x: 60, y: 0, z: 60 }); // ~85 away: in range
  await until(() => a.snapshots().some((s) => s.entities.length === 1), 'B in range of A');
  await until(() => b.snapshots().some((s) => s.entities.length === 1), 'A in range of B');

  b.state(2, { x: 60, y: 60, z: 60 }); // ~104 away in 3D: out of range
  await until(() => a.snapshots().some((s) => s.entities.length === 0), 'empty snapshot for A');
  await until(() => b.snapshots().some((s) => s.entities.length === 0), 'empty snapshot for B');
  await sleep(120); // a dozen more ticks

  for (const c of [a, b]) {
    const counts = c.snapshots().map((s) => s.entities.length);
    assert.equal(counts.filter((n) => n === 0).length, 1, 'exactly one empty snapshot');
    assert.equal(counts[counts.length - 1], 0, 'and nothing after it');
  }

  const seen = a.bins.length;
  b.state(3, { x: 10, y: 0, z: 0 }); // back in range
  await until(() => a.bins.length > seen, 'snapshots resume');
  assert.equal(a.snapshots().at(-1).entities[0].id, b.id);
});

test('streamDistance <= 0 means unlimited', async (t) => {
  const { port } = await startServer(t, { tickRate: 100, streamDistance: 0 });
  const a = await join(t, port, 'A');
  const b = await join(t, port, 'B');
  a.state(1, { x: 0 });
  b.state(1, { x: 1e6, y: -1e6, z: 1e6 });
  const snap = await until(() => a.snapshots().find((s) => s.entities.length === 1), 'far player visible');
  assert.equal(snap.entities[0].id, b.id);
});

test('stale and duplicate seq are ignored; uint32 wraparound is accepted', async (t) => {
  const { server, port } = await startServer(t);
  const events = [];
  server.on('netEvent', (p, name) => events.push(name));
  const a = await join(t, port, 'A');
  const player = server.getPlayer(a.id);
  const sync = async (label) => {
    a.send(Msg.EVENT, { name: label });
    await until(() => events.includes(label), `marker ${label}`);
  };

  a.state(5, { x: 1 });
  a.state(3, { x: 99 }); // older
  a.state(5, { x: 77 }); // duplicate
  await sync('m1');
  assert.equal(player.lastSeq, 5);
  assert.equal(player.state.x, 1);

  a.state(0xfffffff0, { x: 2 }); // more than half the range "ahead" == behind
  await sync('m2');
  assert.equal(player.lastSeq, 5);

  a.state(0x7fffffff, { x: 3 });
  a.state(0xfffffffe, { x: 4 });
  await sync('m3');
  assert.equal(player.lastSeq, 0xfffffffe);

  a.state(2, { x: 5 }); // wrapped past 0xFFFFFFFF
  await sync('m4');
  assert.equal(player.lastSeq, 2);
  assert.equal(player.state.x, 5);
});

test('malformed frames do not crash the server and eventually kick', async (t) => {
  const { server, port, url } = await startServer(t, { maxStrikes: 4 });
  const dropped = [];
  server.on('playerDropped', (p, reason) => dropped.push(reason));
  const events = [];
  server.on('netEvent', (p, name) => events.push(name));

  const a = await join(t, port, 'A');
  const witness = await join(t, port, 'W');

  a.ws.send('this is not json');
  a.ws.send(Buffer.from([1, 2, 3])); // wrong-size binary
  const nan = encodeState(1, { ...emptyState(), x: NaN });
  a.ws.send(nan); // non-finite state
  a.send(Msg.EVENT, { name: 'still-here' });
  await until(() => events.includes('still-here'), 'event after three strikes');
  assert.equal(server.players.has(a.id), true);
  assert.equal(server.getPlayer(a.id).lastSeq, -1, 'the NaN state was not applied');

  a.send(Msg.EVENT, { name: 'x'.repeat(200) }); // event name too long: 4th strike
  const kick = await a.next(Msg.KICK);
  assert.match(kick.reason, /malformed/i);
  await a.untilClosed();
  assert.deepEqual(dropped, ['too many malformed messages']);
  assert.equal(server.players.has(a.id), false);

  // Other strike sources, on a fresh connection.
  const b = await join(t, port, 'B');
  b.send(Msg.EVENT, { name: 'e', args: 'not-an-array' });
  b.send(Msg.EVENT, { name: 42 });
  b.send('made-up-type', {});
  b.send(Msg.HELLO, { proto: PROTOCOL_VERSION, name: 'again' });
  assert.match((await b.next(Msg.KICK)).reason, /malformed/i);

  // The server is still healthy.
  assert.equal(server.players.has(witness.id), true);
  assert.equal((await fetch(`${url}/dynamic.json`)).status, 200);
});

test('an oversized frame only costs the sender its connection', async (t) => {
  const { server, port } = await startServer(t);
  const a = await join(t, port, 'A');
  const w = await join(t, port, 'W');
  a.ws.send(JSON.stringify({ t: Msg.EVENT, name: 'big', args: ['x'.repeat(300 * 1024)] }));
  await a.untilClosed();
  await until(() => !server.players.has(a.id), 'oversized sender dropped');
  assert.equal(server.players.has(w.id), true);
});

test('a throwing listener does not break message handling', async (t) => {
  const { server, port } = await startServer(t);
  let good = 0;
  server.on('netEvent', () => { throw new Error('listener bug'); });
  server.on('netEvent', async () => { throw new Error('async listener bug'); });
  server.on('netEvent', () => { good++; });
  const a = await join(t, port, 'A');
  a.send(Msg.EVENT, { name: 'one' });
  a.send(Msg.EVENT, { name: 'two' });
  await until(() => good === 2, 'later listeners still run');
  assert.equal(server.players.has(a.id), true);
});

test('event flood is kicked with "event rate limit"', async (t) => {
  const { server, port } = await startServer(t, { eventRate: 1, eventBurst: 10 });
  const dropped = [];
  server.on('playerDropped', (p, reason) => dropped.push(reason));
  let received = 0;
  server.on('netEvent', () => { received++; });

  const a = await join(t, port, 'A');
  for (let i = 0; i < 40; i++) a.send(Msg.EVENT, { name: 'spam', args: [i] });
  const kick = await a.next(Msg.KICK);
  assert.equal(kick.reason, 'event rate limit');
  await a.untilClosed();
  assert.deepEqual(dropped, ['event rate limit']);
  assert.equal(received, 10, 'only the burst allowance got through');
});

test('state flood is dropped silently, not kicked', async (t) => {
  const { server, port } = await startServer(t, { tickRate: 10, stateRate: 5 });
  const events = [];
  server.on('netEvent', (p, name) => events.push(name));
  const a = await join(t, port, 'A');
  for (let seq = 1; seq <= 50; seq++) a.state(seq, { x: seq });
  a.send(Msg.EVENT, { name: 'done' });
  await until(() => events.includes('done'), 'marker');
  const player = server.getPlayer(a.id);
  assert.ok(player, 'still connected');
  assert.equal(player.lastSeq, 5, 'only the allowance was applied');
});

test('playerConnecting: synchronous reject', async (t) => {
  const { server, port } = await startServer(t);
  const seen = [];
  server.on('playerConnecting', (info, deferrals) => {
    seen.push(info);
    if (info.name === 'Banned') deferrals.reject('you are banned');
  });
  let joined = 0;
  server.on('playerJoined', () => { joined++; });

  const reject = await expectReject(t, port, { proto: PROTOCOL_VERSION, name: 'Banned' });
  assert.equal(reject.reason, 'you are banned');
  assert.equal(joined, 0);
  assert.equal(server.players.size, 0);
  assert.equal(seen[0].name, 'Banned');
  assert.match(seen[0].endpoint, /^127\.0\.0\.1:\d+$/);
  assert.deepEqual(seen[0].identifiers, ['ip:127.0.0.1']);

  const ok = await join(t, port, 'Fine');
  assert.equal(ok.id, 1, 'the rejected connection did not consume an id');
});

test('playerConnecting: async deferral can reject, accept, or time out', async (t) => {
  const { server, port } = await startServer(t, { deferTimeoutMs: 80 });
  server.on('playerConnecting', (info, deferrals) => {
    if (info.name === 'Slow-No') {
      const done = deferrals.defer();
      setTimeout(() => done('whitelist only'), 30);
    } else if (info.name === 'Slow-Yes') {
      const done = deferrals.defer();
      setTimeout(() => done(), 30);
    } else if (info.name === 'Never') {
      deferrals.defer();
    }
  });

  const no = connect(t, port);
  await no.opened;
  no.hello('Slow-No');
  await sleep(10);
  assert.equal(server.players.size, 0, 'held while deferred');
  assert.equal((await no.next(Msg.REJECT)).reason, 'whitelist only');
  await no.untilClosed();

  const yes = connect(t, port);
  await yes.opened;
  yes.hello('Slow-Yes');
  await sleep(10);
  assert.equal(server.players.size, 0, 'no slot assigned before the deferral completes');
  assert.equal((await yes.next(Msg.WELCOME)).id, 1);

  const never = connect(t, port);
  await never.opened;
  never.hello('Never');
  assert.match((await never.next(Msg.REJECT)).reason, /timed out/i);
  await never.untilClosed();
  assert.equal(server.players.size, 1);
});

test('a client that disconnects during a deferral never joins', async (t) => {
  const { server, port } = await startServer(t);
  let finish = null;
  server.on('playerConnecting', (info, deferrals) => { finish = deferrals.defer(); });
  let joined = 0;
  server.on('playerJoined', () => { joined++; });

  const c = connect(t, port);
  await c.opened;
  c.hello('Ghost');
  await until(() => finish, 'deferral');
  c.ws.terminate();
  await until(() => server._conns.size === 0, 'socket cleanup');
  finish();
  await sleep(20);
  assert.equal(joined, 0);
  assert.equal(server.players.size, 0);
});

test('kick(): client gets KICK, others get LEAVE, playerDropped fires, id is freed at once', async (t) => {
  const { server, port } = await startServer(t);
  const dropped = [];
  server.on('playerDropped', (p, reason) => dropped.push([p.id, reason]));
  const a = await join(t, port, 'A');
  const b = await join(t, port, 'B');

  const player = server.getPlayer(a.id);
  player.kick('bye');
  assert.equal(server.players.has(a.id), false, 'removed synchronously');
  assert.equal(player.connected, false);
  player.kick('twice'); // harmless
  assert.equal(player.send(Msg.EVENT, { name: 'x', args: [] }), false);

  assert.deepEqual(await a.next(Msg.KICK), { t: Msg.KICK, reason: 'bye' });
  await a.untilClosed();
  assert.deepEqual(await b.next(Msg.LEAVE), { t: Msg.LEAVE, id: a.id, reason: 'bye' });
  assert.deepEqual(dropped, [[a.id, 'bye']]);
});

test('heartbeat drops a client that stops answering pings', async (t) => {
  const { server, port } = await startServer(t, { heartbeatIntervalMs: 25, heartbeatTimeoutMs: 90 });
  const dropped = [];
  server.on('playerDropped', (p, reason) => dropped.push([p.name, reason]));

  const alive = await join(t, port, 'Alive');
  const dead = await join(t, port, 'Dead', { autoPong: false });
  await until(() => dropped.length > 0, 'timeout drop');
  assert.deepEqual(dropped, [['Dead', 'timed out']]);
  await dead.untilClosed();
  assert.equal((await alive.next(Msg.LEAVE)).reason, 'timed out');

  await sleep(120);
  assert.equal(server.players.size, 1, 'the responsive client stays');
  assert.equal(dropped.length, 1);
});

test('stop() kicks everyone, is idempotent and frees the port', async (t) => {
  const server = new SixMServer({ port: 0, host: '127.0.0.1', logger: silentLogger });
  const port = await server.start();
  assert.equal(server.port, port);
  assert.equal(await server.start(), port, 'start() twice returns the same port');

  const dropped = [];
  server.on('playerDropped', (p, reason) => dropped.push(reason));
  const a = await join(t, port, 'A');
  const idle = connect(t, port); // connected but never said hello
  await idle.opened;

  const first = server.stop('maintenance');
  const second = server.stop('ignored');
  assert.equal(first, second);
  await first;

  assert.deepEqual(await a.next(Msg.KICK), { t: Msg.KICK, reason: 'maintenance' });
  await a.untilClosed();
  await idle.untilClosed();
  assert.deepEqual(dropped, ['maintenance']);
  assert.equal(server.players.size, 0);
  await assert.rejects(fetch(`http://127.0.0.1:${port}/info.json`));
  await assert.rejects(server.start(), /stopped/);

  // The port can be bound again straight away.
  const again = new SixMServer({ port, host: '127.0.0.1', logger: silentLogger });
  assert.equal(await again.start(), port);
  const ticksAtStop = server.tick;
  await sleep(60);
  assert.equal(server.tick, ticksAtStop, 'the stopped server no longer ticks');
  await again.stop();
});

test('start() rejects when the port is taken; stop() before start() is fine', async (t) => {
  const { port } = await startServer(t);
  const clash = new SixMServer({ port, host: '127.0.0.1', logger: silentLogger });
  await assert.rejects(clash.start(), /EADDRINUSE/);
  await clash.stop();

  const unused = new SixMServer({ port: 0, logger: silentLogger });
  await unused.stop();
  assert.throws(() => new SixMServer({ maxPlayers: 70000 }), TypeError);
  assert.throws(() => new SixMServer({ tickRate: 0 }), TypeError);
});

test('HTTP: /info.json', async (t) => {
  const { server, url } = await startServer(t, {
    name: 'Info <b>Box</b>', maxPlayers: 12, tickRate: 25, build: '9.9.9', tags: ['rp', 'eu'], vars: { locale: 'en' },
  });
  server.setInfoVar('gametype', 'freeroam');
  server.setInfoVar('locale', null);
  server.setResourceNames(['chat', 'spawn']);

  const res = await fetch(`${url}/info.json`);
  assert.equal(res.status, 200);
  assert.match(res.headers.get('content-type'), /^application\/json/);
  assert.equal(res.headers.get('access-control-allow-origin'), '*');
  assert.equal(res.headers.get('x-content-type-options'), 'nosniff');
  assert.equal(res.headers.get('cache-control'), 'no-store');
  assert.deepEqual(await res.json(), {
    server: 'SixM',
    name: 'Info <b>Box</b>',
    build: '9.9.9',
    protocol: PROTOCOL_VERSION,
    maxPlayers: 12,
    tickRate: 25,
    tags: ['rp', 'eu'],
    vars: { gametype: 'freeroam' },
    resources: ['chat', 'spawn'],
  });
});

test('HTTP: /players.json and /dynamic.json expose no addresses or identifiers', async (t) => {
  const { url, port } = await startServer(t, { name: 'Dyn', maxPlayers: 5 });
  await join(t, port, 'Alice');
  await join(t, port, 'Bob');

  const res = await fetch(`${url}/players.json?cachebust=1`);
  assert.equal(res.status, 200);
  assert.match(res.headers.get('content-type'), /^application\/json/);
  assert.equal(res.headers.get('access-control-allow-origin'), '*');
  const text = await res.text();
  const players = JSON.parse(text);
  assert.equal(players.length, 2);
  for (const p of players) {
    assert.deepEqual(Object.keys(p).sort(), ['id', 'name', 'ping']);
    assert.equal(typeof p.ping, 'number');
  }
  assert.deepEqual(players.map((p) => [p.id, p.name]), [[1, 'Alice'], [2, 'Bob']]);
  assert.doesNotMatch(text, /127\.0\.0\.1|identifier|endpoint|ip:/);

  const dyn = await fetch(`${url}/dynamic.json`);
  assert.equal(dyn.status, 200);
  assert.match(dyn.headers.get('content-type'), /^application\/json/);
  const body = await dyn.json();
  assert.deepEqual(Object.keys(body).sort(), ['clients', 'maxClients', 'name', 'uptime']);
  assert.equal(body.name, 'Dyn');
  assert.equal(body.clients, 2);
  assert.equal(body.maxClients, 5);
  assert.ok(Number.isInteger(body.uptime) && body.uptime >= 0);
});

test('HTTP: status page is self-contained and escapes user-controlled text', async (t) => {
  const { url, port } = await startServer(t, { name: '<script>alert(1)</script> & "Co"' });
  await join(t, port, '<img src=x onerror=1>');

  const res = await fetch(`${url}/`);
  assert.equal(res.status, 200);
  assert.match(res.headers.get('content-type'), /^text\/html/);
  assert.equal(res.headers.get('x-content-type-options'), 'nosniff');
  assert.equal(res.headers.get('cache-control'), 'no-store');
  const html = await res.text();
  assert.ok(html.includes('&lt;script&gt;alert(1)&lt;/script&gt; &amp; &quot;Co&quot;'));
  assert.ok(html.includes('&lt;img src=x onerror=1&gt;'));
  assert.equal(html.includes('<script>alert(1)'), false);
  assert.equal(html.includes('<img'), false);
  assert.ok(html.includes('1 / 32'));
  assert.ok(html.includes('players.json'));
  assert.doesNotMatch(html, /(?:src|href)\s*=\s*["']?(?:https?:)?\/\//i, 'no external requests');
  assert.doesNotMatch(html, /127\.0\.0\.1/);
});

test('HTTP: 404 for unknown paths, 405 for other methods, HEAD has no body', async (t) => {
  const { url } = await startServer(t);

  const missing = await fetch(`${url}/nope`);
  assert.equal(missing.status, 404);
  assert.match(missing.headers.get('content-type'), /^application\/json/);
  assert.equal(missing.headers.get('x-content-type-options'), 'nosniff');
  assert.deepEqual(await missing.json(), { error: 'not found' });

  for (const method of ['POST', 'PUT', 'DELETE']) {
    const res = await fetch(`${url}/info.json`, { method, body: method === 'DELETE' ? undefined : '{}' });
    assert.equal(res.status, 405, method);
    assert.equal(res.headers.get('allow'), 'GET, HEAD');
    assert.match(res.headers.get('content-type'), /^application\/json/);
    await res.text();
  }

  const head = await fetch(`${url}/info.json`, { method: 'HEAD' });
  assert.equal(head.status, 200);
  assert.ok(Number(head.headers.get('content-length')) > 0);
  assert.equal(await head.text(), '');
});
