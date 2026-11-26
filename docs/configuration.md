# Configuration: server.cfg

The dedicated server is configured with a plain-text `server.cfg` file: one directive per line.

## Example

```cfg
sv_hostname "My SixM server"
sv_maxclients 32
endpoint_add_tcp "0.0.0.0:30120"

sets tags "roleplay, dev"
set mykey value

ensure hello
```

## Syntax

- One directive per line; arguments are separated by spaces or tabs. Directive names are not case-sensitive.
- `#` or `//` at the start of a word begins a comment that runs to the end of the line.
- Quote values that contain spaces with double quotes. Inside quotes, `\"` is a quote and `\\` is a backslash.
- A line the server cannot use (an unknown directive, a value out of range, an unclosed quote) is reported as a
  warning with its line number when the server starts. The rest of the file still applies.

## Where the file is looked for

The server reads `server.cfg` and the `resources` folder from the folder that holds the executable when that folder
contains a `resources` folder (the layout of a release archive). Otherwise it reads them from the working directory,
which is what happens when you run from a source checkout with `cargo run`.

If there is no configuration file, the server logs a warning, uses the defaults and starts every resource it finds.

## Directives

| Directive | Example | Default | Purpose |
|---|---|---|---|
| `sv_hostname` | `sv_hostname "My SixM server"` | `SixM Server` | Server name shown to clients and in the HTTP endpoints |
| `sv_maxclients` | `sv_maxclients 32` | `32` | Number of player slots, 1 to 65535 |
| `endpoint_add_tcp` | `endpoint_add_tcp "0.0.0.0:30120"` | `0.0.0.0:30120` | Address and port to listen on. Also accepts a port alone, or `[v6-address]:port` |
| `sv_tickrate` | `sv_tickrate 20` | `20` | Ticks per second, 1 to 240: how often snapshots are sent and sleeping script threads are woken |
| `sv_streamdistance` | `sv_streamdistance 500` | `500` | Players further apart than this are not sent to each other. `0` or less means no limit |
| `sv_proxies` | `sv_proxies "127.0.0.1, 10.0.0.5"` | none | Reverse proxies whose `X-Forwarded-For` is believed. The header from any other peer is ignored |
| `ensure` | `ensure hello` | | Start the resource named `hello`. `start` means the same |
| `stop` | `stop hello` | | Take a resource back off the start list |
| `set` | `set mykey value` | | Set a convar |
| `sets` | `sets tags "roleplay, dev"` | | Set a convar that is also published in `/info.json` |

Less common:

- `sv_port <port>` and `sv_host <address>` set the two halves of `endpoint_add_tcp` separately. Port `0` lets the
  operating system pick a free port.
- The `sv_*` settings can also be written with `set`, as in `set sv_maxclients 64`.
- `setr <key> <value>` is accepted and behaves like `set`. Convars are not sent to clients yet.
- `endpoint_add_udp` is accepted and ignored, so a configuration that has the line still loads. SixM listens on TCP
  only.

Settings are read once, at startup. Changing a `sv_*` convar from a script with `SetConvar` does not change the
running server.

## Command-line options

These override or locate the configuration:

| Option | Meaning |
|---|---|
| `-c`, `--config <file>` | Configuration file (default `server.cfg`) |
| `-r`, `--resources <dir>` | Resources folder (default `resources`) |
| `-p`, `--port <number>` | Override the port from the configuration |
| `--no-console` | Do not read commands from standard input |
| `--log-level <level>` | `debug`, `info`, `warn` or `error` (default `info`) |
| `-v`, `--version` | Print the version |
| `-h`, `--help` | Print the help |

Relative `--config` and `--resources` paths are resolved against the folder described under
[Where the file is looked for](#where-the-file-is-looked-for). The environment variable `SIXM_LOG_LEVEL` sets the
log level when `--log-level` is not given.

## Network

- The default port is **30120**.
- The port is **TCP only**. HTTP and WebSocket traffic share it, so one firewall rule covers both the game
  connection and the HTTP endpoints.
- There is no UDP transport yet. It is a roadmap item.

`0.0.0.0` listens on all IPv4 interfaces. Use `127.0.0.1` to accept local connections only.

## Convars

Values set with `set` and `sets` are convars. Read them from Lua:

```cfg
set motd "Welcome to the server"
set max_warnings 3
```

```lua
local motd = GetConvar('motd', 'Welcome!')
local maxWarnings = GetConvarInt('max_warnings', 5)
```

Server scripts can change them at runtime with `SetConvar(name, value)`. Client scripts cannot read them yet. See
[scripting.md](scripting.md#convars).

Use `sets` for values you want visible to the outside (for example `tags`), and `set` for everything else. Do not
put secrets in `sets`. `sets tags` is special: the comma-separated value is also published as the `tags` list in
`/info.json`.

## Resources

Each `ensure` line starts one resource, in the order written. Order matters when resources depend on each other:
start the one that provides exports first.

```cfg
ensure bank
ensure shop
```

See [resources.md](resources.md).

## HTTP endpoints

The server answers HTTP `GET` and `HEAD` on the same port:

| Path | Returns |
|---|---|
| `/` | Human-readable status page |
| `/info.json` | `server`, `name`, `build`, `protocol`, `maxPlayers`, `tickRate`, `tags`, `vars` (the `sets` convars) and `resources` (names of the started resources) |
| `/players.json` | A list of `{ id, name, ping }` |
| `/dynamic.json` | `name`, `clients`, `maxClients` and `uptime` in seconds |

These endpoints are unauthenticated, and the JSON ones allow cross-origin requests. Anything they return is public
to whoever can reach the port. They never include player addresses.
