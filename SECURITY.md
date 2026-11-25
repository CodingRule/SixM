# Security Policy

## Supported versions

SixM is pre-alpha. Only the latest commit on `main` is supported. There are no backported fixes.

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately through GitHub security advisories:

1. Go to <https://github.com/CodingRule/SixM/security/advisories/new>
   (or the repository's **Security** tab, then **Report a vulnerability**).
2. Describe the issue, the affected component (server, client, Lua sandbox, protocol, HTTP endpoints, build
   tooling), and how to reproduce it. A minimal proof of concept helps a lot.
3. Say whether you would like to be credited.

We will acknowledge the report, work on a fix in private, and publish an advisory once a fix is available. This is a
volunteer project, so we do not promise response times, but security reports are handled before other work.

## What counts

Examples of things we want to hear about:

- Escaping the Lua sandbox (reaching the filesystem, the process, or native code from a script).
- A remote client crashing, hanging or exhausting the server with crafted frames or messages.
- A client impersonating another player or receiving data it should not.
- Bypassing rate limiting.
- Problems in the HTTP endpoints that expose more than intended.

## Known limitations, not vulnerabilities

These are documented gaps on the roadmap rather than security bugs:

- Player state (position and so on) is reported by clients and is not validated server-side yet.
- There is no ACL / permission system yet.
- The HTTP endpoints are unauthenticated by design. Do not put secrets in `sets` convars.

## Out of scope

- Anything involving a game, game files or third-party runtimes. SixM does not contain or ship any.
- Vulnerabilities in dependencies with no demonstrated impact on SixM. Report those upstream.
