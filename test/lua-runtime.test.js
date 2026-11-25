import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { LuaRuntime, setBootstrapSource } from '../src/shared/lua/runtime.js';

// A runtime wired to capture arrays and a fake clock. `log(...)` in Lua
// appends its (marshaled) arguments to `logged`.
function make({ resourceName = 'test', side = 'server', host = {} } = {}) {
  const lines = [];
  const errors = [];
  const logged = [];
  const clock = { t: 0 };
  const rt = new LuaRuntime({
    resourceName,
    side,
    print: (line) => lines.push(line),
    onError: (err, context) => errors.push({ err, context }),
    host: { now: () => clock.t, ...host },
  });
  rt.registerGlobal('log', (...values) => { logged.push(values.length === 1 ? values[0] : values); });
  rt.registerGlobal('echo', (value) => value);
  return { rt, lines, errors, logged, clock };
}

// Loads a script and fails the test with the Lua error if it did not run cleanly.
function run(env, code, chunkName = 'test.lua') {
  const ok = env.rt.loadScript(code, chunkName);
  assert.deepEqual(env.errors.map((e) => e.err.message), [], 'script raised errors');
  assert.equal(ok, true);
}

// ------------------------------------------------------------------ marshaling

test('marshaling: primitives, integers vs floats', () => {
  const env = make();
  run(env, `
    log(echo(nil) == nil, echo(true), echo(false))
    log(math.type(echo(3)), math.type(echo(3.5)), echo(-7), echo(0.25))
    local big = echo(2^40)
    log(math.type(big), big == 1099511627776)
  `);
  assert.deepEqual(env.logged, [
    [true, true, false],
    ['integer', 'float', -7, 0.25],
    ['float', true],
  ]);

  env.rt.setGlobal('fromJs', { int: 42, float: 1.5, huge: 2 ** 40, nothing: null });
  run(env, `log(math.type(fromJs.int), math.type(fromJs.float), math.type(fromJs.huge), fromJs.huge, fromJs.nothing == nil)`);
  assert.deepEqual(env.logged[3], ['integer', 'float', 'float', 2 ** 40, true]);
});

test('marshaling: unicode strings and raw bytes', () => {
  const env = make();
  const text = 'héllo wörld ✓ 日本語 😀';
  env.rt.setGlobal('text', text);
  env.rt.setGlobal('blob', Buffer.from([0, 255, 65]));
  run(env, `
    log(text, #text)
    log(echo('añb€'), #blob, blob:byte(1), blob:byte(2), blob:byte(3))
  `);
  assert.deepEqual(env.logged, [
    [text, Buffer.byteLength(text, 'utf8')],
    ['añb€', 3, 0, 255, 65],
  ]);
});

test('marshaling: tables to JS (sequence, object, empty, sparse, nested)', () => {
  const env = make();
  run(env, `
    log({ 1, 2, 3 })
    log({ name = 'x', list = { 'a', 'b' }, nested = { deep = { ok = true } } })
    log({})
    log({ [1] = 'a', [3] = 'c' })
    log({ 'a', extra = 1 })
    log({ [1.5] = 'f', [true] = 'b' })
  `);
  assert.deepEqual(env.logged, [
    [1, 2, 3],
    { name: 'x', list: ['a', 'b'], nested: { deep: { ok: true } } },
    {},
    { 1: 'a', 3: 'c' },
    { 1: 'a', extra: 1 },
    { 1.5: 'f', true: 'b' },
  ]);
  assert.ok(Array.isArray(env.logged[0]));
  assert.ok(!Array.isArray(env.logged[2]));
  assert.ok(!Array.isArray(env.logged[3]));
});

test('marshaling: JS to Lua round trips nested data', () => {
  const env = make();
  const value = { players: [{ id: 1, name: 'a' }, { id: 2, name: 'b' }], meta: { tags: ['x', 'y'], n: 2.5 } };
  env.rt.setGlobal('data', value);
  run(env, `
    log(#data.players, data.players[2].name, data.meta.tags[1], data.meta.n)
    log(echo(data))
    log(echo({ [5] = 'five', [9] = 'nine' })[9], echo({ [5] = 'five' })['5'] == nil)
  `);
  assert.deepEqual(env.logged[0], [2, 'b', 'x', 2.5]);
  assert.deepEqual(env.logged[1], value);
  // Integer keys survive Lua -> JS -> Lua even though JS object keys are strings.
  assert.deepEqual(env.logged[2], ['nine', true]);
});

test('marshaling: cycles and excessive depth raise clear errors', () => {
  const env = make();
  run(env, `
    local t = {} ; t.self = t
    local ok, err = pcall(log, t)
    log(ok, err)
    local deep = {} ; local cur = deep
    for i = 1, 80 do cur.next = {} ; cur = cur.next end
    ok, err = pcall(log, deep)
    log(ok, err)
    local shared = { 1 }
    log({ a = shared, b = shared })
  `);
  assert.equal(env.logged[0][0], false);
  assert.match(env.logged[0][1], /cycle/);
  assert.equal(env.logged[1][0], false);
  assert.match(env.logged[1][1], /deeper than 64/);
  assert.deepEqual(env.logged[2], { a: [1], b: [1] }); // shared, not cyclic

  const cyclic = {};
  cyclic.self = cyclic;
  assert.throws(() => env.rt.setGlobal('bad', cyclic), /cycle/);
  run(env, `log(bad == nil)`);
  assert.equal(env.logged[3], true);
});

test('marshaling: functions cross in both directions', () => {
  const env = make();
  let luaFn;
  env.rt.registerGlobal('keep', (fn) => { luaFn = fn; });
  env.rt.registerGlobal('getAdder', () => (a, b) => a + b);
  run(env, `
    keep(function(a, b) return a * b, 'ignored' end)
    log(getAdder()(2, 3))
    local function original() end
    log(echo(original) == original)
  `);
  assert.equal(typeof luaFn, 'function');
  assert.equal(luaFn(6, 7), 42);
  assert.deepEqual(env.logged, [5, true]);

  run(env, `keep(function() error('callback failed') end)`);
  assert.throws(() => luaFn(), /callback failed/);

  env.rt.destroy();
  assert.equal(luaFn(1, 2), undefined);
});

// -------------------------------------------------------------- registerGlobal

test('registerGlobal: multi returns, plain arrays, JS exceptions -> pcall', () => {
  const env = make();
  env.rt.registerGlobal('pair', () => LuaRuntime.multi(1, 'two', { three: 3 }));
  env.rt.registerGlobal('list', () => [1, 2, 3]);
  env.rt.registerGlobal('nothing', () => {});
  env.rt.registerGlobal('explode', () => { throw new Error('kaboom from JS'); });
  env.rt.registerGlobal('explodeString', () => { throw 'plain string'; }); // eslint-disable-line no-throw-literal
  run(env, `
    local a, b, c = pair()
    log(a, b, c.three, select('#', pair()))
    local l = list()
    log(type(l), #l, select('#', list()), select('#', nothing()))
    log(pcall(explode))
    log(pcall(explodeString))
    log('still alive')
  `);
  assert.deepEqual(env.logged, [
    [1, 'two', 3, 3],
    ['table', 3, 1, 0],
    [false, 'kaboom from JS'],
    [false, 'plain string'],
    'still alive',
  ]);
});

test('registerGlobal: uncaught JS exception surfaces through onError, not a throw', () => {
  const env = make();
  env.rt.registerGlobal('explode', () => { throw new Error('kaboom'); });
  assert.equal(env.rt.loadScript('explode()', 'bad.lua'), false);
  assert.equal(env.errors.length, 1);
  assert.match(env.errors[0].err.message, /kaboom/);
  assert.equal(env.errors[0].context, 'load:bad.lua');
});

// ------------------------------------------------------------------ loadScript

test('loadScript: syntax and runtime errors are reported with a traceback', () => {
  const env = make();
  assert.equal(env.rt.loadScript('local = 5', 'syntax.lua'), false);
  assert.match(env.errors[0].err.message, /syntax\.lua:1:/);

  assert.equal(env.rt.loadScript('local function f() error("oops") end\nf()', 'runtime.lua'), false);
  assert.match(env.errors[1].err.message, /runtime\.lua:1: oops/);
  assert.match(env.errors[1].err.message, /stack traceback/);
  assert.equal(env.errors[1].context, 'load:runtime.lua');

  assert.equal(env.rt.loadScript('x = 1'), true);
});

test('print is tab-joined and routed to the print callback', () => {
  const env = make();
  run(env, `print('a', 1, 2.5, nil, true) print() print(vector3(1, 2, 3))`);
  assert.deepEqual(env.lines, ['a\t1\t2.5\tnil\ttrue', '', 'vector3(1.0, 2.0, 3.0)']);
});

test('environment globals', () => {
  const server = make({ resourceName: 'my-res', side: 'server' });
  run(server, `log(GetCurrentResourceName(), IsDuplicityVersion(), IS_SERVER, IS_CLIENT)`);
  assert.deepEqual(server.logged[0], ['my-res', true, true, false]);

  const client = make({ side: 'client' });
  run(client, `log(IsDuplicityVersion(), IS_SERVER, IS_CLIENT)`);
  assert.deepEqual(client.logged[0], [false, false, true]);

  assert.throws(() => new LuaRuntime({ resourceName: 'x', side: 'sideways' }), /side/);
});

// --------------------------------------------------------------------- threads

test('threads: Wait ordering is driven by tick', () => {
  const env = make();
  run(env, `
    Citizen.CreateThread(function()
      log('a0') ; Citizen.Wait(100) ; log('a100') ; Wait(0) ; log('a-next')
    end)
    CreateThread(function()
      log('b0') ; Wait(50) ; log('b50') ; Wait(50) ; log('b100')
    end)
  `);
  assert.deepEqual(env.logged, ['a0', 'b0']); // threads start immediately
  assert.equal(env.rt.pendingThreads, 2);

  env.rt.tick(49);
  assert.deepEqual(env.logged, ['a0', 'b0']);
  env.rt.tick(50);
  assert.deepEqual(env.logged, ['a0', 'b0', 'b50']);
  env.rt.tick(100);
  // Same deadline: the one that started waiting first resumes first.
  // Wait(0) does not resume within the same tick.
  assert.deepEqual(env.logged, ['a0', 'b0', 'b50', 'a100', 'b100']);
  assert.equal(env.rt.pendingThreads, 1);
  env.rt.tick(100);
  assert.deepEqual(env.logged.at(-1), 'a-next');
  assert.equal(env.rt.pendingThreads, 0);
});

test('threads: Wait outside a thread is a clear error', () => {
  const env = make();
  assert.equal(env.rt.loadScript('Wait(10)', 'main.lua'), false);
  assert.match(env.errors[0].err.message, /main\.lua:1: Wait\(\) can only be called inside a thread/);
});

test('threads: tick without an argument uses host.now()', () => {
  const env = make();
  run(env, `CreateThread(function() Wait(10) log('woke') end)`);
  env.clock.t = 9;
  env.rt.tick();
  assert.deepEqual(env.logged, []);
  env.clock.t = 10;
  env.rt.tick();
  assert.deepEqual(env.logged, ['woke']);
});

test('SetTimeout fires once when due', () => {
  const env = make();
  run(env, `
    Citizen.SetTimeout(30, function() log('late') end)
    SetTimeout(10, function() log('early') ; Wait(5) ; log('early+5') end)
  `);
  assert.equal(env.rt.pendingThreads, 2);
  env.rt.tick(9);
  assert.deepEqual(env.logged, []);
  env.rt.tick(10);
  assert.deepEqual(env.logged, ['early']);
  env.rt.tick(30);
  assert.deepEqual(env.logged, ['early', 'early+5', 'late']);
  env.rt.tick(1000);
  assert.equal(env.logged.length, 3);
  assert.equal(env.rt.pendingThreads, 0);
});

test('threads: an error kills only that thread and is reported with a traceback', () => {
  const env = make();
  assert.equal(env.rt.loadScript(`
    CreateThread(function()
      Wait(10)
      error('boom')
    end)
    CreateThread(function()
      while true do Wait(10) ; log('healthy') end
    end)
    CreateThread(function() local x = nil ; x.field = 1 end)
  `, 'threads.lua'), true);
  // The third thread failed synchronously, during loadScript.
  assert.equal(env.errors.length, 1);
  assert.equal(env.errors[0].context, 'thread');
  assert.match(env.errors[0].err.message, /attempt to index a nil value/);

  env.rt.tick(10);
  env.rt.tick(20);
  assert.equal(env.errors.length, 2);
  assert.equal(env.errors[1].context, 'thread');
  assert.match(env.errors[1].err.message, /threads\.lua:4: boom/);
  assert.match(env.errors[1].err.message, /stack traceback/);
  assert.deepEqual(env.logged, ['healthy', 'healthy']);
  assert.equal(env.rt.pendingThreads, 1);
});

test('threads: an onError callback that throws does not break the scheduler', () => {
  const logged = [];
  const rt = new LuaRuntime({
    resourceName: 'r', side: 'server',
    onError: () => { throw new Error('handler is broken'); },
    host: { now: () => 0 },
  });
  rt.registerGlobal('log', (v) => logged.push(v));
  rt.loadScript(`
    CreateThread(function() Wait(1) error('x') end)
    CreateThread(function() Wait(1) log('second still runs') end)
  `);
  rt.tick(1);
  assert.deepEqual(logged, ['second still runs']);
});

test('stress: 2,000 waiting threads finish quickly', () => {
  const env = make();
  run(env, `
    finished = 0
    for i = 1, 2000 do
      CreateThread(function()
        for step = 1, 5 do Wait(i % 50) end
        finished = finished + 1
      end)
    end
  `);
  assert.equal(env.rt.pendingThreads, 2000);
  const started = process.hrtime.bigint();
  let ticks = 0;
  for (let now = 0; now <= 300 && env.rt.pendingThreads > 0; now++, ticks++) env.rt.tick(now);
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
  run(env, `log(finished)`);
  assert.equal(env.logged[0], 2000);
  assert.equal(env.rt.pendingThreads, 0);
  assert.ok(elapsedMs < 5000, `10,000 resumes over ${ticks} ticks took ${elapsedMs.toFixed(0)}ms`);

  // Idle ticks must be cheap.
  const idleStart = process.hrtime.bigint();
  for (let i = 0; i < 20000; i++) env.rt.tick(1000 + i);
  const idleMs = Number(process.hrtime.bigint() - idleStart) / 1e6;
  assert.ok(idleMs < 2000, `20,000 idle ticks took ${idleMs.toFixed(0)}ms`);
});

// ---------------------------------------------------------------------- events

test('events: local handlers, arguments and the source global', () => {
  const env = make();
  run(env, `
    AddEventHandler('greet', function(name, data) log('first', source, name, data.n) end)
    AddEventHandler('greet', function(name) log('second', source, name) end)
    source = 'outer'
  `);
  assert.equal(env.rt.triggerEvent('greet', ['bob', { n: 1 }], 7), 2);
  assert.deepEqual(env.logged, [['first', 7, 'bob', 1], ['second', 7, 'bob']]);
  assert.equal(env.rt.triggerEvent('unknown', []), 0);
  run(env, `log(source)`);
  assert.equal(env.logged[2], 'outer'); // restored after the handlers
});

test('events: fromNet is gated by RegisterNetEvent', () => {
  const env = make();
  run(env, `
    AddEventHandler('local:only', function() log('local:only') end)
    RegisterNetEvent('net:a')
    AddEventHandler('net:a', function(x) log('net:a', x, source) end)
    RegisterNetEvent('net:b', function(x) log('net:b', x) end)
  `);
  assert.equal(env.rt.triggerEvent('local:only', [], 3, { fromNet: true }), 0);
  assert.equal(env.rt.triggerEvent('local:only', [], 3), 1);
  assert.equal(env.rt.triggerEvent('net:a', [1], 3, { fromNet: true }), 1);
  assert.equal(env.rt.triggerEvent('net:b', [2], 3, { fromNet: true }), 1);
  assert.deepEqual(env.logged, ['local:only', ['net:a', 1, 3], ['net:b', 2]]);
});

test('events: RemoveEventHandler, including from inside a dispatch', () => {
  const env = make();
  run(env, `
    local second
    local first = AddEventHandler('e', function()
      log('first')
      RemoveEventHandler(second) -- removed mid-dispatch: must not run
    end)
    second = AddEventHandler('e', function() log('second') end)
    AddEventHandler('e', function() log('third') end)
    function removeFirst() RemoveEventHandler(first) end
  `);
  assert.equal(env.rt.triggerEvent('e', []), 2);
  assert.deepEqual(env.logged, ['first', 'third']);
  run(env, `removeFirst() removeFirst()`); // removing twice is harmless
  assert.equal(env.rt.triggerEvent('e', []), 1);
  assert.deepEqual(env.logged, ['first', 'third', 'third']);
});

test('events: a handler may Wait and keeps its own source', () => {
  const env = make();
  run(env, `
    AddEventHandler('slow', function(tag)
      log(tag .. ':start', source)
      Wait(20)
      log(tag .. ':end', source)
    end)
  `);
  assert.equal(env.rt.triggerEvent('slow', ['a'], 1), 1);
  assert.equal(env.rt.triggerEvent('slow', ['b'], 2), 1);
  assert.equal(env.rt.pendingThreads, 2);
  env.rt.tick(20);
  assert.deepEqual(env.logged, [['a:start', 1], ['b:start', 2], ['a:end', 1], ['b:end', 2]]);
});

test('events: a failing handler is reported and does not stop the others', () => {
  const env = make();
  env.rt.loadScript(`
    AddEventHandler('e', function() error('handler broke') end)
    AddEventHandler('e', function() log('ran') end)
  `, 'ev.lua');
  assert.equal(env.rt.triggerEvent('e', []), 2);
  assert.deepEqual(env.logged, ['ran']);
  assert.equal(env.errors.length, 1);
  assert.equal(env.errors[0].context, 'event:e');
  assert.match(env.errors[0].err.message, /ev\.lua:2: handler broke/);
});

test('events: TriggerEvent dispatches locally without a host, through the host with one', () => {
  const standalone = make();
  run(standalone, `
    AddEventHandler('ping', function(a, b, c) log(a, b, c) end)
    TriggerEvent('ping', 1, nil, 'three')
  `);
  assert.deepEqual(standalone.logged, [[1, undefined, 'three']]);

  const seen = [];
  const hosted = make({ host: { triggerEvent: (name, args) => seen.push({ name, args }) } });
  run(hosted, `
    AddEventHandler('ping', function() log('should not run: the host decides') end)
    TriggerEvent('ping', 'x', { y = 1 })
  `);
  assert.deepEqual(seen, [{ name: 'ping', args: ['x', { y: 1 }] }]);
  assert.deepEqual(hosted.logged, []);
});

// -------------------------------------------------------------------- commands

test('commands: registration, dispatch and arguments', () => {
  const env = make();
  run(env, `
    RegisterCommand('heal', function(source, args, raw)
      log(source, args, raw)
    end, true)
    RegisterCommand('slow', function(source) Wait(5) log('slow done', source) end)
  `);
  assert.equal(env.rt.hasCommand('heal'), true);
  assert.equal(env.rt.hasCommand('nope'), false);
  assert.equal(env.rt.isCommandRestricted('heal'), true);
  assert.equal(env.rt.isCommandRestricted('slow'), false);

  assert.equal(env.rt.runCommand('heal', 4, ['me', '50'], 'heal me 50'), true);
  assert.equal(env.rt.runCommand('nope', 4, [], 'nope'), false);
  assert.deepEqual(env.logged, [[4, ['me', '50'], 'heal me 50']]);

  assert.equal(env.rt.runCommand('slow', 9, [], 'slow'), true);
  env.rt.tick(5);
  assert.deepEqual(env.logged[1], ['slow done', 9]);
});

test('commands: a failing command is reported', () => {
  const env = make();
  run(env, `RegisterCommand('bad', function() error('nope') end)`);
  assert.equal(env.rt.runCommand('bad', 0, [], 'bad'), true);
  assert.equal(env.errors[0].context, 'command:bad');
  assert.match(env.errors[0].err.message, /nope/);
});

// --------------------------------------------------------------------- exports

test('exports: publish, hasExport and callExport', () => {
  const env = make({ resourceName: 'bank' });
  run(env, `
    exports('getBalance', function(id, bonus) return 100 + id + (bonus or 0), 'second result' end)
    exports('describe', function() return { owner = 'bob', accounts = { 1, 2 } } end)
    exports('fail', function() error('vault locked') end)
    exports('later', function() Wait(10) log('later finished') end)
    -- a resource can call its own exports
    log(exports.bank:getBalance(1), exports['bank']:getBalance(2, 10), exports.bank.getBalance(3))
  `);
  assert.deepEqual(env.logged, [[101, 112, 103]]);
  assert.equal(env.rt.hasExport('getBalance'), true);
  assert.equal(env.rt.hasExport('missing'), false);
  assert.equal(env.rt.callExport('getBalance', [5]), 105);
  assert.deepEqual(env.rt.callExport('describe', []), { owner: 'bob', accounts: [1, 2] });
  assert.throws(() => env.rt.callExport('fail', []), /vault locked/);
  assert.throws(() => env.rt.callExport('missing', []), /resource 'bank' has no export named 'missing'/);

  assert.equal(env.rt.callExport('later', []), undefined);
  env.rt.tick(10);
  assert.equal(env.logged[1], 'later finished');
});

test('exports: calling another resource through the host', () => {
  const runtimes = new Map();
  const host = {
    callExport(resource, name, args) {
      const target = runtimes.get(resource);
      if (!target) throw new Error(`no such resource '${resource}'`);
      return target.rt.callExport(name, args);
    },
  };
  const bank = make({ resourceName: 'bank', host });
  const shop = make({ resourceName: 'shop', host });
  runtimes.set('bank', bank).set('shop', shop);

  run(bank, `
    local balances = { alice = 50 }
    exports('withdraw', function(who, amount)
      if (balances[who] or 0) < amount then error('insufficient funds') end
      balances[who] = balances[who] - amount
      return balances[who]
    end)
  `);
  run(shop, `
    log(exports.bank:withdraw('alice', 20))
    log(exports['bank']:withdraw('alice', 5))
    local ok, err = pcall(function() return exports.bank:withdraw('alice', 999) end)
    log(ok, err)
    ok, err = pcall(function() return exports.ghost:anything() end)
    log(ok, err)
  `);
  assert.equal(shop.logged[0], 30);
  assert.equal(shop.logged[1], 25);
  assert.equal(shop.logged[2][0], false);
  assert.match(shop.logged[2][1], /insufficient funds/);
  assert.equal(shop.logged[3][0], false);
  assert.match(shop.logged[3][1], /no such resource 'ghost'/);
});

// ------------------------------------------------------------------------ json

test('json: encode and decode bridge to JS JSON', () => {
  const env = make();
  run(env, `
    log(json.encode({ 1, 2, 3 }), json.encode({ a = 'é', b = true }), json.encode('s'), json.encode(nil), json.encode({}))
    local t = json.decode('{"list":[1,2.5,"x"],"obj":{"k":null,"7":"seven"},"flag":false}')
    log(#t.list, math.type(t.list[1]), t.list[2], t.list[3], t.obj.k == nil, t.obj['7'], t.flag)
    log(json.decode(json.encode({ pos = vector3(1, 2, 3) })).pos)
    log(pcall(json.decode, 'not json'))
    log(pcall(json.encode, { f = function() end }))
    log(json.decode('null') == nil, json.decode('"str"'), json.decode('12'))
  `);
  assert.deepEqual(env.logged[0], ['[1,2,3]', '{"a":"é","b":true}', '"s"', 'null', '{}']);
  assert.deepEqual(env.logged[1], [3, 'integer', 2.5, 'x', true, 'seven', false]);
  assert.deepEqual(env.logged[2], { x: 1, y: 2, z: 3 });
  assert.equal(env.logged[3][0], false);
  assert.equal(typeof env.logged[3][1], 'string');
  assert.equal(env.logged[4][0], false);
  assert.match(env.logged[4][1], /cannot encode a function/);
  assert.deepEqual(env.logged[5], [true, 'str', 12]);
});

// --------------------------------------------------------------------- vector3

test('vector3: fields, operators and marshaling', () => {
  const env = make();
  run(env, `
    local a, b = vector3(1, 2, 3), vector3(4, 5, 6)
    local sum, diff, scaled, scaled2, neg = a + b, b - a, a * 2, 2 * a, -a
    log(sum.x, sum.y, sum.z)
    log(diff.x, diff.y, diff.z)
    log(scaled == vector3(2, 4, 6), scaled2 == scaled, neg == vector3(-1, -2, -3))
    log(a == vector3(1, 2, 3), a == b, a ~= b, a == 5)
    log(#vector3(3, 4, 0), tostring(a), tostring(vector3(0.5, -1, 2)))
    log((b / 2).x, vector3().z)
    log(a)
    log(pcall(function() return a + 1 end))
    log(pcall(vector3, 'x', 0, 0))
  `);
  assert.deepEqual(env.logged[0], [5, 7, 9]);
  assert.deepEqual(env.logged[1], [3, 3, 3]);
  assert.deepEqual(env.logged[2], [true, true, true]);
  assert.deepEqual(env.logged[3], [true, false, true, false]);
  assert.deepEqual(env.logged[4], [5, 'vector3(1.0, 2.0, 3.0)', 'vector3(0.5, -1.0, 2.0)']);
  assert.deepEqual(env.logged[5], [2, 0]);
  assert.deepEqual(env.logged[6], { x: 1, y: 2, z: 3 });
  assert.equal(env.logged[7][0], false);
  assert.match(env.logged[7][1], /non-vector3/);
  assert.equal(env.logged[8][0], false);
});

// --------------------------------------------------------------------- sandbox

test('sandbox: dangerous libraries and functions are unavailable', () => {
  const env = make();
  run(env, `
    log(io == nil, package == nil, js == nil, fengari == nil, loadfile == nil, dofile == nil)
    log(os.execute == nil, os.exit == nil, os.remove == nil, os.rename == nil, os.tmpname == nil, os.getenv == nil)
    log(type(os.time()), type(os.clock()), type(os.date()), type(os.difftime(2, 1)))
    log(type(debug.traceback), debug.getinfo == nil, debug.getregistry == nil, debug.sethook == nil, debug.setmetatable == nil)
    local ok, err = pcall(require, 'socket')
    log(ok, err)
    -- text chunks load, bytecode does not
    log(load('return 1 + 1')(), load(string.dump(function() return 1 end)) == nil)
    log(load('return x', 'env', 't', { x = 'from env' })())
    -- the standard safe libraries are present
    log(type(string.format), type(table.concat), type(math.floor), type(utf8.char), type(coroutine.wrap))
  `);
  assert.deepEqual(env.logged[0], [true, true, true, true, true, true]);
  assert.deepEqual(env.logged[1], [true, true, true, true, true, true]);
  assert.deepEqual(env.logged[2], ['number', 'number', 'string', 'number']);
  assert.deepEqual(env.logged[3], ['function', true, true, true, true]);
  assert.equal(env.logged[4][0], false);
  assert.match(env.logged[4][1], /require\("socket"\) is not available/);
  assert.deepEqual(env.logged[5], [2, true]);
  assert.equal(env.logged[6], 'from env');
  assert.deepEqual(env.logged[7], ['function', 'function', 'function', 'function', 'function']);
});

test('sandbox: runtimes are isolated from each other', () => {
  const a = make({ resourceName: 'a' });
  const b = make({ resourceName: 'b' });
  run(a, `shared = 'from a' ; string.custom = true`);
  run(b, `log(shared == nil, string.custom == nil)`);
  assert.deepEqual(b.logged[0], [true, true]);
});

// --------------------------------------------------------------------- destroy

test('destroy: every method becomes a safe no-op', () => {
  const env = make();
  run(env, `
    AddEventHandler('e', function() log('handler') end)
    RegisterCommand('c', function() log('command') end)
    exports('x', function() return 1 end)
    CreateThread(function() while true do Wait(1) log('tick') end end)
  `);
  env.rt.destroy();
  env.rt.destroy();

  assert.equal(env.rt.loadScript('log("nope")'), false);
  assert.equal(env.rt.triggerEvent('e', []), 0);
  assert.equal(env.rt.runCommand('c', 0, [], 'c'), false);
  assert.equal(env.rt.hasCommand('c'), false);
  assert.equal(env.rt.isCommandRestricted('c'), false);
  assert.equal(env.rt.hasExport('x'), false);
  assert.equal(env.rt.callExport('x', []), undefined);
  assert.equal(env.rt.pendingThreads, 0);
  env.rt.tick(100);
  env.rt.registerGlobal('f', () => 1);
  env.rt.setGlobal('g', 1);
  assert.deepEqual(env.logged, []);
  assert.deepEqual(env.errors, []);
});

test('destroy: called from inside a running script closes after it unwinds', () => {
  const env = make();
  env.rt.registerGlobal('stopSelf', () => env.rt.destroy());
  run(env, `
    RegisterCommand('stop', function()
      stopSelf()
      log('still finishing the current call')
    end)
  `);
  assert.equal(env.rt.runCommand('stop', 0, [], 'stop'), true);
  assert.deepEqual(env.logged, ['still finishing the current call']);
  assert.equal(env.rt.loadScript('log("after")'), false);
  assert.equal(env.rt.hasCommand('stop'), false);
  assert.deepEqual(env.errors, []);
});

// ------------------------------------------------------------------- bootstrap

test('setBootstrapSource: an injected source is used instead of the file on disk', () => {
  const original = readFileSync(new URL('../src/shared/lua/bootstrap.lua', import.meta.url), 'utf8');
  try {
    setBootstrapSource(`${original}\n`.replace('return internal', 'INJECTED = true\nreturn internal'));
    const env = make();
    run(env, `log(INJECTED)`);
    assert.equal(env.logged[0], true);
  } finally {
    setBootstrapSource(original);
  }
});
