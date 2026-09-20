# Setup

macOS only. Everything runs on one machine: a daemon, a vault, and OpenCode.

## 1. Build

```sh
cargo build --release                       # the daemon
cargo test --workspace                      # 172 tests, no network needed
(cd apps/cockpit/src-tauri && cargo build)  # the cockpit
```

The cockpit is a separate cargo workspace on purpose, so the daemon's tests do not build
Tauri's dependency tree.

If any curator model uses the `bedrock-runtime` endpoint, build with `--features bedrock`.
Without it the daemon refuses to start a run — it constructs every model client, curators
included, before the first rollout — and the error names the missing feature. It is off by
default because it pulls in the AWS SDK, which a Fireworks-only install should not pay for.

## 2. Install the executor, pinned

OpenCode **V2, pinned to 2.0.x**. The daemon speaks only V2 and checks the version it finds,
refusing anything outside the pinned minor:

```sh
npm install -g @opencode/cli
opencode2 --version
```

The package installs two binaries, `opencode` and `opencode2`; `executor.binary` defaults to
`opencode2` so a V1 `opencode` left on `PATH` cannot be picked up by accident. If the install is
not global, set `executor.binary` to the absolute path — the daemon does not search for it.

Three things about V2 that the daemon handles but that are worth knowing when reading its logs:

- **The server requires HTTP Basic auth.** Username `opencode`; the daemon generates a random
  password per supervisor and passes it in the child's environment, never in argv, so `ps` does
  not publish it. A stale server from an earlier run cannot answer for the current daemon.
- **Prompting is asynchronous.** `POST /api/session/{id}/prompt` returns as soon as the turn is
  admitted; completion is `POST /api/experimental/session/{id}/wait`, with the `idle` entry in
  `GET /api/session/{id}/message` as the fallback signal.
- **Permissions are an ordered array**, `permissions`, not V1's `permission` object, and an action
  with no matching rule defaults to **ask** — which in a non-interactive client means a stuck
  turn. The daemon therefore opens with an allow-all rule and denies from there; see
  `rollout_permissions()` in `crates/wikiskill-core/src/executor/opencode.rs`.

Every call still goes through the `Executor` trait (`crates/wikiskill-core/src/executor/`), but
there is no V1 implementation behind it any more: V2 changed the server API, the plugin API and
the config shape, and keeping both would have meant two of everything for a line that is being
retired.

## 3. Initialise the vault

The vault is an Obsidian folder that is also a git repo. It must be an absolute path, and
outside `~/Documents`, `~/Desktop` and iCloud Drive — those are sync roots, and a syncing
client racing the daemon's writes corrupts history.

```sh
wikiskilld init --vault ~/Vaults/WikiSkill
```

This writes `<support dir>/config.json`, creates `raw/ wiki/ skills/ eval/`, makes the first
commit, generates the API token, seeds an eval template, and writes `.obsidian/app.json` so the
wiki's `[[wikilinks]]` resolve the way the Wiki Maintainer writes them.

`<support dir>` is `~/Library/Application Support/wikiskill/`.

## 3b. Tell Obsidian about the vault

`init` can configure the folder but cannot register it: Obsidian records a vault in its own
`obsidian.json` when you open one, and there is no supported interface for a program to add an
entry. So this step is yours, once:

1. Open Obsidian.
2. In the vault switcher (bottom of the left sidebar), choose **Open folder as vault**.
3. Select the vault path.

Until you do, `obsidian://` links — the "open in Obsidian" links beside every skill, note and
proposal — will not open. The cockpit's **Setup** tab shows this state, with a banner on every
view while the vault is unregistered, and a Re-check button. `GET /v1/obsidian` is the same
state if you would rather see it from a script.

## 4. Credentials

Keys live in the Keychain. The daemon reads them once at startup into its own environment;
the webview never sees a stored value, and only rollout-scoped keys are passed to
`opencode serve`. Three ways to set one, in order of preference:

```sh
wikiskilld credential set wikiskill-fireworks   # prompts hidden; the value never enters the daemon
wikiskilld credential list                      # which are set, which this config requires
wikiskilld credential unset wikiskill-jev
```

The cockpit's **Setup** tab does the same thing for anyone not at a terminal: each credential is
a card with its purpose, whether it is in the Keychain, whether the running daemon has it, and
whether this config requires it. A value typed there is sent write-only and the field is cleared
immediately — no endpoint reads a credential back, so a saved key can be replaced but never
shown. Changing one is refused while a run is active, because that run is already using the old
key.

Or set the Keychain items directly:

```sh
security add-generic-password -a "$USER" -s wikiskill-fireworks -w   # rollouts
security add-generic-password -a "$USER" -s wikiskill-jev -w         # only if Jev is on
```

An already-set environment variable wins over the Keychain, which is how to run the daemon
from a shell without a Keychain prompt.

For Bedrock's own runtime (the `bedrock` cargo feature) the AWS credential chain is used
instead — `aws sso login` or an instance role, not a Keychain item.

## 5. Set the model IDs

`config.json` ships with placeholders, not real IDs. Set:

- `models.inference` — the deploy model, which runs rollouts. Everything is measured on it.
  Its `endpoint` must be `fireworks`: rollouts reach their model through the executor, and the
  only key the executor is given is the Fireworks one.
- `models.maintainer` and `models.proposer` — the curator models, which are the only thing
  allowed to see the wiki. These the daemon calls directly, so they can sit on any endpoint.

On Bedrock, use the **inference profile** id — `global.anthropic.claude-sonnet-5`, not the bare
`anthropic.claude-sonnet-5`, which Converse rejects as an unknown model. `aws bedrock
list-inference-profiles` shows which are ACTIVE for your account.

Leave `temperature` **unset** for the newer Claude models: they reject the parameter outright
(`ValidationException: \`temperature\` is deprecated for this model`) rather than ignoring it.
The Bedrock client retries once without it and logs a warning, so this costs a wasted round trip
rather than a run — but a run that dies here has already paid for a full iteration of rollouts,
so it is worth getting right in the config.

Use the **dated** id, not the undated alias — `…/deepseek-v4-pro-0813`, not `…/deepseek-v4-pro`.
An alias can be re-pointed under you, which silently changes what every stored score means; and
in practice an undated alias is often not deployed for a given account at all.

The daemon declares the deploy models itself rather than relying on whatever the executor's
bundled catalog happens to know: a `providers` entry named by `executor.model_provider` (default
`wikiskill-deploy`) in the rollout server's private config, using V2's native
`@opencode/ai/providers/openai-compatible` package and listing each model with explicit
`capabilities`. An uncatalogued model would otherwise be given V2's fallback limits.

The key is declared by **name** — `"env": ["FIREWORKS_API_KEY"]` — and never as a value or a
`{env:…}` template: the rollout can read that file, so nothing credential-shaped goes in it.

## 6. Write the task set

`<vault>/eval/{train,val,test}.jsonl`, 30 / 15 / 15, no id repeated between splits. See
`docs/eval-tasks.md` and the generated `<vault>/eval/README.md`. This is the part that cannot
be generated: a verifier that flakes turns the gate into noise, and every number in the
cockpit is downstream of it.

## 7. Read the sandbox policy

```sh
wikiskilld profile
```

That is the actual policy the next rollout runs under. See `profiles/README.md` for how to
read it and why the ordering matters.

## 8. Measure the baseline

```sh
wikiskilld baseline
```

The empty-skill validation score. It is the bar the first proposal has to beat, and the
number that makes any later improvement meaningful.

## 9. Run

```sh
wikiskilld run            # in the foreground, printing each phase
wikiskilld serve          # the localhost API and the schedules
wikiskilld install-agent  # a launchd user agent, so it survives logout
```

Logs go to `~/Library/Logs/wikiskill/` under launchd.

## 10. The cockpit

```sh
cd apps/cockpit/src-tauri && cargo run
```

It reads the config and token from the support directory (or `WIKISKILL_CONFIG`) and relays
everything through the daemon. Closing it does not stop a run.

## 11. Daily-coding capture (phase 4)

```sh
mkdir -p ~/.config/opencode/plugins
ln -s "$PWD/plugins/opencode-capture/index.js" ~/.config/opencode/plugins/wikiskill-capture.js
export WIKISKILL_API="http://127.0.0.1:8787"
export WIKISKILL_API_TOKEN="$(wikiskilld token)"
```

`plugins/`, plural — V2's directory. A file in V1's `plugin/` is not an error; it is a plugin
that silently never loads.

Then enable the nightly maintainer and weekly proposer in the cockpit's Schedule view. They
default to **off**. The weekly proposer never writes `skills/` on its own: daily work has no
validation split, so its proposal is rolled back and held for review.
