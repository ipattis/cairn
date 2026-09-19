# Cockpit

A Tauri window over `wikiskilld`. Six views: Runs, Gate, Skills, Patterns, Labels, Schedule.

```sh
cd src-tauri && cargo run
```

## It is a relay, not a second brain

No loop logic lives here. A run takes hours and must survive this window closing, so the
daemon owns every run, and the cockpit only calls the localhost API. There are exactly three
commands on the Rust side:

- `api(method, path, body)` — forwards to the daemon with the bearer token attached. The
  webview names a path, never a host, so it cannot be pointed at another machine.
- `open_obsidian(link)` — hands an `obsidian://` link to the OS, and refuses any other scheme.
- `where_is_the_daemon()` — for the footer, so an unreachable daemon is diagnosable.

The token stays in the Rust process. The webview never sees it, and never sees a provider key:
`/v1/config` strips `daemon.token` before returning.

## Frontend

Plain HTML, CSS and one JS file in `dist/` — no bundler, no npm install, no framework. The
CSP allows `'self'` only, so there is nothing inline.

Everything a model wrote — skill bodies, diffs, proposal summaries, the impact log — goes into
the DOM with `textContent`, never `innerHTML`. A skill is text a model produced, and this
window has IPC access to the daemon.

The Gate view renders `wiki/skill-impact.md` verbatim rather than reformatting it, so what a
reviewer reads is exactly what is committed to the vault.

## Icons

`src-tauri/icons/*` are placeholder flat squares, generated so `generate_context!` has the
icon it requires. Replace them before shipping this anywhere.
