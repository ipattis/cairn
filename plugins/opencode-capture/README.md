# opencode-capture

Posts finished OpenCode sessions to the WikiSkill daemon, so the user's real coding feeds the
same wiki the rollouts do (phase 4 of the rationale).

```sh
ln -s "$PWD/index.js" ~/.config/opencode/plugin/wikiskill-capture.js
export WIKISKILL_API="http://127.0.0.1:8787"       # optional; this is the default
export WIKISKILL_API_TOKEN="$(wikiskilld token)"   # required, or the plugin stays silent
```

Single file, no dependencies, no build step.

## What it does and does not do

It listens for `session.idle`, reads the session with the SDK `client`, maps it to the
daemon's transcript shape, and `fetch`es it to `POST /v1/capture`. That is all.

- **No redaction here.** The daemon redacts, because it is the component that has to be
  trusted with that; a second implementation in a plugin is a second place to get it wrong.
- **No shell, no filesystem.** A plugin runs inside the user's daily agent, which is the last
  place that should be able to run commands or read files.
- **Never fails a session.** Every error path logs and returns. Losing a capture is a
  nuisance; interrupting work the user is in the middle of is not acceptable for a feature
  that only collects data.
- **Sends each session once.** `session.idle` can fire repeatedly for one session, and the
  daemon would happily file it twice.

Without `WIKISKILL_API_TOKEN` the plugin does nothing at all, which is the right behaviour on
a machine where the daemon is not set up.

## What happens next

The daemon redacts the session, labels it (Jev, if enabled), and writes
`raw/daily/<date>/<session>.md` read-only plus the full transcript under `raw/_jsonl/`. The
nightly maintainer folds the day into `wiki/`. The weekly proposer may suggest one skill
change from the week's patterns — but daily work has no validation split, so that proposal is
rolled back and held in the cockpit for a human to accept or reject.
