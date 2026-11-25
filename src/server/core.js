// SixM server core: networking, players, state sync and the HTTP status
// endpoints. This file knows nothing about scripting; the Lua layer sits on top
// and talks to it through the events and methods of `SixMServer`.

import { EventEmitter } from 'node:events';
import http from 'node:http';
import { readFileSync } from 'node:fs';
import { WebSocketServer } from 'ws';
import {
  PROTOCOL_VERSION,
  DEFAULT_PORT,
  Msg,
  MAX_EVENT_NAME_LENGTH,
  MAX_JSON_FRAME,
  emptyState,
  decodeState,
  encodeSnapshot,
  encodeMsg,
  decodeMsg,
  sanitizeName,
} from '../shared/protocol.js';
import { createLogger } from './logger.js';

function readPackageVersion() {
  try {
    const pkg = JSON.parse(readFileSync(new URL('../../package.json', import.meta.url), 'utf8'));
    return typeof pkg.version === 'string' ? pkg.version : '0.0.0';
  } catch {
    return '0.0.0'; // e.g. when bundled into a single executable
  }
}

const PACKAGE_VERSION = readPackageVersion();
const WS_OPEN = 1;
const CLOSE_REJECTED = 4001;
const CLOSE_KICKED = 4002;
/** Snapshots are skipped for a client whose send buffer is this far behind. */
const SNAPSHOT_BACKLOG_LIMIT = 1024 * 1024;
/** A client that lets this much pile up is not reading at all: drop it. */
const SEND_BACKLOG_LIMIT = 16 * 1024 * 1024;

const now = () => performance.now();

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (ch) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch]
  ));
}

function normalizeAddress(addr) {
  if (typeof addr !== 'string' || addr === '') return 'unknown';
  return addr.startsWith('::ffff:') ? addr.slice(7) : addr;
}

function positiveNumber(value, fallback) {
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

class TokenBucket {
  constructor(ratePerSecond, capacity) {
    this.rate = ratePerSecond;
    this.capacity = capacity;
    this.tokens = capacity;
    this.last = now();
  }

  take() {
    const t = now();
    this.tokens = Math.min(this.capacity, this.tokens + ((t - this.last) / 1000) * this.rate);
    this.last = t;
    if (this.tokens < 1) return false;
    this.tokens -= 1;
    return true;
  }
}

/** A connected, fully joined player. Created by the server only. */
export class Player {
  constructor(server, conn, id, name) {
    this.id = id;
    this.name = name;
    this.endpoint = conn.endpoint;
    this.identifiers = conn.identifiers.slice();
    this.ping = 0;
    this.joinedAt = Date.now();
    this.state = emptyState();
    /** Sequence number of the last accepted STATE frame, -1 before the first. */
    this.lastSeq = -1;

    Object.defineProperties(this, {
      _server: { value: server },
      _conn: { value: conn },
      _hasState: { value: false, writable: true },
      _dropped: { value: false, writable: true },
      _strikes: { value: 0, writable: true },
      _lastStrikeAt: { value: 0, writable: true },
      _lastSeen: { value: now(), writable: true },
      _pingSentAt: { value: 0, writable: true },
      _lastSnapshotCount: { value: 0, writable: true },
      _eventBucket: { value: new TokenBucket(server._opts.eventRate, server._opts.eventBurst) },
      _stateBucket: { value: new TokenBucket(server._opts.stateRate, server._opts.stateRate) },
    });
  }

  /** False once the player has been dropped. */
  get connected() {
    return !this._dropped;
  }

  /** Send a JSON control message. @returns {boolean} whether it was queued */
  send(type, fields) {
    let text;
    try {
      text = encodeMsg(type, fields);
    } catch (err) {
      this._server._log.error(`cannot encode "${type}" for player ${this.id}:`, err);
      return false;
    }
    return this._server._sendRaw(this, text, false);
  }

  /** Send a binary frame. @returns {boolean} whether it was queued */
  sendBinary(buf) {
    return this._server._sendRaw(this, buf, true);
  }

  /** Tell the client why, close the socket and drop the player immediately. */
  kick(reason = 'kicked') {
    this._server._kick(this, reason);
  }
}

export class SixMServer extends EventEmitter {
  /**
   * @param {object} [options]
   * Timing / limit options beyond the documented ones exist mainly so tests can
   * run fast: helloTimeoutMs, deferTimeoutMs, heartbeatIntervalMs,
   * heartbeatTimeoutMs, maxStrikes, strikeDecayMs, eventRate, eventBurst,
   * stateRate, closeGraceMs, maxPendingConnections.
   */
  constructor({
    port = DEFAULT_PORT,
    host = '0.0.0.0',
    name = 'SixM Server',
    maxPlayers = 32,
    tickRate = 20,
    streamDistance = 500,
    logger,
    build = PACKAGE_VERSION,
    tags = [],
    vars = {},
    helloTimeoutMs = 5000,
    deferTimeoutMs = 10000,
    heartbeatIntervalMs = 5000,
    heartbeatTimeoutMs = 15000,
    maxStrikes = 5,
    strikeDecayMs = 10000,
    eventRate = 50,
    eventBurst = 100,
    stateRate,
    closeGraceMs = 1000,
    maxPendingConnections,
  } = {}) {
    super();
    if (!Number.isInteger(port) || port < 0 || port > 65535) throw new TypeError('port must be an integer 0-65535');
    if (!Number.isInteger(maxPlayers) || maxPlayers < 1 || maxPlayers > 65535) {
      throw new TypeError('maxPlayers must be an integer 1-65535');
    }
    if (!Number.isFinite(tickRate) || tickRate <= 0 || tickRate > 1000) {
      throw new TypeError('tickRate must be a number in (0, 1000]');
    }
    if (typeof streamDistance !== 'number' || Number.isNaN(streamDistance)) {
      throw new TypeError('streamDistance must be a number');
    }

    this.name = String(name);
    this.host = String(host);
    this.maxPlayers = maxPlayers;
    this.tickRate = tickRate;
    this.streamDistance = streamDistance;
    this.build = String(build);
    this.tags = Array.isArray(tags) ? tags.map(String) : [];
    /** @type {Map<number, Player>} */
    this.players = new Map();
    /** Current tick counter; 0 until the first tick has run. */
    this.tick = 0;

    this._log = logger ?? createLogger({ name: 'server' });
    this._requestedPort = port;
    this._boundPort = null;
    this._opts = {
      helloTimeoutMs: positiveNumber(helloTimeoutMs, 5000),
      deferTimeoutMs: positiveNumber(deferTimeoutMs, 10000),
      heartbeatIntervalMs: positiveNumber(heartbeatIntervalMs, 5000),
      heartbeatTimeoutMs: positiveNumber(heartbeatTimeoutMs, 15000),
      maxStrikes: Math.max(1, Math.floor(positiveNumber(maxStrikes, 5))),
      strikeDecayMs: positiveNumber(strikeDecayMs, 10000),
      eventRate: positiveNumber(eventRate, 50),
      eventBurst: Math.max(1, positiveNumber(eventBurst, 100)),
      stateRate: Math.max(1, positiveNumber(stateRate, tickRate * 4)),
      closeGraceMs: positiveNumber(closeGraceMs, 1000),
      maxPendingConnections: Math.floor(positiveNumber(maxPendingConnections, maxPlayers * 2 + 16)),
    };

    this._vars = Object.create(null);
    if (vars && typeof vars === 'object') {
      for (const [k, v] of Object.entries(vars)) this._vars[k] = v;
    }
    this._clientResources = [];
    this._resourceNames = [];

    /** Every open socket, joined or not. */
    this._conns = new Set();
    this._pendingCount = 0;
    this._http = null;
    this._wss = null;
    this._startPromise = null;
    this._stopPromise = null;
    this._running = false;
    this._startedAt = 0;
    this._tickTimer = null;
    this._heartbeatTimer = null;
    this._tickInterval = 1000 / tickRate;
    this._tickEpoch = 0;
    this._tickIndex = 0;
    this._lastTickAt = 0;
  }

  /** The bound port once started, otherwise the requested one. */
  get port() {
    return this._boundPort ?? this._requestedPort;
  }

  getPlayer(id) {
    return this.players.get(Number(id));
  }

  // ---------------------------------------------------------------- lifecycle

  /** @returns {Promise<number>} the port actually bound */
  start() {
    if (this._stopPromise) return Promise.reject(new Error('server was stopped; create a new instance'));
    if (this._startPromise) return this._startPromise;

    this._startPromise = new Promise((resolve, reject) => {
      const server = http.createServer((req, res) => {
        try {
          this._handleHttp(req, res);
        } catch (err) {
          this._log.error('http handler failed:', err);
          try {
            if (!res.headersSent) res.writeHead(500);
            res.end();
          } catch { /* socket already gone */ }
        }
      });
      const wss = new WebSocketServer({ noServer: true, maxPayload: MAX_JSON_FRAME, perMessageDeflate: false });
      this._http = server;
      this._wss = wss;

      wss.on('error', (err) => this._log.error('websocket server error:', err));
      server.on('upgrade', (req, socket, head) => {
        try {
          this._handleUpgrade(req, socket, head);
        } catch (err) {
          this._log.error('upgrade failed:', err);
          socket.destroy();
        }
      });
      server.on('clientError', (err, socket) => {
        if (socket.writable) socket.end('HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n');
        else socket.destroy();
      });

      const onListenError = (err) => {
        this._startPromise = null;
        this._http = null;
        this._wss = null;
        reject(err);
      };
      server.once('error', onListenError);
      server.listen(this._requestedPort, this.host, () => {
        server.removeListener('error', onListenError);
        server.on('error', (err) => this._log.error('http server error:', err));
        const address = server.address();
        this._boundPort = address && typeof address === 'object' ? address.port : this._requestedPort;
        this._running = true;
        this._startedAt = Date.now();
        this._tickEpoch = now();
        this._lastTickAt = this._tickEpoch;
        this._tickIndex = 0;
        this._scheduleTick();
        this._heartbeatTimer = setInterval(() => {
          try {
            this._heartbeat();
          } catch (err) {
            this._log.error('heartbeat failed:', err);
          }
        }, this._opts.heartbeatIntervalMs);
        this._log.info(`"${this.name}" listening on ${this.host}:${this._boundPort} (${this.tickRate} Hz, ${this.maxPlayers} slots)`);
        resolve(this._boundPort);
      });
    });
    return this._startPromise;
  }

  /** Kick everyone, close every socket and the HTTP server. Idempotent. */
  stop(reason = 'server shutting down') {
    if (this._stopPromise) return this._stopPromise;
    this._stopPromise = this._stop(String(reason));
    return this._stopPromise;
  }

  async _stop(reason) {
    if (this._startPromise) {
      try {
        await this._startPromise;
      } catch { /* never came up; nothing to tear down */ }
    }
    this._running = false;
    clearTimeout(this._tickTimer);
    clearInterval(this._heartbeatTimer);
    this._tickTimer = null;
    this._heartbeatTimer = null;

    for (const player of [...this.players.values()]) this._kick(player, reason);
    for (const conn of [...this._conns]) {
      if (conn.stage !== 'closing') this._reject(conn, reason);
    }

    // Give clients a moment to complete the close handshake, then cut them off.
    const deadline = now() + Math.min(this._opts.closeGraceMs, 500);
    while (this._conns.size > 0 && now() < deadline) {
      await new Promise((r) => setTimeout(r, 5));
    }
    for (const conn of [...this._conns]) {
      try {
        conn.ws.terminate();
      } catch { /* already gone */ }
      this._cleanupConn(conn);
    }

    const server = this._http;
    if (server) {
      await new Promise((resolve) => {
        if (!server.listening) return resolve();
        server.close(() => resolve());
        if (typeof server.closeAllConnections === 'function') server.closeAllConnections();
      });
    }
    if (this._wss) {
      try {
        this._wss.close();
      } catch { /* not running */ }
    }
    this._log.info(`server stopped (${reason})`);
  }

  // ------------------------------------------------------------- public sends

  /**
   * Trigger a client event. `target` is a player id, or -1 for everyone.
   * @returns {boolean} false when the target is unknown or args cannot be serialised
   */
  emitNet(target, name, args = []) {
    if (typeof name !== 'string' || name.length === 0 || name.length > MAX_EVENT_NAME_LENGTH) {
      this._log.warn('emitNet: invalid event name');
      return false;
    }
    let text;
    try {
      text = encodeMsg(Msg.EVENT, { name, args: Array.isArray(args) ? args : [args], src: 0 });
    } catch (err) {
      this._log.error(`emitNet: cannot serialise args of "${name}":`, err);
      return false;
    }
    if (target === -1) {
      for (const p of [...this.players.values()]) this._sendRaw(p, text, false);
      return true;
    }
    const player = this.getPlayer(target);
    return player ? this._sendRaw(player, text, false) : false;
  }

  /** Send one control message to every player, optionally skipping one id. */
  broadcast(type, fields, exceptId) {
    let text;
    try {
      text = encodeMsg(type, fields);
    } catch (err) {
      this._log.error(`broadcast: cannot encode "${type}":`, err);
      return;
    }
    for (const p of [...this.players.values()]) {
      if (p.id !== exceptId) this._sendRaw(p, text, false);
    }
  }

  /** Value sent as `resources` in WELCOME; a function is called with each joining player. */
  setClientResources(fnOrArray) {
    this._clientResources = fnOrArray ?? [];
  }

  /** Names listed under `resources` in /info.json. */
  setResourceNames(names) {
    this._resourceNames = Array.isArray(names) ? names.map(String) : [];
  }

  /** Set (or with `undefined`/`null`, remove) a public variable shown in /info.json. */
  setInfoVar(key, value) {
    const k = String(key);
    if (value === undefined || value === null) delete this._vars[k];
    else this._vars[k] = value;
  }

  // ------------------------------------------------------------------ helpers

  /**
   * Like emit(), but a throwing (or rejecting async) listener is logged instead
   * of propagating into the networking code.
   */
  _safeEmit(event, ...args) {
    for (const listener of this.rawListeners(event)) {
      try {
        const result = listener.apply(this, args);
        if (result && typeof result.then === 'function') {
          result.then(undefined, (err) => this._log.error(`async "${event}" listener failed:`, err));
        }
      } catch (err) {
        this._log.error(`"${event}" listener threw:`, err);
      }
    }
  }

  _sendRaw(player, data, binary) {
    if (player._dropped) return false;
    const ws = player._conn.ws;
    if (ws.readyState !== WS_OPEN) return false;
    if (ws.bufferedAmount > SEND_BACKLOG_LIMIT) {
      this._drop(player, 'send buffer overflow', true);
      return false;
    }
    try {
      ws.send(data, { binary });
      return true;
    } catch (err) {
      this._log.debug(`send to player ${player.id} failed:`, err);
      return false;
    }
  }

  _sendConn(conn, text) {
    try {
      if (conn.ws.readyState === WS_OPEN) conn.ws.send(text);
    } catch (err) {
      this._log.debug(`send to ${conn.endpoint} failed:`, err);
    }
  }

  /** Start a graceful close and make sure the socket dies even if the peer stalls. */
  _closeConn(conn, code, why) {
    conn.stage = 'closing';
    clearTimeout(conn.timer);
    conn.timer = null;
    try {
      conn.ws.close(code, why);
    } catch { /* already closing */ }
    if (!conn.closeTimer) {
      conn.closeTimer = setTimeout(() => {
        try {
          conn.ws.terminate();
        } catch { /* already gone */ }
      }, this._opts.closeGraceMs);
      conn.closeTimer.unref();
    }
  }

  _cleanupConn(conn) {
    clearTimeout(conn.timer);
    clearTimeout(conn.closeTimer);
    conn.timer = null;
    conn.closeTimer = null;
    if (conn.settle) conn.settle();
    if (this._conns.delete(conn) && !conn.player) this._pendingCount--;
  }

  _reject(conn, reason) {
    if (conn.stage === 'closing') return;
    this._log.debug(`rejected ${conn.endpoint}: ${reason}`);
    this._sendConn(conn, encodeMsg(Msg.REJECT, { reason }));
    this._closeConn(conn, CLOSE_REJECTED, 'rejected');
  }

  _kick(player, reason) {
    if (player._dropped) return;
    const why = String(reason ?? 'kicked');
    this._sendRaw(player, encodeMsg(Msg.KICK, { reason: why }), false);
    this._drop(player, why, false);
  }

  /** Remove a player from the world. `hard` skips the close handshake. */
  _drop(player, reason, hard) {
    if (player._dropped) return;
    player._dropped = true;
    if (this.players.get(player.id) === player) this.players.delete(player.id);
    const conn = player._conn;
    if (hard) {
      conn.stage = 'closing';
      try {
        conn.ws.terminate();
      } catch { /* already gone */ }
    } else if (conn.stage !== 'closing') {
      this._closeConn(conn, CLOSE_KICKED, 'kicked');
    }
    this._log.info(`player ${player.id} "${player.name}" dropped (${reason})`);
    this.broadcast(Msg.LEAVE, { id: player.id, reason });
    this._safeEmit('playerDropped', player, reason);
  }

  _lowestFreeId() {
    for (let id = 1; id <= this.maxPlayers; id++) {
      if (!this.players.has(id)) return id;
    }
    return 0;
  }

  // --------------------------------------------------------------- connection

  _handleUpgrade(req, socket, head) {
    const path = (req.url || '/').split('?')[0];
    const refuse = (status) => {
      socket.end(`HTTP/1.1 ${status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`);
    };
    socket.on('error', () => {});
    if (!this._running) return refuse('503 Service Unavailable');
    if (path !== '/' && path !== '/ws') return refuse('404 Not Found');
    if (this._pendingCount >= this._opts.maxPendingConnections) return refuse('503 Service Unavailable');
    this._wss.handleUpgrade(req, socket, head, (ws) => {
      try {
        this._onConnection(ws, req);
      } catch (err) {
        this._log.error('connection setup failed:', err);
        ws.terminate();
      }
    });
  }

  _onConnection(ws, req) {
    const address = normalizeAddress(req.socket.remoteAddress);
    const remotePort = req.socket.remotePort ?? 0;
    const conn = {
      ws,
      address,
      endpoint: address.includes(':') ? `[${address}]:${remotePort}` : `${address}:${remotePort}`,
      identifiers: [`ip:${address}`],
      stage: 'hello', // hello -> pending -> joined -> closing
      player: null,
      timer: null,
      closeTimer: null,
      settle: null,
    };
    this._conns.add(conn);
    this._pendingCount++;

    conn.timer = setTimeout(() => this._reject(conn, 'handshake timed out'), this._opts.helloTimeoutMs);

    ws.on('error', (err) => this._log.debug(`socket error from ${conn.endpoint}: ${err.message}`));
    ws.on('pong', () => {
      const p = conn.player;
      if (!p) return;
      const t = now();
      p._lastSeen = t;
      if (p._pingSentAt) {
        p.ping = Math.max(0, Math.round(t - p._pingSentAt));
        p._pingSentAt = 0;
      }
    });
    ws.on('close', () => {
      const player = conn.player;
      this._cleanupConn(conn);
      conn.stage = 'closing';
      if (player && !player._dropped) {
        try {
          this._drop(player, 'disconnected', true);
        } catch (err) {
          this._log.error('drop failed:', err);
        }
      }
    });
    ws.on('message', (data, isBinary) => {
      try {
        this._onMessage(conn, data, isBinary);
      } catch (err) {
        this._log.error(`message handler failed for ${conn.endpoint}:`, err);
      }
    });
  }

  _onMessage(conn, data, isBinary) {
    switch (conn.stage) {
      case 'hello':
        return this._onHello(conn, data, isBinary);
      case 'joined':
        if (conn.player && !conn.player._dropped) {
          conn.player._lastSeen = now();
          if (isBinary) this._onState(conn.player, data);
          else this._onControl(conn.player, data);
        }
        return undefined;
      default:
        return undefined; // pending a deferral, or closing: ignore
    }
  }

  _onHello(conn, data, isBinary) {
    const msg = isBinary ? null : decodeMsg(data.toString('utf8'));
    if (!msg || msg.t !== Msg.HELLO) return this._reject(conn, 'expected a hello message');
    if (msg.proto !== PROTOCOL_VERSION) {
      return this._reject(conn, `protocol version mismatch (server ${PROTOCOL_VERSION}, client ${
        typeof msg.proto === 'number' ? msg.proto : 'unknown'})`);
    }
    const name = sanitizeName(msg.name);
    if (!name) return this._reject(conn, 'invalid player name');
    if (this.players.size >= this.maxPlayers) return this._reject(conn, 'server is full');

    clearTimeout(conn.timer);
    conn.timer = null;
    conn.stage = 'pending';

    const info = {
      name,
      endpoint: conn.endpoint,
      identifiers: conn.identifiers.slice(),
      build: typeof msg.build === 'string' ? msg.build.slice(0, 64) : null,
      token: typeof msg.token === 'string' ? msg.token.slice(0, 4096) : null,
    };

    let settled = false;
    let sync = true;
    let pending = 0;
    let rejection = null;

    const finish = () => {
      if (settled || sync) return;
      if (rejection === null && pending > 0) return;
      settled = true;
      conn.settle = null;
      clearTimeout(conn.timer);
      conn.timer = null;
      if (conn.stage !== 'pending') return; // socket closed meanwhile
      if (rejection !== null) this._reject(conn, rejection);
      else this._join(conn, name);
    };
    const reject = (reason) => {
      if (settled) return;
      if (rejection === null) {
        rejection = reason === undefined || reason === null || reason === '' ? 'connection rejected' : String(reason);
      }
      finish();
    };
    const deferrals = {
      reject,
      defer: () => {
        if (settled) return () => {};
        pending++;
        let called = false;
        return (rejectReason) => {
          if (called) return;
          called = true;
          pending--;
          if (rejectReason !== undefined && rejectReason !== null && rejectReason !== false) reject(rejectReason);
          else finish();
        };
      },
    };
    // Lets the close handler cancel an outstanding deferral.
    conn.settle = () => {
      settled = true;
    };

    this._safeEmit('playerConnecting', info, deferrals);
    sync = false;
    if (settled) return undefined; // socket closed while listeners ran
    if (rejection === null && pending > 0) {
      conn.timer = setTimeout(() => reject('connection deferral timed out'), this._opts.deferTimeoutMs);
    }
    finish();
    return undefined;
  }

  _join(conn, name) {
    if (conn.ws.readyState !== WS_OPEN) return;
    if (!this._running) return this._reject(conn, 'server shutting down');
    const id = this._lowestFreeId();
    if (!id) return this._reject(conn, 'server is full');

    const player = new Player(this, conn, id, name);
    conn.player = player;
    conn.stage = 'joined';
    this._pendingCount--;

    let resources = this._clientResources;
    if (typeof resources === 'function') {
      try {
        resources = resources(player);
      } catch (err) {
        this._log.error('client resources callback threw:', err);
        resources = [];
      }
    }

    const others = [];
    for (const p of this.players.values()) others.push({ id: p.id, name: p.name });
    this.players.set(id, player);

    const sent = player.send(Msg.WELCOME, {
      id,
      name,
      serverName: this.name,
      tickRate: this.tickRate,
      maxPlayers: this.maxPlayers,
      players: others,
      resources: resources ?? [],
    });
    if (!sent) {
      // Unserialisable resources or a dead socket: do not leave a ghost behind.
      this.players.delete(id);
      player._dropped = true;
      return this._reject(conn, 'internal server error');
    }

    this._log.info(`player ${id} "${name}" joined from ${conn.endpoint}`);
    this.broadcast(Msg.JOIN, { id, name }, id);
    this._ping(player);
    this._safeEmit('playerJoined', player);
    return undefined;
  }

  // ----------------------------------------------------------- client frames

  _strike(player, why) {
    const t = now();
    if (player._strikes > 0) {
      const forgiven = Math.floor((t - player._lastStrikeAt) / this._opts.strikeDecayMs);
      if (forgiven > 0) player._strikes = Math.max(0, player._strikes - forgiven);
    }
    player._lastStrikeAt = t;
    player._strikes++;
    this._log.debug(`player ${player.id} sent a malformed frame (${why}); strike ${player._strikes}/${this._opts.maxStrikes}`);
    if (player._strikes >= this._opts.maxStrikes) this._kick(player, 'too many malformed messages');
  }

  _onControl(player, data) {
    const msg = decodeMsg(data.toString('utf8'));
    if (!msg) return this._strike(player, 'invalid json');
    switch (msg.t) {
      case Msg.EVENT: {
        if (!player._eventBucket.take()) return this._kick(player, 'event rate limit');
        const { name } = msg;
        if (typeof name !== 'string' || name.length === 0 || name.length > MAX_EVENT_NAME_LENGTH) {
          return this._strike(player, 'bad event name');
        }
        const args = msg.args === undefined ? [] : msg.args;
        if (!Array.isArray(args)) return this._strike(player, 'event args not an array');
        return this._safeEmit('netEvent', player, name, args);
      }
      case Msg.PING: {
        if (typeof msg.ts !== 'number' || !Number.isFinite(msg.ts)) return this._strike(player, 'bad ping');
        // Shares the event budget, but running dry only drops the reply.
        if (player._eventBucket.take()) player.send(Msg.PONG, { ts: msg.ts });
        return undefined;
      }
      default:
        return this._strike(player, `unexpected message type "${String(msg.t).slice(0, 32)}"`);
    }
  }

  _onState(player, data) {
    if (!player._stateBucket.take()) return; // flooding: drop silently
    const decoded = decodeState(data);
    if (!decoded) return this._strike(player, 'bad state frame');
    if (player._hasState) {
      // Serial-number arithmetic: "newer" means less than half the uint32
      // range ahead, which keeps working across the 0xFFFFFFFF -> 0 wrap.
      const ahead = (decoded.seq - player.lastSeq) >>> 0;
      if (ahead === 0 || ahead >= 0x80000000) return; // duplicate or stale
    }
    player.lastSeq = decoded.seq;
    player.state = decoded.state;
    player._hasState = true;
  }

  // ------------------------------------------------------- heartbeat and tick

  _ping(player) {
    const ws = player._conn.ws;
    if (ws.readyState !== WS_OPEN) return;
    if (!player._pingSentAt) player._pingSentAt = now();
    try {
      ws.ping();
    } catch (err) {
      this._log.debug(`ping to player ${player.id} failed:`, err);
    }
  }

  _heartbeat() {
    const t = now();
    for (const player of [...this.players.values()]) {
      if (t - player._lastSeen > this._opts.heartbeatTimeoutMs) {
        this._sendRaw(player, encodeMsg(Msg.KICK, { reason: 'timed out' }), false);
        this._drop(player, 'timed out', true);
      } else {
        this._ping(player);
      }
    }
  }

  /** Deadlines are derived from the epoch, so timer lateness does not accumulate. */
  _scheduleTick() {
    if (!this._running) return;
    const t = now();
    let due = this._tickEpoch + (this._tickIndex + 1) * this._tickInterval;
    if (t - due > this._tickInterval * 5) {
      // Stalled badly (debugger, suspended machine): resynchronise instead of
      // firing a burst of catch-up ticks.
      this._tickEpoch = t - (this._tickIndex + 1) * this._tickInterval;
      due = t;
    }
    this._tickTimer = setTimeout(() => this._runTick(), Math.max(0, due - t));
  }

  _runTick() {
    if (!this._running) return;
    const t = now();
    const dt = t - this._lastTickAt;
    this._lastTickAt = t;
    this._tickIndex++;
    this.tick++;
    try {
      this._safeEmit('tick', this.tick, dt);
      this._sendSnapshots();
    } catch (err) {
      this._log.error('tick failed:', err);
    }
    this._scheduleTick();
  }

  _sendSnapshots() {
    if (this.players.size < 2) {
      // Nobody else to report, but a lone player may still need the one empty
      // snapshot that despawns a neighbour who just left.
      for (const p of this.players.values()) {
        if (p._lastSnapshotCount > 0) this._sendSnapshot(p, []);
      }
      return;
    }
    const visible = [];
    for (const p of this.players.values()) if (p._hasState) visible.push(p);
    const unlimited = !(this.streamDistance > 0);
    const maxSq = this.streamDistance * this.streamDistance;

    for (const p of [...this.players.values()]) {
      const entities = [];
      // With culling on, a player who has not reported a position yet is not
      // "anywhere", so nothing is in range of them.
      if (unlimited || p._hasState) {
        const a = p.state;
        for (const o of visible) {
          if (o === p) continue;
          if (!unlimited) {
            const dx = o.state.x - a.x;
            const dy = o.state.y - a.y;
            const dz = o.state.z - a.z;
            if (dx * dx + dy * dy + dz * dz > maxSq) continue;
          }
          entities.push({ id: o.id, state: o.state });
        }
      }
      if (entities.length === 0 && p._lastSnapshotCount === 0) continue;
      this._sendSnapshot(p, entities);
    }
  }

  _sendSnapshot(player, entities) {
    // A client that is not keeping up gets fewer snapshots, not a longer queue.
    if (player._conn.ws.bufferedAmount > SNAPSHOT_BACKLOG_LIMIT) return;
    if (player.sendBinary(encodeSnapshot(this.tick, entities))) {
      player._lastSnapshotCount = entities.length;
    }
  }

  // --------------------------------------------------------------------- http

  _handleHttp(req, res) {
    const path = (req.url || '/').split('?')[0];
    const head = req.method === 'HEAD';
    res.setHeader('X-Content-Type-Options', 'nosniff');
    res.setHeader('Cache-Control', 'no-store');

    const sendJson = (status, body, extra) => {
      const payload = Buffer.from(JSON.stringify(body));
      res.writeHead(status, {
        'Content-Type': 'application/json; charset=utf-8',
        'Content-Length': payload.length,
        'Access-Control-Allow-Origin': '*',
        ...extra,
      });
      res.end(head ? undefined : payload);
    };

    if (req.method !== 'GET' && !head) {
      req.resume();
      return sendJson(405, { error: 'method not allowed' }, { Allow: 'GET, HEAD' });
    }

    switch (path) {
      case '/info.json':
        return sendJson(200, {
          server: 'SixM',
          name: this.name,
          build: this.build,
          protocol: PROTOCOL_VERSION,
          maxPlayers: this.maxPlayers,
          tickRate: this.tickRate,
          tags: this.tags,
          vars: { ...this._vars },
          resources: this._resourceNames,
        });
      case '/players.json':
        return sendJson(200, this._publicPlayers());
      case '/dynamic.json':
        return sendJson(200, {
          name: this.name,
          clients: this.players.size,
          maxClients: this.maxPlayers,
          uptime: this._startedAt ? Math.floor((Date.now() - this._startedAt) / 1000) : 0,
        });
      case '/':
      case '/index.html': {
        const payload = Buffer.from(this._statusPage());
        res.writeHead(200, {
          'Content-Type': 'text/html; charset=utf-8',
          'Content-Length': payload.length,
          'Content-Security-Policy':
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'",
          'Referrer-Policy': 'no-referrer',
        });
        return res.end(head ? undefined : payload);
      }
      default:
        return sendJson(404, { error: 'not found' });
    }
  }

  /** Deliberately no endpoints or identifiers: this list is public. */
  _publicPlayers() {
    const list = [];
    for (const p of this.players.values()) list.push({ id: p.id, name: p.name, ping: p.ping });
    return list;
  }

  _statusPage() {
    const title = escapeHtml(this.name);
    const players = this._publicPlayers();
    const rows = players.length
      ? players.map((p) => `<tr><td>${p.id}</td><td>${escapeHtml(p.name)}</td><td>${p.ping} ms</td></tr>`).join('')
      : '<tr><td colspan="3" class="empty">No players online</td></tr>';
    // The script below contains no server-side interpolation; everything it
    // renders goes through textContent.
    return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${title}</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; background: #0e1116; color: #e6e9ef; font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
  main { max-width: 640px; margin: 0 auto; padding: 48px 16px; }
  h1 { margin: 0 0 4px; font-size: 24px; font-weight: 600; overflow-wrap: anywhere; }
  .meta { color: #8b93a1; font-size: 13px; margin-bottom: 24px; }
  .count { display: inline-block; padding: 2px 10px; border-radius: 999px; background: #17202e; color: #7cc4ff; font-variant-numeric: tabular-nums; }
  table { width: 100%; border-collapse: collapse; background: #151a22; border: 1px solid #232a35; border-radius: 8px; overflow: hidden; }
  th, td { text-align: left; padding: 9px 14px; border-bottom: 1px solid #232a35; overflow-wrap: anywhere; }
  th { font-size: 12px; font-weight: 600; text-transform: uppercase; letter-spacing: .05em; color: #8b93a1; background: #181e28; }
  tr:last-child td { border-bottom: 0; }
  td:first-child, th:first-child { width: 56px; }
  td:last-child, th:last-child { width: 96px; text-align: right; font-variant-numeric: tabular-nums; }
  .empty { text-align: center !important; color: #8b93a1; padding: 22px; }
  footer { margin-top: 20px; color: #5d6573; font-size: 12px; }
</style>
</head>
<body>
<main>
  <h1>${title}</h1>
  <div class="meta">Players <span class="count" id="count">${players.length} / ${this.maxPlayers}</span></div>
  <table>
    <thead><tr><th>ID</th><th>Name</th><th>Ping</th></tr></thead>
    <tbody id="players">${rows}</tbody>
  </table>
  <footer>SixM ${escapeHtml(this.build)} &middot; protocol ${PROTOCOL_VERSION}</footer>
</main>
<script>
(function () {
  var max = ${this.maxPlayers};
  var body = document.getElementById('players');
  var count = document.getElementById('count');
  function cell(row, text, cls) {
    var td = document.createElement('td');
    td.textContent = text;
    if (cls) td.className = cls;
    row.appendChild(td);
    return td;
  }
  function render(list) {
    count.textContent = list.length + ' / ' + max;
    body.textContent = '';
    if (!list.length) {
      var empty = document.createElement('tr');
      cell(empty, 'No players online', 'empty').colSpan = 3;
      body.appendChild(empty);
      return;
    }
    list.forEach(function (p) {
      var row = document.createElement('tr');
      cell(row, String(p.id));
      cell(row, String(p.name));
      cell(row, String(p.ping) + ' ms');
      body.appendChild(row);
    });
  }
  function refresh() {
    fetch('players.json', { cache: 'no-store' })
      .then(function (r) { return r.json(); })
      .then(function (list) { if (Array.isArray(list)) render(list); })
      .catch(function () {});
  }
  setInterval(refresh, 5000);
})();
</script>
</body>
</html>
`;
  }
}

export default SixMServer;
