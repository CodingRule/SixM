// Builds standalone executables into dist/:
//
//   dist/sixm-server(.exe)   dist/sixm-client(.exe)
//   dist/server.cfg          dist/resources/
//
// Each executable is this machine's Node.js binary with the bundled program
// embedded (a Node "single executable application"), so the result runs on the
// platform it was built on without Node installed.

import { build } from 'esbuild';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const dist = path.join(root, 'dist');
const work = path.join(root, 'build');
const exe = process.platform === 'win32' ? '.exe' : '';
const FUSE = 'NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2';

const targets = [
  { name: 'sixm-server', main: 'src/server/main.js' },
  { name: 'sixm-client', main: 'src/client/main.js' },
];

fs.rmSync(dist, { recursive: true, force: true });
fs.rmSync(work, { recursive: true, force: true });
fs.mkdirSync(dist, { recursive: true });
fs.mkdirSync(work, { recursive: true });

for (const t of targets) {
  // The entry hands the Lua standard library to the runtime as text, because
  // inside an executable there is no bootstrap.lua on disk to read.
  const entry = path.join(work, `${t.name}.entry.js`);
  const rel = (p) => JSON.stringify(path.relative(work, path.join(root, p)).replaceAll('\\', '/'));
  fs.writeFileSync(entry, [
    `import bootstrap from ${rel('src/shared/lua/bootstrap.lua')};`,
    `import { setBootstrapSource } from ${rel('src/shared/lua/runtime.js')};`,
    `import { main } from ${rel(t.main)};`,
    `setBootstrapSource(bootstrap);`,
    `main().catch((err) => { console.error(err?.stack ?? err); process.exit(1); });`,
    '',
  ].join('\n'));

  const bundle = path.join(work, `${t.name}.cjs`);
  await build({
    entryPoints: [entry],
    outfile: bundle,
    bundle: true,
    platform: 'node',
    format: 'cjs',
    target: 'node20',
    loader: { '.lua': 'text' },
    // Optional native speed-ups of `ws`; it works without them.
    external: ['bufferutil', 'utf-8-validate'],
    define: { 'import.meta.url': '__sixm_import_meta_url' },
    banner: { js: `const __sixm_import_meta_url = require('node:url').pathToFileURL(__filename).href;` },
    logLevel: 'warning',
  });

  const blob = path.join(work, `${t.name}.blob`);
  const config = path.join(work, `${t.name}.sea.json`);
  fs.writeFileSync(config, JSON.stringify({
    main: bundle,
    output: blob,
    disableExperimentalSEAWarning: true,
    useCodeCache: false,
  }));
  execFileSync(process.execPath, ['--experimental-sea-config', config], { stdio: 'inherit' });

  const out = path.join(dist, t.name + exe);
  fs.copyFileSync(process.execPath, out);
  fs.chmodSync(out, 0o755);
  const postject = path.join(root, 'node_modules', 'postject', 'dist', 'cli.js');
  const args = [postject, out, 'NODE_SEA_BLOB', blob, '--sentinel-fuse', FUSE];
  if (process.platform === 'darwin') args.push('--macho-segment-name', 'NODE_SEA');
  execFileSync(process.execPath, args, { stdio: 'inherit' });

  console.log(`built ${path.relative(root, out)} (${(fs.statSync(out).size / 1048576).toFixed(1)} MB)`);
}

fs.copyFileSync(path.join(root, 'server.cfg'), path.join(dist, 'server.cfg'));
fs.cpSync(path.join(root, 'resources'), path.join(dist, 'resources'), { recursive: true });
fs.rmSync(work, { recursive: true, force: true });
console.log('copied server.cfg and resources/ into dist/');
