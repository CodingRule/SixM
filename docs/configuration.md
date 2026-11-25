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

Quote values that contain spaces.

## Directives

| Directive | Example | Purpose |
|---|---|---|
| `sv_hostname` | `sv_hostname "My SixM server"` | Server name shown to clients and in the HTTP endpoints |
| `sv_maxclients` | `sv_maxclients 32` | Number of player slots |
| `endpoint_add_tcp` | `endpoint_add_tcp "0.0.0.0:30120"` | Address and port to listen on |
| `ensure` | `ensure hello` | Start the resource `resources/hello` |
| `set` | `set mykey value` | Set a convar |
| `sets` | `sets tags "roleplay, dev"` | Set a convar that is also published as server info |

## Network

- The default port is **30120**.
- The port is **TCP only**. HTTP and WebSocket traffic share it, so one firewall rule covers both the game
  connection and the HTTP endpoints.
- There is no UDP transport yet. It is a roadmap item.

`0.0.0.0` listens on all interfaces. Use `127.0.0.1` to accept local connections only.

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

Server scripts can change them at runtime with `SetConvar(name, value)`. See
[scripting.md](scripting.md#convars).

Use `sets` for values you want visible to the outside (for example `tags`), and `set` for everything else. Do not
put secrets in `sets`.

## Resources

Each `ensure` line starts one resource. Order matters when resources depend on each other: start the one that
provides exports first.

```cfg
ensure bank
ensure shop
```

See [resources.md](resources.md).

## HTTP endpoints

The server answers HTTP on the same port:

| Path | Returns |
|---|---|
| `/` | Human-readable status page |
| `/info.json` | Server information |
| `/players.json` | Connected players |
| `/dynamic.json` | Values that change while the server runs |

These endpoints are unauthenticated. Anything they return is public to whoever can reach the port.
