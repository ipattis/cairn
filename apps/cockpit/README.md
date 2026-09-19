# Cockpit

A Tauri window over `wikiskilld`. Seven views: Runs, Gate, Skills, Patterns, Labels, Schedule,
Setup.

```sh
cd src-tauri && cargo run
```

## It is a relay, not a second brain

No loop logic lives here. A run takes hours and must survive this window closing, so the
daemon owns every run, and the cockpit only calls the localhost API. There are exactly five
commands on the Rust side:

- `api(method, path, body)` — forwards to the daemon with the bearer token attached. The
  webview names a path, never a host, so it cannot be pointed at another machine.
- `open_obsidian(link)` — hands an `obsidian://` link to the OS, and refuses any other scheme.
- `launch_obsidian()` — opens Obsidian, for the one setup step the daemon cannot do.
- `reveal_vault()` — opens the vault in Finder, so Obsidian's folder picker can be aimed at it.
- `where_is_the_daemon()` — for the footer, so an unreachable daemon is diagnosable.

The last two take **no arguments**. The path comes from the daemon's config, read by the Rust
side at startup; an "open this path" or "launch this app" command the webview parameterises
would turn a window that renders model output into a file browser and a launcher.

The token stays in the Rust process. The webview never sees it, and never sees a stored provider
key: `/v1/config` strips `daemon.token`, and no credential endpoint returns a value.

## Frontend

Plain HTML, CSS and one JS file in `dist/` — no bundler, no npm install, no framework. The
CSP allows `'self'` only, so there is nothing inline.

Everything a model wrote — skill bodies, diffs, proposal summaries, the impact log — goes into
the DOM with `textContent`, never `innerHTML`. A skill is text a model produced, and this
window has IPC access to the daemon.

The Gate view renders `wiki/skill-impact.md` verbatim rather than reformatting it, so what a
reviewer reads is exactly what is committed to the vault.

The Setup view is the one place the webview handles a secret, and only inbound: a key typed into
a `password` field goes straight out on a `PUT` and the field is cleared in a `finally`, whether
the save succeeded or not. Nothing reads a credential back, so the view can show that a key is
set but never what it is. `wikiskilld credential set <service>` is offered in the view itself for
anyone who would rather a key never be in a window at all.

## Icons

`src-tauri/icons/*` are placeholder flat squares, generated so `generate_context!` has the
icon it requires. Replace them before shipping this anywhere.
