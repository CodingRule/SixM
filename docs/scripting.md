# Lua scripting API

SixM resources are written in Lua 5.3. Server scripts run inside the dedicated server; client scripts run inside the
client (today, the headless client). Each resource gets its own Lua runtime.

> [!NOTE]
> There are no game-world natives. SixM is not attached to a game yet, so there is nothing to spawn, drive or shoot.
> The API below covers threads, events, players, commands, convars and exports.

**Availability column:** `S` = server scripts, `C` = client scripts, `S C` = both.

- [Threads and timers](#threads-and-timers)
- [Events](#events)
- [Built-in events](#built-in-events)
- [Players (server)](#players-server)
- [Local player and others (client)](#local-player-and-others-client)
- [Commands](#commands)
- [Convars](#convars)
- [Exports](#exports)
- [Utilities](#utilities)
- [Sandbox](#sandbox)

## Threads and timers

| Function | Avail. | Description |
|---|:-:|---|
| `Citizen.CreateThread(fn)` / `CreateThread(fn)` | S C | Run `fn` as a thread that may call `Wait` |
| `Citizen.Wait(ms)` / `Wait(ms)` | S C | Suspend the current thread for `ms` milliseconds |
| `SetTimeout(ms, fn)` | S C | Call `fn` once after `ms` milliseconds |

```lua
Citizen.CreateThread(function()
  while true do
    Citizen.Wait(5000)
    print('five seconds passed')
  end
end)

SetTimeout(1000, function()
  print('one second after start')
end)
```

A loop inside a thread must call `Wait`, otherwise it never yields.

## Events

| Function | Avail. | Description |
|---|:-:|---|
| `AddEventHandler(name, fn)` | S C | Register a handler for a local event. Returns a handle |
| `RemoveEventHandler(handle)` | S C | Remove a handler registered with `AddEventHandler` |
| `RegisterNetEvent(name[, fn])` | S C | Allow `name` to be received over the network. With `fn`, also registers it as the handler |
| `TriggerEvent(name, ...)` | S C | Trigger a local event on the same side |
| `TriggerClientEvent(name, target, ...)` | S | Send an event to a client. `target` is a player id, or `-1` for everyone |
| `TriggerServerEvent(name, ...)` | C | Send an event to the server |
| `source` (global) | S | Inside a server handler for a net event: the id of the player who sent it |

An event must be registered with `RegisterNetEvent` on the receiving side before it can arrive over the network.

```lua
-- server.lua
RegisterNetEvent('chat:say', function(text)
  local id = source
  TriggerClientEvent('chat:message', -1, GetPlayerName(id), text)
end)
```

```lua
-- client.lua
RegisterNetEvent('chat:message', function(author, text)
  print(author .. ': ' .. text)
end)

TriggerServerEvent('chat:say', 'hello everyone')
```

Removing a handler:

```lua
local handle = AddEventHandler('my:event', function(value)
  print('got', value)
end)

TriggerEvent('my:event', 42)
RemoveEventHandler(handle)
```

`source` is a global that is only meaningful while a handler runs. Copy it into a local (`local id = source`) before
calling `Wait`.

Never trust arguments that arrive from a client. Check types and ranges in every server-side net event handler.

## Built-in events

| Event | Arguments | Avail. | When |
|---|---|:-:|---|
| `playerConnecting` | `name, setKickReason, deferrals` | S | A player is connecting, before they get a slot |
| `playerJoined` | `id` | S C | A player has joined |
| `playerDropped` | `reason` | S | A player left. `source` is their id |
| `onResourceStart` | `resourceName` | S C | A resource started |
| `onResourceStop` | `resourceName` | S C | A resource is stopping |

```lua
AddEventHandler('playerJoined', function(id)
  print('player joined: ' .. tostring(id))
end)

AddEventHandler('playerDropped', function(reason)
  print(('%s left (%s)'):format(GetPlayerName(source), reason))
end)

AddEventHandler('onResourceStart', function(resourceName)
  if resourceName == GetCurrentResourceName() then
    print('this resource just started')
  end
end)
```

## Players (server)

| Function | Avail. | Returns |
|---|:-:|---|
| `GetPlayers()` | S | List of connected player ids |
| `GetNumPlayers()` | S | Number of connected players |
| `GetPlayerName(id)` | S | The player's name |
| `GetPlayerPing(id)` | S | The player's ping |
| `GetPlayerIdentifiers(id)` | S | List of identifiers for the player |
| `GetPlayerEndpoint(id)` | S | The player's network endpoint |
| `GetPlayerCoords(id)` | S | The player's position as a `vector3` |
| `GetPlayerHeading(id)` | S | The player's heading |
| `DropPlayer(id, reason)` | S | Disconnects the player with `reason` |

```lua
for _, id in ipairs(GetPlayers()) do
  local pos = GetPlayerCoords(id)
  print(GetPlayerName(id), GetPlayerPing(id), pos.x, pos.y, pos.z)
end

DropPlayer(3, 'Kicked by an admin')
```

Positions and headings are reported by clients. There is no server-authoritative validation yet, so do not treat
them as proof of anything.

## Local player and others (client)

| Function | Avail. | Returns |
|---|:-:|---|
| `GetLocalPlayerId()` | C | This client's player id |
| `GetLocalCoords()` | C | This client's position |
| `SetLocalCoords(x, y, z)` | C | Sets this client's position |
| `SetLocalHeading(h)` | C | Sets this client's heading |
| `GetActivePlayers()` | C | List of player ids this client currently knows about |
| `GetPlayerName(id)` | C | Name of a player |
| `GetPlayerCoords(id)` | C | Position of a player, or `nil` while that player is out of streaming range |
| `IsPlayerStreamed(id)` | C | Whether that player's position is currently being sent to this client |

```lua
SetLocalCoords(100.0, 200.0, 30.0)
SetLocalHeading(90.0)

for _, id in ipairs(GetActivePlayers()) do
  print(id, GetPlayerName(id))
end
```

The server only streams players within range of a client, so position data on the client is limited to players that
are currently streamed to it.

## Commands

| Function | Avail. | Description |
|---|:-:|---|
| `RegisterCommand(name, fn, restricted)` | S C | Register a command. `fn` receives `(source, args, raw)` |
| `ExecuteCommand(str)` | S C | Run a command line as if it had been typed |

`args` is a list of the words after the command name; `raw` is the full command line. Command names are case-sensitive.

A server command registered with `restricted` set to `true` can only be run from the server console. With `false`,
connected players can run it too, and `source` is their player id (`0` for the console). A command typed in a client
runs there if a client script registered it, and is sent to the server otherwise.

```lua
RegisterCommand('announce', function(source, args, raw)
  TriggerClientEvent('chat:message', -1, 'server', table.concat(args, ' '))
end, false)

ExecuteCommand('announce Restart in five minutes')
```

There is no ACL/permission system yet (it is on the roadmap).

## Convars

| Function | Avail. | Description |
|---|:-:|---|
| `GetConvar(name, default)` | S C | Read a convar as a string, or `default` when unset |
| `GetConvarInt(name, default)` | S C | Read a convar as an integer, or `default` when unset |
| `SetConvar(name, value)` | S | Set a convar |

```lua
local motd = GetConvar('motd', 'Welcome!')
local limit = GetConvarInt('sv_maxclients', 32)
SetConvar('motd', 'Server restarts at midnight')
```

Convars are usually set in [`server.cfg`](configuration.md) with `set` and `sets`.

## Exports

| Function | Avail. | Description |
|---|:-:|---|
| `exports('name', fn)` | S C | Expose `fn` to other resources under `name` |
| `exports.<resource>:<name>(...)` | S C | Call a function exported by another resource |

```lua
-- resource "bank", server.lua
local balances = {}

exports('getBalance', function(id)
  return balances[id] or 0
end)
```

```lua
-- another resource, server.lua
local balance = exports.bank:getBalance(source)
```

Exports connect resources on the same side: server scripts call server exports, client scripts call client exports.
The exporting resource has to be started first (`ensure` it earlier in `server.cfg`).

## Utilities

| Function | Avail. | Description |
|---|:-:|---|
| `GetCurrentResourceName()` | S C | Name of the resource the script belongs to |
| `IsDuplicityVersion()` | S C | `true` on the server, `false` on the client. Useful in shared scripts |
| `json.encode(value)` | S C | Serialise a Lua value to a JSON string |
| `json.decode(str)` | S C | Parse a JSON string into a Lua value |
| `vector3(x, y, z)` | S C | Create a 3D vector |
| `print(...)` | S C | Write to the console |

```lua
if IsDuplicityVersion() then
  print('running on the server, in ' .. GetCurrentResourceName())
end

local text = json.encode({ name = 'Alice', level = 3 })
local data = json.decode(text)

local spawn = vector3(0.0, 0.0, 72.0)
print(spawn.x, spawn.y, spawn.z)
```

## Sandbox

Scripts run in a Lua 5.3 VM implemented in JavaScript (fengari). They are sandboxed:

- no `io` library
- no `os.execute`
- no filesystem access from scripts
- no loading of native code

A resource talks to the outside world only through the API on this page.

## Limits to know about

- A script that loops without calling `Wait` blocks the whole server: there is no watchdog yet.
- Integers are 32-bit. Larger whole numbers coming from the host arrive as floats.
- `Wait` only works in threads, event handlers, commands and exports, not inside a coroutine a script creates itself.
- An export or callback that calls `Wait` returns nothing to its caller.
