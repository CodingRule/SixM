// End-to-end: a real server with the bundled resources, and real clients
// running the client-side Lua the server hands them.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { startServer } from '../src/server/main.js';
import { SixMClient } from '../src/client/client.js';
import { silentLogger } from '../src/server/logger.js';
import { VERSION } from '../src/shared/version.js';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

/** Poll until `check` returns something truthy. */
async function until(check, what, timeoutMs = 3000) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const value = check();
    if (value) return value;
    if (Date.now() > end) assert.fail(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 10));
  }
}

async function boot(t, extraConfig = '') {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sixm-test-'));
  const configPath = path.join(dir, 'server.cfg');
  fs.writeFileSync(configPath, [
    'sv_hostname "Test Server"',
    'sv_maxclients 8',
    'sv_tickrate 60',
    'sv_streamdistance 100',
    'set welcome_message "hi there"',
    'ensure chat',
    'ensure welcome',
    'ensure hello',
    extraConfig,
  ].join('\n'));
  const serverLog = [];
  const logger = { ...silentLogger, info: (m) => serverLog.push(String(m)), warn: (m) => serverLog.push(String(m)), error: (m) => serverLog.push('ERROR ' + m), child() { return this; } };
  const app = await startServer({ configPath, resourcesDir: path.join(root, 'resources'), port: 0, logger });
  const clients = [];
  t.after(async () => {
    for (const c of clients) c.disconnect();
    await app.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  const join = async (name) => {
    const lines = [];
    const client = new SixMClient({ host: '127.0.0.1', port: app.server.port, name, print: (l) => lines.push(l) });
    client.lines = lines;
    clients.push(client);
    await client.connect();
    return client;
  };
  return { ...app, join, serverLog };
}

test('package.json and the VERSION constant agree', () => {
  const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
  assert.equal(VERSION, pkg.version);
});

test('all three bundled resources start without script errors', async (t) => {
  const { resources, serverLog } = await boot(t);
  assert.deepEqual([...resources.running.keys()], ['chat', 'welcome', 'hello']);
  assert.deepEqual(serverLog.filter((l) => l.startsWith('ERROR')), []);
});

test('a joining player is welcomed and announced to the others', async (t) => {
  const { join } = await boot(t);
  const alice = await join('Alice');
  await until(() => alice.lines.some((l) => l.includes('<server> hi there')), 'the welcome message');
  const bob = await join('Bob');
  await until(() => alice.lines.some((l) => l.includes('Bob joined (2 online)')), 'the join announcement');
  assert.equal(alice.players.get(bob.id).name, 'Bob');
  assert.equal(bob.players.get(alice.id).name, 'Alice');
});

test('chat goes client -> server Lua -> every client', async (t) => {
  const { join } = await boot(t);
  const alice = await join('Alice');
  const bob = await join('Bob');
  alice.executeCommand('say hello "big world"');
  await until(() => bob.lines.some((l) => l.includes('<Alice> hello big world')), 'chat at Bob');
  await until(() => alice.lines.some((l) => l.includes('<Alice> hello big world')), 'chat echoed to Alice');
});

test('net event round trip: hello -> ping -> pong', async (t) => {
  const { join } = await boot(t);
  const alice = await join('Alice');
  alice.executeCommand('/hello');
  await until(() => alice.lines.some((l) => /pong: round trip \d+ ms, 1 player/.test(l)), 'the pong');
});

test('a command the client does not own runs on the server for that player', async (t) => {
  const { join } = await boot(t);
  const alice = await join('Alice');
  alice.setCoords(1, 2, 3);
  await new Promise((r) => setTimeout(r, 100));
  alice.executeCommand('players');
  await until(() => alice.lines.some((l) => l.includes('#1 Alice') && l.includes('(1.0, 2.0, 3.0)')), 'the player list');
});

test('players cannot run restricted commands or unregistered net events', async (t) => {
  const { join, resources } = await boot(t);
  const alice = await join('Alice');
  const bob = await join('Bob');
  // `say` on the server is console-only; sent raw it must not be broadcast as "console".
  alice.emitNet('__sixm:command', 'say impersonating the console');
  // An event no resource registered as a net event must be ignored.
  let fired = 0;
  resources.running.get('hello').runtime.loadScript("AddEventHandler('secret:local', function() TriggerClientEvent('chat:message', -1, 'x', 'leaked') end)", '@test');
  alice.emitNet('secret:local');
  alice.executeCommand('say marker');
  await until(() => bob.lines.some((l) => l.includes('<Alice> marker')), 'the marker line');
  assert.ok(!bob.lines.some((l) => l.includes('impersonating')), 'restricted command ran for a player');
  assert.ok(!bob.lines.some((l) => l.includes('leaked')), 'unregistered net event was delivered');
  void fired;
});

test('positions sync between players and respect the stream distance', async (t) => {
  const { join } = await boot(t);
  const alice = await join('Alice');
  const bob = await join('Bob');
  alice.setCoords(10, 20, 30);
  alice.setHeading(90);
  bob.setCoords(12, 20, 30);
  const seen = await until(() => bob.players.get(alice.id)?.state, 'Alice streamed to Bob');
  assert.deepEqual([seen.x, seen.y, seen.z, seen.heading], [10, 20, 30, 90]);
  await until(() => alice.players.get(bob.id)?.state?.x === 12, 'Bob streamed to Alice');

  bob.setCoords(5000, 0, 0); // well past sv_streamdistance 100
  await until(() => alice.players.get(bob.id)?.state === null, 'Bob to leave Alice\'s range');
  await until(() => bob.players.get(alice.id)?.state === null, 'Alice to leave Bob\'s range');

  bob.setCoords(11, 20, 30);
  await until(() => alice.players.get(bob.id)?.state?.x === 11, 'Bob back in range');
});

test('a resource can refuse a connection in playerConnecting', async (t) => {
  const { join, server } = await boot(t);
  await assert.rejects(join('X'), /at least 2 characters/);
  assert.equal(server.players.size, 0);
});

test('leaving is announced, and stopping a resource removes its handlers', async (t) => {
  const { join, resources } = await boot(t);
  const alice = await join('Alice');
  const bob = await join('Bob');
  bob.disconnect();
  await until(() => alice.lines.some((l) => l.includes('Bob left')), 'the leave announcement');
  assert.equal(alice.players.has(bob.id), false);

  assert.equal(resources.stop('hello'), true);
  const before = alice.lines.length;
  alice.executeCommand('hello'); // client side still sends the ping; nobody answers
  await new Promise((r) => setTimeout(r, 150));
  assert.ok(!alice.lines.slice(before).some((l) => l.includes('pong')));
});

test('the HTTP endpoints describe the running server', async (t) => {
  const { join, server } = await boot(t);
  await join('Alice');
  const base = `http://127.0.0.1:${server.port}`;
  const info = await (await fetch(`${base}/info.json`)).json();
  assert.equal(info.name, 'Test Server');
  assert.equal(info.maxPlayers, 8);
  assert.deepEqual(info.resources, ['chat', 'welcome', 'hello']);
  const players = await (await fetch(`${base}/players.json`)).json();
  assert.equal(players.length, 1);
  assert.equal(players[0].name, 'Alice');
  assert.deepEqual(Object.keys(players[0]).sort(), ['id', 'name', 'ping']);
});
