# Security boundaries

The threats this design actually takes seriously, and where each one is stopped. Every claim
below is enforced in code with a test, not just a convention.

## 1. A prompt-injected rollout

A rollout runs untrusted text: task workspaces, tool output, web content. Assume it can be
made to try anything the agent can do.

- **Seatbelt is the boundary.** Rollouts and verifier runs go through
  `/usr/bin/sandbox-exec` with a generated profile: `(deny default)`, the run's own work tree
  writable, temp writable, the system read-only. OpenCode's permission rules are a second
  layer only — bash redirection can bypass its `external_directory` rules.
- **The vault is denied to rollouts**, in a deny block placed after the allows, because SBPL's
  last matching rule wins. The inference agent must never read `wiki/`: if it could, a skill's
  measured effect would be confounded by the agent having read the notes behind it.
- **Only rollout-scoped keys reach `opencode serve`.** The environment is cleared and rebuilt
  with `FIREWORKS_API_KEY` alone — not the curator credentials, not the Jev key.
- **No isolation must be declared.** The daemon refuses to start a run with the `NoIsolation`
  backend unless `sandbox.disabled` is explicitly true, and warns whenever it is.

## 2. A prompt-injected curator

The Wiki Maintainer and Skill Proposer are the only things that see the wiki, and they write
to it. They are native Rust agents with exactly four tools — read, list, str_replace, create —
and no shell, no network, no fetch.

- Path rules are in Rust, not in the agent's prompt or the executor's config. Each area
  (`wiki/` for the maintainer, `skills/` for the proposer) resolves through
  `Vault::resolve_in`, which rejects `..`, absolute paths, and **symlinks planted inside the
  vault** — the resolved parent is canonicalised and checked against the area root.
- The proposer may touch exactly one skill folder per iteration. A diff that touches two is
  rejected **unscored**, before a validation run is spent on it.
- `raw/` is daemon-only, and raw notes are written `0444`. History is not editable, so a
  curator cannot rewrite the evidence a gate decision was based on.

## 3. Credential exfiltration

- Keys live in the macOS Keychain and are read once at startup into the **daemon's** process
  environment. Nothing returns them: `load_into_env` deliberately reports only which keys were
  found, so a value cannot end up in a log line or an API response by accident.
- **Redaction runs before any vault write, any git commit, and any Jev call.** Rule-based
  patterns plus a high-entropy token filter; the counts are recorded in the note's frontmatter
  so a reader can see that redaction happened and how much it caught.
- The webview never holds a credential, including the daemon's own bearer token. The cockpit's
  Rust side holds the token and relays paths the webview names; `/v1/config` strips
  `daemon.token` from what it returns.

## 4. Anything local talking to the daemon

Loopback is not an authorisation boundary: any process on the machine — including a page in a
browser — can reach `127.0.0.1`. These endpoints start runs and write the vault.

- A bearer token is required on every endpoint except `/health`, compared in constant time.
- The token file is `0600`, generated on first run.
- `/v1/note` reads the vault through `Vault::resolve_in`, so a traversal is refused rather
  than sanitised.

## 5. Losing work, or keeping work that should have been dropped

- Run state is written atomically (temp file, then rename) under
  `~/Library/Application Support/wikiskill/`, so a crash mid-write cannot leave the cockpit
  reading half a run. A run left `running` by a crash is reconciled to `failed` at startup.
- On a rejected proposal, `skills/` is rolled back but the wiki and raw notes are kept. The
  order matters: the wiki commit happens **after** `skills/` is dropped from the index, or the
  commit would preserve the rejected proposal and the rollback would restore it. There is a
  regression test for exactly that.
- Nothing reaches `skills/` unless the gate accepted it or a human did. The weekly proposer,
  which has no validation split to gate against, captures its diff, rolls back, and waits.
