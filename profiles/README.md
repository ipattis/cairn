# Sandbox profiles

The real security boundary for rollouts and verifier runs is macOS Seatbelt, invoked as
`/usr/bin/sandbox-exec` with a profile the daemon generates. OpenCode's own permission rules
are a second layer only: bash redirection can slip past its `external_directory` rules, so
path enforcement that matters lives either in Seatbelt or in Rust.

`rollout.sbpl.example` is a copy of a generated profile with the machine-specific paths
replaced, kept here so the policy can be read and reviewed in a diff. It is **not** loaded by
anything — the daemon writes the live profile into
`~/Library/Application Support/wikiskill/profiles/` at run time from
`crates/wikiskill-core/src/sandbox.rs`, and

```sh
wikiskilld profile
```

prints exactly what the next rollout will run under. Read that, not this file, before running
anything for the first time.

## Reading it

SBPL's rule is **last matching rule wins**, which is why ordering in the generated profile is
load-bearing:

1. `(deny default)` — nothing is permitted that is not named.
2. Allows: the system and toolchain read-only, the per-run work tree read-write, temp
   directories read-write.
3. The vault deny block, **last** among the file rules. The inference agent must never read
   `wiki/` — if it could, a skill's measured effect would be confounded by the agent having
   read the notes the skill was derived from, and the gate's numbers would mean nothing.

Network is allowed only because rollouts call a hosted model; `sandbox.allow_network_for_rollouts`
turns it off, and a run against a local provider should have it off.

## Changing it

`sandbox.disabled = true` in the config runs rollouts with no isolation at all. The daemon
refuses to start a run with a `NoIsolation` backend unless that flag is explicitly set, and
logs a warning every time it is — it exists for CI containers, where Seatbelt is unavailable,
not for convenience on a real machine.
