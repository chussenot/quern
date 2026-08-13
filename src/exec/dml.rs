//! bead: quern-exec-dml — INSERT / UPDATE / DELETE, as a free function.
//!
//! DML is **not** an [`Operator`](super::Operator): the frozen `next()` returns
//! a `Row`, which cannot carry a `RowId` or an affected-row count, and a
//! `Box<dyn Operator + 'static>` cannot hold `&mut dyn Storage`. Proven with
//! rustc in bead .35 — see the module docs of `exec/mod.rs`.
//!
//! The other half of that rule shapes every mutation here. `Storage::scan`
//! takes `&self` and returns `Box<dyn Iterator + '_>`, so the borrow lives as
//! long as the iterator and no `&mut self` mutator can be called while it is
//! alive (E0502). So UPDATE and DELETE run in **two phases**: collect the hits
//! under the scan borrow, drop the iterator, then mutate. [`matching`] is the
//! whole of phase one and is the only place that scans.
//!
//! The schema arrives as a parameter rather than through a `Storage::schema()`
//! (bead .42): the caller in `plan/mod.rs` holds the `Catalog`.

use super::eval;
use crate::plan::logical::LogicalPlan;
use crate::sql::ast::Expr;
use crate::storage::Storage;
use crate::types::{QuernError, Result, Row, RowId, Schema, Type, Value};

/// Execute a DML plan, returning the number of rows affected.
///
/// Only `Insert`, `Update` and `Delete` are DML; anything else is a planner bug
/// (`QuernError::Type`, never a panic). `schema` must be the schema of the
/// plan's table — every variant carries the canonical table name, so the caller
/// looks it up once.
pub fn execute(plan: &LogicalPlan, storage: &mut dyn Storage, schema: &Schema) -> Result<usize> {
    match plan {
        LogicalPlan::Insert { table, rows } => insert(table, rows, storage, schema),
        LogicalPlan::Update {
            table,
            sets,
            predicate,
        } => update(table, sets, predicate.as_ref(), storage, schema),
        LogicalPlan::Delete { table, predicate } => delete(table, predicate.as_ref(), storage),
        _ => Err(QuernError::Type(
            "exec::dml::execute expects INSERT, UPDATE or DELETE".to_string(),
        )),
    }
}

// --- phase one: who matches --------------------------------------------------

/// Every `(RowId, Row)` in `table` whose `predicate` is exactly `Bool(true)`;
/// all rows when there is no predicate (§1). The `Row`s come back with the ids
/// because UPDATE evaluates its SET expressions against the *old* row.
///
/// This function owns the scan borrow and ends it by returning: the caller gets
/// an owned `Vec` and is free to take `&mut storage`.
fn matching(
    table: &str,
    predicate: Option<&Expr>,
    storage: &dyn Storage,
) -> Result<Vec<(RowId, Row)>> {
    let mut hits = Vec::new();
    for entry in storage.scan(table)? {
        let (id, row) = entry?;
        match predicate {
            None => hits.push((id, row)),
            // §1: a Null predicate matches nothing, so `WHERE a = NULL`
            // affects zero rows.
            Some(p) => {
                if eval(p, &row)?.is_true() {
                    hits.push((id, row));
                }
            }
        }
    }
    Ok(hits)
}

// --- type checking -----------------------------------------------------------

/// Check one row against the schema: right width, right types, no `Null` in the
/// primary key. `Null` is accepted in any other column (§1).
fn type_check(row: &Row, schema: &Schema) -> Result<()> {
    if row.len() != schema.columns.len() {
        return Err(QuernError::Type(format!(
            "table {} has {} column(s), got a row of {}",
            schema.table,
            schema.columns.len(),
            row.len()
        )));
    }
    for (value, column) in row.iter().zip(&schema.columns) {
        let ok = match value {
            // plan/logical.rs already rejects an INSERT that *omits* the PK
            // (§6); this catches an explicit `VALUES (NULL, ...)`.
            Value::Null => !column.primary_key,
            Value::Int(_) => column.ty == Type::Int,
            Value::Text(_) => column.ty == Type::Text,
            Value::Bool(_) => column.ty == Type::Bool,
        };
        if !ok {
            return Err(QuernError::Type(format!(
                "column {}.{} is {}, got {}",
                schema.table,
                column.name,
                column.ty,
                value.type_name()
            )));
        }
    }
    Ok(())
}

/// The one duplicate-key message, shared by INSERT and UPDATE so the two read
/// alike. Storage only knows the key is taken (a bool, from the btree); naming
/// the column it belongs to needs the schema, which is why this lives here.
fn duplicate_pk(key: i64, schema: &Schema, column: usize) -> QuernError {
    QuernError::Type(format!(
        "duplicate PRIMARY KEY value {key} for {}.{}",
        schema.table, schema.columns[column].name
    ))
}

// --- the three statements ----------------------------------------------------

/// `rows` arrive already normalised to full schema-ordered rows by
/// `plan/logical.rs` (§6), so there is no column list to permute here.
///
/// Two phases again, for a different reason: §4's implicit transaction commits
/// only on success, so a multi-row INSERT whose third row is bad must land none
/// of the first two. Every row is evaluated and checked before any is written.
fn insert(
    table: &str,
    rows: &[Vec<Expr>],
    storage: &mut dyn Storage,
    schema: &Schema,
) -> Result<usize> {
    let pk = schema.primary_key();
    let mut checked: Vec<Row> = Vec::with_capacity(rows.len());
    for exprs in rows {
        // VALUES cannot reference columns, so the input row is empty.
        let row: Row = exprs
            .iter()
            .map(|e| eval(e, &Vec::new()))
            .collect::<Result<_>>()?;
        type_check(&row, schema)?;
        if let Some(i) = pk {
            // Rows earlier in this same statement are not in storage yet, so
            // `lookup_pk` cannot see them: they are checked separately.
            if let Value::Int(key) = row[i] {
                let taken = storage.lookup_pk(table, key)?.is_some()
                    || checked.iter().any(|r| r[i] == Value::Int(key));
                if taken {
                    return Err(duplicate_pk(key, schema, i));
                }
            }
        }
        checked.push(row);
    }
    for row in &checked {
        // ponytail: an Err from storage here (I/O, a full page) can still leave
        // a partial batch; undoing it belongs to the implicit transaction in
        // Storage, not to a second undo log up here.
        storage.insert(table, row)?;
    }
    Ok(checked.len())
}

/// SET expressions are evaluated against the **old** row, so
/// `UPDATE t SET a = a + 1` sees the pre-update value.
///
/// Two phases here too, and for INSERT's reason: an UPDATE that moves a row
/// onto a PRIMARY KEY another row holds is an error (the btree maps one key to
/// one RowId, §4), and §4's implicit transaction means the earlier rows of the
/// same statement must not survive it. So every new row is built and validated
/// before any is written.
fn update(
    table: &str,
    sets: &[(String, Expr)],
    predicate: Option<&Expr>,
    storage: &mut dyn Storage,
    schema: &Schema,
) -> Result<usize> {
    // Names are canonical (plan/logical.rs), so resolve them once, not per row.
    let targets: Vec<(usize, &Expr)> = sets
        .iter()
        .map(|(name, e)| {
            schema
                .column_index(name)
                .map(|i| (i, e))
                .ok_or_else(|| QuernError::Catalog(format!("unknown column: {table}.{name}")))
        })
        .collect::<Result<_>>()?;

    // Only an UPDATE that assigns the PK column can collide; SET on any other
    // column leaves every key where it was, so it never needs the lookup.
    let pk = schema
        .primary_key()
        .filter(|p| targets.iter().any(|(i, _)| i == p));

    let hits = matching(table, predicate, storage)?; // borrow ends here
    let mut pending: Vec<(RowId, Row)> = Vec::with_capacity(hits.len());
    for (id, old) in &hits {
        let mut new = old.clone();
        for (i, expr) in &targets {
            new[*i] = eval(expr, old)?;
        }
        type_check(&new, schema)?;
        if let Some(i) = pk {
            if let Value::Int(key) = new[i] {
                // A hit on the row being updated is fine — `SET a = a` is a
                // no-op, not a violation. A hit on any other row is not, and
                // neither is a clash with a row already validated in this same
                // statement, which is not written yet for `lookup_pk` to find.
                //
                // ponytail: strictly row-at-a-time, so a rotation like
                // `SET a = a + 1` over {1, 2} is refused even though the final
                // key set would have been unique. That matches what storage can
                // actually do (it writes one row at a time and the btree would
                // reject the intermediate state), so relaxing it means ordering
                // the writes, not just relaxing the check.
                let clash = storage
                    .lookup_pk(table, key)?
                    .is_some_and(|(other, _)| other != *id)
                    || pending.iter().any(|(_, r)| r[i] == Value::Int(key));
                if clash {
                    return Err(duplicate_pk(key, schema, i));
                }
            }
        }
        pending.push((*id, new));
    }
    for (id, row) in &pending {
        storage.update(table, *id, row)?;
    }
    Ok(pending.len())
}

/// DELETE needs no schema: nothing is written, so nothing is type-checked.
fn delete(table: &str, predicate: Option<&Expr>, storage: &mut dyn Storage) -> Result<usize> {
    let hits = matching(table, predicate, storage)?; // borrow ends here
    for (id, _) in &hits {
        storage.delete(table, *id)?;
    }
    Ok(hits.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::BinOp;
    use crate::types::Column;

    /// A `Vec`-backed `Storage`. `Db` is not finished yet, and these tests only
    /// need the six methods DML calls; the rest are `unimplemented!()`-free
    /// errors so a mistake shows up as a failing assert, not a panic.
    struct Mock {
        rows: Vec<(RowId, Row)>,
        next: RowId,
        pk: Option<usize>,
    }

    impl Mock {
        fn new(pk: Option<usize>) -> Mock {
            Mock {
                rows: Vec::new(),
                next: 1,
                pk,
            }
        }

        fn values(&self, col: usize) -> Vec<Value> {
            self.rows.iter().map(|(_, r)| r[col].clone()).collect()
        }
    }

    impl Storage for Mock {
        fn insert(&mut self, _table: &str, row: &Row) -> Result<RowId> {
            let id = self.next;
            self.next += 1;
            self.rows.push((id, row.clone()));
            Ok(id)
        }

        fn delete(&mut self, _table: &str, id: RowId) -> Result<()> {
            let before = self.rows.len();
            self.rows.retain(|(i, _)| *i != id);
            if self.rows.len() == before {
                return Err(QuernError::Storage(format!("no such row {id}")));
            }
            Ok(())
        }

        fn update(&mut self, _table: &str, id: RowId, row: &Row) -> Result<()> {
            for (i, r) in self.rows.iter_mut() {
                if *i == id {
                    *r = row.clone();
                    return Ok(());
                }
            }
            Err(QuernError::Storage(format!("no such row {id}")))
        }

        #[allow(clippy::type_complexity)]
        fn scan(
            &self,
            _table: &str,
        ) -> Result<Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_>> {
            Ok(Box::new(self.rows.iter().cloned().map(Ok)))
        }

        fn lookup_pk(&self, _table: &str, key: i64) -> Result<Option<(RowId, Row)>> {
            let pk = self
                .pk
                .ok_or_else(|| QuernError::Type("no primary key".to_string()))?;
            Ok(self
                .rows
                .iter()
                .find(|(_, r)| r[pk] == Value::Int(key))
                .cloned())
        }

        fn create_table(&mut self, _schema: &Schema) -> Result<()> {
            Ok(())
        }
        fn drop_table(&mut self, _table: &str) -> Result<()> {
            Ok(())
        }
        fn begin(&mut self) -> Result<()> {
            Ok(())
        }
        fn commit(&mut self) -> Result<()> {
            Ok(())
        }
        fn rollback(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// `t (a INT PRIMARY KEY, b TEXT)`.
    fn schema() -> Schema {
        Schema {
            table: "t".to_string(),
            columns: vec![
                Column {
                    name: "a".to_string(),
                    ty: Type::Int,
                    primary_key: true,
                },
                Column {
                    name: "b".to_string(),
                    ty: Type::Text,
                    primary_key: false,
                },
            ],
        }
    }

    fn lit(v: Value) -> Expr {
        Expr::Literal(v)
    }

    fn int(i: i64) -> Value {
        Value::Int(i)
    }

    fn txt(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    fn ins(rows: Vec<Vec<Expr>>) -> LogicalPlan {
        LogicalPlan::Insert {
            table: "t".to_string(),
            rows,
        }
    }

    /// `a = <v>`, the predicate the corpus leans on.
    fn a_eq(v: Value) -> Expr {
        Expr::Binary {
            op: BinOp::Eq,
            left: Box::new(Expr::ColumnRef(0)),
            right: Box::new(lit(v)),
        }
    }

    /// Two rows: `(1, 'one')`, `(2, 'two')`.
    fn seeded() -> Mock {
        let mut m = Mock::new(Some(0));
        let plan = ins(vec![
            vec![lit(int(1)), lit(txt("one"))],
            vec![lit(int(2)), lit(txt("two"))],
        ]);
        assert_eq!(execute(&plan, &mut m, &schema()), Ok(2));
        m
    }

    #[test]
    fn insert_one_and_many_return_their_counts() {
        let mut m = Mock::new(Some(0));
        let one = ins(vec![vec![lit(int(1)), lit(txt("one"))]]);
        assert_eq!(execute(&one, &mut m, &schema()), Ok(1));
        let many = ins(vec![
            vec![lit(int(2)), lit(txt("two"))],
            vec![lit(int(3)), lit(txt("three"))],
        ]);
        assert_eq!(execute(&many, &mut m, &schema()), Ok(2));
        assert_eq!(m.values(0), vec![int(1), int(2), int(3)]);
    }

    #[test]
    fn insert_type_mismatch_is_a_type_error_and_writes_nothing() {
        let mut m = Mock::new(Some(0));
        let plan = ins(vec![vec![lit(txt("not an int")), lit(txt("b"))]]);
        let err = execute(&plan, &mut m, &schema());
        assert!(matches!(err, Err(QuernError::Type(_))), "got {err:?}");
        assert!(m.rows.is_empty());

        // ...and so is the wrong arity, which plan/logical.rs normally catches.
        let short = ins(vec![vec![lit(int(9))]]);
        assert!(matches!(
            execute(&short, &mut m, &schema()),
            Err(QuernError::Type(_))
        ));
    }

    #[test]
    fn duplicate_primary_key_names_the_table_and_column() {
        let mut m = seeded();
        let dup = ins(vec![vec![lit(int(1)), lit(txt("again"))]]);
        match execute(&dup, &mut m, &schema()) {
            Err(QuernError::Type(msg)) => {
                assert!(msg.contains("t.a"), "message must name t.a: {msg}");
                assert!(msg.contains('1'), "message must name the key: {msg}");
            }
            other => panic!("expected a type error, got {other:?}"),
        }
        assert_eq!(m.rows.len(), 2, "nothing may be written");
    }

    /// §4: the implicit transaction commits only on success, so a bad row in a
    /// multi-row INSERT must land none of the good ones — including when the
    /// clash is against an earlier row of the same statement, which is not in
    /// storage yet for `lookup_pk` to find.
    #[test]
    fn a_failed_multi_row_insert_lands_nothing() {
        let mut m = Mock::new(Some(0));
        let bad_type = ins(vec![
            vec![lit(int(1)), lit(txt("one"))],
            vec![lit(int(2)), lit(int(2))],
        ]);
        assert!(matches!(
            execute(&bad_type, &mut m, &schema()),
            Err(QuernError::Type(_))
        ));
        assert!(m.rows.is_empty(), "the good first row must not land");

        let self_clash = ins(vec![
            vec![lit(int(1)), lit(txt("one"))],
            vec![lit(int(1)), lit(txt("clash"))],
        ]);
        assert!(matches!(
            execute(&self_clash, &mut m, &schema()),
            Err(QuernError::Type(_))
        ));
        assert!(m.rows.is_empty());
    }

    #[test]
    fn null_goes_into_a_nullable_column_but_never_the_primary_key() {
        let mut m = Mock::new(Some(0));
        let ok = ins(vec![vec![lit(int(1)), lit(Value::Null)]]);
        assert_eq!(execute(&ok, &mut m, &schema()), Ok(1));
        assert_eq!(m.values(1), vec![Value::Null]);

        let bad = ins(vec![vec![lit(Value::Null), lit(txt("x"))]]);
        assert!(matches!(
            execute(&bad, &mut m, &schema()),
            Err(QuernError::Type(_))
        ));
        assert_eq!(m.rows.len(), 1);
    }

    #[test]
    fn update_with_a_predicate_touches_only_the_matches() {
        let mut m = seeded();
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![("b".to_string(), lit(txt("w")))],
            predicate: Some(a_eq(int(2))),
        };
        assert_eq!(execute(&plan, &mut m, &schema()), Ok(1));
        assert_eq!(m.values(1), vec![txt("one"), txt("w")]);
    }

    #[test]
    fn update_without_a_predicate_touches_every_row() {
        let mut m = seeded();
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![("b".to_string(), lit(txt("w")))],
            predicate: None,
        };
        assert_eq!(execute(&plan, &mut m, &schema()), Ok(2));
        assert_eq!(m.values(1), vec![txt("w"), txt("w")]);
    }

    #[test]
    fn update_sets_are_evaluated_against_the_old_row() {
        let mut m = seeded();
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![(
                "a".to_string(),
                Expr::Binary {
                    op: BinOp::Add,
                    left: Box::new(Expr::ColumnRef(0)),
                    right: Box::new(lit(int(10))),
                },
            )],
            predicate: None,
        };
        assert_eq!(execute(&plan, &mut m, &schema()), Ok(2));
        assert_eq!(m.values(0), vec![int(11), int(12)]);
    }

    #[test]
    fn update_matching_nothing_returns_zero() {
        let mut m = seeded();
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![("b".to_string(), lit(txt("w")))],
            predicate: Some(a_eq(int(99))),
        };
        assert_eq!(execute(&plan, &mut m, &schema()), Ok(0));
        assert_eq!(m.values(1), vec![txt("one"), txt("two")]);
    }

    /// `tests/logic/100_update_delete.slt` cases 20 and 21: `UPDATE u SET a = 1
    /// WHERE a = 2` when row 1 already holds key 1 is an error, and the failed
    /// statement leaves nothing behind — not even the `b` of the row it matched.
    #[test]
    fn update_onto_a_key_another_row_holds_errors_and_lands_nothing() {
        let mut m = seeded();
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![("a".to_string(), lit(int(1)))],
            predicate: Some(a_eq(int(2))),
        };
        match execute(&plan, &mut m, &schema()) {
            Err(QuernError::Type(msg)) => assert!(msg.contains("t.a"), "must name t.a: {msg}"),
            other => panic!("expected a type error, got {other:?}"),
        }
        assert_eq!(m.values(0), vec![int(1), int(2)]);
        assert_eq!(m.values(1), vec![txt("one"), txt("two")]);
    }

    /// The same-row hit is not a violation: `lookup_pk` finds the row being
    /// updated, and that must not be mistaken for another row's key.
    #[test]
    fn update_assigning_a_row_its_own_key_succeeds() {
        let mut m = seeded();
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![
                ("a".to_string(), Expr::ColumnRef(0)),
                ("b".to_string(), lit(txt("w"))),
            ],
            predicate: Some(a_eq(int(1))),
        };
        assert_eq!(execute(&plan, &mut m, &schema()), Ok(1));
        assert_eq!(m.values(0), vec![int(1), int(2)]);
        assert_eq!(m.values(1), vec![txt("w"), txt("two")]);
    }

    /// Two matched rows given the same new key collide with each other. Neither
    /// key exists yet, so `lookup_pk` sees nothing and only the pending-row
    /// check catches it — the UPDATE twin of the multi-row INSERT case.
    #[test]
    fn two_rows_colliding_within_one_update_error_and_land_nothing() {
        let mut m = seeded();
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![("a".to_string(), lit(int(3)))],
            predicate: None,
        };
        assert!(matches!(
            execute(&plan, &mut m, &schema()),
            Err(QuernError::Type(_))
        ));
        assert_eq!(m.values(0), vec![int(1), int(2)]);
    }

    /// A SET that does not touch the PK column never consults the index — the
    /// no-PK mock would error if it did.
    #[test]
    fn an_update_that_leaves_the_key_alone_skips_the_lookup() {
        let mut s = schema();
        s.columns[0].primary_key = false;
        let mut m = Mock::new(None);
        let seed = ins(vec![vec![lit(int(1)), lit(txt("one"))]]);
        assert_eq!(execute(&seed, &mut m, &s), Ok(1));
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![("b".to_string(), lit(txt("w")))],
            predicate: None,
        };
        assert_eq!(execute(&plan, &mut m, &s), Ok(1));
        assert_eq!(m.values(1), vec![txt("w")]);
    }

    #[test]
    fn update_to_a_wrong_type_is_rejected() {
        let mut m = seeded();
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![("a".to_string(), lit(txt("nope")))],
            predicate: None,
        };
        assert!(matches!(
            execute(&plan, &mut m, &schema()),
            Err(QuernError::Type(_))
        ));
    }

    #[test]
    fn update_to_an_unknown_column_is_a_catalog_error() {
        let mut m = seeded();
        let plan = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![("nosuch".to_string(), lit(txt("w")))],
            predicate: None,
        };
        assert!(matches!(
            execute(&plan, &mut m, &schema()),
            Err(QuernError::Catalog(_))
        ));
    }

    #[test]
    fn delete_with_a_predicate_removes_only_the_matches() {
        let mut m = seeded();
        let plan = LogicalPlan::Delete {
            table: "t".to_string(),
            predicate: Some(a_eq(int(1))),
        };
        assert_eq!(execute(&plan, &mut m, &schema()), Ok(1));
        assert_eq!(m.values(0), vec![int(2)]);
    }

    #[test]
    fn delete_without_a_predicate_empties_the_table() {
        let mut m = seeded();
        let plan = LogicalPlan::Delete {
            table: "t".to_string(),
            predicate: None,
        };
        assert_eq!(execute(&plan, &mut m, &schema()), Ok(2));
        assert!(m.rows.is_empty());
    }

    /// §1: `= NULL` evaluates to `Null`, which is not `Bool(true)`, so it keeps
    /// no rows. The corpus asserts this for both UPDATE and DELETE.
    #[test]
    fn a_null_predicate_affects_zero_rows() {
        let mut m = seeded();
        let del = LogicalPlan::Delete {
            table: "t".to_string(),
            predicate: Some(a_eq(Value::Null)),
        };
        assert_eq!(execute(&del, &mut m, &schema()), Ok(0));
        let upd = LogicalPlan::Update {
            table: "t".to_string(),
            sets: vec![("b".to_string(), lit(txt("w")))],
            predicate: Some(a_eq(Value::Null)),
        };
        assert_eq!(execute(&upd, &mut m, &schema()), Ok(0));
        assert_eq!(m.values(1), vec![txt("one"), txt("two")]);
    }

    /// A non-boolean predicate is falsy too, not an error (§1).
    #[test]
    fn a_non_boolean_predicate_keeps_no_rows() {
        let mut m = seeded();
        let plan = LogicalPlan::Delete {
            table: "t".to_string(),
            predicate: Some(lit(int(1))),
        };
        assert_eq!(execute(&plan, &mut m, &schema()), Ok(0));
        assert_eq!(m.rows.len(), 2);
    }

    #[test]
    fn a_non_dml_plan_is_a_type_error_not_a_panic() {
        let mut m = Mock::new(Some(0));
        let plan = LogicalPlan::Scan {
            table: "t".to_string(),
            schema: schema(),
        };
        assert!(matches!(
            execute(&plan, &mut m, &schema()),
            Err(QuernError::Type(_))
        ));
    }

    #[test]
    fn a_table_without_a_primary_key_skips_the_duplicate_check() {
        let mut s = schema();
        s.columns[0].primary_key = false;
        // pk: None makes lookup_pk an error, so reaching it would fail here.
        let mut m = Mock::new(None);
        let plan = ins(vec![
            vec![lit(int(1)), lit(txt("one"))],
            vec![lit(int(1)), lit(txt("dup"))],
        ]);
        assert_eq!(execute(&plan, &mut m, &s), Ok(2));
    }
}
