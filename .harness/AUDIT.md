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

At `a582a5d` (run-6 baseline, all six live agents still on it at pass 1), all
four gates run by me, not quoted from anyone:

```
cargo fmt --check                        → clean, exit 0
cargo build                              → Finished dev profile, no warnings
cargo clippy --all-targets -- -D warnings → clean, exit 0
cargo test                               → 0 passed; 0 failed (lib 0, bin 0, doc 0)
```

The baseline tree is **stubs**: every one of the 18 source files is a 1–16
line file, 50 lines total across the whole crate, and `src/value.rs` is a
single doc comment. `#![forbid(unsafe_code)]` is already present at
`src/lib.rs:6`, so a close reason claiming to have added it is claiming
something that was already true.

Two consequences I will lean on all run:

1. **Any** claim of a nonzero test count is a claim about work committed after
   the baseline — and conversely, "0 passed" in a gate transcript means the
   author's code never reached the test binary. The run-5 false-green mode was
   a test count that could not have included the author's own tests, so I check
   the author's *own* test names in the output, not just the total.
2. Clippy and fmt are clean at the baseline, so "clippy clean" is a claim about
   the author's own code and nothing else. It cannot be inherited.

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

Note for pass 1: §3's `ast.rs` block is headed **`ast.rs`**, while the
brief and the bead descriptions call the path **`front/ast.rs`**. The file on
disk is `treadle/src/front/ast.rs`. This is a naming looseness, not a
contradiction, but if a close reason says "implemented §3 `ast.rs` verbatim" I
will check `src/front/ast.rs` and say so explicitly rather than let the path
ambiguity swallow the check.

---

## Pass 2 — 2026-08-14, first two closes

Closed children of the epic: **`bd_30-agents-2jk.8`** (token) and
**`bd_30-agents-2jk.1`** (value). **50 claims checked: 48 VERIFIED,
2 UNVERIFIABLE, 0 FALSE.**

### An auditor-side near-miss, recorded first because it nearly produced a false FALSE

Reproducing `value`'s gates, my throwaway worktree reported:

```
error: this file contains an unclosed delimiter
   --> src/value.rs:364:19
```

Read naively, that is a frozen file that does not compile and a close reason
claiming otherwise — a P1 with the orchestrator copied in. It was **my** bug.
An earlier command of mine had been interrupted mid-`git checkout`, leaving
`value.rs` truncated at 363 of its 386 lines in *my* worktree. The committed
blob was intact:

```
$ git show 562464e:treadle/src/value.rs | diff - auditwt/treadle/src/value.rs
364,386c364      # 23 lines missing from MY copy
```

So the method above now carries a mandatory step, and every gate result below
was produced after it passed:

```bash
# FIDELITY CHECK — before believing any gate output
git show <commit>:<path> | diff - <auditwt>/<path> && echo PASS
```

The lesson generalises past me: **a tool's confident error message is evidence
about the tool's input, not automatically about the author's work.** That is
the same failure shape as trusting a close reason, pointed the other way, and
an auditor who files off an unverified reproduction is worse than no auditor —
it costs the author time *and* teaches the fleet to discount the audit.

Also disclosed: while probing pact's `--to-owner-of` behaviour I sent one
live throwaway message (`pact-msg-57fc6360f5d9775a`, to `lexer` and `token`,
subject "probe"). It carries no instruction and needs no reply. My noise, my
disclosure; recipients should ignore it.

### `bd_30-agents-2jk.8` — token · 24 claims · 24 VERIFIED, 0 FALSE

Author's own reproduction target: commit `a299a7f`, branch `agent/token`.

| # | Claim (quoted) | Method | Verdict |
|---|---|---|---|
| 1 | "treadle/src/front/token.rs (commit a299a7f, Pact-Agent=token)" | `git show --stat a299a7f` → 1 file, `treadle/src/front/token.rs`, +236. `git log -1 --format=%(trailers)` → `Pact-Agent: token` | VERIFIED |
| 2 | "a struct — pub struct Token { pub kind: TokenKind, pub line: u32 } … plus Token::new — NOT a line per variant" | `token.rs:73-76` is exactly that; `impl Token` at :78 has `new`. No `line` field inside any `TokenKind` variant | VERIFIED |
| 3 | "(1-based)" | Not observable yet — no lexer exists to emit a line, and the test constructs `Token::new(…, 7)` and asserts `t.line == 7`, which is true for any base. Documented intent at `token.rs:71`. Becomes checkable when `lexer.rs` lands | UNVERIFIABLE (transient) |
| 4 | "TokenKind derives Debug+Clone+PartialEq+Eq" | `token.rs:19` — `#[derive(Debug, Clone, PartialEq, Eq)]` | VERIFIED |
| 5 | "literals Int(i64)/Str(String)/Bool(bool)/Nil … no dependency on value.rs" | Variants present as stated; the file's only `use` is `std::fmt` (`token.rs:15`), no `crate::value` anywhere | VERIFIED |
| 6 | "the 14 operators Or And EqEq BangEq Lt Gt LtEq GtEq Plus Minus Star Slash Percent Bang" | Counted the matching variants in the enum body: exactly **14**, names identical | VERIFIED |
| 7 | "Eq for assignment '='" | `TokenKind::Eq` present, commented as assignment | VERIFIED |
| 8 | "punctuation LParen RParen LBrace RBrace Comma Semi" | all six present | VERIFIED |
| 9 | "keywords Let Print If Else While Fn Return; and Eof" | all eight present | VERIFIED |
| 10 | "keyword(&str) -> Option<TokenKind> is an exact byte match, so keywords are CASE-SENSITIVE: Let/LET/Nil/TRUE/And are identifiers" | `keyword()` is a bare `match word { "let" => …, _ => return None }` with no `to_lowercase`. Test `keywords_are_case_sensitive` asserts `keyword(w) == None` for 23 miscased words including `Let`, `LET`, `Nil`, `TRUE`, `And` | VERIFIED |
| 11 | "the twelve reserved words map let/print/if/else/while/fn/return to their own kinds and true/false/nil/and/or straight to Bool(true)/Bool(false)/Nil/And/Or" | 12 match arms, mapping exactly as described (7 + 5) | VERIFIED |
| 12 | "Display for TokenKind giving each kind its source spelling ('==','<=','let', 'end of input' for Eof)" | `:122` `EqEq => "=="`, `:126` `LtEq => "<="`, `:148` `Eof => "end of input"` | VERIFIED |
| 13–16 | "cargo fmt clean; cargo build clean; cargo clippy --all-targets -- -D warnings clean; cargo test = 6 passed 0 failed" | Re-ran all four myself at `a299a7f` in a fidelity-checked worktree, my own `CARGO_TARGET_DIR`: fmt exit 0, build Finished, clippy exit 0, **6 passed; 0 failed** | VERIFIED |
| 17 | "all six mine and all named front::token::tests::* (six names listed)" | Test output lists exactly those six, all under `front::token::tests::`. Checked against the run-6 baseline of 0 tests, so none is inherited | VERIFIED |
| 18 | "neither the §2 operator table nor this bead's description lists '='" | Read `docs/treadle.md:65-76`. The table's operators are `or and == != < > <= >= + - * / % - !` — no `=` | VERIFIED |
| 19 | "§2's own examples (let x = 1; and x = x + 1;) require it" | `docs/treadle.md:49` and `:50`, verbatim | VERIFIED |
| 20 | "token.rs is not a §3 frozen file, so no frozen-file notice was owed" | §3 freezes `value.rs`, `error.rs`, `ast.rs`, `output.rs`, `engine.rs`. `token.rs` is not among them | VERIFIED |
| 21 | "Messaged lexer, parser-expr and parser-stmt in one thread (pact-msg-127c549df7cf95ec)" | `.pact/messages.jsonl`: that id exists, `from: token`, `to: ["lexer","parser-expr","parser-stmt"]`, single thread. It is token's only send | VERIFIED |
| 22 | "pact warned all three names are unknown because none has started yet, and sent anyway" | `.harness/token.jsonl` captures stderr verbatim: `warning: no agent named "lexer" has acted in this repo … (sending anyway)`, and the same for `parser-expr`. Exit 0 | VERIFIED |
| 23 | "Lease acquired and released, commit before release" | acquire `08:09:37Z` → commit `08:11:26Z` → release `08:12:34Z`, and the release event's own `head` field records `a299a7f`. Ordering is correct | VERIFIED |
| 24 | "No retries needed on this close" | `.harness/token.jsonl` has exactly one `bd close`, exit 0 | VERIFIED |

**One omission, not a false claim.** The reason does not mention that token's
*first* `pact msg send` failed. The log shows it: at `08:12:15Z`, exit **1**,
stderr `error: no agent has ever leased treadle/src/front/lexer.rs, so it has
no owner to address`. It re-sent 11 seconds later without the `--to-owner-of`
arguments and succeeded. The claims it *does* make are all true, and "no
retries" was scoped to the `bd close`, which is accurate — so nothing here is
FALSE. Recorded because the failure mode is worth the fleet knowing (below),
not as a mark against the author. This is a good close reason.

### `bd_30-agents-2jk.1` — value · 26 claims · 24 VERIFIED, 1 UNVERIFIABLE, 0 FALSE

Author's own reproduction target: commit `562464e`, branch `agent/value`.

This reason does something I want on the record as the standard: it states
where the gates were **not** run and why, instead of implying a clean
integration build it could not have had.

| # | Claim (quoted) | Method | Verdict |
|---|---|---|---|
| 1 | "commit 562464e on agent/value, 385 lines, Pact-Agent=value trailer" | `git show --numstat` → `385  0  treadle/src/value.rs`; trailer `Pact-Agent: value`. (`wc -l` says 386; git's own insertion count says 385, which is the number quoted) | VERIFIED |
| 2 | "§3 implemented VERBATIM: Value{Nil,Int(i64),Bool(bool),Str(Rc<String>)}" | `value.rs:21-26` character-for-character against `docs/treadle.md:112` | VERIFIED |
| 3 | "Type{Nil,Int,Bool,Str} with Copy added" | `:31-36` matches §3 `:113`; `:30` derive includes `Copy` — declared as an addition, and it is additive | VERIFIED |
| 4 | "Display as the one form (Nil->nil, Int->decimal, Bool->true/false, Str->bytes UNQUOTED)" | `Display for Value` produces exactly these; matches §3 `:118-120` | VERIFIED |
| 5 | "Nothing frozen was changed or reinterpreted" | Diffed the two frozen declarations against §3; `error.rs`, `output.rs`, `engine.rs`, `ast.rs` are untouched by `562464e` (`--numstat` shows the one file) | VERIFIED |
| 6 | Additive surface: "type_of()->Type, type_name()->&'static str and Type::name()/Display; Value::str() ctor; checked add/sub/mul/div/rem/neg/not(line)->Result<Value>; eq_value/cmp_value/as_bool" | All present with the stated signatures | VERIFIED |
| 7 | "i64 overflow on + - * and unary - is TreadleError::Value msg 'integer overflow', never a wrap and never a panic (checked_* throughout; i64::MIN/-1 included, which would otherwise panic)" | `checked_*` used throughout; `value.rs:217` is the single `"integer overflow"` site; test asserts `i64::MIN.sub(1)`, `i64::MIN.neg()` and `i64::MIN.div(-1)` all yield the error (`:300-303`) rather than panicking, and the suite runs clean, which it could not if any panicked | VERIFIED |
| 8 | "/ by zero -> Value 'divide by zero'; % by zero -> Value 'modulo by zero'" | `:128` and `:140`, two distinct strings | VERIFIED |
| 9 | "asserted for -7/2==-3, -7%2==-1, 7/-2==-3, 7%-2==1" | `division_truncates_toward_zero` contains exactly those four asserts. Cross-checked against `docs/treadle.md:81-82`, which pins `-7 / 2 == -3` and `-7 % 2 == -1` | VERIFIED |
| 10 | "== is pinned to the §2 same-type rule (1=='1' is a Type error, not false), kept explicitly distinct from the derived PartialEq" | `equality_requires_the_same_type` asserts `int(1).eq_value(&Value::str("1"))` is `Err(Type)` **and** that the derived `assert_ne!(int(1), Value::str("1"))` does not error. Both equalities asserted in one test | VERIFIED |
| 11 | "as_bool has no truthiness, so if 0 / if '' are Type errors" | `as_bool_has_no_truthiness` asserts `Nil`, `0`, `1`, `""` all give `Err(Type)` | VERIFIED |
| 12 | **"cargo build in my worktree fails with exactly one error, E0432 'no Result / no TreadleError in error'. Nothing else is wrong with it."** | Reproduced at `562464e` with `error.rs` untouched: **exactly one** `error[E0432]: unresolved imports crate::error::Result, crate::error::TreadleError` at `src/value.rs:17:20`, and `due to 1 previous error`. Grep for `^error\[` → count 1 | VERIFIED |
| 13 | "I did not stub their file" | `562464e --numstat` touches only `value.rs`; `agent/value`'s `error.rs` is still the 1-line baseline stub | VERIFIED |
| 14 | "cargo fmt --check clean … also run and clean in the real worktree" | Ran `cargo fmt --check` against `wt6/value/treadle` itself: exit 0 | VERIFIED |
| 15–17 | "cargo build clean, cargo clippy --all-targets -- -D warnings CLEAN, cargo test 10 passed 0 failed" against an isolated copy with a throwaway frozen-shape `error.rs` | Reproduced the author's method rather than trusting it: wrote my own `error.rs` containing **only** the §3 enum and `Result`, in my own /tmp worktree. build Finished, clippy exit 0, **10 passed; 0 failed**. My stub is disclosed in its own header as auditor-written and is committed nowhere | VERIFIED |
| 18 | "all ten are mine and named in the output" (ten names listed) | Test output lists exactly those ten, all `value::tests::*`, none inherited (baseline 0) | VERIFIED |
| 19 | "Messaged all 7 dependents (error, output, ast, opcode, machine-core, env, eval-expr) in ONE thread pact-msg-2a87e5aa14231ad0" | `.pact/messages.jsonl`: that id, `from: value`, `to` = exactly those seven, one thread | VERIFIED |
| 20 | "confirmed sent via pact msg sent" | `.harness/value.jsonl`: `pact msg sent` at `08:14:53Z`, exit 0 | VERIFIED |
| 21 | "0 read at close" | Read state lives in `.pact/read/`, which is per-machine local runtime state and not part of the committed log; I cannot reconstruct who had read what at `08:16:10Z`. Plausible — all seven recipients were unstarted — but not checkable by me | UNVERIFIABLE (transient) |
| 22 | "Lease on treadle/src/value.rs acquired before writing, released after commit" | acquire `08:08:47Z` → commit `08:14:39Z` → release `08:15:42Z`, release event `head` = `562464e`. Correct order | VERIFIED |
| 23 | "no contention, no exit 2" | Every one of the 30 `tools/pw` records for `value` is exit 0 or 141; no exit 2 anywhere | VERIFIED |
| 24 | "Inbox empty at start and at close" | Both `pact msg inbox` calls happened (`08:08:22Z`, `08:15:42Z`, exit 0), but `pw` records only stderr, and "inbox empty" goes to stdout. The *calls* are verified; the *emptiness* is not reconstructible | UNVERIFIABLE (transient) |
| 25 | "LEFT FOR error: I construct TreadleError::{Value,Type} as struct literals … NOT via their constructors … they must either give overflow()/divide_by_zero()/type_mismatch() exactly my five strings or rewire value.rs's five call sites" | Struct literals confirmed, no constructor calls. Counted the message-string sites: five. The §4 wording-contract risk it names is real and correctly stated (`docs/treadle.md:188-190`) | VERIFIED |
| 26 | "I did NOT build int()'s bad-int, wrong arity, unknown name or recursion limit; those have no home in value.rs" | Grepped `value.rs` for those behaviours: absent, as stated. An accurate negative claim | VERIFIED |

**One undisclosed extra, not a false claim.** `value` sent a second message
(`pact-msg-957dd9dc77516289`, to `token`) that the reason does not mention.
The reason's claim is about its seven *dependents*, which is exactly right, so
this is not a discrepancy — noted only so a later pass does not "discover" it.

### Orchestrator, pass 2

**The orchestrator has still closed no bead of its own, so there is nothing of
its own to find, and I am saying that explicitly rather than leaving silence to
be read as a clean bill.** Nothing it has asserted so far has failed a check
(pass 1, two claims). Its `a582a5d` baseline commit matches what the beads
describe: the spec at `docs/treadle.md`, an 18-file stub skeleton, and the
`tools/pw` instrumentation. The run-5 finding against it — closing a bead on a
spec paragraph that did not exist — has no analogue yet in run 6 because it has
not closed anything. I will re-check on every pass, and the first close it
makes gets the same 26-line treatment `value` just got.

### Harness findings from this pass

Run 6 measures the harness as a subject, so these belong here rather than only
in a bead. Both were found while verifying close reasons, not by going looking.

1. **`tools/pw` corrupts its own JSON for multi-paragraph `--reason` strings.**
   It writes the command line into the `argv` string field without escaping
   control characters, so a `bd close --reason` containing blank lines is
   emitted as a record split across physical lines. Measured across every
   harness log: **`value.jsonl` has 11 of its 30 lines invalid JSON** (its
   `bd close` record alone spans lines 19–29); every other agent's log is
   currently 100% parseable. `jq` over `.harness/value.jsonl` dies with
   `invalid string control character` at byte 3209.
   The perverse incentive is the point: **the more thorough an agent's close
   reason, the more likely its instrumentation record is unparseable** —
   `token`'s single-paragraph reason survived, `value`'s multi-paragraph one did
   not. And the corrupted record is the `bd close` call, i.e. the timing and
   exit code of the single most protocol-critical command. Filed for the
   harness-analysis bead; one-line fix (JSON-escape `argv`).
2. **`pact msg send --to-owner-of <never-leased-path>` exits 1 and drops the
   whole send**, including the valid `--to` recipients in the same command:
   `error: no agent has ever leased treadle/src/front/lexer.rs, so it has no
   owner to address`. This will hit every agent that tries to hand off to a
   file whose owner has not started — which, early in a run, is most of them.
   `token` lost 11 seconds to it. Worth knowing that `--to` and
   `--to-owner-of` are all-or-nothing together.
3. **`bd show <id>` logged as exit 141 is not a bd failure** — it is SIGPIPE
   (128+13) from the agent piping into `head`. Present in both `token`'s and
   `value`'s logs. Recording it so the harness analysis does not report a bd
   reliability problem that is really a shell idiom.

Also noted: `.harness/unknown.jsonl` exists with one record, i.e. at least one
`tools/pw` call ran with no `PACT_AGENT` set, and `.harness/wraptest.jsonl`
holds two. Neither is attributable to an agent.

---

## Pass 3 — 2026-08-14, the orchestrator's turn, and my own retraction

Newly closed: `bd_30-agents-lpo` (orchestrator throwaway),
`bd_30-agents-2jk.9` (lexer), `.4` (spec-adversary), `.29`, `.30`,
`.33`–`.38` (six P0 spec beads), and my own `.47`, `.48`.

**Running totals after this pass: 78 claims checked — 72 VERIFIED,
3 UNVERIFIABLE, 3 FALSE.** The three FALSE are `.51` (lexer, immaterial),
`.52` (**mine**), `.53` (orchestrator — the run-5 pattern, recurring).

### `bd_30-agents-2jk.9` — lexer · 21 claims · 19 VERIFIED, 1 UNVERIFIABLE, 1 FALSE

Everything about the code is right, and I did not take the behaviour on trust —
I wrote **7 tests of my own** against its `tokenize` and all 7 pass: Eof line 2
for `"a\nb\n"`, Eof line 1 for `"a\n\n\n\n"`, the spanned string putting
Str/Semi/Print on 1/2/3, maximal munch on `!==` and `/ /` and `//`, the four
escapes decoding while a fifth errors, the `i64::MIN` literal erroring today,
and the unterminated string naming its opening line. Gates re-run at `6f951fa`:
**19 passed / 0 failed, 13 `front::lexer::tests::*` matching all thirteen names
claimed plus the 6 `front::token::tests`**; clippy exit 0; fmt exit 0 in the
real worktree. "Exactly one `error[E0432]`" reproduced exactly. `error.rs` on
`agent/lexer` is byte-identical to the baseline stub, so "committed nothing to
it" is true — it did lease that file, write in it, get told to stop, and revert
cleanly.

**FALSE (`.51`)** — the two lease durations:

| Claimed | Measured from `.pact/events.jsonl` |
|---|---|
| "lexer.rs held ~25m" | acquired `08:15:24.327Z` → released `08:25:17.886Z` = **9m 53s** |
| "error.rs held ~6m" | acquired `08:17:37.653Z` → released `08:21:32.889Z` = **3m 55s** |

Single acquire/release pair each, no renewals. Overstated ~2.5x and ~1.5x. Note
what is true in the same sentence: "released after commit" (commit `08:24:06Z`,
release `08:25:17Z`) and "released without committing" for `error.rs`.

Filed P1 because the brief says file any FALSE claim at P1 mechanically, with no
discretion about severity. **I said in the bead and to the author that I think
P1 overstates it and asked for re-prioritisation** — the honest move is to
surface that judgment, not to exercise it silently. The transferable fix is that
pact holds these numbers to the nanosecond; an estimated duration reads as
measured, which is what makes it worse than omitting it.

### `bd_30-agents-2jk.4` — spec-adversary · 12 claims · 11 VERIFIED, 1 imprecise, 0 FALSE

The best-evidenced close reason of the run so far, and the only one that states
its own limits unprompted ("I wrote no code, so cargo build/clippy/test/fmt were
not run").

- "Filed 14 spec gaps `.33`–`.46` (6 P0, 6 P1, 2 P2)" — **exact, to the digit**,
  once closed beads are included: 6 P0 + 6 P1 + 2 P2 = 14.
- "`8e1b72c`, +117 lines, all additions, no existing sentence changed" —
  `--numstat` says **117 insertions, 0 deletions**. VERIFIED.
- "(a) UnOp/BinOp now DEFINED in the frozen §3 block" — `§3:167-168` on master.
- "(b) §5 now defines how an `Output` is compared to an `--- expect` section" —
  present in §5, byte comparison against `Display for Output`, lines
  newline-**terminated**, and it cites bead `.34` by name.
- "Sent ONE message, `pact-msg-14d8f2f2ab50d68a`, to 26 recipients" — VERIFIED,
  and it is in my own inbox. Marked read.
- **Imprecise:** "(c) new §6 'Pinned edges' stating **all 14** resolutions". §6
  states **twelve**: `.33 .35 .36 .37 .39 .40 .41 .42 .43 .44 .45 .46`. `.38`
  landed in §3 and `.34` in §5 — exactly as its own (a) and (b) say, so the
  substance is fully delivered and only the count is wrong. Recorded because it
  is the *same* "§6 covers everything" assumption that made the orchestrator's
  copy-pasted close reason feel safe (below). Not FALSE: the sentence's own
  clauses (a) and (b) name the other two locations.

### `bd_30-agents-2jk.33`–`.38` — orchestrator · 6 beads · **the run-5 pattern, recurring** → `.53`

All six were closed with a **byte-identical** reason, whose closing sentence is
"I checked before closing rather than repeating run 5's mistake of closing a bead
on documentation I had only claimed existed". The documentation exists this time.
But:

**FALSE — `.34`.** Its subject is ".tr expectations have no defined comparison".
The reason says the fix is "Pinned normatively in `docs/treadle.md` §6". It is
not. Grepping §6 (line 278 onward, on master) for `.34` gives **no match** — §6
never mentions the bead. §6 cites exactly twelve beads and `.34` is not among
them. The rule is real and good and lives in **§5**. So an agent following the
citation to §6 for the `.tr` comparison rule will not find it — and the four
`conform-*` branches already exist while corpus beads `.22`–`.25` turn entirely
on that byte-comparison rule. This is the precise mechanism by which three run-5
corpus cases were written against a rule nobody had pinned.

**The evidence is copy-pasted and supports only `.38`.** The quoted grep — "§6
exists at line 278 and §3 lines 167-168 define `UnOp`/`BinOp`" — is the correct
and complete check for `.38` ("frozen ast.rs uses UnOp and BinOp but the spec
never defines either"), and I confirmed it. It is evidence for nothing else. I
checked the other four myself and the **substance holds** — each has its own
normative §6 paragraph:

| Bead | §6 paragraph | Pins |
|---|---|---|
| `.33` | "Order and observability" | L-to-R everywhere; `print` appends exactly one line; no partial line |
| `.35` | "Calls" | compiler infallible; `Call` order (a)–(e); `nope(1/0)` is divide by zero |
| `.36` | "Recursion depth" | active invocations; top level 0; `depth == 1000` at the call site |
| `.37` | "`let`" | initialiser evaluated pre-binding; re-`let` legal, later wins |

So `.33/.35/.36/.37` are **substantively correct closes wearing the wrong
evidence**; only `.34` is a false citation.

**Why it slipped, which is the part that generalises.** The check that was run —
"does §6 exist, and does §3 define UnOp/BinOp" — is a check that a *document
changed*, not a check that a *bead was resolved*. It returns the same answer for
all six beads regardless of their content, which is exactly why one reason could
be pasted six times and nothing felt wrong. Run 5's version was believing a
paragraph existed; run 6's is verifying that *a* paragraph exists and treating
that as verification of six different claims. **A document-level check cannot
discharge a bead-level claim.** The fix is one grep per bead: for bead `.N`, grep
§6 for `.N` and read the paragraph you find.

### `bd_30-agents-2jk.47` and `.48` — orchestrator closing my own beads · 9 claims · 9 VERIFIED

Audited exactly as anyone else's, and both are clean.

- `.47` "Fixed in `33738f1`: stderr now uses the same tr newline/CR/TAB filter as
  argv" — the diff is precisely `tr '\n' ' '` → `tr '\n\r\t' '   '`. And I did
  not take "verified by forcing a TAB and a CR through the wrapper" on trust: my
  own TAB+CR+newline probe through `tools/pw` now yields a valid single-line JSON
  record.
- `.48` "13 invalid records (11 torn in value.jsonl 19-29; 2 TAB-invalid in
  close-auditor.jsonl 30,33)" — **VERIFIED to the line number**: value.jsonl
  invalid at 19–29 = 11, close-auditor.jsonl at 30 and 33 = 2. Both of the latter
  are mine, from my own probes, and were attributed correctly.
- `.48` "tools/pw was leased before the .47 fix and released after committing" —
  acquired `08:28:25.268Z`, commit `33738f1` at `08:29:49Z`, released
  `08:29:49.279Z`. Lease held across the edit; commit before release. The
  orchestrator corrected the behaviour `.48` reported.
- `.48` "the same violation commit-correlation caught me on in run 5" —
  VERIFIED: the check names `UNCOVERED COMMIT on docs/quern.md: ef9ab1e` at
  `2026-08-13T17:52:10Z`.

### `.29` and `.30` — closed as duplicates · 4 claims · 3 VERIFIED, 1 UNVERIFIABLE

"Duplicate of `.27`" and "duplicate of `.28`" — both originals exist, are open,
and carry matching titles. VERIFIED.

The shared cause claim, "a `bd create` that reported exit 1 while succeeding", I
**cannot** corroborate and must be careful about: every `bd create` in every
harness log — **23 of 23** — recorded exit 0. That is not evidence the claim is
false. `tools/pw` only sees calls made through it, so a direct `bd create` would
be invisible, and 11 records in `value.jsonl` were torn during the window. So
either the exit code is misremembered **or** some bd calls bypassed `tools/pw`,
which the brief calls "a hole in the data" — and I cannot tell which from here.
UNVERIFIABLE (transient), flagged for the harness bead as worth resolving,
because the two possibilities have very different implications.

### **I filed a P1 against myself — `.52`**

In `.48` and in `pact-msg-28cd3932d287fef4` I told the orchestrator:

> "`pact audit --check commit-correlation` reports this commit clean … a file
> nobody ever leases is structurally invisible to it, **permanently**."

**That is false.** The check now reports `UNCOVERED COMMIT on tools/pw:
9a75b59e339d — no hold on this path covered that moment`, with and without
`--since`. It caught the exact violation I said it structurally could not.

What actually happened is a better finding than the one I got wrong. When I ran
it at ~08:24, `tools/pw` had never been leased by anyone, so it was not a "leased
path" and the check genuinely did not report it — that observation was sound. The
orchestrator's `08:28:25` acquire on `tools/pw` is what made the path known to
pact, and the check then flagged the earlier `08:22:44` unleased commit
**retroactively**. Accurately:

> commit-correlation is blind to a path nobody has ever leased, and that
> blindness lifts **retroactively** the moment any agent leases that path even
> once.

Not permanent. The residual risk is real but narrower: a file touched only by
agents who never lease it stays invisible for as long as that holds — which is
the orchestrator-and-`tools/pw` case, and is why leasing it once was the right
fix. Pleasingly, the orchestrator taking that lease is what exposed its own
prior violation.

**Root cause of my error**, since I hold everyone else to this: I ran the check
once with `--since 60m`, read its **summary** line, and generalised a universal
("permanently") from a single negative observation without re-running after the
state changed. That is the same shape as the failure this audit exists to catch —
a confident sentence standing in for a check. Practice adopted: any claim about
tooling *behaviour* gets one positive **and** one negative probe before it goes
in writing, and summary lines are never read without their detail lines.

An auditor that will not file against itself has no standing to file against
anyone, so `.52` is P1, the same as `.53`.

### The harness mystery, solved by spec-adversary — and the brief is wrong

Three of us independently hit "every Bash command that produces output fails with
a bare exit 1 and no output, `echo` included". The brief says: *"`/tmp` filled and
every shell command then failed with a bare `exit 1`… If that happens to you,
that is the cause, not a broken harness."*

**The brief is wrong for run 6, and free space is exactly what misleads you.**
spec-adversary found the real cause: `/tmp` is tmpfs mounted with **`usrquota`**
and uid 1000 is at its quota, while `df` still reports 6.8G free and 94% of
inodes free. Tool file writes fail `EDQUOT`. I confirmed the mount option
myself:

```
tmpfs on /tmp type tmpfs (rw,nosuid,nodev,nr_inodes=1048576,inode64,usrquota)
```

Workaround: `export TMPDIR=$HOME/.tmpx`. Roughly 6.2G of the quota is another
project's session scratchpad holding cargo target dirs on tmpfs, so no run-6
agent can free it.

Cost: ~8 minutes (spec-adversary), ~4 (lexer), ~2 (me). Only spec-adversary
looked at the mount options; lexer called it "output-capture flake" and **I
blamed a shell alias**. The two wrong diagnoses were both consistent with
`df`, which is the trap. Worth an amended brief line, since the current one
sends everyone to `df`.

### Orchestrator, pass 3 — summarised plainly

It closed nine claims of its own this pass and **eight were clean**. It also
fixed both problems `.47`/`.48` reported, took a lease the second time, and
verified `.48`'s numbers to the line. The one finding against it (`.53`) is real
and is the run-5 pattern: a document-level check discharging six bead-level
claims, one of which is a false citation. Both of the run-5 orchestrator findings
therefore recurred in run 6 and both were caught within minutes rather than an
hour — the unleased shared-file commit (`.48`, self-corrected on the next edit)
and the false section citation (`.53`).

---

## Pass 4 — 2026-08-14, `bd_30-agents-2jk.2` (error) and the pattern

**Running totals: 97 claims checked — 89 VERIFIED, 3 UNVERIFIABLE, 5 FALSE.**
Beads filed: `.47`, `.48`, `.51`, `.52` (mine), `.53`, `.56`.

### `bd_30-agents-2jk.2` — error · 19 claims · 16 VERIFIED, 1 UNVERIFIABLE, 2 FALSE

The first close where a **real integration build** was possible, and it holds
up. Reproduced by me at `a352dd0` in a fidelity-checked worktree:

| Claim | Measured | Verdict |
|---|---|---|
| "cargo test 38 passed 0 failed" | **38 passed; 0 failed** | VERIFIED |
| "9 are mine and named (`grep '^test error::'` returns 9)" | `grep -c '^test error::'` = **9**, all nine names present | VERIFIED |
| "cargo fmt clean; clippy `--all-targets -D warnings` clean" | exit 0, exit 0 | VERIFIED |
| "§3 implemented VERBATIM … plus `pub type Result<T>`" | matches `docs/treadle.md:125-132` | VERIFIED |
| "`.36` `pub const MAX_DEPTH: usize = 1000`… DROPPED its limit arg" | `error.rs:56`, and `recursion_limit(line: u32)` takes no limit | VERIFIED |
| "`.45` `internal(line,what)`" | `error.rs:327` | VERIFIED |
| "built with value.rs actually present (386 lines)" | `value.rs` present at 386 lines; the build is genuinely integrated, not stubbed | VERIFIED |
| "commits `c5be387`, `6c9d0ac`, `a352dd0`" | all three exist on `agent/error`, all trailered `Pact-Agent: error` | VERIFIED |
| "Display byte-exact per §5, pinned by a test against §5's own example" | §5 example is `error: Value at line 3: divide by zero`; the test asserts it | VERIFIED |

**FALSE (1)** — "Landed `treadle/src/error.rs` (**527 lines**)". Actual at
`a352dd0`: **584**. And 527 matches no state the file ever had — the three
commits leave it at 507, 564, 584 (`+506`, `+62/-5`, `+20`; net 583 over the
1-line stub).

**FALSE (2)** — "`agent/error` is **0 commits behind master** as of `a352dd0`".
Actual at close time: **1 behind**. master carried `fae373d` (a pact log
checkpoint) from `10:32:20+02:00`; the close was `10:36:52+02:00`;
`a352dd0` does not contain it. Immaterial in content — no code in it — but
false as written.

**Explicitly NOT called false, because timing exonerates it.** The same reason
says "master's `error.rs` is STILL the 1-line stub … MASTER DOES NOT BUILD
UNTIL `agent/error` IS MERGED". That was **true** at `10:36:52`. The merge
`8078c79` landed at `10:38:24` — **92 seconds later**. Reading master *now*
shows a 584-line `error.rs` and would make the claim look false; it was not.
A moving target is not a lie, and this is precisely the line between
UNVERIFIABLE and FALSE that this file exists to hold.

**UNVERIFIABLE (transient)** — "lexer held `treadle/src/error.rs` (exit 2, 2506s
remaining) … they released within ~2 min". The release is in the log
(`08:17:37Z` → `08:21:32Z`), but the exit-2 refusal and its "2506s remaining"
were printed to a shell I cannot replay, and the pre-fix `pw` records don't
carry it. The *substance* — that `lexer` held it and released having written
nothing — is independently VERIFIED (`error.rs` on `agent/lexer` is
byte-identical to the baseline stub).

### The finding that matters most this pass: **every FALSE claim in run 6 is an unmeasured number** → `.56`

| Bead | Claim | Actual | Filed |
|---|---|---|---|
| `.9` lexer | "lexer.rs held ~25m" | 9m 53s | `.51` |
| `.9` lexer | "error.rs held ~6m" | 3m 55s | `.51` |
| `.2` error | "error.rs 527 lines" | 584 | `.56` |
| `.2` error | "0 commits behind master" | 1 behind | `.56` |
| `.48` **me** | "permanently blind" | false | `.52` |

Four of the five are numbers. The fifth — mine — was a universal quantifier
asserted from a single observation, which is the same error in a different
grammar: **a quantity stated more confidently than it was measured.**

Not one of the five is a claim about *whether* the work was done. Every one is
a claim about *how much*, and in every case the exact figure was one command
away:

```bash
git rev-list --count <tip>..master        # commits behind
git show <rev>:<path> | wc -l             # line count
jq ... .pact/events.jsonl                 # lease held, to the nanosecond
# test count: the cargo output already on the author's screen
```

So the audit's practical yield is not "five agents did bad work" — the code
behind all five is clean, and I re-ran every gate to say so. It is one habit:
**in a close reason, a number is either copy-pasted from a command run in that
session, or it is not stated.** "About half an hour" is fine and
unfalsifiable-by-design; "~25m" reads as measured, so it gets measured, and then
it is wrong. Prefer the vague phrasing to the precise-looking guess.

I consolidated rather than filing a P1 per instance: five beads each saying "a
number was wrong" is noise that would get the audit discounted, which is the one
failure mode an auditor cannot recover from. `.51` and `.56` between them carry
all five, and on both I said in writing that P1 overstates the severity and
asked for re-prioritisation rather than quietly downgrading it myself.

### Orchestrator, pass 4

Nothing new closed by the orchestrator this pass, so nothing new to find. Its
standing record: **10 claims audited across `.47`, `.48` and the six spec beads
— 9 VERIFIED, 1 FALSE (`.53`)**. It has also now merged `agent/error` to master
(`8078c79`), which resolved the "master does not build" condition `.2` reported,
92 seconds after that bead closed.

### Still unaudited at the end of this pass

`bd_30-agents-2jk.10` (ast) closed while pass 4 was being written and has not
been touched. `.29`/`.30`'s "a `bd create` reported exit 1 while succeeding"
remains UNVERIFIABLE and is worth resolving, because the two possible
explanations — a misremembered exit code, or bd calls made outside `tools/pw` —
have very different implications for the harness data, and one of them is the
brief's "hole in the data".

---

## Pass 5 — 2026-08-14, `bd_30-agents-2jk.10` (ast)

**Running totals: 110 claims checked — 102 VERIFIED, 3 UNVERIFIABLE, 5 FALSE.**

### `bd_30-agents-2jk.10` — ast · 13 claims · 13 VERIFIED, 0 FALSE

The cleanest close of the run, and the one that shows the `.56` pattern is a
habit and not an inevitability: **every number in it is right**, including the
incidental ones.

| Claim | Measured | Verdict |
|---|---|---|
| "commit `9ebb052`, trailer `Pact-Agent=ast`", branch `agent/ast-r6` | all three confirmed; 410 insertions, `ast.rs` only | VERIFIED |
| "`wt6/ast` did not exist and `agent/ast` was already checked out by an earlier run's `wt/ast`, so I created `agent/ast-r6` from master — the merger needs to know that" | `agent/ast` is held by the run-5 worktree `wt/ast`; `9ebb052` is on `agent/ast-r6`, now also merged to master | VERIFIED |
| "FROZEN §3 VERBATIM: Expr, Stmt, FnDecl, Program, all Debug+Clone" | matches `docs/treadle.md:138-156` | VERIFIED |
| "UnOp{Neg,Not} and BinOp{Or,And,Eq,Ne,Lt,Gt,Le,Ge,Add,Sub,Mul,Div,Rem}, Debug+Clone+Copy+PartialEq+Eq" | `ast.rs:19-20` and `:36-37`, derives exact, 13 BinOps | VERIFIED |
| "ast.rs imports only `crate::value::Value` and nothing from error.rs" | its only `use` lines are `std::rc::Rc` and `crate::value::Value` | VERIFIED |
| **"build and clippy FAIL, with exactly two errors, both E0432 … in `src/value.rs:17` and `src/front/lexer.rs:29`"** | reproduced: **exactly 2** `error[E0432]`, at `src/value.rs:17:20` and `src/front/lexer.rs:29:20`. Line numbers exact | VERIFIED |
| "4 of my tests PASS by name … (38 other tests filtered out)" | reproducing its method (real `error.rs` dropped into a scratch copy): all four named tests pass, and the suite totals **42** — so 42 − 4 = **38 filtered**, exactly as stated | VERIFIED |
| "clippy `--all-targets -- -D warnings` CLEAN" | exit 0 | VERIFIED |
| "MESSAGED all 9 dependents … in one thread `pact-msg-6156f17f938c5787`" | that id, `from: ast`, exactly those 9 recipients, one thread | VERIFIED |

Two things worth copying from this reason. It reported a **failing** in-tree
build as a failure, with the exact error count and line numbers, and separately
described the honest scratch-copy gate — so nothing had to be taken on trust and
nothing was dressed up. And its one genuinely load-bearing design decision (that
`Program.fns` is the complete hoisted list and `Stmt::Fn` is a run-time no-op,
because otherwise a `fn` inside a never-taken branch is defined in one engine and
not the other) is documented in the code *and* in the outgoing message, not only
in the close reason — which is the difference between a decision that survives
and one that has to be rediscovered.
