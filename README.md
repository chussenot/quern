# quern

A toy SQL engine in Rust. One process, one file-backed database directory, one
writer. No network, no server, no client protocol, **no MVCC** — a transaction
is a single writer holding the whole database, and `ROLLBACK` discards the
write-ahead log. It is a REPL and a script runner over a page-based storage
engine, and that is the whole product.

`docs/quern.md` is the normative spec; this README is the tour.

## Build and run

```
cargo build
./target/debug/quern [--db <dir>] [script.sql]
```

- no script argument — interactive REPL on stdin
- `script.sql` — run every statement in the file, then exit
- `--db <dir>` — the database directory, default `quern.db` in the cwd
- `--help` prints `usage: quern [--db <dir>] [script.sql]`

Statement results go to **stdout**: one row per line, cells tab-separated, no
header and no row count. A statement that succeeds without rows prints nothing.
A failure prints one `Error: <message>` line and the loop continues — a
statement error is output, not a process failure, so the exit code is still 0.
The `quern> ` prompt goes to **stderr**, which is what makes piping a script in
give you byte-clean stdout. Exit code 2 is reserved for CLI failures (unknown
flag, unreadable script, a database that will not open).

## The SQL surface

The whole language (`docs/quern.md` §1), and nothing outside it:

```sql
CREATE TABLE t (a INT PRIMARY KEY, b TEXT, c BOOL);
DROP TABLE t;
INSERT INTO t (a, b, c) VALUES (1, 'x', TRUE), (2, 'y', FALSE);
INSERT INTO t VALUES (3, 'z', TRUE);              -- positional
SELECT a, b + 1 AS n FROM t WHERE a > 1 AND NOT c ORDER BY a DESC LIMIT 5;
SELECT t.a, u.b FROM t JOIN u ON t.a = u.a WHERE u.b <> 'q';
SELECT b, COUNT(*), SUM(a), MIN(a), MAX(a), AVG(a) FROM t GROUP BY b;
UPDATE t SET b = 'w', c = FALSE WHERE a = 2;
DELETE FROM t WHERE a = 3;
BEGIN; ... COMMIT;
BEGIN; ... ROLLBACK;
```

Three types: `INT` (i64), `TEXT`, `BOOL`. Arithmetic `+ - * /` on INT,
comparison `=` `<>` `<` `>` on all three, `AND` `OR` `NOT` on BOOL.
Identifiers and keywords are case-insensitive, strings are single-quoted with
`''` as the escape, `--` runs to end of line.

**The NULL rule, which is the only one:** any arithmetic or comparison with
`NULL` yields `NULL`; `WHERE` keeps a row only when the predicate is exactly
`TRUE`. So `WHERE b = NULL` matches nothing, ever. `NULL` sorts last in `ASC`
and first in `DESC`. Aggregates skip `NULL` inputs, except `COUNT(*)` which
counts rows. `AND`/`OR`/`NOT` absorb `NULL` the same way with no short-circuit,
so `TRUE OR NULL` is `NULL` — real SQL disagrees, quern is deliberately
simpler. Errors are values everywhere: a parse failure, an unknown table, a
divide-by-zero are all `Err`, never a panic.

Not implemented, on purpose: no subqueries, no `LEFT`/`OUTER` join, no
`HAVING`, no `DISTINCT`, no `ALTER TABLE`, no table aliases (`FROM emp JOIN
dept ON emp.dept = dept.id`, never `AS e`), and no index except the implicit
one on `INTEGER PRIMARY KEY`. `ORDER BY` currently resolves against the
projected row, so a sort key must appear in the `SELECT` list — that is the
one known gap, tracked and being fixed.

## A real session

Captured by piping statements into `quern --db <tmpdir>`, one process per step
against the same directory. The output blocks are its stdout verbatim; the
`quern> ` prompts are dropped because they go to stderr.

Create, insert, read back:

```sql
CREATE TABLE emp (id INT PRIMARY KEY, name TEXT, dept INT, bonus INT, active BOOL);
CREATE TABLE dept (id INT PRIMARY KEY, label TEXT);
INSERT INTO emp VALUES (1, 'ada', 10, 100, TRUE), (2, 'bob', 10, NULL, FALSE), (3, 'cy', 20, 300, TRUE);
INSERT INTO dept VALUES (10, 'eng'), (20, 'ops');
SELECT id, name, bonus, active FROM emp ORDER BY id;
```

```
1	ada	100	TRUE
2	bob	NULL	FALSE
3	cy	300	TRUE
```

`NULL` prints as `NULL`, booleans as `TRUE`/`FALSE`. A second process against
the same `--db` directory sees the rows, so the next step starts by sorting
them:

```sql
SELECT bonus FROM emp ORDER BY bonus DESC;
SELECT id, name FROM emp WHERE bonus = NULL;
SELECT id, name FROM emp WHERE NOT active ORDER BY id;
```

```
NULL
300
100
2	bob
```

`NULL` first in `DESC`. `WHERE bonus = NULL` printed nothing at all — zero
rows, no error — because the predicate evaluates to `NULL` and only `TRUE`
keeps a row. `NOT active` then found `bob`.

A join and a grouped aggregate over it:

```sql
SELECT emp.name, dept.label FROM emp JOIN dept ON emp.dept = dept.id ORDER BY emp.name;
SELECT dept.label, COUNT(*), SUM(emp.bonus), AVG(emp.bonus) FROM emp JOIN dept ON emp.dept = dept.id GROUP BY dept.label;
```

```
ada	eng
bob	eng
cy	ops
eng	2	100	100
ops	1	300	300
```

The `eng` group is two rows, one of them `NULL`: `COUNT(*)` is 2, and `SUM` and
`AVG` both skip the `NULL`, so `AVG` is 100 and not 50.

Constraints are enforced and the error names the column:

```sql
INSERT INTO emp VALUES (1, 'dup', 20, 0, TRUE);
SELECT COUNT(*) FROM emp;
```

```
Error: type error: duplicate PRIMARY KEY value 1 for emp.id
3
```

And `ROLLBACK` puts the rows back:

```sql
BEGIN;
DELETE FROM emp WHERE dept = 10;
SELECT id, name FROM emp ORDER BY id;
ROLLBACK;
SELECT id, name FROM emp ORDER BY id;
```

```
3	cy
1	ada
2	bob
3	cy
```

Two rows gone inside the transaction, all three back after it.

## Architecture

One operator per file. The frozen contracts — the `Value`/`Schema`/`QuernError`
types, the `Storage` trait, the `Operator` iterator, the planner entry point —
are in `docs/quern.md` §3 and are not restated here.

```
src/
  main.rs        arg parsing, file mode vs interactive
  repl.rs        line loop, statement splitting, format_row
  types.rs       Value, Type, Schema, Row, QuernError
  catalog.rs     table name -> Schema, persisted via storage
  txn.rs         BEGIN/COMMIT/ROLLBACK state machine, single writer
  sql/           token.rs lexer.rs ast.rs
                 parser_ddl.rs parser_dml.rs parser_query.rs
  plan/          mod.rs        execute(sql, &mut Db) -> Outcome
                 logical.rs    LogicalPlan
                 physical.rs   LogicalPlan -> Box<dyn Operator>
  exec/          scan filter project join aggregate sort limit dml
  storage/       mod.rs   the Storage trait
                 pager.rs 4096-byte pages, file-backed
                 heap.rs  slotted-page heap, insert/scan/delete
                 btree.rs B-tree over INTEGER PRIMARY KEY -> RowId
                 wal.rs   append-only REDO log, replayed on open
```

`plan::execute` is the single entry point from text to rows, and the REPL and
the `.slt` harness both go through it — and both format cells with
`repl::format_row`, so the corpus and the REPL cannot drift. A statement
outside an explicit transaction is wrapped in begin / commit-on-success /
rollback-on-error, so a multi-row `INSERT` that fails partway leaves nothing
behind; DDL is deliberately not transactional.

## Tests

```
cargo test                        # everything
cargo test --test logic_runner    # the .slt corpus only
cargo test --test recovery        # crash recovery, including two kill -9 cases
```

State at the time of writing, all observed on this tree:

- **221 lib tests pass**, 0 failed.
- **3 of 3 crash-recovery tests pass** (`tests/recovery.rs`), including the two
  that `kill -9` a real child process and reopen the database — one before
  `COMMIT`, one after.
- **348 of 353 `.slt` blocks pass** across the ten files in `tests/logic/`.
  The 5 failures are all one bug: `ORDER BY` naming a column that is not in the
  projection, in `030_select.slt`. It is tracked as beads
  `bd_30-agents-dwm.61` / `.62` and was being fixed while this was written, so
  a current run may well be 353/353 — `cargo test --test logic_runner` prints
  the per-file counts and the total.

The corpus is `.slt` — a `statement ok` / `statement error` / `query` block
format, compared byte for byte against stdout.

## Built by a fleet

quern was written by a fleet of ~30 Claude Code agents working in parallel, one
bead (issue) per file, each in its own git worktree. Coordination was `pact`:
an advisory lease per file so two agents never wrote the same path, and
messages for contract changes — the frozen signatures in `docs/quern.md` §3
exist so that thirty agents could compile against each other before the
implementations landed. The 353-block `.slt` corpus was written by five agents
against the spec alone, before the engine could run a single query, which is
why it graded the engine honestly on the first run.
