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

You need Node.js 22 or newer.

```powershell
git clone https://github.com/CodingRule/SixM
```

```powershell
cd SixM
```

```powershell
npm install
```

```powershell
npm test
```

Run a server and a headless client to try things by hand:

```powershell
npm start
```

```powershell
npm run client -- --name Alice --wander
```

## Making a change

1. For anything larger than a small fix, open an issue first so the approach can be agreed before you write code.
2. Create a branch from `main`.
3. Keep the change focused: one topic per pull request.
4. Add or update tests in `test/` for behaviour you change. `npm test` must pass.
5. Update the docs in `docs/` and the README when behaviour, API or configuration changes. If you change
   `src/shared/protocol.js`, update `docs/protocol.md` in the same pull request.
6. Open the pull request and fill in the template.

CI runs `npm test` on Linux and Windows with Node 22 and 24.

## Code style

- ES modules, Node 22+ APIs. No transpilation step.
- Keep dependencies minimal. A new runtime dependency needs a reason in the pull request description.
- Formatting follows `.editorconfig`: two-space indentation, LF line endings, UTF-8.
- Treat everything that arrives from the network as hostile: validate lengths, types and ranges.
- Do not widen the Lua sandbox (no `io`, `os.execute`, filesystem or native loading) without a design discussion in
  an issue first.

## Commit messages

Write a short imperative summary line (`Fix snapshot count overflow`), then a blank line and any detail that helps a
reviewer. Reference issues with `#123` where relevant.

## Reporting bugs

Use the bug report template. Include the SixM version or commit, your OS and Node version, what you did, what you
expected and what happened. Logs help.

## Security issues

Do not open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md).

## Licence

By contributing you agree that your contribution is licensed under the [MIT License](LICENSE).
