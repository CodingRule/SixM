// SixM client core. It is headless: it speaks the protocol, tracks the other
// players, and runs the client side of the server's resources. A game
// integration would feed it the local player's state and read remote players
// from it; without one, the state is set by scripts or by the caller.

import { EventEmitter } from 'node:events';
import WebSocket from 'ws';
import {
  PROTOCOL_VERSION, DEFAULT_PORT, Msg,
  encodeMsg, decodeMsg, encodeState, decodeSnapshot, emptyState, sanitizeName,
} from '../shared/protocol.js';
import { LuaRuntime } from '../shared/lua/runtime.js';
import { splitCommand } from '../shared/command.js';
import { COMMAND_EVENT } from '../shared/events.js';

export class SixMClient extends EventEmitter {
  /**
   * @param {object} [o]
   * @param {string} [o.host] @param {number} [o.port] @param {string} [o.name]
   * @param {string} [o.build]
   * @param {(line:string)=>void} [o.print] where script output and notices go
   * @param {boolean} [o.secure] connect with TLS (wss://), for servers behind a TLS reverse proxy
   * @param {boolean} [o.runScripts] run the server's client-side Lua (default true)
   */
  constructor({ host = '127.0.0.1', port = DEFAULT_PORT, name = 'Player', build = 'sixm-client', print = console.log, runScripts = true, secure = false } = {}) {
    super();
    this.host = host;
    this.port = port;
    this.name = sanitizeName(name) || 'Player';
    this.build = build;
    this.print = print;
    this.runScripts = runScripts;
    this.secure = !!secure;

    this.id = 0;
    this.connected = false;
    this.serverName = '';
    this.tickRate = 20;
    this.ping = -1;
    /** this player's own state; change it with setCoords / setHeading / setState */
    this.state = emptyState();
    /** id -> { id, name, state|null }; state is null while the player is out of range */
    this.players = new Map();
    /** resource name -> LuaRuntime */
    this.runtimes = new Map();
    this.convars = new Map();

    this.ws = null;
    this.seq = 0;
    this.dirty = true;
    this.timers = [];
  }

  /** Connect and finish the handshake. Rejects with the server's reason when refused. */
  connect() {
    return new Promise((resolve, reject) => {
      let settled = false;
      const fail = (err) => {
        if (settled) return;
        settled = true;
        reject(err instanceof Error ? err : new Error(String(err)));
      };
      const ws = new WebSocket(`${this.secure ? 'wss' : 'ws'}://${this.host}:${this.port}/ws`, { maxPayload: 4 * 1024 * 1024 });
      this.ws = ws;
      ws.binaryType = 'nodebuffer';

      ws.on('open', () => {
        ws.send(encodeMsg(Msg.HELLO, { proto: PROTOCOL_VERSION, name: this.name, build: this.build }));
      });
      ws.on('message', (data, isBinary) => {
        try {
          if (isBinary) return this.onBinary(data);
          const msg = decodeMsg(data.toString('utf8'));
          if (!msg) return;
          if (msg.t === Msg.WELCOME && !settled) {
            this.onWelcome(msg);
            settled = true;
            resolve(this);
          } else if (msg.t === Msg.REJECT) {
            fail(new Error(msg.reason || 'Connection rejected.'));
          } else {
            this.onMessage(msg);
          }
        } catch (err) {
          this.emit('scriptError', err);
        }
      });
      ws.on('error', (err) => {
        fail(err);
        if (this.connected) this.emit('error', err);
      });
      ws.on('close', () => {
        fail(new Error('Connection closed before the server answered.'));
        this.teardown(this.kickReason ?? 'Connection lost.');
      });
    });
  }

  disconnect(reason = 'Disconnected.') {
    this.kickReason ??= reason;
    if (this.ws && this.ws.readyState <= WebSocket.OPEN) this.ws.close();
    else this.teardown(reason);
  }

  // ---- local state -----------------------------------------------------

  setCoords(x, y, z) {
    Object.assign(this.state, { x: Number(x) || 0, y: Number(y) || 0, z: Number(z) || 0 });
    this.dirty = true;
  }

  setHeading(heading) {
    this.state.heading = Number(heading) || 0;
    this.dirty = true;
  }

  setState(patch) {
    Object.assign(this.state, patch);
    this.dirty = true;
  }

  // ---- events and commands --------------------------------------------

  /** Send an event to the server (Lua: TriggerServerEvent). */
  emitNet(name, ...args) {
    this.send(Msg.EVENT, { name, args });
  }

  /** Deliver an event to every client-side resource. */
  triggerEvent(name, args = [], source = 0, opts = {}) {
    let handled = 0;
    for (const rt of [...this.runtimes.values()]) handled += rt.triggerEvent(name, args, source, opts) || 0;
    this.emit('event', name, args);
    return handled;
  }

  /** Run a command: a client resource's if one owns it, otherwise the server's. */
  executeCommand(raw) {
    const parts = splitCommand(raw);
    if (!parts.length) return;
    const name = parts[0];
    for (const rt of this.runtimes.values()) {
      if (rt.hasCommand(name)) {
        rt.runCommand(name, this.id, parts.slice(1), raw);
        return;
      }
    }
    this.emitNet(COMMAND_EVENT, String(raw).slice(0, 512));
  }

  // ---- protocol --------------------------------------------------------

  send(type, fields) {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(encodeMsg(type, fields));
  }

  onWelcome(msg) {
    this.id = msg.id;
    this.connected = true;
    this.serverName = msg.serverName ?? '';
    this.tickRate = Math.min(Math.max(Number(msg.tickRate) || 20, 1), 120);
    for (const p of msg.players ?? []) {
      if (p.id !== this.id) this.players.set(p.id, { id: p.id, name: p.name, state: null });
    }
    if (this.runScripts) for (const r of msg.resources ?? []) this.startResource(r);

    const interval = 1000 / this.tickRate;
    this.timers.push(setInterval(() => this.tick(), interval));
    this.timers.push(setInterval(() => this.send(Msg.PING, { ts: Date.now() }), 2000));
    this.emit('connected', this);
    this.triggerEvent('onClientReady', [this.id]);
  }

  onMessage(msg) {
    switch (msg.t) {
      case Msg.JOIN:
        if (msg.id === this.id) break;
        this.players.set(msg.id, { id: msg.id, name: msg.name, state: null });
        this.emit('playerJoined', msg.id, msg.name);
        this.triggerEvent('playerJoined', [msg.id, msg.name], msg.id);
        break;
      case Msg.LEAVE: {
        const p = this.players.get(msg.id);
        if (!p) break;
        this.players.delete(msg.id);
        this.emit('playerLeft', msg.id, p.name, msg.reason);
        this.triggerEvent('playerLeft', [msg.id, p.name, msg.reason ?? ''], msg.id);
        break;
      }
      case Msg.EVENT:
        if (typeof msg.name === 'string') {
          this.triggerEvent(msg.name, Array.isArray(msg.args) ? msg.args : [], 0, { fromNet: true });
        }
        break;
      case Msg.PONG:
        if (typeof msg.ts === 'number') this.ping = Math.max(0, Date.now() - msg.ts);
        break;
      case Msg.KICK:
        this.kickReason = msg.reason || 'Kicked.';
        break;
      default:
        break;
    }
  }

  onBinary(data) {
    const snap = decodeSnapshot(data);
    if (!snap) return;
    const seen = new Set();
    for (const e of snap.entities) {
      const p = this.players.get(e.id);
      if (!p) continue;
      p.state = e.state;
      seen.add(e.id);
    }
    // Anyone missing from the snapshot is out of streaming range.
    for (const p of this.players.values()) if (!seen.has(p.id)) p.state = null;
    this.emit('snapshot', snap.tick, snap.entities.length);
  }

  tick() {
    const now = Date.now();
    for (const rt of [...this.runtimes.values()]) rt.tick(now);
    this.emit('tick', now);
    if (this.dirty && this.ws?.readyState === WebSocket.OPEN) {
      this.dirty = false;
      this.seq = (this.seq + 1) >>> 0;
      this.ws.send(encodeState(this.seq, this.state));
    }
  }

  teardown(reason) {
    if (this.tornDown) return;
    this.tornDown = true;
    const wasConnected = this.connected;
    this.connected = false;
    for (const t of this.timers) clearInterval(t);
    this.timers = [];
    for (const rt of this.runtimes.values()) rt.destroy();
    this.runtimes.clear();
    this.players.clear();
    if (wasConnected) this.emit('disconnected', reason);
  }

  // ---- client-side Lua -------------------------------------------------

  startResource({ name, scripts }) {
    if (typeof name !== 'string' || !Array.isArray(scripts) || this.runtimes.has(name)) return;
    const rt = new LuaRuntime({
      resourceName: name,
      side: 'client',
      print: (line) => this.print(`[${name}] ${line}`),
      onError: (err, context) => {
        this.print(`[${name}] script error${context ? ' in ' + context : ''}: ${err?.message ?? err}`);
        this.emit('scriptError', err);
      },
      host: {
        triggerEvent: (event, args) => this.triggerEvent(event, args, 0),
        callExport: (resource, fn, args) => {
          const other = this.runtimes.get(resource);
          if (!other?.hasExport(fn)) throw new Error(`no export "${fn}" in resource "${resource}"`);
          return other.callExport(fn, args);
        },
        now: () => Date.now(),
      },
    });
    this.registerNatives(rt);
    this.runtimes.set(name, rt);
    for (const s of scripts) {
      if (typeof s?.code === 'string') rt.loadScript(s.code, `@${name}/${s.file}`);
    }
    this.triggerEvent('onClientResourceStart', [name]);
  }

  registerNatives(rt) {
    const coords = (s) => (s ? { x: s.x, y: s.y, z: s.z } : null);
    const natives = {
      TriggerServerEvent: (name, ...args) => {
        if (typeof name !== 'string') throw new Error('TriggerServerEvent: event name must be a string');
        this.emitNet(name, ...args);
      },
      GetLocalPlayerId: () => this.id,
      GetLocalCoords: () => coords(this.state),
      GetLocalHeading: () => this.state.heading,
      SetLocalCoords: (x, y, z) => this.setCoords(x, y, z),
      SetLocalHeading: (h) => this.setHeading(h),
      GetActivePlayers: () => [this.id, ...this.players.keys()].sort((a, b) => a - b),
      GetPlayerName: (id) => (Number(id) === this.id ? this.name : this.players.get(Number(id))?.name ?? null),
      GetPlayerCoords: (id) => (Number(id) === this.id ? coords(this.state) : coords(this.players.get(Number(id))?.state)),
      GetPlayerHeading: (id) => (Number(id) === this.id ? this.state.heading : this.players.get(Number(id))?.state?.heading ?? 0),
      IsPlayerStreamed: (id) => Number(id) === this.id || !!this.players.get(Number(id))?.state,
      GetPing: () => this.ping,
      GetServerName: () => this.serverName,
      GetConvar: (name, fallback) => this.convars.get(String(name)) ?? fallback ?? null,
      GetConvarInt: (name, fallback) => {
        const n = parseInt(this.convars.get(String(name)), 10);
        return Number.isNaN(n) ? (fallback ?? 0) : n;
      },
      ExecuteCommand: (raw) => this.executeCommand(String(raw)),
      GetGameTimer: () => Math.floor(performance.now()),
    };
    for (const [name, fn] of Object.entries(natives)) rt.registerGlobal(name, fn);
  }
}
