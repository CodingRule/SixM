// Tiny leveled console logger for the SixM server.
//
// Colour is used only when the target stream is a TTY and NO_COLOR is unset
// (https://no-color.org: any non-empty value disables colour).

export const LEVELS = Object.freeze({ debug: 10, info: 20, warn: 30, error: 40, silent: 100 });

const COLOURS = { debug: '\x1b[90m', info: '\x1b[36m', warn: '\x1b[33m', error: '\x1b[31m' };
const RESET = '\x1b[0m';
const DIM = '\x1b[2m';

function colourEnabled(stream) {
  const noColor = process.env.NO_COLOR;
  if (noColor !== undefined && noColor !== '') return false;
  return Boolean(stream && stream.isTTY);
}

function formatArg(a) {
  if (typeof a === 'string') return a;
  if (a instanceof Error) return a.stack || `${a.name}: ${a.message}`;
  try {
    return JSON.stringify(a);
  } catch {
    return String(a);
  }
}

function timestamp() {
  const d = new Date();
  const p = (n) => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/**
 * @param {object} [opts]
 * @param {'debug'|'info'|'warn'|'error'|'silent'} [opts.level] defaults to $SIXM_LOG_LEVEL or 'info'
 * @param {string} [opts.name] prefix shown in brackets
 * @param {{write:Function,isTTY?:boolean}} [opts.stdout] where debug/info go
 * @param {{write:Function,isTTY?:boolean}} [opts.stderr] where warn/error go
 */
export function createLogger({ level, name = '', stdout = process.stdout, stderr = process.stderr } = {}) {
  let threshold = LEVELS[level ?? process.env.SIXM_LOG_LEVEL] ?? LEVELS.info;

  function write(lvl, args) {
    if (LEVELS[lvl] < threshold) return;
    const stream = LEVELS[lvl] >= LEVELS.warn ? stderr : stdout;
    const text = args.map(formatArg).join(' ');
    const tag = lvl.toUpperCase().padEnd(5);
    const scope = name ? ` [${name}]` : '';
    let line;
    if (colourEnabled(stream)) {
      line = `${DIM}${timestamp()}${RESET} ${COLOURS[lvl]}${tag}${RESET}${DIM}${scope}${RESET} ${text}\n`;
    } else {
      line = `${timestamp()} ${tag}${scope} ${text}\n`;
    }
    try {
      stream.write(line);
    } catch {
      // A broken stdout must never take the server down.
    }
  }

  return {
    debug: (...a) => write('debug', a),
    info: (...a) => write('info', a),
    warn: (...a) => write('warn', a),
    error: (...a) => write('error', a),
    get level() {
      return Object.keys(LEVELS).find((k) => LEVELS[k] === threshold) ?? 'info';
    },
    set level(l) {
      if (LEVELS[l] !== undefined) threshold = LEVELS[l];
    },
    /** A logger with the same streams and level but a different prefix. */
    child(childName) {
      return createLogger({ level: this.level, name: name ? `${name}:${childName}` : childName, stdout, stderr });
    },
  };
}

/** A logger that discards everything (handy in tests). */
export const silentLogger = Object.freeze({
  debug() {},
  info() {},
  warn() {},
  error() {},
  level: 'silent',
  child() {
    return silentLogger;
  },
});
