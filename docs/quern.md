# quern — a toy SQL engine

`quern` is a single-process SQL database engine in Rust. No network, no
server, no MVCC. A REPL and a file runner over a page-based storage
engine with a write-ahead log. It exists to be built by a fleet of
agents working in parallel, so this spec pins the shared contracts hard
and leaves the interiors to whoever claims the bead.

**Non-goals, stated so nobody builds them:** no concurrency beyond a
single writer, no MVCC, no query optimiser worth the name (one
rule-based pass), no subqueries, no `LEFT`/`RIGHT`/`OUTER` join, no
`HAVING`, no `DISTINCT`, no indexes except the implicit one on
`INTEGER PRIMARY KEY`, no `ALTER TABLE`, no NULL arithmetic subtleties
beyond the rule stated below.

## 1. SQL surface

The whole language, and nothing outside it:

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

Types are `INT` (i64), `TEXT` (String), `BOOL` (bool). Operators:
`+ - * /` on INT, `=` `<>` `<` `>` on INT/TEXT/BOOL, `AND` `OR` `NOT`
on BOOL. Identifiers and keywords are case-insensitive; string literals
are single-quoted with `''` as the escape. Statements are
semicolon-terminated. `--` runs to end of line.

**NULL rule (the only one):** `NULL` is a `Value::Null`. Any arithmetic
or comparison with `Null` yields `Null`; a `WHERE` clause keeps a row
only when the predicate is exactly `Value::Bool(true)`. `NULL` sorts
last in `ASC`, first in `DESC`. Aggregates skip `Null` inputs except
`COUNT(*)`, which counts rows. This rule is normative — do not invent
three-valued-logic refinements beyond it.

**Errors** are values, not panics. Every fallible path returns
`Result<T, QuernError>`. A parse failure, an unknown table, a type
mismatch, a divide-by-zero: all are `Err`, all are reported by the REPL
as `Error: <message>` and by the `.slt` runner as a statement error. A
panic anywhere is a bug, including on malformed input.

## 2. Module layout

One operator per file, one bead per file, so the fleet does not queue
behind a single module.

```
src/
  main.rs             REPL entry, arg parsing (file mode vs interactive)
  repl.rs             line loop, output formatting
  types.rs            HOT: Value, Type, Schema, Row, QuernError
  catalog.rs          table name -> Schema, persisted via storage
  txn.rs              BEGIN/COMMIT/ROLLBACK state machine, single writer
  sql/
    token.rs          Token enum, keyword table
    lexer.rs          &str -> Vec<Token>
    ast.rs            HOT: Statement, Expr, SelectStmt, ...
    parser_ddl.rs     CREATE TABLE, DROP TABLE
    parser_dml.rs     INSERT, UPDATE, DELETE, BEGIN/COMMIT/ROLLBACK
    parser_query.rs   SELECT, incl. JOIN/GROUP BY/ORDER BY/LIMIT
  plan/
    mod.rs            HOT: plan entry point, Planner
    logical.rs        HOT: LogicalPlan
    physical.rs       LogicalPlan -> Box<dyn Operator>
  exec/
    scan.rs filter.rs project.rs join.rs aggregate.rs sort.rs
    limit.rs dml.rs   one Operator impl each
  storage/
    mod.rs            HOT: the Storage trait
    pager.rs          fixed 4096-byte pages, file-backed, LRU-free
    heap.rs           slotted-page heap file, row insert/scan/delete
    btree.rs          B-tree over INTEGER PRIMARY KEY -> RowId
    wal.rs            append-only REDO log, replay on open
tests/
  logic_runner.rs     the .slt harness
  logic/*.slt         the corpus
  recovery.rs         kill -9 child-process crash test
```

## 3. The hot contracts

These four signatures are **frozen by this spec**. Change one and you
break work already in flight, so if a bead genuinely needs a change:
message the dependents, do not just edit.

`types.rs`:

```rust
pub enum Value { Null, Int(i64), Text(String), Bool(bool) }
pub enum Type { Int, Text, Bool }
pub struct Column { pub name: String, pub ty: Type, pub primary_key: bool }
pub struct Schema { pub table: String, pub columns: Vec<Column> }
pub type Row = Vec<Value>;
pub type RowId = u64;
pub enum QuernError { Parse(String), Catalog(String), Type(String), Storage(String), Txn(String) }
pub type Result<T> = std::result::Result<T, QuernError>;
```

`storage/mod.rs`:

```rust
pub trait Storage {
    fn create_table(&mut self, schema: &Schema) -> Result<()>;
    fn drop_table(&mut self, table: &str) -> Result<()>;
    fn insert(&mut self, table: &str, row: &Row) -> Result<RowId>;
    fn delete(&mut self, table: &str, id: RowId) -> Result<()>;
    fn update(&mut self, table: &str, id: RowId, row: &Row) -> Result<()>;
    fn scan(&self, table: &str) -> Result<Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_>>;
    fn lookup_pk(&self, table: &str, key: i64) -> Result<Option<(RowId, Row)>>;
    fn begin(&mut self) -> Result<()>;
    fn commit(&mut self) -> Result<()>;
    fn rollback(&mut self) -> Result<()>;
}
```

`exec/` — every operator is a pull-based iterator, `open`/`next` folded
into Rust's own `Iterator`:

```rust
pub trait Operator {
    fn schema(&self) -> &[Column];
    fn next(&mut self) -> Result<Option<Row>>;   // Ok(None) = exhausted
}
```

`plan/logical.rs`:

```rust
pub enum LogicalPlan {
    Scan { table: String, schema: Schema },
    Filter { input: Box<LogicalPlan>, predicate: Expr },
    Project { input: Box<LogicalPlan>, exprs: Vec<(Expr, String)> },
    Join { left: Box<LogicalPlan>, right: Box<LogicalPlan>, on: Expr },
    Aggregate { input: Box<LogicalPlan>, group_by: Vec<Expr>, aggs: Vec<AggExpr> },
    Sort { input: Box<LogicalPlan>, keys: Vec<(Expr, bool)> },  // bool = descending
    Limit { input: Box<LogicalPlan>, n: usize },
    Insert { table: String, rows: Vec<Vec<Expr>> },
    Update { table: String, sets: Vec<(String, Expr)>, predicate: Option<Expr> },
    Delete { table: String, predicate: Option<Expr> },
    CreateTable { schema: Schema },
    DropTable { table: String },
}
```

## 4. Storage

**Pager.** Fixed 4096-byte pages, file-backed, page 0 is a header
(magic `QUERN\0\0\0`, u32 page count, u32 catalog root). `read_page`/
`write_page` by index, no cache eviction policy — a `HashMap` of dirty
pages flushed on commit is enough at this scale.

**Heap.** Slotted pages: a page holds a slot directory of `(offset,
len)` growing forward and row bytes growing backward. `RowId` is
`(page_idx << 16) | slot_idx`. A delete tombstones the slot; no
compaction. Rows are serialised length-prefixed per value, tag byte
first (`0`=Null `1`=Int `2`=Text `3`=Bool).

**B-tree.** Order-32 B+tree keyed on the `INTEGER PRIMARY KEY` value,
mapping key -> `RowId`. Leaves are linked for range scans. Only built
when a table declares a PK; `lookup_pk` uses it, `scan` does not.

**WAL, REDO-only.** Append-only file next to the heap. Every mutation
appends `(lsn: u64, txn_id: u64, kind: u8, table_len, table, payload)`
and `fsync`s on `COMMIT` along with a commit record. On open, replay
every record belonging to a txn that has a commit record; discard the
rest. That is what makes `ROLLBACK` and crash recovery the same
mechanism: uncommitted work is simply never replayed.

**Single writer.** `txn.rs` holds `Option<TxnId>`. `BEGIN` inside a
transaction is an error. Statements outside an explicit transaction run
in an implicit one that commits on success.

## 5. Grading

**The corpus.** `tests/logic/*.slt`, sqllogictest-flavoured, ~150 cases.
Two directives only:

```
statement ok
CREATE TABLE t (a INT PRIMARY KEY, b TEXT);

statement error
INSERT INTO nosuch VALUES (1);

query
SELECT a, b FROM t ORDER BY a;
----
1	one
2	two
```

Values are tab-separated, `NULL` prints as `NULL`, `TRUE`/`FALSE` for
bools. A `query` block without `ORDER BY` is compared as a sorted
multiset; with `ORDER BY`, in order. `tests/logic_runner.rs` walks the
directory, runs every file against a fresh temp database, and fails
with the file, line, expected and actual on any mismatch.

**Determinism.** Same corpus, same output, every run. No `HashMap`
iteration order in any output path — `ORDER BY` and `GROUP BY` output
must be deterministic, so `GROUP BY` sorts its groups before emitting.

**Recovery.** `tests/recovery.rs` spawns the quern binary as a child,
feeds it `BEGIN` plus mutations, `kill -9`s it before `COMMIT`, reopens
the database and asserts the uncommitted work is absent; then repeats
with a `COMMIT` before the kill and asserts the work is present.

**House rules.** `cargo clippy --all-targets -- -D warnings` clean,
`cargo fmt --check` clean, `#![forbid(unsafe_code)]` in `lib.rs` — the
pager may carry one `unsafe` block if and only if a comment names the
measurement that justified it. Zero dependencies except `rand` and
`tempfile`, both dev-only; anything else needs a comment arguing for it.
