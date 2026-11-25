# Hosting a server

This covers running a SixM server that other people can join. Read the limits at the end before you open one to the
public: SixM is pre-alpha.

## What you need

- A machine that stays on. Windows with the release zip, or any OS with Node.js 22 or newer running from source.
- One **TCP** port, `30120` by default. HTTP and the game connection share it. No UDP port is used.
- Little else: a full 32-player server measured about 65 MB of memory and very little CPU on a desktop PC.

## Start it

From the release zip:

```powershell
.\sixm-server.exe
```

From source:

```powershell
npm start
```

The server reads `server.cfg` and the `resources` folder next to the executable (or in the current folder when run
from source). Useful options:

| Option | Meaning |
|---|---|
| `--config <file>` | Use another configuration file |
| `--resources <dir>` | Use another resources folder |
| `--port <number>` | Override the port from the configuration |
| `--no-console` | Do not read commands from standard input. Use this when running as a service |
| `--log-level <level>` | `debug`, `info`, `warn` or `error` |

Check it is up by opening `http://localhost:30120/` on the server machine.

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

Behind a proxy the server sees every player coming from the proxy's address, because it does not read
`X-Forwarded-For`. `GetPlayerEndpoint` and the `ip:` identifier are therefore the same for everyone.

## Run it as a service on Linux

A systemd unit, assuming the repository is in `/opt/sixm` and `npm ci` has been run there:

```ini
[Unit]
Description=SixM server
After=network-online.target
Wants=network-online.target

[Service]
User=sixm
WorkingDirectory=/opt/sixm
ExecStart=/usr/bin/node bin/sixm-server.js --no-console
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
| `status` | Players, their ping and address |
| `kick <id> [reason]` | Remove a player |
| `resources` | List resources and whether they are started |
| `start`, `stop`, `restart <resource>` | Control a resource. `restart` rereads its files from disk |
| `refresh` | Look for new resource folders |
| `quit` | Stop the server and disconnect everyone |

Monitoring tools can poll `/dynamic.json` for the player count and `/players.json` for names and pings. Those
endpoints are public and unauthenticated; they never include addresses.

## What has been tested

| Scenario | Result |
|---|---|
| 32 players on a 32-slot server, through the machine's LAN address | Stable at 20 updates per second, no errors |
| A 33rd player | Refused with "server is full" |
| A client through a TLS reverse proxy with `--secure` | Works |
| Players joining from another machine or across the internet | **Not tested yet** |
| More than 32 players | **Not tested.** Position sync compares every player with every other, so cost grows quickly |
| The systemd unit above | **Not tested**; the test suite does pass on Linux in CI |

## Limits to know before going public

- **No accounts and no bans.** A player's only identifier is their IP address, names are not unique, and `kick` does
  not stop someone from rejoining. A resource can refuse connections in `playerConnecting`.
- **No server list.** Players need your address.
- **Client state is trusted.** Positions are checked for being valid numbers, not for being possible.
- **A bad script can stall the server.** A resource that loops without calling `Wait` blocks everything.
- **No per-address connection limit.** There is a cap on connections that have not finished joining, and nothing more.
- **Clients are text-only.** There is no in-game client.
