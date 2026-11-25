// sixm-server: reads server.cfg, starts the server and its resources, and
// gives the operator a console.

import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { parseArgs } from 'node:util';
import { SixMServer } from './core.js';
import { parseConfig, applyConfig } from './config.js';
import { createLogger } from './logger.js';
import { ResourceManager } from './resources.js';
import { VERSION } from '../shared/version.js';

const HELP = `SixM server ${VERSION}

Usage: sixm-server [options]

  -c, --config <file>      configuration file        (default: server.cfg)
  -r, --resources <dir>    resources folder          (default: resources)
  -p, --port <number>      override the port from the configuration
      --no-console         do not read commands from standard input
      --log-level <level>  debug | info | warn | error (default: info)
  -v, --version            print the version
  -h, --help               print this help
`;

/**
 * Start a server from a configuration file.
 * @returns {Promise<{server: SixMServer, resources: ResourceManager, stop: (reason?: string) => Promise<void>}>}
 */
export async function startServer({ configPath, resourcesDir, port, logger, interactive = false } = {}) {
  const log = logger ?? createLogger();

  let settings = {};
  let convars = new Map();
  let ensure = [];
  let infoVars = {};
  if (configPath && fs.existsSync(configPath)) {
    const parsed = parseConfig(fs.readFileSync(configPath, 'utf8'));
    const applied = applyConfig(parsed.commands);
    for (const e of [...(parsed.errors ?? []), ...(applied.errors ?? [])]) {
      log.warn(`${path.basename(configPath)}: ${typeof e === 'string' ? e : `line ${e.line}: ${e.message}`}`);
    }
    for (const u of applied.unknown ?? []) {
      log.warn(`${path.basename(configPath)}: line ${u.line}: unknown command "${u.name}"`);
    }
    settings = applied.settings ?? {};
    ensure = applied.resources ?? [];
    infoVars = applied.infoVars ?? {};
    for (const [k, v] of applied.convars ?? []) convars.set(k, typeof v === 'object' && v !== null ? String(v.value) : String(v));
  } else if (configPath) {
    log.warn(`no configuration at ${configPath}; using defaults and starting every resource`);
    ensure = null;
  }

  const defined = (o) => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined && v !== null));
  const server = new SixMServer({
    ...defined({
      port: port ?? settings.port,
      host: settings.host,
      name: settings.name,
      maxPlayers: settings.maxPlayers,
      tickRate: settings.tickRate,
      streamDistance: settings.streamDistance,
      tags: settings.tags,
    }),
    vars: infoVars,
    build: VERSION,
    logger: log,
  });

  const resources = new ResourceManager({ server, resourcesDir, logger: log, convars });
  const found = resources.scan();
  log.info(`found ${found.length} resource(s) in ${resourcesDir}`);

  let stopping = null;
  const stop = (reason = 'Server shutting down.') => {
    stopping ??= (async () => {
      resources.dispose();
      await server.stop(reason);
    })();
    return stopping;
  };

  addConsoleCommands({ server, resources, log, stop });

  const boundPort = await server.start();
  for (const name of ensure ?? found) resources.start(name);
  log.info(`${server.name ?? 'SixM Server'} is listening on port ${boundPort} (http://127.0.0.1:${boundPort}/)`);

  if (interactive) {
    const rl = readline.createInterface({ input: process.stdin, terminal: false });
    rl.on('line', (line) => {
      const raw = line.trim();
      if (!raw) return;
      try {
        if (!resources.executeCommand(raw, 0)) log.warn(`unknown command: ${raw.split(/\s+/)[0]} (try "help")`);
      } catch (err) {
        log.error(err.message);
      }
    });
    log.info('type "help" for console commands');
  }

  return { server, resources, stop };
}

function addConsoleCommands({ server, resources, log, stop }) {
  const add = (name, help, run) => resources.addBuiltin(name, help, run);

  add('help', 'list console commands', () => {
    const rows = [...resources.builtins].map(([name, c]) => `  ${name.padEnd(10)} ${c.help}`);
    log.info('console commands:\n' + rows.join('\n') + '\n  (resources add their own, e.g. "say" and "players")');
  });
  add('status', 'show players and uptime', () => {
    log.info(`${server.players.size} player(s), tick ${server.tick}, ${resources.running.size} resource(s) running`);
    for (const p of server.players.values()) log.info(`  #${p.id} ${p.name}  ${p.ping}ms  ${p.endpoint}`);
  });
  add('resources', 'list resources', () => {
    for (const name of [...resources.available.keys()].sort()) {
      log.info(`  ${resources.running.has(name) ? 'started' : 'stopped'}  ${name}`);
    }
  });
  add('refresh', 'look for new resources on disk', () => log.info(`found ${resources.scan().length} resource(s)`));
  add('start', 'start a resource', ([name]) => (name ? resources.start(name) : log.warn('usage: start <resource>')));
  add('ensure', 'start or restart a resource', ([name]) => (name ? resources.restart(name) : log.warn('usage: ensure <resource>')));
  add('restart', 'restart a resource', ([name]) => (name ? resources.restart(name) : log.warn('usage: restart <resource>')));
  add('stop', 'stop a resource', ([name]) => {
    if (!name) return log.warn('usage: stop <resource>');
    if (!resources.stop(name)) log.warn(`"${name}" is not running`);
  });
  add('kick', 'kick a player: kick <id> [reason]', ([id, ...reason]) => {
    const p = server.getPlayer(Number(id));
    if (!p) return log.warn('usage: kick <id> [reason]  (see "status" for ids)');
    p.kick(reason.join(' ') || 'Kicked by the server.');
  });
  add('quit', 'stop the server', () => {
    stop().then(() => process.exit(0));
  });
}

/** Folder the bundled defaults live in: next to the executable, or the working directory. */
function baseDir() {
  const exeDir = path.dirname(process.execPath);
  if (/sixm-server/i.test(path.basename(process.execPath)) && fs.existsSync(path.join(exeDir, 'resources'))) return exeDir;
  return process.cwd();
}

export async function main(argv = process.argv.slice(2)) {
  let args;
  try {
    args = parseArgs({
      args: argv,
      options: {
        config: { type: 'string', short: 'c' },
        resources: { type: 'string', short: 'r' },
        port: { type: 'string', short: 'p' },
        'no-console': { type: 'boolean' },
        'log-level': { type: 'string' },
        version: { type: 'boolean', short: 'v' },
        help: { type: 'boolean', short: 'h' },
      },
    }).values;
  } catch (err) {
    process.stderr.write(`${err.message}\n\n${HELP}`);
    process.exit(2);
  }
  if (args.help) return void process.stdout.write(HELP);
  if (args.version) return void process.stdout.write(`${VERSION}\n`);

  const port = args.port === undefined ? undefined : Number(args.port);
  if (port !== undefined && !(Number.isInteger(port) && port >= 0 && port <= 65535)) {
    process.stderr.write(`--port must be a number from 0 to 65535\n`);
    process.exit(2);
  }

  const base = baseDir();
  const logger = createLogger({ level: args['log-level'] });
  const { stop } = await startServer({
    configPath: path.resolve(base, args.config ?? 'server.cfg'),
    resourcesDir: path.resolve(base, args.resources ?? 'resources'),
    port,
    logger,
    interactive: !args['no-console'],
  });

  const shutdown = () => stop().then(() => process.exit(0));
  process.on('SIGINT', shutdown);
  process.on('SIGTERM', shutdown);
}
