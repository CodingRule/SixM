# Resources and manifests

A **resource** is a folder of Lua scripts that the server loads as a unit. Gameplay, chat, admin tools: everything
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

The manifest file is always named `manifest.lua`.

## manifest.lua

The manifest is a Lua file made of simple directive calls.

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
| `name` | `name 'hello'` | Resource name |
| `description` | `description 'Greets players'` | One-line description |
| `author` | `author 'you'` | Author |
| `version` | `version '1.0.0'` | Version string |

### Scripts

| Directive | Example | Runs on |
|---|---|---|
| `server_script` | `server_script 'server.lua'` | Server |
| `server_scripts` | `server_scripts { 'db.lua', 'server.lua' }` | Server |
| `client_script` | `client_script 'client.lua'` | Client |
| `client_scripts` | `client_scripts { 'hud.lua', 'client.lua' }` | Client |
| `shared_script` | `shared_script 'config.lua'` | Server and client |

Paths are relative to the resource folder.

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

- `onResourceStart (resourceName)` fires when a resource starts.
- `onResourceStop (resourceName)` fires when a resource stops.

Both fire for every resource, so compare against `GetCurrentResourceName()`:

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
- Be hot-reloaded. Restart the server to pick up changes (hot-reload is on the roadmap).

## Conventions

These are suggestions, not rules enforced by the server:

- Prefix event names with the resource name: `bank:deposit`, not `deposit`.
- Validate every argument of a net event on the server.
- Keep one resource focused on one job, and connect resources with exports and events.
