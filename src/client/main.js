// sixm-client: a headless client. It connects, runs the server's client-side
// Lua, prints what happens, and takes commands from standard input.

import readline from 'node:readline';
import { parseArgs } from 'node:util';
import { SixMClient } from './client.js';
import { DEFAULT_PORT } from '../shared/protocol.js';
import { VERSION } from '../shared/version.js';

const HELP = `SixM headless client ${VERSION}

Usage: sixm-client [options]

  -H, --host <address>   server address            (default: 127.0.0.1)
  -p, --port <number>    server port               (default: ${DEFAULT_PORT})
  -n, --name <name>      player name               (default: Player)
      --pos <x,y,z>      starting position         (default: 0,0,0)
      --wander           walk around at random
  -v, --version          print the version
  -h, --help             print this help

While connected, type a command and press Enter. Commands the server's
resources define (such as "say hello" or "players") work here, plus:

  /pos x y z     move to a position
  /list          list the players you know about
  /quit          disconnect
`;

export async function main(argv = process.argv.slice(2)) {
  let args;
  try {
    args = parseArgs({
      args: argv,
      options: {
        host: { type: 'string', short: 'H' },
        port: { type: 'string', short: 'p' },
        name: { type: 'string', short: 'n' },
        pos: { type: 'string' },
        wander: { type: 'boolean' },
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

  const client = new SixMClient({
    host: args.host ?? '127.0.0.1',
    port: args.port ? Number(args.port) : DEFAULT_PORT,
    name: args.name ?? 'Player',
    build: `sixm-client/${VERSION}`,
  });
  const say = (line) => console.log(line);

  if (args.pos) {
    const [x, y, z] = args.pos.split(',').map(Number);
    if ([x, y, z].some((n) => !Number.isFinite(n))) {
      process.stderr.write('--pos expects three numbers, e.g. --pos 10,20,0\n');
      process.exit(2);
    }
    client.setCoords(x, y, z);
  }

  client.on('playerJoined', (id, name) => say(`* ${name} (#${id}) joined`));
  client.on('playerLeft', (id, name, reason) => say(`* ${name} (#${id}) left${reason ? ': ' + reason : ''}`));
  client.on('disconnected', (reason) => {
    say(`disconnected: ${reason}`);
    process.exit(0);
  });
  client.on('error', (err) => say(`network error: ${err.message}`));

  try {
    await client.connect();
  } catch (err) {
    process.stderr.write(`could not join ${client.host}:${client.port}: ${err.message}\n`);
    process.exit(1);
  }
  say(`joined "${client.serverName}" as ${client.name} (#${client.id}), ${client.players.size} other player(s) online`);

  if (args.wander) startWandering(client);

  const rl = readline.createInterface({ input: process.stdin, terminal: false });
  rl.on('line', (line) => {
    const raw = line.trim();
    if (!raw) return;
    const [cmd, ...rest] = raw.replace(/^\//, '').split(/\s+/);
    if (cmd === 'quit' || cmd === 'exit') return client.disconnect('Quit.');
    if (cmd === 'pos') {
      const [x, y, z] = rest.map(Number);
      if ([x, y, z].some((n) => !Number.isFinite(n))) return say('usage: /pos x y z');
      client.setCoords(x, y, z);
      return say(`moved to ${x}, ${y}, ${z}`);
    }
    if (cmd === 'list') {
      say(`#${client.id} ${client.name} (you)`);
      for (const p of client.players.values()) {
        const s = p.state;
        say(`#${p.id} ${p.name} ${s ? `at ${s.x.toFixed(1)}, ${s.y.toFixed(1)}, ${s.z.toFixed(1)}` : '(out of range)'}`);
      }
      return;
    }
    client.executeCommand(raw);
  });
  rl.on('close', () => {
    // Standard input ended (for example when run without a terminal): stay connected.
  });
  process.on('SIGINT', () => client.disconnect('Quit.'));
}

/** Walk in a slowly turning line, as a stand-in for a player moving about. */
function startWandering(client) {
  let heading = Math.random() * 360;
  const speed = 4; // metres per second
  let last = Date.now();
  client.on('tick', (now) => {
    const dt = (now - last) / 1000;
    last = now;
    heading = (heading + (Math.random() - 0.5) * 40 * dt * 10 + 360) % 360;
    const rad = (heading * Math.PI) / 180;
    const vx = Math.cos(rad) * speed;
    const vy = Math.sin(rad) * speed;
    client.setState({
      x: client.state.x + vx * dt,
      y: client.state.y + vy * dt,
      heading,
      vx,
      vy,
    });
  });
}
