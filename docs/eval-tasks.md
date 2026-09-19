# The task set

60 checkable tasks: 30 train, 15 validation, 15 test, no id in more than one split. The
daemon refuses to run if a split is empty or an id repeats.

What each split is for:

| Split | Used by | Touched how often |
| --- | --- | --- |
| `train.jsonl` | rollouts whose traces the Wiki Maintainer reads | every iteration |
| `val.jsonl` | the gate | every iteration (once, or twice under the mean rule) |
| `test.jsonl` | the final number | **once per run**, at the end |

Test is scored once because it is the only estimate that has not been selected on. Scoring it
per iteration and picking the best turns it into a second validation split.

## One task

```json
{
  "id": "fix-flaky-ordering-test",
  "prompt": "One test fails intermittently. Fix the cause, not the test.",
  "workspace": "/abs/path/eval/workspaces/fix-flaky-ordering-test",
  "check_program": "/bin/sh",
  "check_args": ["-c", "cargo test 2>/dev/null | awk '/test result:/ {print $4\"/\"($4+$6)}' | tail -1"],
  "expect": "fraction",
  "tags": ["rust", "debugging"]
}
```

- `workspace` is copied fresh for every rollout, into the run's private work tree. The
  original is never mutated, so a rollout cannot contaminate a later one.
- `check_program` / `check_args` run under the same Seatbelt profile as rollouts. Deterministic,
  no network.

## The verifier contract

`expect` picks how stdout is read:

| `expect` | Contract | Score |
| --- | --- | --- |
| `fraction` | last stdout line is `passed/total` | `passed / total` |
| `score` | last stdout line is a number in 0–1 | that number, clamped |
| `exit-zero` | exit status | 1.0 or 0.0 |
| `stdout-contains` | a substring given in the task | 1.0 or 0.0 |

A check that crashes, times out, or prints something unparseable scores **0**, and the
rollout is recorded as failed rather than skipped — a split whose size changes between
iterations is not comparable across iterations, which is the whole point of the gate.

## Granularity, and why it matters

15 validation tasks means the smallest possible score change is 6.7 points. A single flaky
task is therefore indistinguishable from a real improvement, and the gate accepts on a
*strictly* higher score. Two consequences:

1. Prefer a check that cannot flake over a check that is easy to write. A test suite with a
   real race in it makes a bad verifier even when it makes a good task.
2. If flakes are unavoidable, set `loop.gate_rule` to the mean-of-two-runs rule. It costs a
   second validation pass per iteration and is recorded per entry in `skill-impact.md`, so a
   run's numbers always say which rule produced them.

## Choosing tasks

Tasks should be things the deploy model fails at *sometimes*. A task it always passes
contributes nothing to any gate decision; a task it never passes contributes nothing either,
and both cost a rollout every iteration. The empty-skill baseline (`wikiskilld baseline`) is
the measurement that tells you which of your 60 are in that dead zone.
