#!/usr/bin/env python3
"""Seed a *smoke* task set into a vault's eval/ splits.

This is not the real task set. It exists to exercise the harness end to end — rollouts,
verifiers, the gate arithmetic, the curators, the commit — on tasks that run in under a second
and need no network. Replace it with `docs/eval-tasks.md`'s 30/15/15 before any number here
means anything about a model.

Two deliberate choices:

- Every grader is embedded in `check_args`, not written into the workspace. The workspace is
  copied into the rollout's work tree and the agent can edit anything it can see, so a grader on
  disk is a grader the agent can rewrite to pass.
- Each task is a pure function with a stub signature, so a wrong answer is wrong for a readable
  reason rather than a crash in someone's framework.

The val split carries a few deliberately fiddly tasks. The easy ones were all solved on the
first attempt, which scored the baseline at 1.0 — and a saturated validation split disables the
thing this harness exists to measure: the gate accepts only on a *strictly* higher score, and
the loop stops early at 100%. So a smoke set that is entirely easy cannot exercise the accept
path at all. These are not hard in the sense of being clever; they are specified tightly enough
that the usual near-miss implementation fails a case.
"""

import json
import pathlib
import sys

TASKS = [
    # id, split, one-line ask, stub, [(call, expected), ...]
    ("rotate-a-list", "train", "Implement rotate(items, k): return items rotated left by k. k may exceed len(items), and k may be negative (rotate right). An empty list rotates to an empty list.",
     "def rotate(items, k):\n    raise NotImplementedError\n",
     [("rotate([1,2,3,4,5], 2)", [3,4,5,1,2]), ("rotate([1,2,3], 7)", [2,3,1]),
      ("rotate([1,2,3], -1)", [3,1,2]), ("rotate([], 3)", [])]),
    ("balanced-brackets", "train", "Implement is_balanced(text): True if (), [] and {} in text are correctly nested and closed. Characters other than brackets are ignored.",
     "def is_balanced(text):\n    raise NotImplementedError\n",
     [("is_balanced('a(b[c]{d})')", True), ("is_balanced('([)]')", False),
      ("is_balanced('(')", False), ("is_balanced('')", True)]),
    ("roman-numerals", "train", "Implement to_roman(n) for 1 <= n <= 3999, using the standard subtractive forms (IV, IX, XL, CM).",
     "def to_roman(n):\n    raise NotImplementedError\n",
     [("to_roman(4)", "IV"), ("to_roman(1994)", "MCMXCIV"),
      ("to_roman(3999)", "MMMCMXCIX"), ("to_roman(40)", "XL")]),
    ("most-common-word", "train", "Implement top_word(text): the most frequent word, lowercased, words being runs of letters. On a tie return the one that appears first. Empty text returns None.",
     "def top_word(text):\n    raise NotImplementedError\n",
     [("top_word('the cat the dog')", "the"), ("top_word('One one TWO two')", "one"),
      ("top_word('')", None), ("top_word('a, b. a!')", "a")]),

    ("flatten-nested", "val", "Implement flatten(value): fully flatten arbitrarily nested lists and tuples into one list. Strings are values, not sequences.",
     "def flatten(value):\n    raise NotImplementedError\n",
     [("flatten([1,[2,[3,[4]]]])", [1,2,3,4]), ("flatten([])", []),
      ("flatten(['ab',['cd']])", ["ab","cd"]), ("flatten([1,(2,3),[[4]]])", [1,2,3,4])]),
    ("dedupe-in-order", "val", "Implement dedupe(items): drop later duplicates, keeping first-seen order. Items may be unhashable.",
     "def dedupe(items):\n    raise NotImplementedError\n",
     [("dedupe([3,1,3,2,1])", [3,1,2]), ("dedupe([])", []),
      ("dedupe([[1],[1],[2]])", [[1],[2]]), ("dedupe(['a','A','a'])", ["a","A"])]),

    ("semver-compare", "val", "Implement compare(a, b): -1, 0 or 1 ordering two semantic versions. Compare major/minor/patch numerically. A version with a prerelease suffix (after '-') precedes the same version without one. Prerelease identifiers are compared dot-separated, left to right: numeric identifiers compare numerically and rank below alphanumeric ones; a shorter run of identifiers precedes a longer one when all preceding identifiers are equal. Build metadata (after '+') is ignored entirely.",
     "def compare(a, b):\n    raise NotImplementedError\n",
     [("compare('1.2.3', '1.2.10')", -1), ("compare('1.0.0-alpha', '1.0.0')", -1),
      ("compare('1.0.0-alpha.1', '1.0.0-alpha.beta')", -1),
      ("compare('1.0.0-alpha', '1.0.0-alpha.1')", -1),
      ("compare('1.0.0+build.9', '1.0.0+build.1')", 0),
      ("compare('2.0.0', '2.0.0')", 0)]),
    # Expectations travel as JSON, which has no tuple, so every sequence arrives back as a list.
    # The prompt therefore asks for lists: a task whose grader demands a type its own expectations
    # cannot express is unpassable, and would read as the model failing.
    ("merge-intervals", "val", "Implement merge(intervals): each interval is a [start, end, closed_end] list with start <= end and closed_end a bool saying whether `end` itself is included. Merge intervals that overlap or touch, where touching means one ends exactly where the next begins AND that end is closed. A half-open interval ending where the next starts does not merge. Return a list of [start, end, closed_end] lists, sorted by start.",
     "def merge(intervals):\n    raise NotImplementedError\n",
     [("merge([[1,3,True],[3,5,True]])", [[1,5,True]]),
      ("merge([[1,3,False],[3,5,True]])", [[1,3,False],[3,5,True]]),
      ("merge([[5,8,True],[1,2,True]])", [[1,2,True],[5,8,True]]),
      ("merge([[1,10,False],[2,4,True]])", [[1,10,False]]),
      ("merge([])", []),
      ("merge([[1,4,False],[2,4,True]])", [[1,4,True]])]),
    ("wrap-text", "val", "Implement wrap(text, width): greedily break text into lines of at most `width` characters. Split on single spaces; runs of whitespace collapse to one space and leading/trailing whitespace is dropped. A word longer than width goes on its own line, unbroken. Return a list of lines; empty or whitespace-only text returns an empty list.",
     "def wrap(text, width):\n    raise NotImplementedError\n",
     [("wrap('the quick brown fox', 9)", ["the quick","brown fox"]),
      ("wrap('  a   b  ', 3)", ["a b"]),
      ("wrap('antidisestablishmentarian x', 5)", ["antidisestablishmentarian","x"]),
      ("wrap('   ', 4)", []),
      ("wrap('aa bb cc', 5)", ["aa bb","cc"]),
      ("wrap('a b', 1)", ["a","b"])]),

    ("chunk-a-list", "test", "Implement chunk(items, size): split into consecutive lists of length size, the last possibly shorter. size < 1 raises ValueError.",
     "def chunk(items, size):\n    raise NotImplementedError\n",
     [("chunk([1,2,3,4,5], 2)", [[1,2],[3,4],[5]]), ("chunk([], 3)", []),
      ("chunk([1,2,3], 3)", [[1,2,3]]), ("_raises(lambda: chunk([1], 0), ValueError)", True)]),
    ("caesar-shift", "test", "Implement caesar(text, shift): shift letters by `shift`, wrapping within case, leaving everything else alone. Negative shifts decode.",
     "def caesar(text, shift):\n    raise NotImplementedError\n",
     [("caesar('abc', 1)", "bcd"), ("caesar('Zz', 1)", "Aa"),
      ("caesar('a-b', 0)", "a-b"), ("caesar('bcd', -1)", "abc")]),
]

GRADER = r'''
import json, sys
sys.path.insert(0, ".")
def _raises(fn, kind):
    try:
        fn()
    except kind:
        return True
    except Exception:
        return False
    return False
cases = json.loads(CASES)
passed = 0
try:
    from solution import *
except Exception as e:
    print("import failed: %s" % e, file=sys.stderr)
    print("0/%d" % len(cases))
    raise SystemExit(0)
for call, want in cases:
    try:
        got = eval(call)
    except Exception as e:
        print("%s raised %r" % (call, e), file=sys.stderr)
        continue
    if got == want and type(got) is type(want):
        passed += 1
    else:
        print("%s -> %r, wanted %r" % (call, got, want), file=sys.stderr)
print("%d/%d" % (passed, len(cases)))
'''


def main(vault: pathlib.Path) -> None:
    workspaces = vault / "eval" / "workspaces"
    splits: dict[str, list[dict]] = {"train": [], "val": [], "test": []}

    for task_id, split, ask, stub, cases in TASKS:
        work = workspaces / task_id
        work.mkdir(parents=True, exist_ok=True)
        (work / "solution.py").write_text(stub)
        (work / "README.md").write_text(f"# {task_id}\n\n{ask}\n\nEdit `solution.py`. Nothing else is read.\n")

        # The grader is a program in the task, not a file in the workspace.
        program = f"CASES = {json.dumps(json.dumps(cases))}\n{GRADER}"
        splits[split].append({
            "id": task_id,
            "prompt": f"{ask}\n\nEdit solution.py in the working directory. Do not add dependencies.",
            "workspace": str(work),
            "check_program": "/usr/bin/python3",
            "check_args": ["-c", program],
            "expect": {"kind": "fraction"},
            "tags": ["python", "smoke"],
        })

    for name, tasks in splits.items():
        path = vault / "eval" / f"{name}.jsonl"
        path.write_text("".join(json.dumps(t) + "\n" for t in tasks))
        print(f"{path}: {len(tasks)} task(s)")


if __name__ == "__main__":
    main(pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else pathlib.Path.home() / "wikiskill-demo"))
