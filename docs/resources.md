# Resources and manifests

A **resource** is a folder of Lua 5.4 scripts that the server loads as a unit. Gameplay, chat, admin tools: everything
beyond the core is a resource.

## Layout

Resources live in `resources/`, one folder each. A resource needs a `manifest.lua`.

```
resources/
  hello/
    manifest.lua
    server.lua
    client.lua
```

The manifest file is always named `manifest.lua`. The resource's name is the name of its folder; that is the name
`ensure` and `exports.<name>` use.

Folders whose name is in square brackets, such as `resources/[core]/`, are categories: the server looks inside them
for resources (up to four levels deep) and the bracketed name is not part of any resource name. If two folders hold
a resource with the same name, the first one found is kept and a warning is logged.

## manifest.lua

The manifest is a Lua file made of simple directive calls. It is run in an empty Lua state that has only these
directives (no standard library), so a manifest can describe its resource and nothing else.

```lua
name 'hello'
description 'Greets players'
author 'you'
version '1.0.0'

server_script 'server.lua'
client_script 'client.lua'
```

### Metadata

| Directive | Example | Meaning |
|---|---|---|
| `name` | `name 'hello'` | Display name. The folder name is what identifies the resource |
| `description` | `description 'Greets players'` | One-line description |
| `author` | `author 'you'` | Author |
| `version` | `version '1.0.0'` | Version string |

`url` and `sixm_version` are accepted as metadata strings too. Metadata is informational: the server reads it and
does not act on it.

### Scripts

| Directive | Example | Runs on |
|---|---|---|
| `server_script` | `server_script 'server.lua'` | Server |
| `server_scripts` | `server_scripts { 'db.lua', 'server.lua' }` | Server |
| `client_script` | `client_script 'client.lua'` | Client |
| `client_scripts` | `client_scripts { 'hud.lua', 'client.lua' }` | Client |
| `shared_script` | `shared_script 'config.lua'` | Server and client |
| `shared_scripts` | `shared_scripts { 'config.lua', 'util.lua' }` | Server and client |

Paths are relative to the resource folder and must stay inside it: absolute paths and paths containing `..` are
refused. On each side the shared scripts run first, then that side's scripts, each group in the order listed. All
scripts of one resource on one side share the same globals. If a listed file is missing, the resource does not
start.

`dependency 'chat'` and `dependencies { ... }` are accepted so a manifest can record what it needs, but the server
does not check or order anything by them yet. Use the `ensure` order in `server.cfg`.

A resource with several files:

```lua
name 'bank'
description 'Accounts and transfers'
author 'you'
version '0.3.0'

shared_script 'config.lua'

server_scripts {
  'accounts.lua',
  'server.lua',
}

client_script 'client.lua'
```

In a shared script, use `IsDuplicityVersion()` to tell which side you are on:

```lua
-- config.lua
Config = { startingBalance = 500 }

if IsDuplicityVersion() then
  print('config loaded on the server')
end
```

## Starting a resource

Add an `ensure` line to [`server.cfg`](configuration.md):

```cfg
ensure hello
```

If one resource uses another's [exports](scripting.md#exports), `ensure` the provider first.

## Lifecycle

On the server:

- `onResourceStart (resourceName)` fires when a resource has started.
- `onResourceStop (resourceName)` fires when a resource is about to stop.

On the client, `onClientResourceStart (resourceName)` fires when a resource's client scripts have been loaded.

These fire in every running resource, so compare against `GetCurrentResourceName()`:

```lua
AddEventHandler('onResourceStart', function(resourceName)
  if resourceName ~= GetCurrentResourceName() then return end
  print('ready')
end)
```

## Server scripts and client scripts

- **Server scripts** run inside the dedicated server and can use the server-only API (`GetPlayers`,
  `TriggerClientEvent`, `DropPlayer`, ...).
- **Client scripts** run inside each connected client and can use the client-only API (`GetLocalPlayerId`,
  `TriggerServerEvent`, `SetLocalCoords`, ...). Today the only client is the headless client, which has no graphics.
- The two sides talk through [net events](scripting.md#events).

## What a resource cannot do

- Call game natives. SixM is not attached to a game yet.
- Draw UI. There is no UI layer.
- Touch the filesystem, run processes or load native code. See the [sandbox](scripting.md#sandbox).
- Share Lua values with another resource directly. Each resource has its own Lua state; events and exports copy
  their arguments. See [Values that cross a boundary](scripting.md#values-that-cross-a-boundary).
- Be hot-reloaded automatically. The console command `restart <resource>` rereads a resource from disk and restarts
  its server scripts, but clients that are already connected keep the client scripts they received when they
  joined. Automatic hot-reload is on the roadmap.

## Conventions

These are suggestions, not rules enforced by the server:

- Prefix event names with the resource name: `bank:deposit`, not `deposit`.
- Validate every argument of a net event on the server.
- Keep one resource focused on one job, and connect resources with exports and events.
