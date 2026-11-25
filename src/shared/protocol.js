// SixM wire protocol, shared by the server and every client.
//
// Transport: one WebSocket per player.
//   - Text frames carry JSON control messages: { t: <type>, ... }.
//   - Binary frames carry entity state, which is sent many times per second and
//     therefore packed tightly (little-endian).

export const PROTOCOL_VERSION = 1;
export const DEFAULT_PORT = 30120;

/** JSON control message types. */
export const Msg = Object.freeze({
  // client -> server
  HELLO: 'hello', // { proto, name, build, token? }
  EVENT: 'event', // { name, args }            (also server -> client, with `src`)
  PING: 'ping', // { ts }
  // server -> client
  WELCOME: 'welcome', // { id, serverName, tickRate, maxPlayers, players:[{id,name}], resources:[...] }
  REJECT: 'reject', // { reason }
  JOIN: 'join', // { id, name }
  LEAVE: 'leave', // { id, reason }
  PONG: 'pong', // { ts }
  KICK: 'kick', // { reason }
});

/** Binary frame types (first byte). */
export const Bin = Object.freeze({
  STATE: 1, // client -> server: this player's own state
  SNAPSHOT: 2, // server -> client: states of the entities relevant to that client
});

/** Bit flags in EntityState.flags. */
export const StateFlags = Object.freeze({
  IN_VEHICLE: 1 << 0,
  AIMING: 1 << 1,
  SHOOTING: 1 << 2,
  DEAD: 1 << 3,
  SPRINTING: 1 << 4,
});

// x y z | heading | vx vy vz | model | flags
export const STATE_SIZE = 12 + 4 + 12 + 4 + 2; // 34 bytes
const STATE_HEADER = 1 + 4; // type + seq
const SNAPSHOT_HEADER = 1 + 4 + 2; // type + tick + count
const SNAPSHOT_ENTRY = 2 + STATE_SIZE; // id + state

export const MAX_NAME_LENGTH = 32;
export const MAX_EVENT_NAME_LENGTH = 128;
export const MAX_JSON_FRAME = 256 * 1024;

/** @returns {EntityState} a zeroed state */
export function emptyState() {
  return { x: 0, y: 0, z: 0, heading: 0, vx: 0, vy: 0, vz: 0, model: 0, flags: 0 };
}

function writeState(view, o, s) {
  view.setFloat32(o, s.x, true);
  view.setFloat32(o + 4, s.y, true);
  view.setFloat32(o + 8, s.z, true);
  view.setFloat32(o + 12, s.heading, true);
  view.setFloat32(o + 16, s.vx, true);
  view.setFloat32(o + 20, s.vy, true);
  view.setFloat32(o + 24, s.vz, true);
  view.setUint32(o + 28, s.model >>> 0, true);
  view.setUint16(o + 32, s.flags & 0xffff, true);
}

function readState(view, o) {
  return {
    x: view.getFloat32(o, true),
    y: view.getFloat32(o + 4, true),
    z: view.getFloat32(o + 8, true),
    heading: view.getFloat32(o + 12, true),
    vx: view.getFloat32(o + 16, true),
    vy: view.getFloat32(o + 20, true),
    vz: view.getFloat32(o + 24, true),
    model: view.getUint32(o + 28, true),
    flags: view.getUint16(o + 32, true),
  };
}

function viewOf(buf) {
  return new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
}

/** A state is usable only if every float is finite. */
export function isSaneState(s) {
  return (
    Number.isFinite(s.x) && Number.isFinite(s.y) && Number.isFinite(s.z) &&
    Number.isFinite(s.heading) &&
    Number.isFinite(s.vx) && Number.isFinite(s.vy) && Number.isFinite(s.vz)
  );
}

/** client -> server. @returns {Uint8Array} */
export function encodeState(seq, state) {
  const buf = new Uint8Array(STATE_HEADER + STATE_SIZE);
  const v = viewOf(buf);
  v.setUint8(0, Bin.STATE);
  v.setUint32(1, seq >>> 0, true);
  writeState(v, STATE_HEADER, state);
  return buf;
}

/** @returns {{seq:number, state:EntityState}|null} null when malformed */
export function decodeState(buf) {
  if (buf.byteLength !== STATE_HEADER + STATE_SIZE) return null;
  const v = viewOf(buf);
  if (v.getUint8(0) !== Bin.STATE) return null;
  const state = readState(v, STATE_HEADER);
  if (!isSaneState(state)) return null;
  return { seq: v.getUint32(1, true), state };
}

/**
 * server -> client.
 * @param {number} tick
 * @param {Array<{id:number, state:EntityState}>} entities
 * @returns {Uint8Array}
 */
export function encodeSnapshot(tick, entities) {
  const buf = new Uint8Array(SNAPSHOT_HEADER + entities.length * SNAPSHOT_ENTRY);
  const v = viewOf(buf);
  v.setUint8(0, Bin.SNAPSHOT);
  v.setUint32(1, tick >>> 0, true);
  v.setUint16(5, entities.length, true);
  let o = SNAPSHOT_HEADER;
  for (const e of entities) {
    v.setUint16(o, e.id, true);
    writeState(v, o + 2, e.state);
    o += SNAPSHOT_ENTRY;
  }
  return buf;
}

/** @returns {{tick:number, entities:Array<{id:number, state:EntityState}>}|null} */
export function decodeSnapshot(buf) {
  if (buf.byteLength < SNAPSHOT_HEADER) return null;
  const v = viewOf(buf);
  if (v.getUint8(0) !== Bin.SNAPSHOT) return null;
  const count = v.getUint16(5, true);
  if (buf.byteLength !== SNAPSHOT_HEADER + count * SNAPSHOT_ENTRY) return null;
  const entities = [];
  let o = SNAPSHOT_HEADER;
  for (let i = 0; i < count; i++) {
    entities.push({ id: v.getUint16(o, true), state: readState(v, o + 2) });
    o += SNAPSHOT_ENTRY;
  }
  return { tick: v.getUint32(1, true), entities };
}

/** Encode a JSON control message. */
export function encodeMsg(type, fields = {}) {
  return JSON.stringify({ t: type, ...fields });
}

/** @returns {object|null} the message, or null when it is not a valid control message */
export function decodeMsg(text) {
  if (typeof text !== 'string' || text.length > MAX_JSON_FRAME) return null;
  try {
    const m = JSON.parse(text);
    return m && typeof m === 'object' && typeof m.t === 'string' ? m : null;
  } catch {
    return null;
  }
}

/** Trim and bound a player-supplied display name; '' when nothing usable is left. */
export function sanitizeName(name) {
  if (typeof name !== 'string') return '';
  // eslint-disable-next-line no-control-regex
  return name.replace(/[\u0000-\u001f\u007f]/g, '').trim().slice(0, MAX_NAME_LENGTH);
}

/**
 * @typedef {object} EntityState
 * @property {number} x @property {number} y @property {number} z
 * @property {number} heading degrees
 * @property {number} vx @property {number} vy @property {number} vz
 * @property {number} model uint32 model hash
 * @property {number} flags StateFlags bitmask
 */
