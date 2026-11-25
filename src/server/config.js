// server.cfg parser.
//
// Syntax: one command per line, arguments separated by whitespace.
//   - `#` or `//` starts a comment when it appears at the start of a token
//     (so an unquoted `https://example.com` is NOT cut short).
//   - "double quoted" arguments may contain spaces; `\"` and `\\` are escapes.
//   - Blank lines are ignored.
//
// Nothing here throws on bad input: problems are reported as
// `{ line, message }` entries so the caller can print them all at once.

import { readFile } from 'node:fs/promises';

const isSpace = (ch) => ch === ' ' || ch === '\t' || ch === '\r' || ch === '\f' || ch === '\v';

/** Split one line into tokens. @returns {{tokens:string[], error:string|null}} */
function tokenizeLine(line) {
  const tokens = [];
  let i = 0;
  const n = line.length;
  while (i < n) {
    while (i < n && isSpace(line[i])) i++;
    if (i >= n) break;
    if (line[i] === '#' || (line[i] === '/' && line[i + 1] === '/')) break; // comment
    let token = '';
    while (i < n && !isSpace(line[i])) {
      if (line[i] === '"') {
        i++;
        let closed = false;
        while (i < n) {
          const ch = line[i];
          if (ch === '\\' && (line[i + 1] === '"' || line[i + 1] === '\\')) {
            token += line[i + 1];
            i += 2;
          } else if (ch === '"') {
            closed = true;
            i++;
            break;
          } else {
            token += ch;
            i++;
          }
        }
        if (!closed) return { tokens, error: 'unterminated quoted string' };
      } else {
        token += line[i];
        i++;
      }
    }
    tokens.push(token);
  }
  return { tokens, error: null };
}

/**
 * @param {string} text
 * @returns {{ commands: Array<{name:string, args:string[], line:number}>, errors: Array<{line:number, message:string}> }}
 */
export function parseConfig(text) {
  const commands = [];
  const errors = [];
  if (typeof text !== 'string') {
    return { commands, errors: [{ line: 0, message: 'config text must be a string' }] };
  }
  if (text.charCodeAt(0) === 0xfeff) text = text.slice(1); // BOM
  const lines = text.split('\n');
  for (let idx = 0; idx < lines.length; idx++) {
    const line = idx + 1;
    const { tokens, error } = tokenizeLine(lines[idx]);
    if (error) {
      errors.push({ line, message: error });
      continue;
    }
    if (tokens.length === 0) continue;
    const name = tokens[0].toLowerCase();
    if (name === '') {
      errors.push({ line, message: 'empty command name' });
      continue;
    }
    commands.push({ name, args: tokens.slice(1), line });
  }
  return { commands, errors };
}

function parseInteger(value, min, max) {
  if (!/^[+-]?\d+$/.test(value)) return null;
  const n = Number(value);
  return Number.isSafeInteger(n) && n >= min && n <= max ? n : null;
}

function parseEndpoint(value) {
  const s = value.trim();
  const colon = s.lastIndexOf(':');
  let host;
  let portText;
  if (colon === -1) {
    portText = s;
  } else {
    host = s.slice(0, colon);
    portText = s.slice(colon + 1);
    if (host.startsWith('[') && host.endsWith(']')) host = host.slice(1, -1);
    if (host === '') host = undefined;
  }
  const port = parseInteger(portText, 0, 65535);
  return port === null ? null : { host, port };
}

/** Convars that also drive a server setting. Each returns an error message or null. */
const SETTING_CONVARS = {
  sv_hostname(settings, value) {
    if (value.trim() === '') return 'sv_hostname must not be empty';
    settings.name = value;
    return null;
  },
  sv_maxclients(settings, value) {
    const n = parseInteger(value, 1, 65535);
    if (n === null) return `sv_maxclients must be an integer between 1 and 65535 (got "${value}")`;
    settings.maxPlayers = n;
    return null;
  },
  sv_port(settings, value) {
    const n = parseInteger(value, 0, 65535);
    if (n === null) return `sv_port must be an integer between 0 and 65535 (got "${value}")`;
    settings.port = n;
    return null;
  },
  sv_host(settings, value) {
    if (value.trim() === '') return 'sv_host must not be empty';
    settings.host = value.trim();
    return null;
  },
  sv_tickrate(settings, value) {
    const n = parseInteger(value, 1, 240);
    if (n === null) return `sv_tickrate must be an integer between 1 and 240 (got "${value}")`;
    settings.tickRate = n;
    return null;
  },
  sv_streamdistance(settings, value) {
    const n = value.trim() === '' ? NaN : Number(value);
    if (!Number.isFinite(n)) return `sv_streamdistance must be a number (got "${value}")`;
    settings.streamDistance = n;
    return null;
  },
};

/**
 * Interpret parsed commands.
 *
 * `settings` only contains keys that were actually configured, so it can be
 * spread straight into the `SixMServer` constructor without clobbering defaults.
 *
 * @param {Array<{name:string, args:string[], line:number}>} commands
 * @returns {{
 *   convars: Map<string,string>, replicated: string[], resources: string[],
 *   settings: {name?:string, maxPlayers?:number, port?:number, host?:string, tickRate?:number, streamDistance?:number, tags?:string[]},
 *   infoVars: Record<string,string>, unknown: Array<{name:string,args:string[],line:number}>,
 *   errors: Array<{line:number, message:string}>
 * }}
 */
export function applyConfig(commands) {
  const convars = new Map();
  const replicated = new Set();
  const resources = [];
  const settings = {};
  const infoVars = Object.create(null);
  const unknown = [];
  const errors = [];

  const setConvar = (cmd, key, value, kind) => {
    convars.set(key, value);
    if (kind === 'setr') replicated.add(key);
    if (kind === 'sets') {
      infoVars[key] = value;
      if (key.toLowerCase() === 'tags') {
        settings.tags = value.split(',').map((t) => t.trim()).filter(Boolean);
      }
    }
    const apply = SETTING_CONVARS[key.toLowerCase()];
    if (apply) {
      const message = apply(settings, value);
      if (message) errors.push({ line: cmd.line, message });
    }
  };

  for (const cmd of Array.isArray(commands) ? commands : []) {
    const { name, args, line } = cmd;
    switch (name) {
      case 'set':
      case 'setr':
      case 'sets': {
        if (args.length < 2) {
          errors.push({ line, message: `${name} expects a name and a value` });
          break;
        }
        setConvar(cmd, args[0], args.slice(1).join(' '), name);
        break;
      }
      case 'ensure':
      case 'start': {
        if (args.length !== 1 || args[0] === '') {
          errors.push({ line, message: `${name} expects exactly one resource name` });
          break;
        }
        if (!resources.includes(args[0])) resources.push(args[0]);
        break;
      }
      case 'stop': {
        if (args.length !== 1 || args[0] === '') {
          errors.push({ line, message: 'stop expects exactly one resource name' });
          break;
        }
        const at = resources.indexOf(args[0]);
        if (at !== -1) resources.splice(at, 1);
        break;
      }
      case 'endpoint_add_tcp': {
        const ep = args.length === 1 ? parseEndpoint(args[0]) : null;
        if (!ep) {
          errors.push({ line, message: 'endpoint_add_tcp expects "host:port" with a port between 0 and 65535' });
          break;
        }
        settings.port = ep.port;
        if (ep.host !== undefined) settings.host = ep.host;
        break;
      }
      case 'endpoint_add_udp':
        // Accepted for familiarity; SixM only listens on TCP (WebSocket).
        break;
      default: {
        if (SETTING_CONVARS[name]) {
          // Bare form: `sv_hostname "My Server"`.
          if (args.length < 1) errors.push({ line, message: `${name} expects a value` });
          else setConvar(cmd, name, args.join(' '), 'set');
        } else {
          unknown.push(cmd);
        }
      }
    }
  }

  return { convars, replicated: [...replicated], resources, settings, infoVars: { ...infoVars }, unknown, errors };
}

/**
 * Read, parse and interpret a config file. Rejects only when the file cannot
 * be read; syntax and validation problems land in `errors`.
 * @param {string} path
 */
export async function loadConfig(path) {
  const text = await readFile(path, 'utf8');
  const parsed = parseConfig(text);
  const applied = applyConfig(parsed.commands);
  const errors = [...parsed.errors, ...applied.errors].sort((a, b) => a.line - b.line);
  return { path, commands: parsed.commands, ...applied, errors };
}
