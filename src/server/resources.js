// Resource manager: finds resources on disk, runs their server-side Lua, hands
// their client-side Lua to joining players, and connects every Lua runtime to
// the server (events, players, commands, convars, exports).

import fs from 'node:fs';
import path from 'node:path';
import { LuaRuntime } from '../shared/lua/runtime.js';
import { parseManifest, MANIFEST_FILE } from '../shared/manifest.js';
import { splitCommand } from '../shared/command.js';
import { COMMAND_EVENT } from '../shared/events.js';

const MAX_SCAN_DEPTH = 4;

export class ResourceManager {
  /**
   * @param {object} o
   * @param {import('./core.js').SixMServer} o.server
   * @param {string} o.resourcesDir
   * @param {{info:Function, warn:Function, error:Function}} o.logger
   * @param {Map<string,string>} [o.convars]
   */
  constructor({ server, resourcesDir, logger, convars = new Map() }) {
    this.server = server;
    this.resourcesDir = resourcesDir;
    this.log = logger;
    this.convars = convars;
    /** name -> directory, for every resource found on disk */
    this.available = new Map();
    /** name -> { name, dir, manifest, runtime, clientScripts } for started resources */
    this.running = new Map();
    /** console commands implemented by the host: name -> { help, run(args, raw, source) } */
    this.builtins = new Map();

    server.setClientResources(() => this.clientPayload());
    this.unhook = this.hookServer();
  }

  // ---- discovery -------------------------------------------------------

  /** Find every folder holding a manifest. `[category]` folders are looked into. */
  scan() {
    this.available.clear();
    const walk = (dir, depth) => {
      let entries;
      try {
        entries = fs.readdirSync(dir, { withFileTypes: true });
      } catch {
        return;
      }
      for (const e of entries) {
        if (!e.isDirectory()) continue;
        const full = path.join(dir, e.name);
        if (/^\[.*\]$/.test(e.name)) {
          if (depth < MAX_SCAN_DEPTH) walk(full, depth + 1);
        } else if (fs.existsSync(path.join(full, MANIFEST_FILE))) {
          if (this.available.has(e.name)) {
            this.log.warn(`duplicate resource "${e.name}" at ${full}; keeping ${this.available.get(e.name)}`);
          } else {
            this.available.set(e.name, full);
          }
        }
      }
    };
    walk(this.resourcesDir, 0);
    return [...this.available.keys()];
  }

  // ---- lifecycle -------------------------------------------------------

  /** @returns {boolean} whether the resource is running afterwards */
  start(name) {
    if (this.running.has(name)) return true;
    const dir = this.available.get(name);
    if (!dir) {
      this.log.error(`cannot start "${name}": no such resource in ${this.resourcesDir}`);
      return false;
    }

    let manifest;
    try {
      manifest = parseManifest(fs.readFileSync(path.join(dir, MANIFEST_FILE), 'utf8'), `@${name}/${MANIFEST_FILE}`);
    } catch (err) {
      this.log.error(`cannot start "${name}": ${err.message}`);
      return false;
    }

    // Scripts are read up front so a missing file fails the start cleanly.
    const read = (rel) => {
      const full = path.resolve(dir, rel);
      if (full !== dir && !full.startsWith(dir + path.sep)) {
        throw new Error(`script "${rel}" is outside the resource folder`);
      }
      return { file: rel, code: fs.readFileSync(full, 'utf8') };
    };
    let serverScripts;
    let clientScripts;
    try {
      const shared = manifest.sharedScripts.map(read);
      serverScripts = [...shared, ...manifest.serverScripts.map(read)];
      clientScripts = [...shared, ...manifest.clientScripts.map(read)];
    } catch (err) {
      this.log.error(`cannot start "${name}": ${err.message}`);
      return false;
    }

    const runtime = new LuaRuntime({
      resourceName: name,
      side: 'server',
      print: (line) => this.log.info(`[${name}] ${line}`),
      onError: (err, context) => this.log.error(`[${name}] ${context ? context + ': ' : ''}${err?.message ?? err}`),
      host: {
        triggerEvent: (event, args) => this.triggerEvent(event, args, 0),
        callExport: (resource, fn, args) => this.callExport(resource, fn, args),
        now: () => Date.now(),
      },
    });
    this.registerNatives(runtime);

    const resource = { name, dir, manifest, runtime, clientScripts };
    this.running.set(name, resource);
    for (const s of serverScripts) runtime.loadScript(s.code, `@${name}/${s.file}`);

    this.syncServerInfo();
    this.triggerEvent('onResourceStart', [name], 0);
    this.log.info(`started resource ${name}`);
    return true;
  }

  stop(name) {
    const resource = this.running.get(name);
    if (!resource) return false;
    this.triggerEvent('onResourceStop', [name], 0);
    this.running.delete(name);
    resource.runtime.destroy();
    this.syncServerInfo();
    this.log.info(`stopped resource ${name}`);
    return true;
  }

  restart(name) {
    this.stop(name);
    this.scan();
    return this.start(name);
  }

  stopAll() {
    for (const name of [...this.running.keys()].reverse()) this.stop(name);
  }

  dispose() {
    this.stopAll();
    this.unhook();
  }

  /** What a joining client needs to run the client side of every resource. */
  clientPayload() {
    const out = [];
    for (const r of this.running.values()) {
      if (r.clientScripts.length) out.push({ name: r.name, scripts: r.clientScripts });
    }
    return out;
  }

  syncServerInfo() {
    this.server.setResourceNames?.([...this.running.keys()]);
  }

  // ---- events, exports, commands --------------------------------------

  /** Deliver an event to every running resource. */
  triggerEvent(name, args = [], source = 0, { fromNet = false } = {}) {
    let handled = 0;
    for (const r of [...this.running.values()]) {
      handled += r.runtime.triggerEvent(name, args, source, { fromNet }) || 0;
    }
    return handled;
  }

  callExport(resourceName, exportName, args) {
    const r = this.running.get(resourceName);
    if (!r) throw new Error(`no running resource "${resourceName}"`);
    if (!r.runtime.hasExport(exportName)) throw new Error(`resource "${resourceName}" has no export "${exportName}"`);
    return r.runtime.callExport(exportName, args);
  }

  /** Register a console command implemented in JS. */
  addBuiltin(name, help, run) {
    this.builtins.set(name.toLowerCase(), { help, run });
  }

  /**
   * Run a command line. source 0 is the server console; a player id is a player.
   * Players can only run commands that resources registered as unrestricted.
   * @returns {boolean} whether something handled it
   */
  executeCommand(raw, source = 0) {
    const parts = splitCommand(raw);
    if (!parts.length) return false;
    const name = parts[0].toLowerCase();
    const args = parts.slice(1);

    for (const r of this.running.values()) {
      if (!r.runtime.hasCommand(name)) continue;
      if (source !== 0 && r.runtime.isCommandRestricted?.(name)) {
        this.log.warn(`player ${source} was refused restricted command "${name}"`);
        return false;
      }
      r.runtime.runCommand(name, source, args, raw);
      return true;
    }
    if (source === 0) {
      const builtin = this.builtins.get(name);
      if (builtin) {
        builtin.run(args, raw, source);
        return true;
      }
    }
    return false;
  }

  // ---- server -> Lua ---------------------------------------------------

  hookServer() {
    const s = this.server;

    const onConnecting = (info, deferrals) => {
      let kickReason = null;
      let finishDeferral = null;
      const api = {
        defer: () => {
          finishDeferral ??= deferrals.defer();
        },
        update: () => {},
        done: (reason) => {
          if (finishDeferral) finishDeferral(reason ?? undefined);
          else if (reason) deferrals.reject(String(reason));
        },
      };
      this.triggerEvent('playerConnecting', [info.name, (reason) => { kickReason = String(reason ?? 'Rejected.'); }, api], 0);
      if (kickReason !== null) {
        if (finishDeferral) finishDeferral(kickReason);
        else deferrals.reject(kickReason);
      }
    };
    const onJoined = (player) => this.triggerEvent('playerJoined', [player.id, player.name], player.id);
    const onDropped = (player, reason) => {
      // The slot is already free, but handlers still ask who it was.
      this.dropping = player;
      try {
        this.triggerEvent('playerDropped', [reason ?? 'Disconnected.'], player.id);
      } finally {
        this.dropping = null;
      }
    };
    const onNetEvent = (player, name, args) => {
      if (name === COMMAND_EVENT) {
        if (typeof args[0] === 'string' && args[0].length <= 512) this.executeCommand(args[0], player.id);
        return;
      }
      this.triggerEvent(name, args, player.id, { fromNet: true });
    };
    const onTick = () => {
      const now = Date.now();
      for (const r of [...this.running.values()]) r.runtime.tick(now);
    };

    s.on('playerConnecting', onConnecting);
    s.on('playerJoined', onJoined);
    s.on('playerDropped', onDropped);
    s.on('netEvent', onNetEvent);
    s.on('tick', onTick);
    return () => {
      s.off('playerConnecting', onConnecting);
      s.off('playerJoined', onJoined);
      s.off('playerDropped', onDropped);
      s.off('netEvent', onNetEvent);
      s.off('tick', onTick);
    };
  }

  // ---- Lua -> server (natives) ----------------------------------------

  registerNatives(rt) {
    const s = this.server;
    const player = (id) => s.getPlayer(Number(id)) ?? (this.dropping?.id === Number(id) ? this.dropping : undefined);
    const vec = (x, y, z) => ({ x, y, z });

    const natives = {
      // players
      GetPlayers: () => [...s.players.keys()].sort((a, b) => a - b),
      GetNumPlayers: () => s.players.size,
      GetPlayerName: (id) => player(id)?.name ?? null,
      GetPlayerPing: (id) => player(id)?.ping ?? -1,
      GetPlayerEndpoint: (id) => player(id)?.endpoint ?? null,
      GetPlayerIdentifiers: (id) => [...(player(id)?.identifiers ?? [])],
      GetPlayerCoords: (id) => {
        const p = player(id);
        return p ? vec(p.state.x, p.state.y, p.state.z) : null;
      },
      GetPlayerHeading: (id) => player(id)?.state.heading ?? 0,
      DropPlayer: (id, reason) => {
        player(id)?.kick(String(reason ?? 'Dropped by the server.'));
      },

      // events
      TriggerClientEvent: (name, target, ...args) => {
        if (typeof name !== 'string') throw new Error('TriggerClientEvent: event name must be a string');
        const to = Number(target);
        if (!Number.isInteger(to)) throw new Error('TriggerClientEvent: target must be a player id or -1');
        s.emitNet(to, name, args);
      },

      // convars
      GetConvar: (name, fallback) => this.convars.get(String(name)) ?? fallback ?? null,
      GetConvarInt: (name, fallback) => {
        const n = parseInt(this.convars.get(String(name)), 10);
        return Number.isNaN(n) ? (fallback ?? 0) : n;
      },
      SetConvar: (name, value) => {
        this.convars.set(String(name), String(value));
      },

      // resources and commands
      GetResourceState: (name) => (this.running.has(name) ? 'started' : this.available.has(name) ? 'stopped' : 'missing'),
      GetNumResources: () => this.running.size,
      ExecuteCommand: (raw) => this.executeCommand(String(raw), 0),
      GetGameTimer: () => Math.floor(performance.now()),
      GetHostName: () => s.name ?? '',
    };
    for (const [name, fn] of Object.entries(natives)) rt.registerGlobal(name, fn);
  }
}
