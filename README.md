# WikiSkill harness

A macOS implementation of the WikiSkill loop: an agent runs tasks, a curator folds what
happened into a wiki, a second curator proposes one skill change, and the change is kept only
if it measurably helps. Skills that survive are the ones that beat a validation score; the
wiki is kept either way.

Three parts:

| Part | What it is | Where |
| --- | --- | --- |
| `wikiskilld` | The daemon. Owns the vault, the executor, and the runs. | `crates/wikiskilld` |
| `wikiskill-core` | The loop, the gate, the sandbox, the curators, the providers. | `crates/wikiskill-core` |
| Cockpit | A Tauri window that relays through the daemon. | `apps/cockpit` |

The executor is **OpenCode V2, pinned to 2.0.x** (`opencode2`), behind an `Executor` trait. It
runs both the rollouts and the user's daily coding.

See [`docs/setup.md`](docs/setup.md) to install it, [`docs/eval-tasks.md`](docs/eval-tasks.md)
for the task set that everything is measured against, and
[`docs/security.md`](docs/security.md) for what is isolated from what.

## The loop, once per iteration

1. Run the **train** split on the deploy model. Score every rollout with a deterministic
   verifier. Condense each trace into `raw/iter-NNN/`, redacted, read-only.
2. **Wiki Maintainer** reads those traces and patches `wiki/`. It is the only role that sees
   the wiki.
3. **Skill Proposer** reads `wiki/patterns/` and writes one skill diff into `skills/`.
4. If the diff touched more than one skill folder, reject it **unscored** — no validation run
   is spent on a proposal that cannot be attributed.
5. Otherwise run the **validation** split. Accept only if the score is *strictly* higher than
   the best so far. On accept, commit. On reject, `git restore skills/` — and commit the wiki
   and traces anyway.
6. Append one entry to `wiki/skill-impact.md` either way.

The **test** split is scored once, at the end of a run. Stop early at 100% validation.

Two invariants the implementation is built around:

- **Only `skills/` feeds back into rollouts.** The wiki is never rolled back, and the
  inference agent is denied read access to it by the sandbox profile — otherwise a skill's
  measured effect would be confounded by the agent having read the notes behind it.
- **The best score starts at the empty-skill baseline.** `wikiskilld baseline` measures it. A
  first proposal that ties the no-skill score is a rejection.

## Full skill injection

Every active `SKILL.md` is concatenated into `agents/wiki-inference.md` before each
iteration's rollouts, byte-identical across all of them so prompt caching absorbs it. The
native `skill` tool is denied, so skill *selection* is never a variable in a measurement —
the alternative would confound "the skill was unhelpful" with "the model didn't load it".

## Layout

```
crates/wikiskill-core     the loop, gate, sandbox, curators, providers, redaction
crates/wikiskilld         daemon: CLI, localhost API, launchd agent, schedules, capture
apps/cockpit              Tauri cockpit (its own cargo workspace)
plugins/opencode-capture  OpenCode plugin: posts finished daily sessions to the daemon
profiles/                 a readable copy of the generated Seatbelt policy
docs/                     setup, task set, security
```

The vault the daemon writes:

```
raw/iter-NNN/<task>.md    condensed trace, redacted, 0444
raw/_jsonl/               full transcripts, redacted, 0444
raw/daily/<date>/         captured daily-coding sessions
wiki/index.md             static catalog        (maintainer-owned)
wiki/logs.md              append-only           (maintainer-owned)
wiki/skill-impact.md      append-only           (daemon-owned)
wiki/patterns/*.md        one pattern per note
skills/<name>/SKILL.md    what the agent executes
skills/<name>/PURPOSE.md  [[links]] to the patterns that motivated it
eval/{train,val,test}.jsonl
```

It is an Obsidian folder and a git repo at the same time: the wiki is meant to be read by a
person, and every accepted change is a commit that can be reverted from the cockpit.

## State of the build

172 tests pass with no network and no credentials (`cargo test --workspace`), and the cockpit
compiles (`cd apps/cockpit/src-tauri && cargo build`). Bedrock's own runtime is behind a
non-default feature: `cargo test -p wikiskill-core --features bedrock`.

Live rollouts have been run: `wikiskilld baseline` drives real sandboxed `opencode serve`
rollouts against a Fireworks deploy model and returns a validation score with token and cache
accounting. Bedrock Converse has been exercised live too — that is how the inference-profile id
requirement and the newer Claude models' rejection of `temperature` were found. Still
unexercised against real services: Jev, and the launchd agent under a real login session.
Everything around them is covered by mocks — `MockExecutor`, `ScriptedClient`, `NoIsolation` — so the
control flow, the gate arithmetic, the rollback ordering and the redaction are all tested; the
provider wire formats are tested against captured response bodies rather than live services.

Jev ships in **shadow mode** by default: its answers are logged and never acted on. The repeat
check warns the proposer; it never gates. Occurrence counting stays in Rust, because counting
is the thing a model is least reliable at.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
