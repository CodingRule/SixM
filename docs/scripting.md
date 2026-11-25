# Lua scripting API

SixM resources are written in Lua 5.4. Server scripts run inside the dedicated server; client scripts run inside the
client (today, the headless client). Each resource gets its own Lua runtime on each side.

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
- [Values that cross a boundary](#values-that-cross-a-boundary)
- [Sandbox](#sandbox)
- [Limits to know about](#limits-to-know-about)
- [Changes since 0.1](#changes-since-01)

## Threads and timers

| Function | Avail. | Description |
|---|:-:|---|
| `Citizen.CreateThread(fn)` / `CreateThread(fn)` | S C | Run `fn` as a thread that may call `Wait`. It runs immediately, up to its first `Wait` |
| `Citizen.Wait(ms)` / `Wait(ms)` | S C | Suspend the current thread for `ms` milliseconds |
| `Citizen.SetTimeout(ms, fn)` / `SetTimeout(ms, fn)` | S C | Call `fn` once after `ms` milliseconds. `fn` may call `Wait` |

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

Sleeping threads are woken on the server's tick (20 times per second by default), so a `Wait` is never shorter than
asked but can be up to one tick longer. `Wait(0)` resumes on the next tick.

A loop inside a thread must call `Wait`. A thread that never yields is stopped by the
[watchdog](#limits-to-know-about), and until that happens nothing else on the server runs.

## Events

| Function | Avail. | Description |
|---|:-:|---|
| `AddEventHandler(name, fn)` | S C | Register a handler for an event. Returns a handle |
| `RemoveEventHandler(handle)` | S C | Remove a handler registered with `AddEventHandler` |
| `RegisterNetEvent(name[, fn])` | S C | Allow `name` to be received over the network. With `fn`, also registers it as a handler and returns its handle |
| `TriggerEvent(name, ...)` | S C | Trigger a local event: every resource on the same side that has a handler for `name` receives it |
| `TriggerClientEvent(name, target, ...)` | S | Send an event to a client. `target` is a player id, or `-1` for everyone |
| `TriggerServerEvent(name, ...)` | C | Send an event to the server |
| `source` (global) | S | Inside a server handler for a net event: the id of the player who sent it. `0` for events that did not come from a player |

An event must be registered with `RegisterNetEvent` on the receiving side before it can arrive over the network.
An event that arrives without being registered is ignored.

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

Every handler runs as its own thread, so it may call `Wait`. `TriggerEvent` runs each handler up to its first `Wait`
(or to its end) before it returns.

`source` is a global that is only meaningful while a handler runs. Copy it into a local (`local id = source`) before
calling `Wait`.

Event arguments are copied, not shared: see [Values that cross a boundary](#values-that-cross-a-boundary). The
name of an event sent over the network can be up to 128 characters long.

Never trust arguments that arrive from a client. Check types and ranges in every server-side net event handler. The
server allows each client 50 events per second on average, with bursts of up to 100, and kicks a client that goes
over.

## Built-in events

Server:

| Event | Arguments | When |
|---|---|---|
| `playerConnecting` | `name, setKickReason, deferrals` | A player is connecting, before they get a slot or an id |
| `playerJoined` | `id, name` | A player has joined. `source` is their id too |
| `playerDropped` | `reason` | A player left. `source` is their id |
| `onResourceStart` | `resourceName` | A resource started (its scripts have already run) |
| `onResourceStop` | `resourceName` | A resource is about to stop |

Client:

| Event | Arguments | When |
|---|---|---|
| `onClientResourceStart` | `resourceName` | A resource's client scripts have been loaded |
| `onClientReady` | `id` | The client has joined and all client scripts are loaded. `id` is the local player's id |
| `playerJoined` | `id, name` | Another player joined after you |
| `playerLeft` | `id, name, reason` | Another player left |

```lua
AddEventHandler('playerJoined', function(id, name)
  print(('player %d joined: %s'):format(id, name))
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

Inside a `playerDropped` handler the player's slot is already free, but `GetPlayerName(source)` and the other player
functions still answer for that player until the handlers have run.

### Refusing a connection

`playerConnecting` handlers decide whether a player may join:

```lua
AddEventHandler('playerConnecting', function(name, setKickReason, deferrals)
  if #name < 2 then
    setKickReason('Your name must be at least 2 characters long.')
  end
end)
```

- `setKickReason(reason)` refuses the player with that reason.
- `deferrals.defer()` postpones the decision, for example to `Wait` or to ask another resource. Finish with
  `deferrals.done()` to accept or `deferrals.done(reason)` to refuse. A deferred connection that is not finished
  within 10 seconds is refused.
- `deferrals.update(message)` is accepted and does nothing yet.

These three are real functions passed by the server. They are the one exception to the rule that functions cannot
be passed in events.

## Players (server)

| Function | Avail. | Returns |
|---|:-:|---|
| `GetPlayers()` | S | List of connected player ids, ascending |
| `GetNumPlayers()` | S | Number of connected players |
| `GetPlayerName(id)` | S | The player's name, or `nil` for an unknown id |
| `GetPlayerPing(id)` | S | The player's ping in milliseconds (`0` until first measured), or `-1` for an unknown id |
| `GetPlayerIdentifiers(id)` | S | List of identifiers for the player. Today that is one entry, `ip:<address>` |
| `GetPlayerEndpoint(id)` | S | The player's network endpoint as `address:port`, or `nil` for an unknown id |
| `GetPlayerCoords(id)` | S | The player's position as a table with `x`, `y` and `z`, or `nil` for an unknown id |
| `GetPlayerHeading(id)` | S | The player's heading in degrees (`0.0` for an unknown id) |
| `DropPlayer(id, reason)` | S | Disconnects the player with `reason` |

Player ids start at 1; a new player gets the lowest free id. Functions that take an id accept a number or a string
holding one.

```lua
for _, id in ipairs(GetPlayers()) do
  local pos = GetPlayerCoords(id)
  print(GetPlayerName(id), GetPlayerPing(id), pos.x, pos.y, pos.z)
end

DropPlayer(3, 'Kicked by an admin')
```

`GetPlayerCoords` returns a plain table, not a `vector3`. Wrap it if you want vector arithmetic:
`vector3(pos.x, pos.y, pos.z)`. A player who has not reported a position yet is at `0, 0, 0`.

Positions and headings are reported by clients. There is no server-authoritative validation yet, so do not treat
them as proof of anything.

## Local player and others (client)

| Function | Avail. | Returns |
|---|:-:|---|
| `GetLocalPlayerId()` | C | This client's player id |
| `GetLocalCoords()` | C | This client's position, as a table with `x`, `y` and `z` |
| `GetLocalHeading()` | C | This client's heading in degrees |
| `SetLocalCoords(x, y, z)` | C | Sets this client's position |
| `SetLocalHeading(h)` | C | Sets this client's heading |
| `GetActivePlayers()` | C | List of the player ids this client knows about, including its own |
| `GetPlayerName(id)` | C | Name of a player, or `nil` for an unknown id |
| `GetPlayerCoords(id)` | C | Position of a player, or `nil` while that player is out of streaming range |
| `GetPlayerHeading(id)` | C | Heading of a player (`0.0` while that player is out of streaming range) |
| `IsPlayerStreamed(id)` | C | Whether that player's state is currently being sent to this client. Always `true` for the local player |
| `GetPing()` | C | Round trip to the server in milliseconds, or `-1` before the first measurement |
| `GetServerName()` | C | The server's name (`sv_hostname`) |

```lua
SetLocalCoords(100.0, 200.0, 30.0)
SetLocalHeading(90.0)

for _, id in ipairs(GetActivePlayers()) do
  print(id, GetPlayerName(id), IsPlayerStreamed(id))
end
```

The server only streams players within range of a client, so position data on the client is limited to players that
are currently streamed to it.

## Commands

| Function | Avail. | Description |
|---|:-:|---|
| `RegisterCommand(name, fn, restricted)` | S C | Register a command. `fn` receives `(source, args, raw)` |
| `ExecuteCommand(str)` | S C | Run a command line as if it had been typed in that side's console |

`args` is a list of the words after the command name; `raw` is the full command line. Double quotes group words
(`say "hello world"` has one argument), and a leading `/` is ignored. Command names are case-sensitive.

A server command registered with `restricted` set to `true` can only be run from the server console. With `false`,
connected players can run it too, and `source` is their player id (`0` for the console). A command typed in a client
runs there if a client script registered it (`source` is then the local player's id), and is sent to the server
otherwise. `restricted` has no effect on client commands.

```lua
RegisterCommand('announce', function(source, args, raw)
  TriggerClientEvent('chat:message', -1, 'server', table.concat(args, ' '))
end, false)

ExecuteCommand('announce Restart in five minutes')
```

On the server, `ExecuteCommand` runs the line as the console (`source` is `0`) and returns `true` when a command
handled it. The server console's built-in commands (`status`, `kick`, ...) are listed in
[hosting.md](hosting.md#operating-it).

There is no ACL/permission system yet (it is on the roadmap).

## Convars

| Function | Avail. | Description |
|---|:-:|---|
| `GetConvar(name, default)` | S C | Read a convar as a string, or `default` when unset |
| `GetConvarInt(name, default)` | S C | Read a convar as an integer, or `default` (`0` if omitted) when unset or not a number |
| `SetConvar(name, value)` | S | Set a convar |

```lua
local motd = GetConvar('motd', 'Welcome!')
local limit = GetConvarInt('sv_maxclients', 32)
SetConvar('motd', 'Server restarts at midnight')
```

Convars are usually set in [`server.cfg`](configuration.md) with `set` and `sets`.

`SetConvar` changes what `GetConvar` returns from then on. It does not change server settings such as
`sv_maxclients`, which are read once at startup, and it does not change what `/info.json` shows.

Convars are not sent to clients yet. In client scripts `GetConvar` and `GetConvarInt` exist but always return the
default.

## Exports

| Function | Avail. | Description |
|---|:-:|---|
| `exports('name', fn)` | S C | Expose `fn` to other resources under `name` |
| `exports.<resource>:<name>(...)` | S C | Call a function exported by another resource. `exports.<resource>.<name>(...)` works too |

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
The exporting resource has to be started first (`ensure` it earlier in `server.cfg`). Calling an export of a
resource that is not running, or one that does not exist, raises an error, and so does an export that raises one.

An export call returns one value: the export's first result. Arguments and the result are copied, not shared: see
the next section.

## Utilities

| Function | Avail. | Description |
|---|:-:|---|
| `GetCurrentResourceName()` | S C | Name of the resource the script belongs to |
| `IsDuplicityVersion()` | S C | `true` on the server, `false` on the client. Useful in shared scripts |
| `IS_SERVER`, `IS_CLIENT` (globals) | S C | The same information as booleans |
| `GetGameTimer()` | S C | Milliseconds since the server started (server) or since this client connected (client) |
| `GetResourceState(name)` | S | `'started'`, `'stopped'` (found on disk, not running) or `'missing'` |
| `GetNumResources()` | S | Number of running resources |
| `GetHostName()` | S | The server's name (`sv_hostname`) |
| `json.encode(value)` | S C | Serialise a Lua value to a JSON string |
| `json.decode(str)` | S C | Parse a JSON string into a Lua value |
| `vector3(x, y, z)` | S C | Create a 3D vector |
| `print(...)` | S C | Write to the console, prefixed with the resource name |

```lua
if IsDuplicityVersion() then
  print('running on the server, in ' .. GetCurrentResourceName())
end

local text = json.encode({ name = 'Alice', level = 3 })
local data = json.decode(text)

local spawn = vector3(0.0, 0.0, 72.0)
print(spawn.x, spawn.y, spawn.z)
```

A `vector3` has the fields `x`, `y` and `z` and supports `+` and `-` with another vector, `*` and `/` with a number,
unary minus, `==`, and `#v` for its length:

```lua
local distance = #(vector3(10.0, 0.0, 0.0) - vector3(4.0, 8.0, 0.0)) -- 10.0
```

`json.encode` raises an error for values JSON cannot hold (functions, userdata, threads), for tables that contain
themselves, and for tables nested more than 64 levels deep. `json.decode` raises an error for invalid JSON.

## Values that cross a boundary

Each resource has its own Lua state, so a value is never shared between resources: it is converted to JSON and
rebuilt on the other side. This applies to:

- arguments of `TriggerEvent`, `TriggerClientEvent` and `TriggerServerEvent`;
- arguments and the result of an export call.

It applies even when the handler is in the same resource as the caller. The rules:

| Lua value | Arrives as |
|---|---|
| `nil`, booleans, strings | The same value |
| Integers | The same integer (the full 64-bit range) |
| Floats | The same float. NaN and infinities become `nil` |
| A table whose keys are exactly `1..n` | A list with the same items |
| Any other table | A table with the same entries. Integer keys stay integers, and so do string keys that are a whole number in text form (`"7"` arrives as `7`). Float and boolean keys arrive as strings |
| Functions, userdata, threads | `nil` |

What follows from that:

- An export cannot return a function, and a function cannot be passed to another resource in an event. The only
  functions that arrive in an event are the ones the server passes to `playerConnecting`.
- Metatables are not kept. A `vector3` arrives as a plain table with `x`, `y` and `z`.
- The receiver gets a copy. Changing a table you received does not change the sender's table.
- A table that contains itself, or is nested more than 64 levels deep, cannot be sent: the triggering call raises an
  error.
- Strings must be valid UTF-8 to arrive unchanged; other bytes are replaced.

## Sandbox

Scripts run in a real Lua 5.4 interpreter with a reduced standard library:

- available: the base functions, `coroutine`, `table`, `string`, `utf8`, `math`
- `os` has only `os.time`, `os.clock`, `os.date` and `os.difftime`
- `debug` has only `debug.traceback`
- no `io` library, no `os.execute`, no filesystem access from scripts
- no `package` library: `require` raises an error, and `dofile` and `loadfile` do not exist. Split a resource into
  several files by listing them in its [manifest](resources.md)
- `load` accepts Lua source text only, not precompiled bytecode
- no loading of native code

A resource talks to the outside world only through the API on this page.

## Limits to know about

- **Watchdog.** A script that executes more than 50,000,000 Lua VM instructions without yielding is stopped with an
  "exceeded instruction limit" error. The error is logged with a traceback and the server keeps running. The budget
  applies separately to each thread, event handler, command, export call and timeout, and starts again every time
  one of them resumes after a `Wait`. Only that thread or handler is stopped; the resource stays started, and its
  other threads and handlers keep working. `pcall` cannot catch the error. While a script is using up its budget,
  nothing else on the server runs.
- **What the watchdog does not cover.** It counts Lua VM instructions only. A single long call into a built-in
  function, such as `string.find` with a pathological pattern on a long string, is not interrupted. There is also no
  memory limit: a script can allocate until the process runs out of memory.
- `Wait` only works in threads, event handlers, commands, exports and timeouts. It raises an error at the top level
  of a script file, and it does not sleep inside a coroutine a script creates itself.
- An export that calls `Wait` returns nothing (`nil`) to its caller; it finishes later on its own.
- Values passed between resources and over the network are copied through JSON, as described
  [above](#values-that-cross-a-boundary).
- Command names are case-sensitive.

## Changes since 0.1

SixM 0.1 ran scripts on a Lua 5.3 VM written in JavaScript. Since 0.2 the server and client are written in Rust and
embed the reference Lua 5.4 interpreter. The API functions are the same, and existing resources run unchanged unless
they relied on one of these:

- **Lua 5.4 instead of 5.3.** Mostly additions, such as `<const>` and `<close>` variables. The Lua 5.4 manual lists
  the few incompatibilities with 5.3.
- **Integers are 64-bit.** The 0.1 limit of 32-bit integers is gone.
- **There is a watchdog.** In 0.1 a script that looped without calling `Wait` blocked the server for good. Now it is
  stopped, subject to the caveats above.
- **Values between resources go through JSON.** Functions, userdata and non-finite numbers arrive as `nil` in events
  and export calls, as described above.
