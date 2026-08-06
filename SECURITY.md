# Security Policy

## Reporting a vulnerability

**Please do not open a public issue.**

Report privately through GitHub Security Advisories:
[**Report a vulnerability**](https://github.com/imateusdev/loom-router/security/advisories/new).
That opens a thread visible only to you and the maintainer, and it's the only
channel that lets a fix ship before the details are public.

Include the version, the OS, and the smallest reproduction you have. If a
proof of concept needs an API key to demonstrate, describe the shape of the
request instead of sending a working key.

Expect an acknowledgement within a few days. This is a project maintained by
one person in their own time — if you've had no reply after a week, ping the
advisory thread rather than assuming it was missed on purpose.

You'll be credited in the advisory and the release notes unless you'd rather
not be.

## Supported versions

Only the latest release. Fixes ship forward in a new version and reach users
through the built-in updater; there are no backports to older tags.

## What counts as a vulnerability here

LoomRouter is a local desktop app that holds provider credentials and runs a
proxy on localhost. The interesting parts of its attack surface:

- **Credential exposure at rest.** API keys live in
  `~/.loomrouter/config.json`; the local proxy token lives in the managed
  block of `~/.codex/config.toml`. Both are written through `secure_fs` and
  are owner-only on Unix. Anything that lands either of these on disk
  world-readable is a real bug — this has happened before: the token file was
  once written at `0644` by a third copy of the atomic-write helper that
  didn't tighten permissions, and anything that could read it could spend the
  stored keys.
- **Credential exposure in transit or in logs.** Keys are blanked in
  `get_config` before reaching the webview, which only ever learns `has_key`.
  Request and response bodies are never logged, because they carry user
  prompts and source code. A path that leaks a key to the UI, to a log, to a
  crash report, or to an upstream that shouldn't receive it is in scope.
- **The local proxy as an open door.** The proxy is authenticated by the
  token in `~/.codex/config.toml`. Anything that lets an unauthenticated
  local process — or a web page in a browser on the same machine — reach the
  proxy and spend your credits is in scope.
- **Request routing.** A request reaching a different provider than the one
  selected, or credentials for provider A being attached to a request bound
  for provider B.
- **Update integrity.** Releases are signed and verified by the Tauri
  updater. Anything that would let an unsigned or substituted build install
  is in scope.

### Known limitation, not a vulnerability

On Windows, the owner-only guarantee that `secure_fs` provides on Unix is not
currently matched by an equivalent ACL. This is documented at the function so
callers can't mistake success for safety. Reports that Windows config files
are readable by other accounts on the same machine describe a known gap — a
proposed fix is very welcome, filed as a normal issue or PR.

### Out of scope

- An attacker who already has code execution as your user. At that point the
  files are readable by definition.
- Vulnerabilities in the upstream providers themselves, or in the models.
  Report those to the provider.
- Missing hardening headers or similar findings against the local webview
  with no path to a concrete impact.
