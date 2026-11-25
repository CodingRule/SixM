import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { parseConfig, applyConfig, loadConfig } from '../src/server/config.js';
import { createLogger, silentLogger } from '../src/server/logger.js';

test('parseConfig: commands, blank lines and line numbers', () => {
  const { commands, errors } = parseConfig('ensure chat\n\n   \nSET  foo   bar\r\n\tstart spawn');
  assert.deepEqual(errors, []);
  assert.deepEqual(commands, [
    { name: 'ensure', args: ['chat'], line: 1 },
    { name: 'set', args: ['foo', 'bar'], line: 4 },
    { name: 'start', args: ['spawn'], line: 5 },
  ]);
});

test('parseConfig: # and // comments, full-line and trailing', () => {
  const text = [
    '# a comment',
    '// another',
    'ensure chat # trailing',
    'ensure map // trailing',
    'sets url https://example.com/a#frag',
    'set motd "has # and // inside"',
    '   # indented comment',
  ].join('\n');
  const { commands, errors } = parseConfig(text);
  assert.deepEqual(errors, []);
  assert.deepEqual(commands, [
    { name: 'ensure', args: ['chat'], line: 3 },
    { name: 'ensure', args: ['map'], line: 4 },
    { name: 'sets', args: ['url', 'https://example.com/a#frag'], line: 5 },
    { name: 'set', args: ['motd', 'has # and // inside'], line: 6 },
  ]);
});

test('parseConfig: quoted strings and escapes', () => {
  const { commands, errors } = parseConfig(
    'sv_hostname "My \\"Cool\\" Server"\nset path "C:\\\\games\\\\sixm"\nset empty ""\nset glued pre"mid dle"post\n',
  );
  assert.deepEqual(errors, []);
  assert.deepEqual(commands.map((c) => c.args), [
    ['My "Cool" Server'],
    ['path', 'C:\\games\\sixm'],
    ['empty', ''],
    ['glued', 'premid dlepost'],
  ]);
});

test('parseConfig: unterminated quote is reported with its line and skipped', () => {
  const { commands, errors } = parseConfig('ensure a\nset motd "oops\nensure b\n');
  assert.deepEqual(commands.map((c) => c.args[0]), ['a', 'b']);
  assert.equal(errors.length, 1);
  assert.equal(errors[0].line, 2);
  assert.match(errors[0].message, /unterminated/);
});

test('parseConfig: BOM and non-string input', () => {
  assert.deepEqual(parseConfig('\ufeffensure a').commands, [{ name: 'ensure', args: ['a'], line: 1 }]);
  assert.equal(parseConfig(undefined).errors.length, 1);
  assert.deepEqual(parseConfig('').commands, []);
});

test('applyConfig: a typical server.cfg', () => {
  const { commands, errors } = parseConfig(`
endpoint_add_tcp "0.0.0.0:30125"
endpoint_add_udp "0.0.0.0:30125"
sv_hostname "Leonida Nights"
sv_maxclients 48
set sv_tickrate 30
set sv_streamdistance 350.5
sets tags "roleplay, eu ,, drift"
sets locale en-GB
setr voice_enabled true
set mysql_string "server=localhost;db=x"
ensure chat
start spawn
ensure chat
ensure hud
stop hud
frobnicate 1 2
`);
  assert.deepEqual(errors, []);
  const cfg = applyConfig(commands);
  assert.deepEqual(cfg.errors, []);
  assert.deepEqual(cfg.settings, {
    port: 30125,
    host: '0.0.0.0',
    name: 'Leonida Nights',
    maxPlayers: 48,
    tickRate: 30,
    streamDistance: 350.5,
    tags: ['roleplay', 'eu', 'drift'],
  });
  assert.deepEqual(cfg.resources, ['chat', 'spawn']);
  assert.deepEqual(cfg.infoVars, { tags: 'roleplay, eu ,, drift', locale: 'en-GB' });
  assert.ok(cfg.convars instanceof Map);
  assert.equal(cfg.convars.get('mysql_string'), 'server=localhost;db=x');
  assert.equal(cfg.convars.get('voice_enabled'), 'true');
  assert.equal(cfg.convars.get('sv_hostname'), 'Leonida Nights');
  assert.equal(cfg.convars.get('locale'), 'en-GB');
  assert.deepEqual(cfg.replicated, ['voice_enabled']);
  assert.deepEqual(cfg.unknown, [{ name: 'frobnicate', args: ['1', '2'], line: 17 }]);
  assert.equal('mysql_string' in cfg.infoVars, false, 'private convars are not public');
});

test('applyConfig: settings stay undefined unless configured; sv_port and ipv6 endpoint', () => {
  assert.deepEqual(applyConfig([]).settings, {});
  assert.deepEqual(applyConfig(parseConfig('sv_port 4000').commands).settings, { port: 4000 });
  assert.deepEqual(applyConfig(parseConfig('endpoint_add_tcp "[::1]:4001"').commands).settings, { host: '::1', port: 4001 });
  assert.deepEqual(applyConfig(parseConfig('endpoint_add_tcp 4002').commands).settings, { port: 4002 });
  assert.deepEqual(applyConfig(parseConfig('set a one two three').commands).convars.get('a'), 'one two three');
});

test('applyConfig: invalid values are reported with line numbers, never thrown', () => {
  const { commands } = parseConfig([
    'sv_maxclients lots',
    'sv_maxclients 0',
    'set sv_tickrate 12.5',
    'sv_port 70000',
    'endpoint_add_tcp "0.0.0.0:http"',
    'sv_streamdistance far',
    'set lonely',
    'ensure',
    'ensure a b',
    'sv_hostname',
    'sv_maxclients 16',
  ].join('\n'));
  const cfg = applyConfig(commands);
  assert.deepEqual(cfg.errors.map((e) => e.line), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
  for (const e of cfg.errors) assert.equal(typeof e.message, 'string');
  assert.match(cfg.errors[0].message, /sv_maxclients/);
  assert.match(cfg.errors[2].message, /sv_tickrate/);
  assert.match(cfg.errors[4].message, /endpoint_add_tcp/);
  assert.deepEqual(cfg.settings, { maxPlayers: 16 }, 'only the valid value was applied');
  assert.deepEqual(cfg.resources, []);
  assert.doesNotThrow(() => applyConfig(null));
});

test('loadConfig: reads a file and merges parse and validation errors in line order', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'sixm-cfg-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const file = join(dir, 'server.cfg');
  await writeFile(file, 'sv_hostname "From Disk"\nsv_maxclients nope\nset x "broken\nensure core\n');

  const cfg = await loadConfig(file);
  assert.equal(cfg.path, file);
  assert.equal(cfg.settings.name, 'From Disk');
  assert.deepEqual(cfg.resources, ['core']);
  assert.deepEqual(cfg.errors.map((e) => e.line), [2, 3]);
  assert.equal(cfg.commands.length, 3);
  await assert.rejects(loadConfig(join(dir, 'missing.cfg')), /ENOENT/);
});

test('logger: plain output when not a TTY, colour only on a TTY without NO_COLOR, level filtering', (t) => {
  const sink = (isTTY) => {
    const lines = [];
    return { isTTY, lines, write: (s) => lines.push(s) };
  };
  const saved = process.env.NO_COLOR;
  t.after(() => {
    if (saved === undefined) delete process.env.NO_COLOR;
    else process.env.NO_COLOR = saved;
  });
  delete process.env.NO_COLOR;

  const pipeOut = sink(false);
  const pipeErr = sink(false);
  const plain = createLogger({ level: 'info', name: 'srv', stdout: pipeOut, stderr: pipeErr });
  plain.debug('hidden');
  plain.info('hello', { a: 1 });
  plain.error('bad', new Error('boom'));
  assert.equal(pipeOut.lines.length, 1);
  assert.match(pipeOut.lines[0], /^\d\d:\d\d:\d\d INFO  \[srv\] hello \{"a":1\}\n$/);
  assert.equal(pipeErr.lines.length, 1);
  assert.match(pipeErr.lines[0], /ERROR \[srv\] bad Error: boom/);
  assert.equal(pipeOut.lines[0].includes('\x1b['), false);

  const tty = sink(true);
  const coloured = createLogger({ level: 'debug', stdout: tty, stderr: tty });
  coloured.warn('careful');
  assert.ok(tty.lines[0].includes('\x1b['));

  process.env.NO_COLOR = '1';
  coloured.warn('careful');
  assert.equal(tty.lines[1].includes('\x1b['), false);

  plain.level = 'silent';
  plain.error('nothing');
  assert.equal(pipeErr.lines.length, 1);
  assert.doesNotThrow(() => silentLogger.child('x').info('ignored'));
});
