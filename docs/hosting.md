# Hosting a server

This covers running a SixM server that other people can join. Read the limits at the end before you open one to the
public: SixM is pre-alpha.

## What you need

- A machine that stays on, running 64-bit Windows or Linux. Use the release archive for your system, or build from
  source on any system with a Rust toolchain (see the [README](../README.md#quick-start)).
- One **TCP** port, `30120` by default. HTTP and the game connection share it. No UDP port is used.
- Little else. `sixm-server` is a single executable (about 2.3 MB on Windows) with nothing to install beside it,
  and a full 32-player server measured about 13 MB of memory on a Windows desktop PC.

> [!NOTE]
> The Linux build is new in 0.2.0. It is built and its tests pass in CI, but nobody has run it on a real Linux host
> yet. Please report how it goes.

## Start it

From a release, unpack the archive first. On Windows, extract `sixm-<version>-windows-x64.zip` and run this in the
extracted folder:

```powershell
.\sixm-server.exe
```

On Linux:

```sh
tar -xzf sixm-0.2.0-linux-x64.tar.gz
```

```sh
cd sixm-0.2.0
```

```sh
./sixm-server
```

From source:

```powershell
cargo run --release -p sixm-server
```

The server reads `server.cfg` and the `resources` folder next to the executable when a `resources` folder is there
(as in a release archive), and otherwise from the current folder (as when run from source). Useful options:

| Option | Meaning |
|---|---|
| `-c`, `--config <file>` | Use another configuration file |
| `-r`, `--resources <dir>` | Use another resources folder |
| `-p`, `--port <number>` | Override the port from the configuration |
| `--no-console` | Do not read commands from standard input. Use this when running as a service |
| `--log-level <level>` | `debug`, `info`, `warn` or `error`. The `SIXM_LOG_LEVEL` environment variable sets the default |

Check it is up by opening `http://localhost:30120/` on the server machine.

The release executables are not code-signed, so Windows SmartScreen may warn the first time you run one. Each
release comes with a `SHA256SUMS.txt` file if you want to check a download. On Windows, compare the output of this
with the line for the zip:

```powershell
Get-FileHash .\sixm-0.2.0-windows-x64.zip -Algorithm SHA256
```

On Linux, with `SHA256SUMS.txt` in the same folder as the archive:

```sh
sha256sum --ignore-missing -c SHA256SUMS.txt
```

## Let players on your network join

Players connect to the server machine's local address, for example `192.168.1.20`:

```powershell
.\sixm-client.exe --host 192.168.1.20 --name Alice
```

On Windows the firewall asks the first time the server runs; allow it on the network types you use. To add the rule
yourself, run this in an administrator PowerShell:

```powershell
New-NetFirewallRule -DisplayName "SixM server" -Direction Inbound -Protocol TCP -LocalPort 30120 -Action Allow
```

## Let players on the internet join

1. Give the server machine a fixed local address (a DHCP reservation in your router).
2. In the router, forward **TCP 30120** to that address.
3. Give players your public IP address, or a DNS name that points at it.

Some internet providers put customers behind carrier-grade NAT, where port forwarding cannot work. In that case rent
a small VPS and run the server there.

The listen address is set in `server.cfg`. `0.0.0.0` accepts connections from anywhere; `127.0.0.1` accepts them only
from the same machine, which is what you want behind a reverse proxy:

```cfg
endpoint_add_tcp "0.0.0.0:30120"
```

## Encryption (TLS)

The server speaks plain HTTP and WebSocket. Anyone on the path can read and alter the traffic. For a public server,
put a reverse proxy that terminates TLS in front of it and have the server listen on `127.0.0.1` only.

Any proxy that supports WebSocket upgrades works. With [Caddy](https://caddyserver.com), which obtains a certificate
by itself, the whole configuration is:

```
play.example.com {
    reverse_proxy 127.0.0.1:30120
}
```

Players then connect with `--secure` on the proxy's port:

```powershell
.\sixm-client.exe --host play.example.com --port 443 --secure --name Alice
```

Behind a proxy the server would otherwise see every player coming from the proxy's address. Name the proxy
addresses in `server.cfg` and the server believes `X-Forwarded-For` from them, and nobody else:

```
sv_proxies "127.0.0.1, 10.0.0.5"
```

The client address is the right-most one in the header that is not itself a listed proxy, so a chain of two
proxies works. `GetPlayerEndpoint` and the `ip:` identifier then show the player, not the proxy.

## Run it as a service on Linux

A systemd unit, assuming the release archive was unpacked into `/opt/sixm` (so that `/opt/sixm/sixm-server`,
`/opt/sixm/server.cfg` and `/opt/sixm/resources` exist):

```ini
[Unit]
Description=SixM server
After=network-online.target
Wants=network-online.target

[Service]
User=sixm
WorkingDirectory=/opt/sixm
ExecStart=/opt/sixm/sixm-server --no-console
Restart=on-failure
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

Run it as its own unprivileged user, never as root.

## Operating it

Type these in the server console:

| Command | Does |
|---|---|
| `help` | List the console commands |
| `status` | Player count, tick and running resources, then each player's id, name, ping and address |
| `kick <id> [reason]` | Remove a player |
| `resources` | List resources and whether they are started |
| `start <resource>`, `stop <resource>` | Start or stop a resource |
| `restart <resource>`, `ensure <resource>` | Stop the resource if it is running, reread its files from disk and start it |
| `refresh` | Look for new resource folders |
| `quit` | Stop the server and disconnect everyone |

Resources add their own commands; the bundled ones add `say <text>` and `players`. Ctrl+C stops the server the same
way `quit` does.

Clients that are already connected keep the client scripts they received when they joined, so after `restart` they
have to reconnect to get new client code.

Monitoring tools can poll `/dynamic.json` for the player count and `/players.json` for names and pings. Those
endpoints are public and unauthenticated; they never include addresses.

## What has been tested

| Scenario | Result |
|---|---|
| 32 players on a 32-slot server, through the machine's LAN address (Windows desktop, 0.2.0) | Every client received 20 snapshots per second, no errors, about 13 MB of server memory |
| A 33rd player | Refused with "server is full" |
| A client through a TLS reverse proxy with `--secure` | Worked with 0.1. **Not tested again** since the client was rewritten in Rust |
| Players joining from another machine or across the internet | **Not tested yet** |
| More than 32 players | **Not tested.** Position sync compares every player with every other, so cost grows quickly |
| The Linux executables and the systemd unit above | **Not run on a real host.** The Linux build and its test suite pass in CI |

## Limits to know before going public

- **No accounts and no bans.** A player's only identifier is their IP address, names are not unique, and `kick` does
  not stop someone from rejoining ([#9](https://github.com/CodingRule/SixM/issues/9)). A resource can refuse
  connections in `playerConnecting`.
- **No server list.** Players need your address.
- **Client state is trusted.** Positions are checked for being valid numbers, not for being possible.
- **A bad script can still hurt.** The watchdog stops a script that runs 50,000,000 Lua instructions without
  yielding, and the server keeps running, but everything waits while that script uses up its budget. The watchdog
  does not interrupt a single long call into a built-in function, and there is no memory limit for scripts. Only
  run resources you trust. See [scripting.md](scripting.md#limits-to-know-about).
- **No per-address connection limit.** There is a cap on connections that have not finished joining, and nothing more.
- **Clients are text-only.** There is no in-game client.
