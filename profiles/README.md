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

## Four grants that look like padding and are not

Each of these was found by the executor dying in a way that pointed anywhere but the policy.
They are listed here so nobody tidies them away:

- `(literal "/")` — the root directory itself, which `(subpath "/usr")` does not cover. Without
  it dyld cannot start **any** binary: the process execs and takes SIGABRT before it can write
  to stderr, so it reads as the executor crashing on its own. It discloses only the names of
  the top-level directories.
- `(subpath "/private/var/db/timezone")` — the executor resolves the local zone during startup
  and dies of SIGTRAP, again with an empty stderr, when it cannot.
- The temp directories appear in **both** the readable and the writable set. A process that can
  write a temp file but not read it back is broken in a way that surfaces as
  "An unknown error occurred (Unexpected)".
- `(allow network-inbound)` — binding a port and accepting a connection on it are different
  operations. The executor is an HTTP server, so with bind alone it starts, then fails its
  first accept and reports a bare `ServeError`.

A useful way to bisect the next one: a profile with `(allow default)` appended runs — last rule
wins — so the missing grant can be found by tightening from there rather than guessing at
operation names. `(deny default)` is silent, so nothing appears in `log show` to help.

## Changing it

`sandbox.disabled = true` in the config runs rollouts with no isolation at all. The daemon
refuses to start a run with a `NoIsolation` backend unless that flag is explicitly set, and
logs a warning every time it is — it exists for CI containers, where Seatbelt is unavailable,
not for convenience on a real machine.
