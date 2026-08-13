//! bead: quern-plan-mod — HOT: planner entry point. See docs/quern.md §3.
//!
//! The one function the REPL and the `.slt` runner both call, and the only
//! place the whole pipeline is spelled out:
//!
//! ```text
//! tokenize -> parse_{select,dml,create_table,drop_table} -> logical::lower
//!          -> physical::build + drain      (queries)
//!          -> exec::dml::execute           (INSERT / UPDATE / DELETE)
//!          -> Storage::create_table/drop_table  (DDL)
//!          -> Storage::begin/commit/rollback    (transaction control)
//! ```
//!
//! It wires; it implements nothing. Five things it has to get right, each one
//! a contract of the layer below rather than a choice made here:
//!
//! * **`BEGIN`/`COMMIT`/`ROLLBACK` are intercepted before `lower`.** §3 gives
//!   them no `LogicalPlan` variant, so `lower` rejects them by design
//!   (`QuernError::Txn`) and they go straight at `Storage`.
//! * **Non-query plans never reach `physical::build`.** It refuses them with
//!   `QuernError::Type("<VARIANT> produces no rows and has no operator: ..")`,
//!   which is a backstop for a mis-route, not a control-flow channel: the
//!   `LogicalPlan` variant is matched here first.
//! * **A statement outside an explicit transaction is atomic** ([`atomic`]).
//!   The storage layer's implicit transaction is per *`Storage` call*, so a
//!   3-row `INSERT` failing on its third row would otherwise leave the first
//!   two committed. `020_insert.slt` ins-24 and `100_update_delete.slt`
//!   cases 20-21 assert it lands nothing.
//! * **DDL is not transactional** (§6): `CREATE TABLE`/`DROP TABLE` take
//!   effect immediately and are not wrapped.
//! * **`exec::dml::execute` needs the `&Schema`**, which is why the entry
//!   point holds a whole [`Db`] rather than a `&mut dyn Storage`:
//!   `physical::build` needs no catalog, but the DML path does, and the
//!   catalog `CREATE TABLE` mutates is the one inside `Db`. Two catalogs
//!   would drift after the first DDL statement.

pub mod logical;
pub mod physical;

use crate::exec::dml;
use crate::sql::ast::Statement;
use crate::sql::token::Token;
use crate::sql::{lexer, parser_ddl, parser_dml, parser_query};
use crate::storage::{Db, Storage};
use crate::types::{Column, QuernError, Result, Row};
use logical::LogicalPlan;

/// What one statement produced. Three shapes, because §5 compares three:
/// rows for a `SELECT`, a count for a mutation, nothing at all for DDL and
/// transaction control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A `SELECT`. `columns` is the operator's output schema — names for the
    /// REPL, which §5 does not compare, and the row width, which it does.
    Rows {
        columns: Vec<Column>,
        rows: Vec<Row>,
    },
    /// Rows affected by an `INSERT`, `UPDATE` or `DELETE`.
    Count(usize),
    /// No output: `CREATE TABLE`, `DROP TABLE`, `BEGIN`, `COMMIT`,
    /// `ROLLBACK`, and input that was blank or nothing but a comment.
    Done,
}

/// Run one SQL statement against `db`.
///
/// Every failure is an `Err(QuernError::..)` — a parse error, an unknown
/// table, a type mismatch and a transaction-edge violation all come back the
/// same way, and nothing here panics.
pub fn execute(sql: &str, db: &mut Db) -> Result<Outcome> {
    let tokens = lexer::tokenize(sql)?;
    if tokens.is_empty() {
        // Blank input, or nothing but a comment: the lexer already dropped it.
        return Ok(Outcome::Done);
    }
    let stmt = parse(&tokens)?;

    // Transaction control has no LogicalPlan variant (§3), so it is routed
    // before planning rather than through it.
    match stmt {
        Statement::Begin => return db.begin().map(|()| Outcome::Done),
        Statement::Commit => return db.commit().map(|()| Outcome::Done),
        Statement::Rollback => return db.rollback().map(|()| Outcome::Done),
        _ => {}
    }

    let plan = logical::lower(&stmt, db.catalog())?;
    match &plan {
        // §6: DDL takes effect immediately and is not undone by ROLLBACK, so
        // it is deliberately outside `atomic`.
        LogicalPlan::CreateTable { schema } => {
            let schema = schema.clone();
            db.create_table(&schema).map(|()| Outcome::Done)
        }
        LogicalPlan::DropTable { table } => {
            let table = table.clone();
            db.drop_table(&table).map(|()| Outcome::Done)
        }
        LogicalPlan::Insert { table, .. }
        | LogicalPlan::Update { table, .. }
        | LogicalPlan::Delete { table, .. } => {
            // Cloned to end the catalog borrow before `db` is used mutably.
            let schema = db.catalog().get(table)?.clone();
            atomic(db, |db| dml::execute(&plan, db, &schema)).map(Outcome::Count)
        }
        // Everything else is a query. No wrapper: it writes nothing, and an
        // implicit transaction around it would cost a WAL commit and a flush.
        _ => query(&plan, db),
    }
}

/// Dispatch on the leading keyword. Anything else is a clean parse error
/// rather than a parser's complaint about a clause it never reached.
fn parse(tokens: &[Token]) -> Result<Statement> {
    match &tokens[0] {
        Token::Select => parser_query::parse_select(tokens),
        Token::Insert | Token::Update | Token::Delete => parser_dml::parse_dml(tokens),
        Token::Begin | Token::Commit | Token::Rollback => parser_dml::parse_dml(tokens),
        Token::Create => parser_ddl::parse_create_table(tokens),
        Token::Drop => parser_ddl::parse_drop_table(tokens),
        other => Err(QuernError::Parse(format!(
            "expected SELECT, INSERT, UPDATE, DELETE, CREATE, DROP, BEGIN, \
             COMMIT or ROLLBACK, found {other:?}"
        ))),
    }
}

/// Build the operator tree and drain it.
fn query(plan: &LogicalPlan, db: &Db) -> Result<Outcome> {
    let mut op = physical::build(plan, db)?;
    let columns = op.schema().to_vec();
    let mut rows = Vec::new();
    while let Some(row) = op.next()? {
        rows.push(row);
    }
    Ok(Outcome::Rows { columns, rows })
}

/// Run `body` as one atomic statement: all of its writes land, or none do.
///
/// `Storage` has no "is a transaction open?" probe, and it does not need one —
/// §6 pins `BEGIN` inside an open transaction as `QuernError::Txn`, so the
/// result of `begin` *is* the probe. Inside an explicit transaction we own
/// nothing and must not commit or roll back the user's work; outside one, this
/// is the implicit transaction §4 asks for, and it is here rather than in
/// `Db` because `Db`'s is per `Storage` call, one per row.
///
/// A failing `rollback` is swallowed on purpose: the error the caller needs is
/// the one that caused the rollback, not the one raised while cleaning up.
fn atomic<T>(db: &mut Db, body: impl FnOnce(&mut Db) -> Result<T>) -> Result<T> {
    let ours = db.begin().is_ok();
    match body(db) {
        Ok(value) if ours => db.commit().map(|()| value),
        Err(e) => {
            if ours {
                let _ = db.rollback();
            }
            Err(e)
        }
        Ok(value) => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    /// A fresh database in a tempdir, plus a `run` that unwraps.
    struct T {
        _dir: tempfile::TempDir,
        db: Db,
    }

    impl T {
        fn new() -> T {
            let dir = tempfile::tempdir().unwrap();
            let db = Db::open(dir.path()).unwrap();
            T { _dir: dir, db }
        }

        fn run(&mut self, sql: &str) -> Outcome {
            execute(sql, &mut self.db).unwrap_or_else(|e| panic!("{sql}: {e}"))
        }

        /// The rows of a query, as the REPL would print them but cell by cell.
        fn rows(&mut self, sql: &str) -> Vec<Row> {
            match self.run(sql) {
                Outcome::Rows { rows, .. } => rows,
                other => panic!("{sql}: expected rows, got {other:?}"),
            }
        }

        fn count(&mut self, sql: &str) -> usize {
            match self.run(sql) {
                Outcome::Count(n) => n,
                other => panic!("{sql}: expected a count, got {other:?}"),
            }
        }

        fn err(&mut self, sql: &str) -> QuernError {
            execute(sql, &mut self.db).expect_err(sql)
        }
    }

    fn int(i: i64) -> Value {
        Value::Int(i)
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    /// CREATE TABLE, then INSERT, then SELECT gets the rows back. The first
    /// point in the build where that is a single assertion.
    #[test]
    fn create_insert_select_round_trips() {
        let mut t = T::new();
        assert_eq!(
            t.run("CREATE TABLE u (a INT PRIMARY KEY, b TEXT, c BOOL)"),
            Outcome::Done
        );
        assert_eq!(
            t.count("INSERT INTO u VALUES (1, 'one', TRUE), (2, 'two', FALSE)"),
            2
        );
        assert_eq!(
            t.rows("SELECT a, b, c FROM u ORDER BY a"),
            vec![
                vec![int(1), text("one"), Value::Bool(true)],
                vec![int(2), text("two"), Value::Bool(false)],
            ]
        );
        // The projection's names reach the caller for the REPL's benefit.
        match t.run("SELECT b FROM u") {
            Outcome::Rows { columns, .. } => assert_eq!(columns[0].name, "b"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn blank_and_comment_only_input_is_done() {
        let mut t = T::new();
        for sql in ["", "   ", "\n", "-- nothing here"] {
            assert_eq!(t.run(sql), Outcome::Done, "{sql:?}");
        }
    }

    #[test]
    fn select_with_where_order_by_and_limit() {
        let mut t = T::new();
        t.run("CREATE TABLE n (a INT PRIMARY KEY, b INT)");
        t.count("INSERT INTO n VALUES (1, 10), (2, 20), (3, 30), (4, 40)");
        assert_eq!(
            t.rows("SELECT a FROM n WHERE b > 10 ORDER BY a DESC LIMIT 2"),
            vec![vec![int(4)], vec![int(3)]]
        );
        // LIMIT 0 is a real node, not an absent one.
        assert!(t.rows("SELECT a FROM n LIMIT 0").is_empty());
    }

    #[test]
    fn join_and_group_by() {
        let mut t = T::new();
        t.run("CREATE TABLE emp (id INT PRIMARY KEY, dept INT, name TEXT)");
        t.run("CREATE TABLE dept (id INT PRIMARY KEY, name TEXT)");
        t.count("INSERT INTO emp VALUES (1, 10, 'ann'), (2, 10, 'bob'), (3, 20, 'cid')");
        t.count("INSERT INTO dept VALUES (10, 'eng'), (20, 'ops')");
        assert_eq!(
            t.rows("SELECT emp.name, dept.name FROM emp JOIN dept ON emp.dept = dept.id ORDER BY emp.name"),
            vec![
                vec![text("ann"), text("eng")],
                vec![text("bob"), text("eng")],
                vec![text("cid"), text("ops")],
            ]
        );
        assert_eq!(
            t.rows("SELECT dept, COUNT(*) FROM emp GROUP BY dept ORDER BY dept"),
            vec![vec![int(10), int(2)], vec![int(20), int(1)]]
        );
        // A join under a GROUP BY, so the aggregate resolves in the combined
        // index space rather than either table's.
        assert_eq!(
            t.rows(
                "SELECT dept.name, COUNT(*) FROM emp JOIN dept ON emp.dept = dept.id \
                 GROUP BY dept.name ORDER BY dept.name"
            ),
            vec![vec![text("eng"), int(2)], vec![text("ops"), int(1)]]
        );
    }

    #[test]
    fn update_and_delete_return_counts() {
        let mut t = T::new();
        t.run("CREATE TABLE n (a INT PRIMARY KEY, b INT)");
        t.count("INSERT INTO n VALUES (1, 10), (2, 20), (3, 30)");
        assert_eq!(t.count("UPDATE n SET b = b + 1 WHERE a > 1"), 2);
        // `ORDER BY` resolves against the PROJECTED row (Sort is above
        // Project), so an ordering key has to be projected — `SELECT b .. ORDER
        // BY a` is a deliberate Err(Catalog), not a re-scan.
        assert_eq!(
            t.rows("SELECT a, b FROM n ORDER BY a"),
            vec![
                vec![int(1), int(10)],
                vec![int(2), int(21)],
                vec![int(3), int(31)],
            ]
        );
        assert!(matches!(
            t.err("SELECT b FROM n ORDER BY a"),
            QuernError::Catalog(_)
        ));
        assert_eq!(t.count("DELETE FROM n WHERE b = 21"), 1);
        assert_eq!(t.count("DELETE FROM n WHERE b = 999"), 0);
        assert_eq!(
            t.rows("SELECT a FROM n ORDER BY a"),
            vec![vec![int(1)], vec![int(3)]]
        );
    }

    #[test]
    fn begin_commit_persists_and_begin_rollback_discards() {
        let mut t = T::new();
        t.run("CREATE TABLE n (a INT PRIMARY KEY)");
        t.run("BEGIN");
        t.count("INSERT INTO n VALUES (1), (2)");
        t.run("COMMIT");
        assert_eq!(t.rows("SELECT a FROM n ORDER BY a").len(), 2);

        t.run("BEGIN");
        t.count("INSERT INTO n VALUES (3)");
        // The open transaction reads its own uncommitted write.
        assert_eq!(t.rows("SELECT a FROM n ORDER BY a").len(), 3);
        t.run("ROLLBACK");
        assert_eq!(
            t.rows("SELECT a FROM n ORDER BY a"),
            vec![vec![int(1)], vec![int(2)]]
        );

        // §6's transaction edges, through this entry point rather than storage's.
        t.run("BEGIN");
        assert!(matches!(t.err("BEGIN"), QuernError::Txn(_)));
        t.run("ROLLBACK"); // the failed BEGIN left the transaction usable
        assert!(matches!(t.err("COMMIT"), QuernError::Txn(_)));
        assert!(matches!(t.err("ROLLBACK"), QuernError::Txn(_)));
    }

    /// The reason `atomic` exists: storage's implicit transaction is per
    /// `Storage` call, so without it rows 1 and 2 of this INSERT would commit.
    /// 020_insert.slt ins-24.
    #[test]
    fn a_failed_multi_row_insert_lands_nothing() {
        let mut t = T::new();
        t.run("CREATE TABLE n (a INT PRIMARY KEY, b TEXT)");
        t.count("INSERT INTO n VALUES (9, 'nine')");
        // Row 3 duplicates the primary key row 1 of the table already holds.
        assert!(matches!(
            t.err("INSERT INTO n VALUES (1, 'one'), (2, 'two'), (9, 'again')"),
            QuernError::Type(_)
        ));
        assert_eq!(t.rows("SELECT a FROM n ORDER BY a"), vec![vec![int(9)]]);

        // And an UPDATE that fails part way: 100_update_delete.slt 20-21.
        t.count("INSERT INTO n VALUES (1, 'one'), (2, 'two')");
        assert!(matches!(
            t.err("UPDATE n SET a = 1 WHERE a = 2"),
            QuernError::Type(_)
        ));
        assert_eq!(
            t.rows("SELECT a FROM n ORDER BY a"),
            vec![vec![int(1)], vec![int(2)], vec![int(9)]]
        );
    }

    /// §6: DDL takes effect immediately and a later ROLLBACK does not undo it.
    #[test]
    fn ddl_survives_a_rollback() {
        let mut t = T::new();
        t.run("CREATE TABLE keep (a INT PRIMARY KEY)");
        t.run("BEGIN");
        t.count("INSERT INTO keep VALUES (1)");
        t.run("ROLLBACK");
        // The table is still there — the rows are not.
        assert!(t.rows("SELECT a FROM keep").is_empty());

        t.run("DROP TABLE keep");
        assert!(matches!(
            t.err("SELECT a FROM keep"),
            QuernError::Catalog(_)
        ));
    }

    #[test]
    fn a_parse_error_and_an_unknown_table_are_errors_not_panics() {
        let mut t = T::new();
        assert!(matches!(t.err("SELCT 1"), QuernError::Parse(_)));
        assert!(matches!(t.err("SELECT FROM"), QuernError::Parse(_)));
        assert!(matches!(t.err("INSERT INTO"), QuernError::Parse(_)));
        assert!(matches!(t.err("42"), QuernError::Parse(_)));
        assert!(matches!(
            t.err("SELECT a FROM nope"),
            QuernError::Catalog(_)
        ));
        assert!(matches!(t.err("DROP TABLE nope"), QuernError::Catalog(_)));
        assert!(matches!(t.err("DELETE FROM nope"), QuernError::Catalog(_)));

        // A statement that failed leaves the session usable.
        t.run("CREATE TABLE n (a INT PRIMARY KEY)");
        assert_eq!(t.count("INSERT INTO n VALUES (1)"), 1);
    }

    /// A `Db` reopened over the same directory sees committed work — the
    /// entry point is where a session boundary becomes observable.
    #[test]
    fn committed_work_survives_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut db = Db::open(dir.path()).unwrap();
            execute("CREATE TABLE n (a INT PRIMARY KEY, b TEXT)", &mut db).unwrap();
            execute("INSERT INTO n VALUES (1, 'one'), (2, 'two')", &mut db).unwrap();
        }
        let mut db = Db::open(dir.path()).unwrap();
        match execute("SELECT a, b FROM n ORDER BY a", &mut db).unwrap() {
            Outcome::Rows { rows, .. } => assert_eq!(
                rows,
                vec![vec![int(1), text("one")], vec![int(2), text("two")]]
            ),
            other => panic!("{other:?}"),
        }
    }
}
