# GTA VI integration: where it stands and how to help

SixM's server, Lua resources and headless client work today. What is missing is the part everyone wants: a client
running **inside the game**. This page says exactly what blocks that, and what you can do about it, with or without a
copy of the game.

## The short version

| Piece | State |
|---|---|
| SixM server, resources, protocol, headless client | ✅ Works. Native code (Rust) since 0.2 |
| GTA VI on PC | ❌ No PC release at the time of writing. The only route today is the community project **Leonida** |
| [Leonida Continued](https://github.com/ubiquitarianism/leonida) | ⚠️ Per its own README: boots from your own Xbox Series copy through the menus and the story load. **Nothing is playable yet** and no world frame has been verified |
| SixM client inside the game | ❌ Not started. It needs a running game world to attach to |

SixM is an independent project. It is not part of Leonida, and it does not include Leonida's code (Leonida is GPLv3,
SixM is MIT).

## What an in-game client needs, in order

1. **A game world that runs.** That is Leonida's work, not SixM's. Until the game reaches a playable frame there is
   nothing to synchronise.
2. **A way to run SixM's client next to the game** ([#2](https://github.com/CodingRule/SixM/issues/2)). Leonida's runtime (`gta6rt`) loads the game's
   executable and resolves its imports to its own code. A native SixM client would be loaded by, or alongside, that
   runtime. Since 0.2 the client core is a Rust library (`crates/sixm-client`), so it is native code already; what
   it lacks is a C interface another program can call ([#4](https://github.com/CodingRule/SixM/issues/4)) and something to load it. See Leonida's
   `docs/modding.md` for the mod loader that exists today (it swaps models by name; it is not a general script
   hook).
3. **Reading the local player** ([#3](https://github.com/CodingRule/SixM/issues/3)). Position, heading, velocity and a few flags, every tick, sent to the
   server as the [`STATE` frame](protocol.md).
4. **Showing remote players.** Creating, moving and removing a character for each player the server streams to you.
5. **Game functions for scripts.** The equivalent of natives, so Lua resources can do more than read positions.

Steps 3 to 5 mean finding things inside the running game. That work can only be done by people who own the game.

## How to help without the game

Most open work needs no game files at all:

- the [issues labelled `help wanted`](https://github.com/CodingRule/SixM/issues?q=is%3Aissue+is%3Aopen+label%3A%22help+wanted%22)
  and [`good first issue`](https://github.com/CodingRule/SixM/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
- networking: UDP transport ([#6](https://github.com/CodingRule/SixM/issues/6)), interpolation ([#7](https://github.com/CodingRule/SixM/issues/7)), delta-compressed snapshots
- client: a C ABI for the client core ([#4](https://github.com/CodingRule/SixM/issues/4)), so a non-Rust host can embed it
- scripting: more of the API, and limits the watchdog does not cover yet (memory, long built-in calls)
- hosting: bans and identifiers ([#9](https://github.com/CodingRule/SixM/issues/9)), `X-Forwarded-For` support behind a proxy ([#10](https://github.com/CodingRule/SixM/issues/10)), trying the
  Linux build on a real host
- writing resources and reporting what the API is missing

The roadmap as a whole is discussed in [#12](https://github.com/CodingRule/SixM/issues/12).

## How to help if you own the Xbox Series copy

You are the people this project cannot move forward without.

- **Report how far it boots for you.** Build [Leonida Continued](https://github.com/ubiquitarianism/leonida) from
  your own copy and post your result in the boot-report issue ([#1](https://github.com/CodingRule/SixM/issues/1)): GPU, driver, operating system, and the
  last stage it reached. Bugs in Leonida itself belong in Leonida's tracker, not here.
- **Take a research issue.** The issues labelled
  [`needs-game-copy`](https://github.com/CodingRule/SixM/issues?q=is%3Aissue+is%3Aopen+label%3Aneeds-game-copy)
  describe what has to be found in the running game.
- **Contribute upstream.** Every step Leonida takes towards a playable frame brings SixM closer. Its
  `CONTRIBUTING.md` explains how work is claimed there.

### Rules for anything involving the game

These are not negotiable, and issues or pull requests that break them are removed.

- **Never post game files.** No archives, executables, shaders, textures, audio, saves, memory dumps, or logs that
  quote them. Describe what you found; do not attach it.
- **No keys, no decryption, no dumping guides.** SixM does not explain how to get the game's files onto a PC, and
  neither may issues, comments or pull requests.
- **Own what you use.** Work only from a copy you bought.
- **Nothing online.** Do not connect a modified game to Rockstar's or Xbox's services.

## Why not just wait?

Because the server side does not have to. The protocol, the scripting API and the tooling can all be designed, tested
and hardened now with the headless client, so that when a game world is available the client is the only missing
piece.
