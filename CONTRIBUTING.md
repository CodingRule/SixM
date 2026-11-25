# Contributing to SixM

Thanks for wanting to help. SixM is pre-alpha, so the most useful contributions right now are bug reports with
reproduction steps, tests, documentation fixes and small focused changes.

## Ground rules

These are not negotiable, because the project depends on them:

1. **No game content.** Never commit or attach game files, executables, keys, assets, memory dumps, decompiled or
   disassembled game code, or anything extracted from a game. Pull requests that include any are closed.
2. **Original code only.** Do not copy code from Cfx.re / FiveM, from leaked sources, or from any project whose
   licence is not compatible with MIT. Using the same public function *names* for familiarity is fine; copying
   implementations is not.
3. **Be honest in docs.** Do not document a feature as working unless it works on `main`. Planned work belongs in the
   roadmap.
4. Follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## If your contribution involves the game

Read [docs/game-integration.md](docs/game-integration.md). In short: never post game files, memory dumps, keys, or
instructions for obtaining or decrypting the game, and work only from a copy you own. Issues and pull requests that
break this are removed.

## Getting set up

You need:

- a [Rust toolchain](https://rustup.rs), 1.80 or newer, with `rustfmt` and `clippy`;
- a C compiler, because the Lua 5.4 interpreter is compiled from source during the build: the MSVC Build Tools on
  Windows, gcc or clang on Linux;
- on Linux, the OpenSSL development headers (`libssl-dev`) and `pkg-config`, for the client's TLS support.

```powershell
git clone https://github.com/CodingRule/SixM
```

```powershell
cd SixM
```

```powershell
cargo test --workspace
```

Run a server and a headless client to try things by hand:

```powershell
cargo run --release -p sixm-server
```

```powershell
cargo run --release -p sixm-client -- --name Alice --wander
```

## Where things are

| Crate | Contents |
|---|---|
| `crates/sixm-protocol` | Wire protocol (`src/lib.rs`) and command-line splitting (`src/command.rs`) |
| `crates/sixm-lua` | Sandboxed Lua 5.4 runtime: `src/lib.rs`, the scheduler and script API in `src/bootstrap.lua`, the JSON bridge in `src/json.rs` |
| `crates/sixm-server` | Dedicated server: `core.rs` (networking, sync, HTTP), `config.rs`, `manifest.rs`, `resources.rs` (resource manager and server natives), `app.rs` (startup and console commands), `main.rs` (CLI) |
| `crates/sixm-client` | Headless client: `lib.rs` (connection and client natives), `main.rs` (CLI) |

## Making a change

1. For anything larger than a small fix, open an issue first so the approach can be agreed before you write code.
2. Create a branch from `main`.
3. Keep the change focused: one topic per pull request.
4. Add or update tests for behaviour you change: unit tests next to the code, or integration tests in the crate's
   `tests/` folder. `cargo test --workspace` must pass.
5. Update the docs in `docs/` and the README when behaviour, API or configuration changes. If you change
   `crates/sixm-protocol/src/lib.rs`, update `docs/protocol.md` in the same pull request. If you add, remove or
   change a Lua function, update `docs/scripting.md`.
6. Open the pull request and fill in the template.

CI checks formatting and lints on Linux, then runs the tests on Linux and Windows with the stable toolchain. Run the
same checks locally before you push:

```powershell
cargo fmt --all --check
```

```powershell
cargo clippy --workspace --all-targets -- -D warnings
```

```powershell
cargo test --workspace
```

## Code style

- Rust 2021, formatted with `rustfmt`, free of `clippy` warnings. The minimum supported Rust version is 1.80.
- The server and client are single-threaded (a tokio current-thread runtime). Shared state uses `Rc`, `Cell` and
  `RefCell`; do not hold a `RefCell` borrow across a call into Lua or an `.await`.
- Keep dependencies minimal. A new dependency needs a reason in the pull request description.
- `unsafe` needs a `SAFETY` comment explaining why it is sound.
- Lua, Markdown and configuration files follow `.editorconfig`: two-space indentation, LF line endings, UTF-8.
- Treat everything that arrives from the network as hostile: validate lengths, types and ranges.
- Do not widen the Lua sandbox (no `io`, `os.execute`, filesystem or native loading) without a design discussion in
  an issue first.

## Commit messages

Write a short imperative summary line (`Fix snapshot count overflow`), then a blank line and any detail that helps a
reviewer. Reference issues with `#123` where relevant.

## Reporting bugs

Use the bug report template. Include the SixM version or commit, your OS, whether you run a release build or built
from source (and then your `rustc --version`), what you did, what you expected and what happened. Logs help.

## Security issues

Do not open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md).

## Licence

By contributing you agree that your contribution is licensed under the [MIT License](LICENSE).
