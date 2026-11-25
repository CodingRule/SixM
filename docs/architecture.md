# Architecture

SixM has two programs and one shared protocol. All of it is written in Rust, in one Cargo workspace.

```mermaid
flowchart LR
  subgraph Clients
    HC["Headless client<br/>(client Lua runtime)"]
    GC["In-game client<br/>(planned, not built)"]
    WEB["Browser / tools"]
  end

  subgraph Server["sixm-server, one TCP port (30120)"]
    WS["WebSocket endpoint"]
    HTTP["HTTP endpoints"]
    CORE["Server core"]
    RM["Resource manager"]
    subgraph Runtimes["Lua 5.4 runtimes, one per resource"]
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

## Pieces

**Server core** (`crates/sixm-server/src/core.rs`). Accepts connections, runs the handshake, assigns player slots
and runs the tick loop (20 Hz by default). On each tick it sends every client a snapshot of the players near it;
players out of range are culled. It also applies rate limiting and tracks heartbeats. The core knows nothing about
scripting: the Lua layer plugs into it through a handler trait.

**HTTP endpoints.** The same port answers plain HTTP: a status page at `/`, plus `/info.json`, `/players.json` and
`/dynamic.json`. A request that asks for a WebSocket upgrade on `/` or `/ws` becomes a player connection.

**Resource manager** (`crates/sixm-server/src/resources.rs`). Finds the resources on disk, loads each started
resource's `manifest.lua`, and routes events, commands and exports between resources and the network. It also
defines the server-side Lua functions (`GetPlayers`, `TriggerClientEvent`, ...). Reading `server.cfg` and the
operator's console commands are in `config.rs` and `app.rs`.

**Lua runtimes** (`crates/sixm-lua`). Each resource runs in its own Lua 5.4 state. The interpreter is the reference
Lua 5.4 implementation, embedded through [mlua](https://github.com/mlua-rs/mlua) and compiled into the executable,
so no system Lua is needed. Scripts are sandboxed: no `io`, no `package`, no `os.execute`, no filesystem access and
no native-code loading. The thread scheduler, events, commands and exports are themselves written in Lua
(`src/bootstrap.lua`); the Rust side sets up the state, converts values and runs the watchdog.

**Watchdog.** Every entry into a script (and every resume of a script thread) gets a budget of 50,000,000 Lua VM
instructions. A script that uses it up without yielding is stopped with an error, the error is logged, and the
server carries on. See [scripting.md](scripting.md#limits-to-know-about) for what the watchdog does not cover.

**Values between runtimes.** Lua states do not share memory. Event arguments, export arguments and results, and
everything sent over the network are converted to JSON and back, so only plain data crosses a resource boundary.

**Headless client** (`crates/sixm-client`). A client with no graphics. It connects, runs the client scripts of the
server's resources in its own Lua runtimes, and can simulate movement. It exists for tests and bots, and it lets
both halves of a resource be developed without a game. The connection logic is a library, so a future in-game
client can reuse it.

**Shared protocol** (`crates/sixm-protocol`). `src/lib.rs` defines the message types and binary layouts used by
both sides. See [protocol.md](protocol.md).

## Threading

The server and the client are single-threaded. Each runs a tokio current-thread runtime that drives the sockets,
the tick loop and every Lua runtime in turn, so server state needs no locks and a script never runs at the same
time as another script. The one extra thread SixM starts reads lines from standard input and hands them to the
main thread.

The cost of this design is that one slow script delays everything else until it yields or the watchdog stops it.

## Technology

| | |
|---|---|
| Language | Rust (edition 2021, minimum version 1.80), one Cargo workspace |
| Async runtime | `tokio`, current-thread |
| Networking | WebSocket over TCP (`tokio-tungstenite`), HTTP/1 with `hyper` on the same port |
| Scripting | Lua 5.4 through `mlua`, built from the vendored Lua sources |
| Packaging | Source, or native executables for Windows x64 and Linux x64 attached to each release |

## Source layout

```
Cargo.toml                    workspace manifest
crates/sixm-protocol/src/
  lib.rs                      wire protocol: messages, binary frames, limits
  command.rs                  command-line splitting
crates/sixm-lua/src/
  lib.rs                      Lua state setup, sandbox, watchdog, entry points
  bootstrap.lua               scheduler, events, commands, exports, vector3
  json.rs                     Lua <-> JSON conversion
crates/sixm-server/src/
  core.rs                     networking, players, state sync, HTTP endpoints
  config.rs                   server.cfg parser
  manifest.rs                 manifest.lua parser
  resources.rs                resource manager and server-side Lua functions
  app.rs                      startup and console commands
  logger.rs                   log output
  main.rs                     sixm-server command line
crates/sixm-client/src/
  lib.rs                      client connection and client-side Lua functions
  main.rs                     sixm-client command line
```

Tests are unit tests inside those files plus integration tests in `crates/sixm-lua/tests` and
`crates/sixm-server/tests`. `cargo test --workspace` runs 119 of them: 8 for the protocol, 47 for the Lua runtime,
and 64 for the server (5 unit, 10 configuration, 38 server core, 11 end-to-end with a real client).

## Releases

Pushing a tag that starts with `v` runs `.github/workflows/release.yml`. It runs the tests, builds `sixm-server`
and `sixm-client` in release mode on Windows and Linux, and attaches `sixm-<version>-windows-x64.zip`,
`sixm-<version>-linux-x64.tar.gz` and `SHA256SUMS.txt` to a GitHub release. Each archive holds the two executables,
`server.cfg`, `LICENSE` and `resources/`.

## What is not here

There is no in-game client. The plan is to hook the open-source Leonida recompilation runtime, and that runtime has
no playable game world yet, so this work is blocked upstream. Until then SixM has no rendering, no game world and no
game natives. See the roadmap in the [README](../README.md#roadmap).
