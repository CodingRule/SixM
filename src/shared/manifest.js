// Resource manifests. A manifest is a small Lua file of directives:
//
//   name 'hello'
//   server_script 'server.lua'
//   client_scripts { 'a.lua', 'b.lua' }
//
// It is evaluated in a throwaway sandboxed Lua state that has only these
// directives, so a manifest cannot do anything but describe its resource.

import { LuaRuntime } from './lua/runtime.js';

export const MANIFEST_FILE = 'manifest.lua';

const INFO_KEYS = ['name', 'description', 'author', 'version', 'url', 'sixm_version'];
const SCRIPT_KEYS = {
  server_script: 'serverScripts',
  server_scripts: 'serverScripts',
  client_script: 'clientScripts',
  client_scripts: 'clientScripts',
  shared_script: 'sharedScripts',
  shared_scripts: 'sharedScripts',
};

function toList(value, directive) {
  const list = Array.isArray(value) ? value : value && typeof value === 'object' ? Object.values(value) : [value];
  for (const item of list) {
    if (typeof item !== 'string' || !item) throw new Error(`${directive} expects a file name or a list of file names`);
    if (/^([a-zA-Z]:|[\\/])|(^|[\\/])\.\.([\\/]|$)/.test(item)) {
      throw new Error(`${directive}: "${item}" must be a path inside the resource`);
    }
  }
  return list;
}

/**
 * @param {string} source manifest.lua text
 * @param {string} [chunkName]
 * @returns {{info:Record<string,string>, serverScripts:string[], clientScripts:string[], sharedScripts:string[], dependencies:string[]}}
 */
export function parseManifest(source, chunkName = '@manifest.lua') {
  const manifest = { info: {}, serverScripts: [], clientScripts: [], sharedScripts: [], dependencies: [] };
  const errors = [];
  const rt = new LuaRuntime({
    resourceName: 'manifest',
    side: 'server',
    print: () => {},
    onError: (err) => errors.push(err?.message ?? String(err)),
  });
  try {
    const guard = (name, fn) =>
      rt.registerGlobal(name, (value) => {
        try {
          fn(value);
        } catch (err) {
          errors.push(err.message);
        }
      });
    for (const key of INFO_KEYS) guard(key, (v) => { manifest.info[key] = String(v); });
    for (const [directive, field] of Object.entries(SCRIPT_KEYS)) {
      guard(directive, (v) => manifest[field].push(...toList(v, directive)));
    }
    guard('dependency', (v) => manifest.dependencies.push(...toList(v, 'dependency')));
    guard('dependencies', (v) => manifest.dependencies.push(...toList(v, 'dependencies')));
    rt.loadScript(source, chunkName);
  } finally {
    rt.destroy();
  }
  if (errors.length) throw new Error(`invalid manifest: ${errors[0]}`);
  return manifest;
}
