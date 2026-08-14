# pact & bd findings from two agent-fleet runs

Field notes from running two large agent fleets against pact, written for
whoever improves pact next. Nothing here is a patch — it is symptom, evidence,
why it matters, and a fix direction, ranked by what it cost.

**Read this caveat first.** Everything below was measured against **pact 0.9.1**
and **bd 1.2.1**. The pact source tree is at **0.9.3**. Two minor versions have
landed since these observations, so *check each finding still reproduces before
acting on it* — some may already be fixed. Where a finding is a reasoning claim
rather than a measurement, it is labelled.

## Provenance

| | run 5 (`quern`) | run 6 (`treadle`) |
|---|---|---|
| Workload | SQL engine, Rust | one language, two engines, differentially tested |
| Agents | 35 identities | 29 identities |
| Coordination events | 249 | 317 |
| Messages | 101 | — |
| Peak concurrency | not measured | **12** agents in one 10-min bucket |
| Instrumented tool calls | none | **875**, wrapped, with wall-time and exit code |
| Topology | worktree per agent, shared scope | same |

Run 6 wrapped every `pact` and `bd` invocation, so its numbers are measured
rather than recalled. Run 5's are from agent reports, which is why two of its
findings were later corrected by run 6's data (see *Withdrawn*, below).

---

## The finding I would act on first

### 1. The union merge driver is unreachable in any configuration a fleet can use — P1

`pact init` installs `merge=union` on `.pact/events.jsonl` and
`.pact/messages.jsonl` so worktree merges keep both sides. In a worktree fleet
that driver can never fire, because `PACT_WORKTREE_SCOPE` forces a choice and
neither branch gives you both halves:

- **`shared` (default)** — every worktree resolves state to the *main* checkout.
  There is exactly **one** physical `events.jsonl` and one `messages.jsonl` for
  the whole fleet. Nothing diverges, so nothing is ever union-merged.
- **`local`** — the logs do diverge and the driver genuinely fires, but
  `pact doctor` itself warns *"leases held here are INVISIBLE to sibling
  worktrees of this repository"*. Leases stop working across the fleet.

**Evidence.** Measured across all 38 worktree merges in run 5: zero conflicts on
either log, and **zero merges in which either log was union-merged**. The
decisive datum is authorship — `git log` over both files shows only
`orchestrator` ever committed them:

```
17 orchestrator
17 <merge commits>
```

**Not one of 35 agents committed either log.** Nine reported the same wall
independently and unprompted: their worktree copy is a stale tracked snapshot,
the live log is in the main checkout, `git add` finds nothing to stage.

**Why it matters beyond the driver.** The protocol block instructs every agent
to *"Commit `.pact/events.jsonl` AND `.pact/messages.jsonl` when you commit your
work."* Under the default scope, from a worktree, that instruction is
**impossible to follow**, and 35 agents each spent time discovering it. Run 6's
brief had to tell agents the opposite — *the orchestrator commits them, you do
not* — which is a documented contradiction of `AGENTS.md`.

**Fix direction.** Split the knob. Keep `leases/`, `waits/` and `read/` shared
while writing `events.jsonl` and `messages.jsonl` per-worktree — that is the
combination a fleet actually wants and the only one that makes the union driver
meaningful. Failing that, drop the union attributes and the commit-both bullet,
and document that only the main worktree commits coordination history. Either
way the protocol block needs a worktree carve-out.

**Related, and cheap:** `pact init` on a repo initialised before 0.9.x adds the
`merge=union` attribute for `messages.jsonl` but never refreshes `.gitignore`,
which is `.pact/*` with a single `!.pact/events.jsonl` allowlist. So the native
store is *gitignored* while carrying a merge attribute. `pact doctor` has an
"event log survives a clone" check and no equivalent for the message store —
that check is exactly what would have caught it.

---

## Silent failures — where pact says nothing went wrong

### 2. `lease release --all` no-ops from a subdirectory and reports success — P1

Three agents independently: `release --all` prints `<agent> held no leases`
while `lease ls` shows leases active *in the same second*, and releases nothing.
Releasing the explicit path works.

**Evidence, with a consequence.** `parser-stmt` found `parser-expr`'s lease on
`src/front/parser.rs` still held with **1721 seconds remaining**, after that
bead had closed, merged and sent its handoff. `parser-expr` had run
`release --all` and been told it held nothing. `parser-stmt` had to `--steal`
from an agent that had finished — the one situation the protocol says to reserve
for *"when you know a peer is gone."* Run 6's audit shows 98 acquired against 96
released.

**Why it matters.** The protocol ends every agent's turn with `release --all`.
An agent that follows it from its natural working directory believes it released
everything, exits, and leaves live leases that only expire on TTL — 45 minutes
of a path being unavailable, attributed to an agent that no longer exists.

It also **corrupts the coordination metrics**: a leaked lease later stolen reads
as contention that never happened, and the steal reads as a protocol violation
when it was the correct recovery from a tool defect. Any `stale-holds` or
`double-win` number from a fleet run has to be read with that in mind.

**Fix direction.** `--all` should mean every lease this *agent* holds — the
identity is the scope, not the directory. If a directory-scoped release is
wanted, make it an explicit flag. At minimum, never print "held no leases" when
the agent demonstrably holds some: report the count found and the scope
searched.

### 3. A space-joined path is accepted as one lease, and `lease ls` renders it as if it were correct — P2

Three separate agents concluded "pact caps multi-path acquires at ~15 paths."
**It does not** — from `bash`, `lease acquire a b c` prints `took 3 lease(s)`
and registers three paths; a 40-path acquire succeeds in 0.560s, 14ms per path.

The real cause is caller-side: **zsh does not word-split an unquoted variable**,
so pact receives ONE argument and takes a single lease whose path is literally
`treadle/src/value.rs treadle/src/error.rs` — one string containing a space.

**What is pact's share, and why this stays on the list:** pact accepted a path
containing a space without a murmur, and `lease ls` displays it **truncated**,
so it looks like an ordinary correct lease on the first path. That rendering is
why three agents each diagnosed a pact arity limit. Past ~5 paths the joined
string exceeds `NAME_MAX` and hard-fails with a raw `os error 36`.

**Fix direction.** Reject or warn on a lease path containing a space — no source
file in a normal repo has one. Show the full path in `lease ls`, or mark a path
that does not exist in the working tree. The tool had the information to say
"that path has a space in it and does not exist" and instead rendered a
reassuring lie.

### 4. `msg send --body-file -` hangs indefinitely and wedges the calling shell — P1 (unreproduced in run 6)

Run 5: an agent fed a heredoc to `--body-file -`; the call hung past 120 s and
left the shell unusable until it killed the process. The identical body via
`--body-file <path>` sent instantly.

**Status.** Run 6 made **49** `msg send` calls with a **max of 0.119s** and did
not reproduce it — but every agent had been told to use `--body-file <path>`, so
the stdin path was barely exercised. Treat as open and unconfirmed rather than
fixed.

**Why it matters.** This is the form the protocol steers agents toward
("`--body-file -` for stdin"), and a hang *after* composing a hot-file contract
message is the worst possible moment to lose a shell.

**Fix direction.** Read stdin to EOF with a bounded timeout; error rather than
block when stdin is a tty with nothing on it. Until confirmed fixed, the
protocol block should stop advertising `-`.

---

## Diagnostics that mislead

### 5. `--check topology --expect worktrees` cannot pass for any real fleet — P2

Run 5 failed it with 19 offending events, **not one of which was an agent
working in the wrong place.** Two independent causes:

1. **Expiry events inherit the collector's invocation context, not the
   holder's.** An agent let two leases lapse from inside its worktree; the locks
   were swept later by a different process running in the main checkout, and the
   resulting `expired` events carry `agent=spec-review` with
   `invoked_from=main`. That is wrong data independent of this check — an expiry
   is a fact about the *holder's* lease.
2. **There is no room for a legitimate main-checkout participant.** The
   remaining offenders are the orchestrator's own protocol-following leases. In
   the worktree topology pact documents, someone *must* sit in the main
   checkout — it is the only place the coordination logs can be committed from
   (finding 1) — so the orchestrator necessarily acts from `main`.

**Fix direction.** Stamp an `expired` event with the context recorded on the
`acquire` it terminates, or add a separate field for the collector and have
topology ignore it. Let `--expect worktrees` accept a declared main-checkout
identity (`--allow-main <agent>`, repeatable). Otherwise document that a fleet
with an orchestrator must use `--expect any`, which costs the check most of its
value.

### 6. `doctor` prescribes a bd config key that bd rejects, and the remediation appears to succeed — P2

`pact doctor` says to enable the audit sidecar with
`bd config set audit.enabled true`. bd 1.2.1 has **no audit namespace**: it
prints `Warning: audit.enabled is not a recognized config key`, writes it
inertly into `config.yaml` so it reads back `true`, and never creates
`.beads/interactions.jsonl`. `pact audit --check claim-lease-divergence` then
reports "could not run" forever while doctor and `bd config get` both look
satisfied.

That is the worst shape a diagnostic can take: **the remediation appears to
work.** In run 5 it made one of the five standard checks permanently
unrunnable.

**Fix direction.** Probe for the capability, not the config key — check whether
`.beads/interactions.jsonl` exists, or whether the bd on `PATH` advertises an
audit namespace — and name the minimum bd version in the warning.
`claim-lease-divergence`'s "could not run" should distinguish *sidecar off* from
*bd too old*.

### 7. `--ttl` is bare seconds while every other duration accepts units — P2

An agent passed `--ttl 20` meaning twenty minutes, got twenty seconds, and its
lease lapsed mid-work. A second agent tried `--ttl 3m` and had it rejected.

The inconsistency is what makes it a trap: `pact audit --since` accepts `90m`,
`24h`, `7d`, `2w`. Bare seconds is also the least useful default unit for a knob
whose own default is `2700`.

**Two knock-on effects, both observed.** The lapsed lease produced a
`commit-correlation` finding (a commit landing under an expired lease), and
because the lease *expired* rather than being released, every `pact watch`
subscriber on that path got **no release diff** — the watch guarantee silently
did not fire.

**Fix direction.** Share the `--since` duration grammar; keep bare integers
meaning seconds for compatibility but warn on a bare value under ~120. Say in
the help text that the default 2700 is seconds.

---

## Scale and ergonomics

### 8. `msg read` repeats the body once per recipient — P2

Three agents hit this. A 15-recipient broadcast costs ~280KB to read; one agent
spent 149KB reading four messages; a 2-recipient message returns two full
copies. I reproduced it in my own tooling.

The **store is correct and efficient** — one send is one row with a `to` array,
verified at 3, 8, 15 and 27 recipients. The amplification is purely in read-side
rendering. It bites hardest on exactly the messages that matter most, because
this run's protocol *required* hot-file changers to broadcast to every
dependent.

**Fix direction.** Render one body per message with recipients listed once as a
header. If per-recipient read state drives the repetition, show it as a compact
roster (`read by: a, b, c — unread: d, e`). Consider `--brief` for triage:
subject, sender, first N lines.

### 9. `watch` notifies a worktree agent of a change it cannot consume — P2

An agent did the protocol-correct thing — `watch add` on an interface it
depended on but did not own — then worked out that the notification is
structurally useless in a worktree fleet: the author writes that file on *their*
branch in *their* worktree, so it can never appear in the watcher's tree. The
only path is orchestrator-merge-to-master, then the consumer merging master.

Its words: *"their file can never reach mine; waiting was structurally
pointless."* It had a waiter running and killed it.

**Fix direction.** Have the release notification say what it is — a contract
notice, not a code delivery — and name the branch that would make it
consumable. pact already knows it: `lease ls` prints `WHERE` as
`agent/error @ error`. Cheapest version: one sentence in the protocol block's
watch bullet.

### 10. `--to-owner-of` cannot pre-address a path nobody has leased — P3 (docs mismatch, not a defect)

I originally filed this as a pact defect. **It is not**, and the agent that
audited it was right to push back: pact refused an unresolvable address, said
exactly why, and named the command that would show the truth
(`pact lease ls --all`). Cost: 11 seconds.

What survives is a **documentation mismatch**. `AGENTS.md` promises *"a message
about a file follows the file … a handoff sent to a path still reaches whoever
picks it up next."* A reader expects to be able to pre-address a path whose
owner has not started — which is the single most useful moment to do it. You
cannot.

**Fix direction.** Either accept an unleased path and hold the message for
whoever leases it next (which is what the prose describes), or say in the prose
that the path must already have a holder.

### 11. Lease and watch paths resolve relative to cwd, with no warning when the result does not exist — P3

Ordinary Unix behaviour, so not a defect — but from inside a crate
subdirectory, passing a repo-relative path silently registers
`treadle/treadle/src/...`, which nothing will ever release. Six such calls in
run 6, **all exit 0**. For a *watch* the failure mode is silence, which is
indistinguishable from "nothing has changed yet" — the exact state a watcher
waits in.

**Fix direction.** Warn when a lease or watch path does not exist in the working
tree. A watch on a nonexistent path is almost always this mistake; if watching a
not-yet-created path is legitimate (it is), say so and print the resolved path.

---

## bd findings

### 12. `bd create -t "<title>"` binds the title to `--type`, then reports the wrong field — P2

`-t` is bd's short flag for `--type`, not `--title`. Two agents passed
`--type=bug … -t "<a long human title>"`; the second `-t` overrode `--type` with
a value that is not a valid issue type, and bd then reported the *other* missing
field: `Error: title required`. Reproduced deterministically:

```
$ bd create --dry-run --type=bug -p 2 -t "a long human title with spaces" -d "desc"
Error: title required (or use --file to create from markdown)     # exit 1

$ bd create --type="a long human title with spaces" --title=PROBE
Error: validation failed: invalid issue type: a long human title with spaces
```

bd *does* reject an invalid `--type` — but only once a title is present, so
validation order hides the real mistake.

**Fix direction.** Validate `--type` before requiring `--title`, or reject a
repeated `--type`.

### 13. `bd create` does not echo the assigned id, and `--notes` replaces a field silently — P2

These compound into the same accident, which I committed **twice in two runs**:
`bd create` prints `Priority: P2 / Status: open` without the assigned id, so I
inferred the next sequential number was mine, and `bd update --notes` replaced
another author's field with no diff and no owner check.

Once I overwrote a peer's *title, notes and priority*, so their finding still
existed but was no longer findable by what it was about — a parenthesis gap
spent an hour titled as a pact path-resolution bug.

**Fix direction, cheapest first.** (a) Have `bd create` print the assigned id on
the success line — the absence of it is the root cause and everything else is
downstream. (b) Require `--force`, or print the current value, when writing a
field on a bead whose owner differs from `BEADS_ACTOR`. (c) Print a diff on any
destructive field write, as `--append-notes` already implies is the safer
default. Note bd *does* correctly refuse a `close` by a non-assignee — that
arbitration exists and could extend to field writes.

### 14. A `bd close` can reach `status=closed` with no reason — P2, narrowed

Originally filed as "`bd close` exits 1 silently under concurrency, and a retry
can land an empty `--reason`". **The first half does not survive run 6's data**
(see *Withdrawn*). The second half does and is unexplained: a close that reaches
`closed` with no reason is a partial write, and in this run the close reason is
the agent's entire handoff record.

**Fix direction.** Make status and reason atomic.

---

## Withdrawn and corrected — do not chase these

Stated because a findings list that only grows is not trustworthy.

| Claim | What it actually was |
|---|---|
| "pact joins paths into one lock filename; multi-path acquire unusable past ~15 paths" | **zsh** not word-splitting an unquoted variable. pact takes 40 paths correctly in 0.560s. Three agents' workarounds were treated as corroboration when they were only avoidance. |
| "the silent-shell failure is output-capture flake, the disk is fine" | `/tmp` is tmpfs mounted **`usrquota`** with uid 1000 at quota. `df` reports 6.8G free and **structurally cannot see a per-user quota**, so the measurement used to rule out disk exhaustion was incapable of detecting it. Both runs, one cause. |
| "`--to-owner-of` is a genuine usability defect" | A correct, specific, actionable refusal. Finding 10 above is the residue. |
| "`bd close` fails silently under fleet concurrency" | 46 of 66 non-zero exits in run 6 were exit 141 = SIGPIPE, every one a bd call with empty stderr — the signature of the agent's own `bd … \| head`. Six byte-identical retries succeeded; in each case the first was piped to `head` and the retry was not. In run 6 `bd close` was 26 calls with one non-zero, and that one had full actionable stderr. |
| "no failures under ~25 concurrent agents" | Peak was **12** concurrent in the busiest 10-minute bucket, at 175 calls per 10 min. The claim is real; the load was roughly half the headline. |

---

## What pact got right, and is worth not regressing

Measured, not flattered — and the reason none of the above is a complaint about
the product as a whole.

- **Latency is a non-story.** 594 pact calls: median **34ms**, p90 **54ms**,
  worst single call **0.560s** (a 40-path acquire). Every one of the 11 calls
  over a second in the run was bd, not pact. `pact audit` over a 300-event log
  runs in 0.00s.
- **The native message store held under concurrent writers.** 101 sends in run
  5 → 101 rows, every line valid JSON, every id distinct, no torn or interleaved
  append across 35 processes. **One send is one row regardless of fan-out** —
  verified at 3, 8, 15 and 27 recipients; the 27-recipient broadcast is a single
  1336-byte line.
- **Refusals carry excellent structured facts.** A refusal names the holder, the
  branch, the worktree, the age, the remaining TTL, the `--steal` escape, and
  echoes the requester's own note. Both refusals in run 6 resolved without
  polling; total time lost to contention across the whole run was **2m03s**.
- **Holder facts arrive even when you are not refused.** Acquiring a lease told
  me unprompted that another agent had let it expire 5m34s earlier and quoted
  their note. Another told me two unread messages were waiting *on the path I
  was about to edit* and quoted the subject. Both changed what I did next —
  which is the whole point of the feature.
- **`chain-integrity` verified 236/236 lines** with no gap, edit or forgery.
- **`commit-correlation` earned its place by catching the one participant who
  thought he was above the protocol.** All 35 agents were clean; the single
  uncovered commit was mine, editing a shared file with no lease held. In a run
  where `double-win` found nothing and there were no refusals at all, that check
  was still the one that found something.
- **`double-win` clean across 236 and 317 events** — no two agents ever held one
  path at once.
