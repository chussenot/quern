# Harness report — run 6 pact/bd instrumentation

Bead `bd_30-agents-2jk.7`. Agent `harness-report`.

Dataset: 29 files `.harness/*.jsonl`, snapshotted at **2026-08-14T12:33:05Z**. Records span **08:01:19Z → 12:32:40Z** (4h31m). Everything below is derived from that snapshot; the logs kept growing after it, so re-running the same counts later will give slightly larger numbers.

**The answer first: no. Nothing in `pact` or `bd` failed or hung under load.** 875 valid records, 66 non-zero exits, and after classification **zero of them are an unexplained pact or bd failure**. Max wall-time for any single call in the run is **2.916s**; nothing over 3s exists. Details and the full exclusion list are in §4.

I re-derived the orchestrator's interim analysis rather than inheriting it. It holds on the substance and is **wrong on three counts of arithmetic** — see §7.

---

## 1. Line accounting and the malformed records

| | count |
|---|---|
| physical lines across 29 files | **888** |
| blank lines | 5 |
| non-blank lines | 883 |
| **valid JSON records** | **875** |
| **non-blank invalid lines** | **8** |
| lost invocations these 8 lines represent | **3** |

The orchestrator's "13 invalid records" and my "8" are the same fact counted differently: **8 non-blank invalid lines + 5 blank lines = 13 physical lines that are not a JSON record.** Both numbers are right; mine is the one you can re-derive with a parser, since a blank line is not a record.

More useful than either: those 13 lines are **3 lost invocations**, not 13.

Every malformed line, enumerated:

| file:line | what | cause |
|---|---|---|
| `value.jsonl:19` | `bd close bd_30-agents-2jk.1 --reason=…` header, string unterminated | v1 pw: multi-line `--reason` not flattened |
| `value.jsonl:20,22,24,26,28` | blank | continuation of the same torn record |
| `value.jsonl:21,23,25,27,29` | prose continuation lines | same torn record |
| `close-auditor.jsonl:30` | `bd show multi<TAB>line arg "quoted" \backslash` — raw control char at col 196 | v2 pw: `stderr` still on the old filter, raw TAB |
| `close-auditor.jsonl:33` | `bd show alpha<LF>beta` — raw control char at col 162 | same |

So: **one** `bd close` from `value` (11 physical lines, 6 of them non-blank), and **two** `bd show` probes from `close-auditor`. All three are pre-v3 pw. No malformed line exists after `close-auditor.jsonl:33` (08:24:25Z) — i.e. none in the last 4h07m of the run, across 700+ records. The v3 fix held.

Total invocations attempted in the run: **875 + 3 = 878**.

## 2. Schema drift, and how much of exit 141 is undecidable

| | count |
|---|---|
| records missing the `sigpipe` key (v1) | **124** of 875 (14.2%) |
| `sigpipe: true` | **42** |
| `exit: 141` total | **46** |
| exit 141 **with** `sigpipe: true` | 42 |
| exit 141 with `sigpipe: false` | **0** |
| exit 141 with the key **absent** — undecidable by flag | **4** |

So 14.2% of the log cannot be asked the sigpipe question at all, but only **4 records** are actually ambiguous, because only those 4 are both v1 and exit 141. The orchestrator's note says "there is one such case". There are **four**:

| at | agent | argv |
|---|---|---|
| 08:10:03Z | close-auditor | `bd list --status=closed --json` |
| 08:11:38Z | value | `bd show bd_30-agents-2jk.2` |
| 08:12:53Z | token | `bd show bd_30-agents-2jk.8` |
| 08:16:10Z | value | `bd show bd_30-agents-2jk.1` |

**All four are resolvable without guessing**, which the note said could not be done. Each has a byte-identical call by the same agent that exited 0 within minutes: `close-auditor` 10s later, `value` 2m40s later, `token` 3m35s *earlier*, `value` 7m38s *earlier*. A `bd show` of an existing bead does not intermittently fail and then succeed unchanged with empty stderr; a truncated pipe does. All four are SIGPIPE. Undecidable **by the flag**, decided **by the data**.

Net: **46 of 66 non-zero exits (70%) are SIGPIPE and not failures.** Every one of them is `bd` (`bd show` x43, `bd list --json` x3) with `stderr` empty — the signature of `bd … | head` in an agent's shell, not of bd falling over.

## 3. Per-subcommand call counts and wall-time distribution

Seconds. `p90` is linear-interpolated. n = 875.

| subcommand | n | min | median | p90 | max |
|---|---|---|---|---|---|
| `pact msg read` | 139 | 0.027 | 0.042 | 0.053 | 0.065 |
| `bd show` | 118 | 0.179 | 0.338 | 0.398 | **2.916** |
| `pact lease ls` | 81 | 0.012 | 0.016 | 0.019 | 0.059 |
| `pact watch add` | 79 | 0.011 | 0.014 | 0.017 | 0.058 |
| `pact msg inbox` | 78 | 0.027 | 0.034 | 0.046 | 0.054 |
| `pact lease acquire` | 61 | 0.015 | 0.036 | 0.054 | 0.560 |
| `bd list` | 59 | 0.314 | 0.398 | 0.859 | 2.326 |
| `pact msg send` | 49 | 0.014 | 0.055 | 0.079 | 0.119 |
| `pact lease release` | 47 | 0.015 | 0.028 | 0.073 | 0.300 |
| `bd create` | 44 | 0.055 | 0.723 | 0.830 | 1.654 |
| `pact agents` | 32 | 0.017 | 0.034 | 0.044 | 0.061 |
| `bd update` | 29 | 0.276 | 0.356 | 0.401 | 1.112 |
| `bd close` | 26 | 0.275 | 0.568 | 0.635 | 0.700 |
| `pact audit` | 11 | 0.013 | 0.017 | 0.032 | 0.034 |
| `pact msg sent` | 6 | 0.029 | 0.036 | 0.046 | 0.046 |
| `pact watch rm` | 5 | 0.014 | 0.015 | 0.016 | 0.016 |
| `pact watch ls` | 2 | 0.013 | 0.015 | 0.017 | 0.017 |
| `pact log` | 2 | 0.031 | 0.037 | 0.041 | 0.042 |
| `pact whoami` | 2 | 0.095 | 0.098 | 0.100 | 0.100 |
| `bd` (bare / `--help`) | 2 | 0.075 | 0.078 | 0.080 | 0.081 |
| `bd comment` | 1 | 0.371 | — | — | 0.371 |
| `bd note` | 1 | 0.272 | — | — | 0.272 |
| `bd stats` | 1 | 0.273 | — | — | 0.273 |

Aggregates:

| | n | min | median | p90 | p99 | max |
|---|---|---|---|---|---|---|
| all | 875 | 0.011 | 0.043 | 0.432 | 1.140 | 2.916 |
| `pact` | 594 | 0.011 | **0.034** | 0.054 | — | 0.560 |
| `bd` | 281 | 0.055 | **0.365** | 0.749 | — | 2.916 |

`>0.5s: 71` · `>1.0s: 11` · `>2.0s: 2` · `>3.0s: 0`.

**The single most useful number in this table: `bd` is a full order of magnitude slower than `pact` at the median (0.365s vs 0.034s) and every one of the 11 calls over a second is `bd`.** pact's p90 is 54 **milliseconds** and its worst call in 594 is 0.560s. pact is flat-file; bd is a Dolt query. There is no pact latency story in this run at all.

`pact lease acquire` max 0.560s is the one pact call worth naming: it is `conform-basics` acquiring **40 paths in one all-or-nothing call**, i.e. 14ms per path. Multi-path acquire is cheap.

### The 11 calls over 1.0s — all `bd`, all exit 0

| secs | agent | argv |
|---|---|---|
| 2.916 | spec-adversary | `bd show bd_30-agents-2jk.10` |
| 2.326 | close-auditor | `bd list --parent=bd_30-agents-2jk` |
| 1.654 | close-auditor | `bd create --type=bug -p 2 --parent=… -l audit,harness,treadle --title=treadle(harness)…` |
| 1.473 | close-auditor | `bd list --status=closed` |
| 1.381 | close-auditor | `bd list --parent=bd_30-agents-2jk` |
| 1.339 | spec-adversary | `bd create --type=bug -p 1 --parent=… -l spec-gap,treadle --title=treadle(spec): i64 b…` |
| 1.271 | close-auditor | `bd list --status=closed --json` |
| 1.261 | close-auditor | `bd list --parent=bd_30-agents-2jk` |
| 1.169 | close-auditor | `bd list --status=closed --json` |
| 1.130 | conform-basics | `bd create --type=bug -p 2 --parent=… -l spec-gap,treadle --title=treadle(spec): group…` |
| 1.112 | token | `bd update bd_30-agents-2jk.8 --claim` |

**Slowest single call in the run: `bd show bd_30-agents-2jk.10` by `spec-adversary`, 2.916s, exit 0, empty stderr.** For scale, the same subcommand's median is 0.338s, so this is 8.6x the median — and `.10` was read twice more by the same agent at 0.396s and 0.213s. A one-off, not a pattern.

Nothing hung. The 120s stdin hang that run 5 saw from `pact msg send --body-file -` did not recur: 49 `pact msg send` calls, max **0.119s**, and one agent used `--body-file <path>` (`unknown`, 09:18:14Z) without incident.

## 4. Every non-zero exit, classified

66 non-zero of 875. **I count 0 as pact/bd failures.** Here is every class and why it is or is not a failure.

| n | class | verdict |
|---|---|---|
| 42 | exit 141, `sigpipe: true` | **not a failure** — 128+SIGPIPE, caller stopped reading |
| 4 | exit 141, `sigpipe` key absent (v1) | **not a failure** — see §2, all four have an identical exit-0 twin |
| 5 | `pact audit --check commit-correlation` exit 1 | **documented success** |
| 4 | `pact lease acquire` exit 1, space-joined path | **caller error** (zsh), bead `.63` |
| 4 | `bd show <nonsense>` exit 1 | **test artifact** — close-auditor's own escaping probes |
| 2 | `pact lease acquire` exit **2** | **documented contention signal**, not a failure |
| 2 | `bd create … -t <title>` exit 1 | **bd defect, newly filed as `.75`** — see §6 |
| 1 | `pact msg send --to-owner-of <never-leased path>` exit 1 | **caller error**, correct refusal with a good message |
| 1 | `bd close` exit 1, blocked issue | **documented refusal**, clear stderr, `--force` resolved it |
| 1 | `pact msg send` exit 1, `PACT_AGENT` unset | **caller error**, correct refusal |

Excluding SIGPIPE leaves **20 non-zero exits**, of which 15 are caller error or deliberate probe, 3 are documented non-zero results, and 2 are the one real tool defect I found (§6).

**Counted as successes, explicitly:**

- **`pact audit --check commit-correlation` exit 1, x5** (close-auditor, 08:25:46 / 08:32:05 / 08:32:25 x2 / 08:32:25). `pact audit --help` says verbatim: *"Exits 1 when a check finds something, 0 when it does not."* stderr is empty in all five; the finding is on stdout. An exit-1 here means the check **worked**.
- **`pact lease acquire` exit 2, x2** — documented contention. Both refusals named the holder and the remaining TTL, and both resolved:

  | at | agent | path | holder | resolution |
  |---|---|---|---|---|
  | 08:20:52Z | `error` | `treadle/src/error.rs` | `lexer` | `lexer` released at 08:21:32Z (**40s**); `error` re-acquired 08:22:35Z (**1m43s** lost) |
  | 08:59:19Z | `parser-stmt` | `treadle/src/front/parser.rs` | `parser-expr` | `--steal` at 08:59:39Z (**20s**) — bead `.11` was already closed |

  Both refusals also emitted the `pact watch add` hint on stderr. Total agent time lost to lease contention across the whole run: **2m03s.**
- **`bd close` exit 1, `cannot close blocked issue: bd_30-agents-2jk.6 is blocked by [bd_30-agents-2jk.26] (use --force to override)`** — differential-fuzzer, 11:26:04Z. Correct behaviour with an actionable message naming the blocker and the flag. Resolved at 11:26:51Z with `--force`.

**Counted as caller error, not tool failure:**

- **`pact lease acquire` with a space-joined path, x4** (conform-fns 08:37:42, conform-errors 08:38:08, conform-basics 08:38:03 and 08:38:36). stderr: `error: creating lock file …/.pact/leases/treadle__ treadle__tests__conform__201_… .tr treadle__…`. pact received **one** argument containing spaces because zsh does not word-split an unquoted variable. Bead `.63`/`.65`. All three agents recovered within 20–50s; conform-errors went to a glob, conform-basics wrote the paths out literally.
- **`pact msg send --to-owner-of treadle/src/front/lexer.rs`**, token 08:12:15Z: `error: no agent has ever leased treadle/src/front/lexer.rs, so it has no owner to address — `pact lease ls --all` lists every path pact knows`. The orchestrator's note calls this "a genuine pact usability defect". **I disagree.** pact refused an unresolvable address, said exactly why, and named the command that would have shown the truth. token recovered 11s later by dropping the two `--to-owner-of` flags and keeping the three `--to` names. The underlying trap is path-vs-cwd (`.66`), not `--to-owner-of`. There *is* a residual protocol gap worth stating: AGENTS.md promises "a message about a file follows the file … a handoff sent to a path still reaches whoever picks it up next", but you cannot pre-address a path that has never been leased. That is a docs/behaviour mismatch, not a failure.
- **`pact msg send` with `PACT_AGENT` unset**, `unknown.jsonl:4`, 09:18:14Z: `error: no agent identity: pass --agent <name> or set PACT_AGENT`. pact refusing to guess an identity, as designed. (This is also why an `unknown.jsonl` exists at all — 5 records with no agent identity, from two different agents' shells.)
- **close-auditor's 4 `bd show <nonsense>` probes** (08:24:25 x2, 08:24:26, 08:31:38) — deliberate quoting/escaping tests of the pw wrapper. bd echoed the argument back correctly each time, which is what the probe was for.

## 5. Retries: was anything retried, and did an identical retry succeed?

**Yes to both — and the run-5 pattern it was looking for did not recur.**

Byte-identical retry after a non-zero exit, **succeeded**: 6 cases, all `bd show`/`bd list` after exit 141.

| agent | argv | first | retry | gap |
|---|---|---|---|---|
| close-auditor | `bd list --status=closed --json` | 141 | **0** | 10s |
| value | `bd show bd_30-agents-2jk.2` | 141 | **0** | 2m40s |
| env | `bd show bd_30-agents-2jk.37` | 141 | **0** | 8s |
| conform-runner | `bd show bd_30-agents-2jk.21` | 141 | **0** | 14s |
| parser-stmt | `bd show bd_30-agents-2jk.12` | 141 | **0** | 10s |
| eval-stmt | `bd show bd_30-agents-2jk.20` | 141 | **0** | 11s |

**This is the exact shape run 5 mistook for "bd fails intermittently under load", and it is not that.** The first call was piped into `head`, the retry was not. Same command, same bd, different pipe.

Byte-identical retry, **failed identically** — i.e. deterministic, load-independent:

| agent | argv | outcome |
|---|---|---|
| lexer | `bd show bd_30-agents-2jk.51` | 141 → 141 (both `sigpipe: true`) |
| lexer | `bd show bd_30-agents-2jk.62` | 141 → 141 (both `sigpipe: true`) |
| error | `bd show bd_30-agents-2jk.2` | 141 → 141 (both `sigpipe: true`) |
| close-auditor | `pact audit --check commit-correlation` | 1 → 1 (documented finding, still found) |
| close-auditor | `pact audit --check commit-correlation --since 60m` | 1 → 1 (same) |
| conform-basics | `pact lease acquire <joined paths>` | 1 → 1 (deterministic caller error) |

**`bd close` specifically — the run-5 failure mode did NOT recur.** 26 `bd close` calls, **1 non-zero**, and that one had a full, actionable stderr and was fixed by *changing* the command (`--force`), not by repeating it. There is no instance in run 6 of `bd close` exiting non-zero with empty stdout and empty stderr, and no instance of the same close being attempted three times. `bd update`: 29 calls, **0 non-zero**. `bd close` max wall-time 0.700s.

Retries that changed the command and then worked: 8 (the 4 joined-path acquires, the `--to-owner-of` drop, the `--steal`, the `--force` close, and two agents who ran `--help` after a rejected `bd create`).

## 6. Failure patterns found and filed

**`bd_30-agents-2jk.75` — `bd create -t "<title>"` silently binds the title to `--type` and then reports the wrong error.** `-t` is bd's short flag for `--type`, not `--title`. Two agents passed `--type=bug … -t "<a long human title>"`; bd let the second `-t` override the first `--type` with a value that is not a valid issue type, and then reported the *other* missing field:

```
08:42:33Z conform-errors  bd create --type=bug -p 2 --parent=bd_30-agents-2jk -l spec-gap,treadle -t treadle(front): lexer.rs formats Lex messages by hand and diverges… -d …
  -> exit 1  "Error: title required (or use --file to create from markdown)"
08:46:13Z conform-fns     bd create --type=bug -p 2 --parent=bd_30-agents-2jk -l harness,pact -t pact lease acquire with several paths stores ONE composite lease… -d …
  -> exit 1  "Error: title required (or use --file to create from markdown)"
```

Reproduced deterministically:

```
$ bd create --dry-run --type=bug -p 2 -t "a long human title with spaces" -d "desc"
Error: title required (or use --file to create from markdown)   # exit 1

$ bd create --type="a long human title with spaces" --title=PROBE
Error: validation failed: invalid issue type: a long human title with spaces
```

bd *does* reject an invalid `--type` — but only once a title is present, so the validation order hides the actual mistake. Both agents lost a call and one went to `bd create --help` to work it out. Cost: 2 calls, ~15s. Cheap fix: validate `--type` before requiring `--title`, or reject a repeated `--type`.

**`bd_30-agents-2jk.76` — pw records exit code and stderr only, so an exit-0-with-stdout-error failure is invisible in this log, and the flattened `argv` cannot distinguish one joined argument from N.** Two concrete blind spots this report ran into:

- Bead `.70` (`lease release --all` no-ops from a subdirectory, printing "held no leases") **cannot be seen in the harness log at all**: all **29** `pact lease release --all` calls in the run exit 0 with empty stderr, as do all 18 explicit-path releases. If `.70` fired during run 6 — and audit's "1 close event with no matching open" suggests something did — the log says nothing. Same for the six `treadle/treadle/…` doubled-path calls (`.66`): `parser-expr` releasing `treadle/treadle/src/front/parser.rs` and `conform-errors` un-watching five doubled paths, **all exit 0**, all no-ops.
- `pw` joins argv with `printf '%s '`, so `acquire a b` and `acquire "a b"` are the same string in the log. Detecting `.63` from the log requires the orchestrator's double-space heuristic, and that heuristic is why the interim note undercounted the joined-path class. The event log settles it — `pathtest` holds a lease literally named `treadle/src/value.rs treadle/src/error.rs` — but the harness log alone cannot.

Distinct from `.32`, which is about pw *corrupting* records; these are records pw writes correctly that cannot answer the question.

Already-filed patterns confirmed present in the data, not re-filed: `.32` (torn lines, SIGPIPE-as-failure), `.63` + `.65` (joined path, 4 occurrences), `.66` (cwd-relative doubled paths, 6 occurrences, all exit 0), `.70` (`release --all`), `.49`/`.50` (`/tmp` quota — invisible here, it fails the *shell*, never a pact/bd call).

## 7. Cross-check against the pact event log

`pact audit --since 2026-08-14T08:09:06Z` — 317 events, 27 agents, 08:09:37Z → 12:31:39Z. Restricting the harness log to the same window (834 of 875 records):

| quantity | harness log | event log | |
|---|---|---|---|
| `pact watch add`, exit 0 | 78 calls / 78 paths | `watched 78` | **exact** |
| `pact watch rm`, exit 0 | 5 calls / 5 paths | `unwatched 5` | **exact** |
| `pact lease acquire` exit 2 | 2 | `refused 2` | **exact** |
| `--steal` acquires | 1 (parser-stmt 08:59:39Z) | `stolen 1`, `displaced 1` | **exact** |
| `pact lease acquire` exit 0 | 54 calls | `acquired 98` | consistent (multi-path) |
| `pact lease release` exit 0 | 47 calls (29 `--all`, 18 explicit) | `released 96` | consistent (`--all` / multi-path) |

Four exact matches and two that reconcile as one-call-to-many-paths. **The two logs do not disagree anywhere I can test them.** The acquire/release rows cannot be tested to the unit because of the flattened-argv blind spot in §6, not because they conflict.

Two event-log facts the harness log has no view of, worth carrying forward: `expired 3` (all before the window — no lease expired during run 6) and `1 close event with no matching open`. Also from audit, and *not* derivable from pw at all: hold time over 96 completed holds, median **8m39s**, p90 **11m17s**, max **29m32s** — comfortably inside the 2700s default TTL, which is why nothing expired and why nobody needed `renew`. Contention: **2 refusals, 0.0 per successful claim.**

### Where the interim analysis was wrong

Substance: correct. **Zero unexplained pact or bd failures** holds at 875 records exactly as it did at 625. Three arithmetic corrections:

1. **"1 refusal" -> 2.** `pact lease acquire` exited 2 **twice**, not once: `error` on `error.rs` (08:20:52) *and* `parser-stmt` on `parser.rs` (08:59:19). `pact audit` independently reports `refused 2`. The brief's "79+ acquired, 1 refusal" is also stale — the run finished at `acquired 98, refused 2`.
2. **"there is one such case" (v1 exit 141) -> 4**, and all four are decidable from the data rather than being left as a guess (§2).
3. **`--to-owner-of` is not "a genuine pact usability defect"** (§4). pact's refusal was correct, specific and actionable. Also, the note cites bead `.68` for it; `.68` is about `bd update --notes` clobbering a field.

One more count that moved: the interim's "21 non-zero that are not sigpipe-flagged" is **24** at 875 records (66 non-zero − 42 flagged), of which 4 are the v1 141s, leaving **20** genuinely non-141 non-zero exits.

## 8. Calls per agent

29 distinct agent identities, 875 records. `real` = non-zero excluding all exit-141.

| agent | calls | pact | bd | non-zero | real failures |
|---|---|---|---|---|---|
| close-auditor | 83 | 22 | 61 | 10 | 9 (5 audit findings, 4 own probes) |
| lexer | 58 | 43 | 15 | 6 | 0 |
| spec-adversary | 57 | 8 | 49 | 2 | 0 |
| conform-fns | 46 | 38 | 8 | 3 | 2 (joined path, `bd create -t`) |
| compiler-stmt | 38 | 31 | 7 | 2 | 0 |
| conform-errors | 38 | 30 | 8 | 4 | 2 (joined path, `bd create -t`) |
| error | 38 | 26 | 12 | 3 | 1 (lease contention, exit 2) |
| parser-stmt | 38 | 29 | 9 | 5 | 1 (lease contention, exit 2) |
| parser-expr | 36 | 31 | 5 | 1 | 0 |
| machine-core | 34 | 27 | 7 | 1 | 0 |
| differential-fuzzer | 32 | 22 | 10 | 3 | 1 (`bd close` blocked) |
| eval-stmt | 32 | 26 | 6 | 1 | 0 |
| orchestrator | 32 | 31 | 1 | **0** | 0 |
| conform-runner | 30 | 19 | 11 | 1 | 0 |
| eval-expr | 28 | 22 | 6 | 2 | 0 |
| conform-basics | 27 | 15 | 12 | 5 | 2 (joined path x2) |
| machine-calls | 25 | 19 | 6 | 2 | 0 |
| env | 23 | 14 | 9 | 4 | 0 |
| opcode | 23 | 19 | 4 | 1 | 0 |
| compiler-expr | 22 | 17 | 5 | 1 | 0 |
| conform-control | 21 | 13 | 8 | 1 | 0 |
| pathtest | 20 | 20 | 0 | **0** | 0 |
| output | 19 | 15 | 4 | 1 | 0 |
| value | 19 | 14 | 5 | 2 | 0 |
| token | 17 | 13 | 4 | 2 | 1 (`--to-owner-of`) |
| ast | 16 | 11 | 5 | 1 | 0 |
| cli | 12 | 10 | 2 | **0** | 0 |
| harness-report | 6 | 5 | 1 | **0** | 0 |
| unknown | 5 | 4 | 1 | 2 | 1 (`PACT_AGENT` unset) |

`unknown.jsonl` is pw's fallback when `PACT_AGENT` is unset — 5 records from at least two agents' shells (a `compiler-expr` handoff at 09:18, a `bd show …7` at 12:30). **These 5 records are unattributable**, and any agent that ran `pact`/`bd` without pw at all is invisible. The pact event log's exact agreement on `watch add`/`watch rm`/`refused`/`stolen` says the coverage hole is small, but it is not provably zero.

## 9. Concurrency — what "under load" actually means here

Peak concurrency measured as distinct agents emitting a pw record in the same 10-minute bucket:

| bucket | agents | calls |
|---|---|---|
| 08:00 | 5 | 54 |
| 08:10 | 7 | 64 |
| 08:20 | 6 | 79 |
| 08:30 | 8 | 148 |
| **08:40** | **12** | **175** |
| 08:50 | 9 | 122 |
| 09:00 | 7 | 63 |
| 09:10 | 5 | 44 |
| 09:20 | 4 | 54 |
| 09:30 | 2 | 19 |
| 10:50–12:30 | 1–3 | 53 |

**29 agents ran over 4h31m; the busiest ten minutes had 12 of them active, at 17.5 calls/minute.** So "no pact or bd failures under ~25 concurrent agents" is better stated as: **no failures at up to 12 concurrent agents and a sustained peak of 175 coordination calls per 10 minutes.** The claim is real but the load is roughly half the headline number, and a report that says "~25 concurrent" without this table is overclaiming. Nothing in the latency data trends upward with the bucket count either — the 08:40 peak contains no call over 1.0s.

---

## Verdict

**Did pact or bd fail or hang under load? No.**

- **pact: 594 calls, 0 failures.** 9 non-zero exits, all correct behaviour: 5 documented audit findings, 2 documented contention refusals, 2 correct refusals of a malformed caller request. Median 34ms, p90 54ms, worst call in the run 0.560s (a 40-path acquire). No hang; the run-5 `--body-file -` stdin hang did not recur.
- **bd: 281 calls, 1 tool defect** (`bd create -t`, x2, filed as `.75`), no hang, and **no silent failure**. 46 of its 57 non-zero exits are SIGPIPE from the agents' own `| head`. `bd close` 26/26 accounted for; the run-5 silent `bd close` failure did not recur. Median 0.365s, p90 0.749s, worst 2.916s.
- **The harness itself was the least reliable component in the run**: it lost 3 invocations to its own encoding bugs (§1) and is structurally blind to two known pact bugs (§6, filed as `.76`).

The only things that cost agents measurable time were **caller-side**: the zsh joined-path trap (4 calls, ~50s), the cwd-relative doubled path (6 silent no-ops), lease contention (2m03s total), and `bd create -t` (2 calls). Not one second was lost to pact or bd being slow, wedged, or wrong.
