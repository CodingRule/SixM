// LuaRuntime: one isolated, sandboxed Lua 5.3 state (fengari) per resource per side.
//
// The runtime owns only Lua. The host owns the event bus, the network and the
// clock, and drives the runtime through triggerEvent / runCommand / tick.
// The scheduler, events, commands and exports live in bootstrap.lua; this file
// is the state setup, the JS <-> Lua marshaling layer and the protected entry
// points.
//
// Marshaling summary (details on _toJS / _push):
//   Lua nil            <-> undefined (JS null also becomes nil)
//   Lua integer/float  <-> number (JS integers outside int32 become Lua floats,
//                          because fengari integers are 32-bit)
//   Lua string         <-> string (UTF-8); Uint8Array/Buffer -> raw Lua string
//   Lua sequence 1..n   -> Array;  any other table -> plain object
//   Lua empty table     -> {} (it cannot be told apart from an empty array)
//   JS Array / object   -> table (object keys that are canonical int32 strings
//                          become integer keys, so sparse/id-keyed tables
//                          survive a Lua -> JS -> Lua round trip)
//   Lua function        -> JS function (pinned in the registry until collected)
//   JS function         -> Lua cfunction

import { readFileSync } from 'node:fs';
import fengari from 'fengari';

const { lua, lauxlib, lualib, to_luastring, to_jsstring } = fengari;
const {
  LUA_OK, LUA_MULTRET, LUA_REGISTRYINDEX,
  LUA_TNIL, LUA_TBOOLEAN, LUA_TNUMBER, LUA_TSTRING, LUA_TTABLE, LUA_TFUNCTION,
} = lua;

const MAX_DEPTH = 64;
const INT32_MIN = -2147483648;
const INT32_MAX = 2147483647;
const INTEGER_KEY = /^(0|-?[1-9]\d{0,9})$/;
const MULTI = Symbol('sixm.multi');
const noop = () => {};

let bootstrapSource = null;

/** Inject bootstrap.lua as a string (for single-file bundles). Otherwise it is read from disk on first use. */
export function setBootstrapSource(source) {
  bootstrapSource = source;
}

function getBootstrapSource() {
  if (bootstrapSource === null) {
    bootstrapSource = readFileSync(new URL('./bootstrap.lua', import.meta.url), 'utf8');
  }
  return bootstrapSource;
}

function messageOf(err) {
  return err instanceof Error ? err.message : String(err);
}

// fengari implements Lua errors by throwing its internal jump record
// ({status, previous}); those must be allowed to propagate untouched.
function isLuaJump(err) {
  return typeof err === 'object' && err !== null && !(err instanceof Error) && 'status' in err && 'previous' in err;
}

function isInt32(n) {
  return Number.isInteger(n) && n >= INT32_MIN && n <= INT32_MAX;
}

export class LuaRuntime {
  /** Mark values as multiple Lua return values: `return LuaRuntime.multi(a, b)`. */
  static multi(...values) {
    Object.defineProperty(values, MULTI, { value: true });
    return values;
  }

  constructor({ resourceName, side, print = noop, onError = noop, host = {} } = {}) {
    if (typeof resourceName !== 'string' || resourceName === '') {
      throw new TypeError('LuaRuntime: resourceName must be a non-empty string');
    }
    if (side !== 'server' && side !== 'client') {
      throw new TypeError("LuaRuntime: side must be 'server' or 'client'");
    }
    this.resourceName = resourceName;
    this.side = side;
    this._print = print;
    this._onError = onError;
    this._host = {
      // Standalone default: events only reach this runtime.
      triggerEvent: host.triggerEvent ?? ((name, args) => { this.triggerEvent(name, args); }),
      callExport: host.callExport ?? ((resource, name, args) => {
        if (resource !== this.resourceName) throw new Error(`no such resource '${resource}'`);
        return this.callExport(name, args);
      }),
      now: host.now ?? Date.now,
    };

    this._dead = false;
    this._depth = 0;                 // nesting of JS -> Lua calls in progress
    this._luaFunctions = new WeakMap(); // JS wrapper -> registry ref of the Lua function
    this._finalizer = new FinalizationRegistry((ref) => {
      if (this._L) lauxlib.luaL_unref(this._L, LUA_REGISTRYINDEX, ref);
    });

    const L = lauxlib.luaL_newstate();
    this._L = L;     // main state; null once closed
    this._cur = L;   // the lua_State currently executing (a coroutine while inside one)
    this._openSafeLibs(L);
    // Safety net: a JS exception escaping into the VM becomes a plain Lua error.
    lua.lua_atnativeerror(L, (L1) => {
      this._pushString(L1, messageOf(lua.lua_touserdata(L1, 1)));
      return 1;
    });
    this._fn = this._runBootstrap(L);
  }

  // ---------------------------------------------------------------- public API

  /** Expose a JS function as a Lua global. */
  registerGlobal(name, fn) {
    if (this._dead) return;
    if (typeof fn !== 'function') throw new TypeError(`registerGlobal('${name}'): fn must be a function`);
    const L = this._cur;
    lua.lua_pushcfunction(L, this._wrapJsFunction(fn));
    lua.lua_setglobal(L, to_luastring(name));
  }

  /** Set a marshaled plain value as a Lua global. */
  setGlobal(name, value) {
    if (this._dead) return;
    const L = this._cur;
    const top = lua.lua_gettop(L);
    try {
      this._push(L, value);
      lua.lua_setglobal(L, to_luastring(name));
    } finally {
      lua.lua_settop(L, top);
    }
  }

  /** Run a chunk. Returns true on success; failures go to onError with a traceback. */
  loadScript(code, chunkName = `${this.resourceName}/chunk`) {
    if (this._dead) return false;
    const L = this._cur;
    const top = lua.lua_gettop(L);
    const context = `load:${chunkName}`;
    this._depth++;
    try {
      lua.lua_pushcfunction(L, tracebackHandler);
      const bytes = typeof code === 'string' ? to_luastring(code) : code;
      let status = lauxlib.luaL_loadbufferx(L, bytes, bytes.length, to_luastring(`@${chunkName}`), to_luastring('t'));
      if (status === LUA_OK) status = lua.lua_pcall(L, 0, 0, top + 1);
      if (status !== LUA_OK) {
        this._report(new Error(this._errorText(L)), context);
        return false;
      }
      return true;
    } catch (err) {
      this._report(err, context);
      return false;
    } finally {
      this._leave(L, top);
    }
  }

  /** Run this runtime's handlers for an event. Returns the number of handlers invoked. */
  triggerEvent(name, args = [], source = undefined, { fromNet = false } = {}) {
    if (this._dead) return 0;
    try {
      return this._call('trigger', [name, source, fromNet, ...args])[0] ?? 0;
    } catch (err) {
      this._report(err, `event:${name}`);
      return 0;
    }
  }

  /** Run a command registered with RegisterCommand. Returns true if this runtime owns it. */
  runCommand(name, source, args = [], raw = name) {
    if (this._dead) return false;
    try {
      return this._call('command', [name, source, args, raw])[0] === true;
    } catch (err) {
      this._report(err, `command:${name}`);
      return false;
    }
  }

  hasCommand(name) {
    return this._query('hasCommand', name);
  }

  /** True if the command was registered with a truthy `restricted` flag (for host ACL checks). */
  isCommandRestricted(name) {
    return this._query('isCommandRestricted', name);
  }

  /** Resume coroutines whose Wait deadline passed and fire due SetTimeout callbacks. */
  tick(nowMs = this._host.now()) {
    if (this._dead) return;
    try {
      this._call('tick', [nowMs]);
    } catch (err) {
      this._report(err, 'tick');
    }
  }

  /** Number of sleeping threads and pending timeouts. */
  get pendingThreads() {
    if (this._dead) return 0;
    try {
      return this._call('pending', [])[0] ?? 0;
    } catch {
      return 0;
    }
  }

  /**
   * Call a function published with `exports('name', fn)` and return its first
   * result. Throws if the export is missing or raises (so a calling resource
   * can pcall it). If the export Waits, it finishes later and undefined is
   * returned now.
   */
  callExport(name, args = []) {
    if (this._dead) return undefined;
    return this._call('callExport', [name, ...args])[0];
  }

  hasExport(name) {
    return this._query('hasExport', name);
  }

  /** Close the state. Afterwards every method is a safe no-op. */
  destroy() {
    if (this._dead) return;
    this._dead = true;
    // If Lua is running right now (destroy called from inside a script), the
    // state is closed when the outermost call unwinds.
    if (this._depth === 0) this._close();
  }

  // ------------------------------------------------------------------- set-up

  // Open only the safe standard libraries. `package`, `io` and fengari's own
  // interop libraries are never loaded; `os` and `debug` are trimmed.
  _openSafeLibs(L) {
    const open = (name, openf, asGlobal) => {
      lauxlib.luaL_requiref(L, to_luastring(name), openf, asGlobal ? 1 : 0);
    };
    for (const [name, openf] of [
      ['_G', lualib.luaopen_base],
      ['coroutine', lualib.luaopen_coroutine],
      ['table', lualib.luaopen_table],
      ['string', lualib.luaopen_string],
      ['utf8', lualib.luaopen_utf8],
      ['math', lualib.luaopen_math],
    ]) {
      open(name, openf, true);
      lua.lua_pop(L, 1);
    }
    // Replace os/debug with fresh tables holding only whitelisted functions.
    const trimmed = (name, openf, keep) => {
      open(name, openf, false);
      lua.lua_createtable(L, 0, keep.length);
      for (const field of keep) {
        lua.lua_getfield(L, -2, to_luastring(field));
        lua.lua_setfield(L, -2, to_luastring(field));
      }
      lua.lua_setglobal(L, to_luastring(name));
      lua.lua_pop(L, 1);
    };
    trimmed('os', lualib.luaopen_os, ['time', 'clock', 'date', 'difftime']);
    trimmed('debug', lualib.luaopen_debug, ['traceback']);
  }

  // Runs bootstrap.lua with the bridge table and pins each internal entry
  // point it returns in the registry. Returns { name: registryRef }.
  _runBootstrap(L) {
    const top = lua.lua_gettop(L);
    try {
      const source = to_luastring(getBootstrapSource());
      if (lauxlib.luaL_loadbufferx(L, source, source.length, to_luastring('@sixm/bootstrap.lua'), to_luastring('t')) !== LUA_OK) {
        throw new Error(`SixM bootstrap failed to compile: ${this._errorText(L)}`);
      }
      this._pushBridge(L);
      if (lua.lua_pcall(L, 1, 1, 0) !== LUA_OK) {
        throw new Error(`SixM bootstrap failed: ${this._errorText(L)}`);
      }
      const refs = {};
      lua.lua_pushnil(L);
      while (lua.lua_next(L, -2) !== 0) {
        const name = to_jsstring(lua.lua_tostring(L, -2));
        refs[name] = lauxlib.luaL_ref(L, LUA_REGISTRYINDEX); // pops the value
      }
      return refs;
    } finally {
      lua.lua_settop(L, top);
    }
  }

  // The table of JS services bootstrap.lua builds the Lua API on.
  _pushBridge(L) {
    const bridge = {
      resourceName: this.resourceName,
      isServer: this.side === 'server',
      print: (line) => { this._print(line); },
      report: (message, context) => { this._report(new Error(message), context); },
      now: () => this._host.now(),
      triggerEvent: (name, ...args) => { this._host.triggerEvent(name, args); },
      callExport: (resource, name, ...args) => this._host.callExport(resource, name, args),
    };
    this._push(L, bridge);
    // json goes through dedicated cfunctions: it needs its own marshaling options.
    lua.lua_pushcfunction(L, this._cfunction((L1) => {
      const text = JSON.stringify(this._toJS(L1, 1, { functions: false }));
      this._pushString(L1, text === undefined ? 'null' : text);
      return 1;
    }));
    lua.lua_setfield(L, -2, to_luastring('jsonEncode'));
    lua.lua_pushcfunction(L, this._cfunction((L1) => {
      if (lua.lua_type(L1, 1) !== LUA_TSTRING) throw new Error('json.decode expects a string');
      this._push(L1, JSON.parse(to_jsstring(lua.lua_tostring(L1, 1))), { integerKeys: false });
      return 1;
    }));
    lua.lua_setfield(L, -2, to_luastring('jsonDecode'));
  }

  // ------------------------------------------------------------ calling Lua

  // Calls one of bootstrap.lua's internal functions with marshaled arguments
  // and returns its results as a JS array. Throws on any failure; the stack
  // is restored either way.
  _call(fnName, args) {
    const L = this._cur;
    const top = lua.lua_gettop(L);
    this._depth++;
    try {
      lauxlib.luaL_checkstack(L, args.length + 2, to_luastring('too many arguments'));
      lua.lua_rawgeti(L, LUA_REGISTRYINDEX, this._fn[fnName]);
      for (const arg of args) this._push(L, arg);
      if (lua.lua_pcall(L, args.length, LUA_MULTRET, 0) !== LUA_OK) {
        throw new Error(this._errorText(L));
      }
      const results = [];
      for (let i = top + 1, end = lua.lua_gettop(L); i <= end; i++) results.push(this._toJS(L, i));
      return results;
    } finally {
      this._leave(L, top);
    }
  }

  // Boolean-returning internal lookups that never raise.
  _query(fnName, name) {
    if (this._dead) return false;
    try {
      return this._call(fnName, [name])[0] === true;
    } catch {
      return false;
    }
  }

  // Unwinds one JS -> Lua call: restore the stack, and finish a destroy()
  // that was requested while Lua was running.
  _leave(L, top) {
    lua.lua_settop(L, top);
    if (--this._depth === 0 && this._dead) this._close();
  }

  _close() {
    if (!this._L) return;
    const L = this._L;
    this._L = null;
    this._cur = null;
    this._fn = null;
    lua.lua_close(L);
  }

  _report(err, context) {
    try {
      this._onError(err instanceof Error ? err : new Error(String(err)), context);
    } catch {
      // An error handler that throws must not take the runtime down with it.
    }
  }

  // Text of the error value on top of the stack.
  _errorText(L) {
    const text = to_jsstring(lauxlib.luaL_tolstring(L, -1), undefined, undefined, true);
    lua.lua_pop(L, 1);
    return text;
  }

  // -------------------------------------------------------- JS called by Lua

  // Wraps `body(L) -> number of results` as a lua_CFunction that tracks the
  // executing lua_State and turns JS exceptions into Lua errors.
  _cfunction(body) {
    return (L) => {
      const previous = this._cur;
      this._cur = L;
      let failure;
      try {
        return body(L);
      } catch (err) {
        if (isLuaJump(err)) throw err;
        failure = err;
      } finally {
        this._cur = previous;
      }
      // Raised outside the try block: lua_error itself unwinds by throwing.
      this._pushString(L, messageOf(failure));
      return lua.lua_error(L);
    };
  }

  // A plain JS function as a lua_CFunction: marshals arguments in, results out.
  _wrapJsFunction(fn) {
    return this._cfunction((L) => {
      const argc = lua.lua_gettop(L);
      const args = new Array(argc);
      for (let i = 0; i < argc; i++) args[i] = this._toJS(L, i + 1);
      const result = fn(...args);
      if (result === undefined) return 0;
      if (Array.isArray(result) && result[MULTI]) {
        lauxlib.luaL_checkstack(L, result.length, to_luastring('too many results'));
        for (const value of result) this._push(L, value);
        return result.length;
      }
      this._push(L, result);
      return 1;
    });
  }

  // Pins the Lua function at `idx` and returns a JS function that calls it.
  // Each call runs in its own coroutine, so the function may Wait(); it then
  // finishes later and the wrapper returns undefined. Lua errors are thrown.
  _wrapLuaFunction(L, idx) {
    lua.lua_pushvalue(L, idx);
    const ref = lauxlib.luaL_ref(L, LUA_REGISTRYINDEX);
    const wrapper = (...args) => {
      if (this._dead) return undefined;
      return this._call('invoke', [wrapper, 'callback', ...args])[0];
    };
    this._luaFunctions.set(wrapper, ref);
    this._finalizer.register(wrapper, ref);
    return wrapper;
  }

  // --------------------------------------------------------------- marshaling

  _pushString(L, text) {
    const bytes = to_luastring(text);
    lua.lua_pushlstring(L, bytes, bytes.length);
  }

  // Pushes a JS value onto L's stack as a Lua value.
  // opts.integerKeys (default true): object keys that are canonical int32
  // strings become integer keys.
  _push(L, value, opts = {}, depth = 0, path = new Set()) {
    switch (typeof value) {
      case 'undefined':
        lua.lua_pushnil(L);
        return;
      case 'boolean':
        lua.lua_pushboolean(L, value);
        return;
      case 'number':
        if (isInt32(value)) lua.lua_pushinteger(L, value);
        else lua.lua_pushnumber(L, value);
        return;
      case 'bigint':
        this._push(L, Number(value));
        return;
      case 'string':
        this._pushString(L, value);
        return;
      case 'function': {
        const ref = this._luaFunctions.get(value);
        if (ref !== undefined) lua.lua_rawgeti(L, LUA_REGISTRYINDEX, ref); // a Lua function coming home
        else lua.lua_pushcfunction(L, this._wrapJsFunction(value));
        return;
      }
      case 'object':
        break;
      default:
        throw new Error(`cannot pass a JS ${typeof value} to Lua`);
    }
    if (value === null) {
      lua.lua_pushnil(L);
      return;
    }
    if (value instanceof Uint8Array) {
      lua.lua_pushlstring(L, value, value.length); // raw bytes; fengari copies
      return;
    }
    if (depth >= MAX_DEPTH) throw new Error(`cannot pass value to Lua: nested deeper than ${MAX_DEPTH} levels`);
    if (path.has(value)) throw new Error('cannot pass value to Lua: it contains a cycle');
    path.add(value);
    lauxlib.luaL_checkstack(L, 3, to_luastring('value nested too deeply'));
    if (Array.isArray(value)) {
      lua.lua_createtable(L, value.length, 0);
      for (let i = 0; i < value.length; i++) {
        this._push(L, value[i], opts, depth + 1, path);
        lua.lua_rawseti(L, -2, i + 1);
      }
    } else {
      const keys = Object.keys(value);
      lua.lua_createtable(L, 0, keys.length);
      for (const key of keys) {
        const item = value[key];
        if (item === undefined || item === null) continue;
        if (opts.integerKeys !== false && INTEGER_KEY.test(key) && isInt32(Number(key))) {
          lua.lua_pushinteger(L, Number(key));
        } else {
          this._pushString(L, key);
        }
        this._push(L, item, opts, depth + 1, path);
        lua.lua_rawset(L, -3);
      }
    }
    path.delete(value);
  }

  // Converts the Lua value at stack index `idx` to a JS value.
  // opts.functions === false: refuse functions instead of wrapping them.
  _toJS(L, idx, opts = {}, depth = 0, path = new Set()) {
    switch (lua.lua_type(L, idx)) {
      case LUA_TNIL:
        return undefined;
      case LUA_TBOOLEAN:
        return lua.lua_toboolean(L, idx);
      case LUA_TNUMBER:
        return lua.lua_tonumber(L, idx);
      case LUA_TSTRING:
        return to_jsstring(lua.lua_tostring(L, idx), undefined, undefined, true);
      case LUA_TFUNCTION:
        if (opts.functions === false) throw new Error('cannot encode a function');
        return this._wrapLuaFunction(L, idx);
      case LUA_TTABLE:
        return this._tableToJS(L, lua.lua_absindex(L, idx), opts, depth, path);
      default:
        throw new Error(`cannot pass a Lua ${to_jsstring(lauxlib.luaL_typename(L, idx))} to JS`);
    }
  }

  // Sequence (keys exactly 1..n, n > 0) -> Array; anything else -> object.
  _tableToJS(L, idx, opts, depth, path) {
    if (depth >= MAX_DEPTH) throw new Error(`cannot pass table to JS: nested deeper than ${MAX_DEPTH} levels`);
    const identity = lua.lua_topointer(L, idx);
    if (path.has(identity)) throw new Error('cannot pass table to JS: it contains a cycle');
    path.add(identity);
    lauxlib.luaL_checkstack(L, 3, to_luastring('table nested too deeply'));

    const keys = [];
    const values = [];
    let sequence = true;
    lua.lua_pushnil(L);
    while (lua.lua_next(L, idx) !== 0) {
      let key;
      switch (lua.lua_type(L, -2)) {
        case LUA_TNUMBER:
          key = lua.lua_tonumber(L, -2);
          if (!lua.lua_isinteger(L, -2) || key < 1) sequence = false;
          break;
        case LUA_TSTRING:
          key = to_jsstring(lua.lua_tostring(L, -2), undefined, undefined, true);
          sequence = false;
          break;
        case LUA_TBOOLEAN:
          key = String(lua.lua_toboolean(L, -2));
          sequence = false;
          break;
        default:
          throw new Error(`cannot pass table to JS: unsupported ${to_jsstring(lauxlib.luaL_typename(L, -2))} key`);
      }
      keys.push(key);
      values.push(this._toJS(L, -1, opts, depth + 1, path));
      lua.lua_pop(L, 1);
    }
    path.delete(identity);

    const count = keys.length;
    // Distinct integer keys >= 1 with none above `count` are exactly 1..count.
    if (sequence && count > 0 && keys.every((key) => key <= count)) {
      const array = new Array(count);
      for (let i = 0; i < count; i++) array[keys[i] - 1] = values[i];
      return array;
    }
    const object = {};
    for (let i = 0; i < count; i++) object[keys[i]] = values[i];
    return object;
  }
}

// lua_pcall message handler: appends a stack traceback to the error message.
function tracebackHandler(L) {
  const message = lauxlib.luaL_tolstring(L, 1);
  lauxlib.luaL_traceback(L, L, message, 1);
  return 1;
}
