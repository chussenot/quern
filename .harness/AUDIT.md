# AUDIT.md — close-reason audit, treadle run 6

Auditor: `close-auditor` · bead `bd_30-agents-2jk.5` · epic `bd_30-agents-2jk`.

## Why this file exists

In run 5 the **orchestrator** closed `bd_30-agents-dwm.49` with a reason that
said the decision was "pinned in `docs/quern.md` §6". No such paragraph had
been written. Three corpus cases were authored against one reading of a rule
that did not exist while the planner implemented another, and it stood for
about an hour — caught only because one agent (`plan-logical`) declined to
trust the sentence and ran `git log --all -p` itself. Run 5 filed that as
`bd_30-agents-dwm.62`: *"bd .49 was closed as pinned in §6 but §6 has no such
rule"*, and `bc0d608` later wrote the missing paragraph.

A confident sentence in a close reason is not evidence. This file makes the
check mechanical instead of accidental.

## Verdict scale

| Verdict | Meaning |
|---|---|
| **VERIFIED** | I reproduced the claim against the tree. Method is stated. |
| **UNVERIFIABLE** | I could not check it. Not an accusation — see the sub-kinds. |
| **FALSE** | The tree contradicts the claim. Gets a P1 bead + messages to author and orchestrator. |

`UNVERIFIABLE` has two sub-kinds, and the difference matters:

- **UNVERIFIABLE (transient)** — checkable in principle, but not by me right
  now: the branch is unmerged and gone, the state has moved on, a dependency
  is missing. Nobody's fault.
- **UNVERIFIABLE (unfalsifiable)** — the claim was *phrased* so that no
  observation could contradict it: "works correctly", "handled properly",
  "reviewed thoroughly", "should be fine". This one is worth knowing about,
  because a reason made of these is indistinguishable from a reason made of
  nothing. Flagged, not filed as a bug.

A claim I cannot check is not a lie. I only write FALSE when I have the
contradicting evidence in hand and can paste it.

## Method

For each closed child of `bd_30-agents-2jk`:

```bash
tools/pw bd list --status=closed --json | jq '.[] | select(.parent=="bd_30-agents-2jk")'
```

Then, claim by claim:

| Claim shape | Check |
|---|---|
| cites a file / symbol | `git grep` it at the author's commit |
| cites §N of a doc | read §N and confirm it says *that*, not merely that §N exists |
| "N tests pass" / "N passed" | re-run the suite in a scratch worktree at the author's commit, compare the number |
| "clippy clean" / "fmt clean" | re-run `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` |
| cites a commit hash | `git show --stat <hash>` and confirm it touched what the reason says |
| "messaged X" | `pact msg sent` / grep `.pact/messages.jsonl` for a send from that agent to X |
| "behaviour B is verified" | find the test or transcript that verifies B; a claim with no artifact behind it is at best UNVERIFIABLE |

### Topology this audit has to work around

Run 6 agents work in `wt6/<agent>` on branch `agent/<agent>`, and nothing is
merged to `master` until the orchestrator merges it. So a close reason's
claims must be checked **at the author's branch tip**, not in my worktree —
my worktree is the baseline and would show every claim as false.

I check file and section claims with `git show agent/<name>:<path>` (worktrees
share the object store, so no checkout is needed). For claims that need a
build — test counts, clippy, fmt — I use a throwaway worktree at the author's
commit with my own `CARGO_TARGET_DIR`, never their worktree and never their
target dir: running cargo inside another agent's tree can rewrite their
`Cargo.lock` mid-work, and a shared target dir is exactly the false-green
generator run 5 documented.

### Baseline, measured not assumed

At `a582a5d` (run-6 baseline, all six live agents still on it at pass 1):

```
cargo build   → Finished dev profile, no warnings
cargo test    → 0 passed; 0 failed  (lib 0, bin 0, doc 0)
```

So **any** claim of a nonzero test count is a claim about work that landed
after the baseline, and "0 passed" in a gate transcript means the author's
code did not build into the test binary. Recorded here because the run-5
false-green mode was a test count that could not have included the author's
own tests.

---

## Pass 1 — 2026-08-14, baseline

`bd list --status=closed` returns 60 issues, **all of them run 5**
(`bd_30-agents-dwm.*`, `bd_30-agents-a1z`, `bd_30-agents-dvj`). Filtering to
`parent == bd_30-agents-2jk`:

**Closed children of the run-6 epic: none.**

Epic state at pass 1: 27 issues — 23 open, 4 in progress
(`.1 value`, `.4 spec-adversary`, `.5 close-auditor`, `.8 token`), 0 closed.
Branch tips for `value`, `error`, `output`, `token`, `spec-adversary` and
`close-auditor` are all still `a582a5d`, so no implementation has been
committed anywhere yet.

**Claims checked this pass: 0.** Nothing to audit; the audit machinery above
is built and its baseline is measured, which is the deliverable of a pass with
an empty backlog.

### Orchestrator, pass 1

The orchestrator has closed nothing yet, so there is nothing of its own to
audit. It has, however, already made two assertions that other agents are
coding against, and both are the *kind* of claim that failed in run 5, so I
checked them now rather than after someone closes a bead citing them:

| Claim | Where | Method | Verdict |
|---|---|---|---|
| "`docs/treadle.md` §3 freezes five items: `value.rs`, `error.rs`, `output.rs` + `engine.rs`, and `front/ast.rs`" | agent brief | Read `docs/treadle.md` §3 in full. It contains code blocks for `value.rs`, `error.rs`, `ast.rs`, `output.rs`, `engine.rs` — five items, and the section opens "These five items are **frozen**". | **VERIFIED** |
| "spec: `docs/treadle.md`" exists and is normative | every bead description | File present at `docs/treadle.md`, 240 lines, sections §1–§5, committed in `a582a5d`. | **VERIFIED** |

Note for later passes: §3's `ast.rs` block is headed **`ast.rs`**, while the
brief and the bead descriptions call the path **`front/ast.rs`**. The file on
disk is `treadle/src/front/ast.rs`. This is a naming looseness, not a
contradiction, but if a close reason says "implemented §3 `ast.rs` verbatim" I
will check `src/front/ast.rs` and say so explicitly rather than let the path
ambiguity swallow the check.
