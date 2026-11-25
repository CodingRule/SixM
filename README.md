<p align="center">
  <img src="assets/banner.svg" alt="SixM — Lua-scripted multiplayer, built for the next generation." width="100%">
</p>

<h1 align="center">SixM: GTA VI multiplayer framework</h1>

<p align="center">
  <b>An open-source multiplayer framework for GTA VI (GTA 6): a dedicated server, a Lua resource system and a client,
  with gameplay written as scripts.</b><br>
  What FiveM is to GTA V, SixM aims to be for GTA VI. In development: there is no in-game client yet.
</p>

<p align="center">
  <a href="https://github.com/CodingRule/SixM/actions/workflows/ci.yml"><img src="https://github.com/CodingRule/SixM/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/status-pre--alpha-orange" alt="Status: pre-alpha">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/rust-1.80%2B-DEA584?logo=rust&logoColor=white" alt="Rust 1.80 or newer">
  <img src="https://img.shields.io/badge/Lua-5.4-2C2D72?logo=lua&logoColor=white" alt="Lua 5.4">
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux-lightgrey" alt="Platform: Windows and Linux">
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#your-first-resource">First resource</a> ·
  <a href="docs/scripting.md">Lua API</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#roadmap">Roadmap</a> ·
  <a href="#help-wanted">Help wanted</a> ·
  <a href="#faq">FAQ</a> ·
  <a href="#legal">Legal</a>
</p>

> [!IMPORTANT]
> **SixM is pre-alpha (v0.2.0) and there is no in-game client yet.**
>
> - SixM does **not** launch, modify or contain GTA VI. You cannot play GTA VI multiplayer with it today.
> - What exists is the dedicated server, the Lua resource system, and a **headless client** (no graphics) used for
>   testing and bots.
> - The planned game integration targets the open-source Leonida runtime
>   ([Leonida Continued](https://github.com/ubiquitarianism/leonida)), which boots the game's menus from your own copy
>   but has nothing playable yet. In-game multiplayer is blocked on that. It is a roadmap item, not a feature.
> - **Own the Xbox Series copy of the game? [You can help.](#help-wanted)**
> - No game files, keys or assets are in this repository, and none ever will be.

## What works today

| | Area | Notes |
|:-:|---|---|
| ✅ | Dedicated server | Handshake, player slots, tick loop (20 Hz by default), heartbeat, rate limiting |
| ✅ | State sync | Position/state snapshots with distance-based stream culling |
| ✅ | HTTP endpoints | `/info.json`, `/players.json`, `/dynamic.json` and a status page at `/`, on the game port |
| ✅ | Configuration | `server.cfg` with `ensure`, convars and endpoint settings |
| ✅ | Resource system | Server-side and client-side Lua scripts, declared in a `manifest.lua` |
| ✅ | Scripting | Lua 5.4. Threads, local and net events (both directions), commands, exports, convars, JSON |
| ✅ | Script watchdog | A script that runs too long without yielding is stopped and the error is logged; the server keeps running |
| ✅ | Headless client | Connects, runs client Lua, can simulate movement. For tests and bots |
| ✅ | Standalone executables | Native `sixm-server` and `sixm-client` binaries for Windows and Linux (about 2 MB each on Windows). The Linux build is built in CI and has not been run on a real host yet |
| 🚧 | In-game client | Planned on top of the Leonida runtime. Blocked upstream: that runtime has no playable game world yet |
| ❌ | Game-world natives | No peds, vehicles, weapons or world natives. There is no game attached to script against |
| ❌ | UDP transport | Transport is WebSocket over TCP for now |
| ❌ | Entity ownership, vehicles, state bags | Only player state is synced |
| ❌ | Server-authoritative validation | Client-reported state is sanity-checked, not validated against game rules |
| ❌ | UI layer, permissions, hot-reload | See the [roadmap](#roadmap) |

## Quick start

SixM is written in Rust. You can download a release or build it yourself.

### Option 1: download a release

Get the archive for your system from the [releases page](https://github.com/CodingRule/SixM/releases):
`sixm-<version>-windows-x64.zip` or `sixm-<version>-linux-x64.tar.gz`. Each one holds `sixm-server`, `sixm-client`,
`server.cfg`, `LICENSE` and the `resources/` folder. Unpack it, open a terminal in the unpacked folder and start the
server:

```powershell
.\sixm-server.exe
```

In a second terminal, connect a headless client:

```powershell
.\sixm-client.exe --name Alice
```

On Linux the commands are `./sixm-server` and `./sixm-client --name Alice`. The executables are not code-signed, so
Windows SmartScreen may warn the first time.

### Option 2: build from source

You need a [Rust toolchain](https://rustup.rs) (1.80 or newer) and a C compiler, because the Lua interpreter is
compiled from source as part of the build: the MSVC Build Tools on Windows, gcc or clang on Linux. On Linux the
client's TLS support also needs the OpenSSL development headers (`libssl-dev`) and `pkg-config`. No system Lua is
needed.

```powershell
git clone https://github.com/CodingRule/SixM
```

```powershell
cd SixM
```

Start the server:

```powershell
cargo run --release -p sixm-server
```

In a second terminal, connect a headless client:

```powershell
cargo run --release -p sixm-client -- --name Alice
```

### Try it

The server listens on port `30120` (TCP) for both HTTP and WebSocket traffic. Open <http://localhost:30120/> for the
status page, or <http://localhost:30120/players.json> to see Alice in the player list.

Headless client flags:

| Flag | Meaning |
|---|---|
| `-H`, `--host <address>` | Server address to connect to (default `127.0.0.1`) |
| `-p`, `--port <number>` | Server port (default `30120`) |
| `-n`, `--name <name>` | Player name (default `Player`) |
| `--pos <x,y,z>` | Starting position (default `0,0,0`) |
| `--wander` | Walk around at random, so there is movement to sync |
| `--secure` | Connect with TLS, for servers behind a TLS reverse proxy |

Both the server and the client read commands from their terminal. In the server console try `help`, `status`,
`players` or `say hello`; in the client try `say hello`, `hello`, `near`, `/pos 10 20 0` or `/list`. `help`, `status`,
`/pos` and `/list` are built in; the others come from the three resources that ship in `resources/`: `chat`,
`welcome` and `hello`.

Run the tests:

```powershell
cargo test --workspace
```

Build optimised executables:

```powershell
cargo build --release -p sixm-server -p sixm-client
```

The binaries land in `target/release/`. They run without Rust or Lua installed. The server looks for `server.cfg`
and `resources/` next to its executable when a `resources` folder is there, and in the working directory otherwise.
A binary started from the repository root as `target\release\sixm-server.exe` therefore uses the repository's
`server.cfg` and `resources/`.

## Your first resource

A resource is a folder under `resources/` with a `manifest.lua` and some scripts.

```
resources/
  greeter/
    manifest.lua
    server.lua
    client.lua
```

**`resources/greeter/manifest.lua`**

```lua
name 'greeter'
description 'Greets players'
author 'you'
version '1.0.0'

server_script 'server.lua'
client_script 'client.lua'
```

**`resources/greeter/server.lua`**

```lua
-- A client asks to be greeted; answer only that client.
RegisterNetEvent('greeter:greetMe', function()
  local id = source -- the player who sent the event
  print(('greeting player %d'):format(id))
  TriggerClientEvent('greeter:greeting', id, 'Welcome, ' .. GetPlayerName(id) .. '!')
end)

-- greet <message...>, typed in the server console or by a connected client, sends a message to every connected client.
RegisterCommand('greet', function(source, args, raw)
  TriggerClientEvent('greeter:greeting', -1, table.concat(args, ' '))
end, false)

-- Threads can sleep without blocking the server.
Citizen.CreateThread(function()
  while true do
    Citizen.Wait(10000)
    print(('%d player(s) online'):format(GetNumPlayers()))
  end
end)
```

**`resources/greeter/client.lua`**

```lua
RegisterNetEvent('greeter:greeting', function(message)
  print(message)
end)

Citizen.CreateThread(function()
  Citizen.Wait(1000)
  TriggerServerEvent('greeter:greetMe')
end)
```

Enable it in `server.cfg`:

```cfg
ensure greeter
```

Restart the server and connect a headless client (the client prefixes script output with the resource name). The client sends `greeter:greetMe`, the server replies with
`greeter:greeting`, and the client prints the greeting. That is a full net event round trip.

Next: the [Lua API reference](docs/scripting.md) and the [resource and manifest reference](docs/resources.md).

## Architecture

```mermaid
flowchart LR
  subgraph Clients
    HC["Headless client<br/>(client Lua runtime)"]
    GC["In-game client<br/>(planned, not built)"]
    WEB["Browser / tools"]
  end

  subgraph Server["sixm-server, one TCP port (30120)"]
    WS["WebSocket endpoint"]
    HTTP["HTTP endpoints<br/>/ · /info.json · /players.json · /dynamic.json"]
    CORE["Server core<br/>handshake · slots · tick loop<br/>snapshots · stream culling · rate limits"]
    RM["Resource manager<br/>server.cfg · manifests · events · exports"]
    subgraph Runtimes["Lua 5.4 runtimes (mlua), one per resource"]
      L1["resource A"]
      L2["resource B"]
    end
  end

  HC <-->|"JSON control + binary state"| WS
  GC -.->|planned| WS
  WEB -->|GET| HTTP
  WS <--> CORE
  HTTP --> CORE
  CORE <--> RM
  RM <--> L1
  RM <--> L2
```

- **One port.** HTTP and WebSocket share TCP port `30120`.
- **One thread.** A single-threaded tokio runtime drives the network, the tick loop and every Lua runtime.
- **Two kinds of frames.** JSON text frames carry control messages and events. Binary frames carry packed entity
  state. The exact layout is in [docs/protocol.md](docs/protocol.md).
- **One Lua runtime per resource.** Scripts run in a sandboxed Lua 5.4 state (the reference interpreter, embedded
  with [mlua](https://github.com/mlua-rs/mlua)). There is no `io`, no `os.execute`, and no filesystem or native-code
  loading from scripts. A watchdog stops a script that runs too long without yielding.
- **Client scripts are real.** The headless client runs each resource's client scripts in its own Lua runtime, so
  both halves of a resource can be tested without a game.
- **Four Rust crates.** `sixm-protocol` (wire format), `sixm-lua` (script runtime), `sixm-server` and `sixm-client`.

**Why Rust.** An in-game client has to be native code that can be loaded next to the game, which the earlier Node.js
implementation could not be. The port also made the Windows executables small (about 2 MB each instead of about 96 MB) and
lets SixM embed the reference Lua 5.4 interpreter. The wire protocol did not change.

More detail in [docs/architecture.md](docs/architecture.md).

## Configuration

The server reads `server.cfg`. A minimal file:

```cfg
sv_hostname "My SixM server"
sv_maxclients 32
endpoint_add_tcp "0.0.0.0:30120"

sets tags "roleplay, dev"
set mykey value

ensure hello
```

| Directive | Purpose |
|---|---|
| `sv_hostname "<name>"` | Server name |
| `sv_maxclients <n>` | Number of player slots |
| `endpoint_add_tcp "<ip>:<port>"` | Address and port to listen on (HTTP and WebSocket) |
| `ensure <resource>` | Start a resource from `resources/` |
| `set <key> <value>` | Set a convar, readable from Lua with `GetConvar` |
| `sets <key> <value>` | Set a convar that is also published as server info |

Full reference: [docs/configuration.md](docs/configuration.md).

## HTTP endpoints

Served on the same port as the game traffic.

| Path | Returns |
|---|---|
| `/` | Human-readable status page |
| `/info.json` | Server information |
| `/players.json` | Connected players |
| `/dynamic.json` | Values that change while the server runs |

## Coming from FiveM?

SixM borrows the developer experience, not the code. Function names for events and threads are deliberately the
same, so the mental model carries over. The game-facing parts do not exist yet.

| In FiveM | In SixM | Notes |
|---|---|---|
| `fxmanifest.lua` | `manifest.lua` | `name`, `description`, `author`, `version`, `server_script(s)`, `client_script(s)`, `shared_script(s)` |
| `server.cfg`, `ensure` | `server.cfg`, `ensure` | Same idea. See [docs/configuration.md](docs/configuration.md) for what is supported |
| `Citizen.CreateThread`, `Citizen.Wait`, `SetTimeout` | Same names | `CreateThread` and `Wait` aliases included |
| `AddEventHandler`, `RegisterNetEvent`, `TriggerEvent` | Same names | |
| `TriggerClientEvent`, `TriggerServerEvent`, `source` | Same names | Target `-1` means everyone |
| `RegisterCommand`, `exports`, convars | Same names | |
| `GetPlayers`, `GetPlayerName`, `DropPlayer`, ... | Same names | A small subset. See [docs/scripting.md](docs/scripting.md) |
| Game natives (peds, vehicles, world) | **Not available** | No game is attached. Only player position, heading and basic state are synced |
| NUI (HTML UI) | **Not available** | |
| OneSync | **No equivalent** | Only basic distance-based stream culling of player state |
| State bags | **Not available** | On the roadmap |
| ACL / `add_ace` | **Not available** | On the roadmap |
| Lua 5.4, JavaScript, C# runtimes | **Lua 5.4 only** | Lua 5.4, as in FiveM. There is no JavaScript or C# runtime |
| UDP (ENet) transport | **WebSocket over TCP** | UDP is on the roadmap |

Existing FiveM resources will not run unmodified: anything that calls game natives, NUI or a framework that depends
on them has nothing to call into.

## Roadmap

- [x] Dedicated server, resource system, net events, headless client
- [x] Server and client core ported to Rust ([#5](https://github.com/CodingRule/SixM/issues/5))
- [x] Watchdog for runaway scripts ([#8](https://github.com/CodingRule/SixM/issues/8))
- [x] Linux release builds ([#11](https://github.com/CodingRule/SixM/issues/11))
- [ ] In-game client through a Leonida runtime hook (**blocked upstream**: no playable game world yet). See [docs/game-integration.md](docs/game-integration.md)
- [ ] UDP transport ([#6](https://github.com/CodingRule/SixM/issues/6))
- [ ] Entity ownership and vehicles
- [ ] Server-authoritative validation
- [ ] State bags
- [ ] Additional script runtimes (JavaScript, C#)? Undecided
- [ ] Resource hot-reload
- [ ] ACL / permissions
- [ ] Web admin panel

No dates. Items land when they work. The roadmap is discussed in
[#12](https://github.com/CodingRule/SixM/issues/12).

## Help wanted

SixM needs two kinds of people.

**Anyone who writes code.** Networking, scripting and hosting work needs no game at all. Start with the issues
labelled [`good first issue`](https://github.com/CodingRule/SixM/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
or [`help wanted`](https://github.com/CodingRule/SixM/issues?q=is%3Aissue+is%3Aopen+label%3A%22help+wanted%22).

**Anyone who owns the Xbox Series copy of GTA VI.** The in-game client cannot be built without people who can run
the game through Leonida and report what happens. See the issues labelled
[`needs-game-copy`](https://github.com/CodingRule/SixM/issues?q=is%3Aissue+is%3Aopen+label%3Aneeds-game-copy) and
read [docs/game-integration.md](docs/game-integration.md) first. Never post game files, keys or instructions for
obtaining them.

## FAQ

### Can I play GTA 6 multiplayer with SixM today?

No. SixM has a working server and a text-only test client, but no client inside the game. Nobody can play GTA VI
multiplayer through SixM yet.

### Is SixM "FiveM for GTA 6"?

That is the goal: custom multiplayer servers for GTA VI with gameplay written in Lua, the way FiveM does it for
GTA V. SixM is a separate project with its own code. It is not made by, or affiliated with, Cfx.re or Rockstar Games.

### When will GTA 6 multiplayer mods work?

There is no date. It depends on GTA VI running on PC with a playable world. Today the only route is the community
project Leonida, which boots the game's menus from your own Xbox Series copy and is not playable yet.

### Is there a GTA 6 roleplay (RP) server I can join?

Not on SixM. You can host a SixM server now and write the Lua resources for one, but players can only connect with
the headless client.

### Do I need to own GTA VI?

Not to run the server, write resources or contribute to the networking and scripting code. Any future in-game client
will require your own copy of the game. SixM will never ship game files.

### Will my FiveM resources work?

Not unmodified. The event, thread and command functions have the same names, so plain Lua logic ports easily.
Anything that calls game natives, NUI or a framework built on them has nothing to call yet.

### How do I host a SixM server?

Download the [latest release](https://github.com/CodingRule/SixM/releases) for Windows or Linux, unpack it, run
`sixm-server` (`sixm-server.exe` on Windows), and read [docs/hosting.md](docs/hosting.md). The server is one
small executable (about 2.3 MB on Windows); there is no runtime to install.

## Project layout

```
SixM/
  Cargo.toml            Cargo workspace: version, shared dependencies, release profile
  crates/
    sixm-protocol/      wire protocol and command splitting, shared by server and client
    sixm-lua/           sandboxed Lua 5.4 runtime: threads, events, commands, exports, watchdog
    sixm-server/        dedicated server (library and the sixm-server binary)
    sixm-client/        headless client (library and the sixm-client binary)
  resources/            resources, one folder each; [brackets] are category folders
  docs/                 documentation
  assets/               logo and banner
  server.cfg            server configuration
```

Tests sit next to the code (`crates/*/src` and `crates/*/tests`) and run with `cargo test --workspace`.

## Documentation

- [Lua scripting API](docs/scripting.md)
- [Resources and manifests](docs/resources.md)
- [Configuration (`server.cfg`)](docs/configuration.md)
- [Hosting a server](docs/hosting.md)
- [GTA VI integration: status and how to help](docs/game-integration.md)
- [Wire protocol](docs/protocol.md)
- [Architecture](docs/architecture.md)
- [Release notes: v0.2.0](docs/releases/v0.2.0.md)

## Contributing

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first, and the
[Code of Conduct](CODE_OF_CONDUCT.md). Report security issues privately as described in [SECURITY.md](SECURITY.md).

## Legal

SixM is an independent, community project. It is **not affiliated with, endorsed by, or connected to Rockstar Games,
Take-Two Interactive, or Cfx.re / FiveM**.

- All code in this repository is original. It is inspired by the developer experience of FiveM and is not derived
  from Cfx.re or FiveM source code.
- Grand Theft Auto, GTA, Rockstar Games, Take-Two, FiveM and all related names and marks are trademarks of their
  respective owners. They are used here only to describe what this project is and is not.
- This repository contains no game code, game files, keys or assets, and contributions that include any will be
  rejected.
- SixM does not launch or modify any game today. Any future game integration will require you to own a legitimate
  copy of the game.

## License

[MIT](LICENSE) © 2026 SixM contributors
