//! bead: quern-plan-physical — LogicalPlan -> Box<dyn Operator>
//!
//! [`build`] walks a lowered [`LogicalPlan`] bottom-up and hands each node to
//! the operator constructor that already exists for it. There is no rewriting
//! left to do: `plan::logical` resolved every column to `Expr::ColumnRef(i)`
//! and every operator takes its logical fields verbatim, so all but one arm of
//! the match is `Box::new(Op::new(build(input)?, field.clone()))`.
//!
//! # The one optimisation
//!
//! A `Filter` directly above a `Scan` whose predicate is `pk = <int literal>`
//! becomes a [`PkLookup`] — a B+tree probe yielding one row or none — instead
//! of materialising the whole table and testing every row. Everything else
//! lowers structurally. The pattern match is deliberately narrow (see
//! [`pk_equality`]): a wrong match here is a silent wrong answer, not a crash.
//!
//! # Storage borrows end inside the constructors
//!
//! `build` takes `&dyn Storage` only to hand to `Scan::new`/`PkLookup::new`,
//! which copy their rows out and let the borrow end (bead .35 — `Box<dyn
//! Operator>` is `'static`, and the trait is frozen). Nothing returned from
//! here still borrows storage.
//!
//! # DML and DDL are not operators
//!
//! `Insert`/`Update`/`Delete` go to `exec::dml::execute` and
//! `CreateTable`/`DropTable` straight to `Storage`; none of them is a row
//! source. `build` refuses them with `QuernError::Type` rather than inventing
//! an operator, so a mis-route from `plan::mod` is a clear error and never a
//! panic.

use crate::exec::aggregate::Aggregate;
use crate::exec::filter::Filter;
use crate::exec::join::Join;
use crate::exec::limit::Limit;
use crate::exec::project::Project;
use crate::exec::scan::Scan;
use crate::exec::sort::Sort;
use crate::exec::Operator;
use crate::plan::logical::LogicalPlan;
use crate::sql::ast::{BinOp, Expr};
use crate::storage::Storage;
use crate::types::{Column, QuernError, Result, Row, Schema, Type, Value};

/// Build the operator tree for a query plan.
///
/// The `Schema` every leaf needs travels in `LogicalPlan::Scan { schema }`
/// already (`plan::logical` put it there), so no `Catalog` argument is needed
/// — `plan::mod` needs its catalog for DML/DDL routing, not for this.
///
/// Errors surface at build time rather than mid-drain, because three operators
/// are eager: `Scan` and `PkLookup` materialise rows, `Join` drains its right
/// input, and `Aggregate` drains and groups its whole input.
pub fn build(plan: &LogicalPlan, storage: &dyn Storage) -> Result<Box<dyn Operator>> {
    match plan {
        LogicalPlan::Scan { table, schema } => Ok(Box::new(Scan::new(storage, table, schema)?)),

        LogicalPlan::Filter { input, predicate } => {
            if let LogicalPlan::Scan { table, schema } = input.as_ref() {
                if let Some(key) = pk_equality(predicate, schema) {
                    return Ok(Box::new(PkLookup::new(storage, table, schema, key)?));
                }
            }
            Ok(Box::new(Filter::new(
                build(input, storage)?,
                predicate.clone(),
            )))
        }

        LogicalPlan::Project { input, exprs } => Ok(Box::new(Project::new(
            build(input, storage)?,
            exprs.clone(),
        ))),

        LogicalPlan::Join { left, right, on } => Ok(Box::new(Join::new(
            build(left, storage)?,
            build(right, storage)?,
            on.clone(),
        )?)),

        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
        } => Ok(Box::new(Aggregate::new(
            build(input, storage)?,
            group_by,
            aggs,
        )?)),

        LogicalPlan::Sort { input, keys } => {
            Ok(Box::new(Sort::new(build(input, storage)?, keys.clone())))
        }

        LogicalPlan::Limit { input, n } => Ok(Box::new(Limit::new(build(input, storage)?, *n))),

        LogicalPlan::Insert { .. } | LogicalPlan::Update { .. } | LogicalPlan::Delete { .. } => {
            Err(not_an_operator("exec::dml::execute", plan))
        }
        LogicalPlan::CreateTable { .. } | LogicalPlan::DropTable { .. } => {
            Err(not_an_operator("Storage", plan))
        }
    }
}

/// The non-operator signal: `QuernError::Type`, naming the variant and where it
/// should have gone. `plan::mod` routes DML/DDL on the variant itself, so this
/// is the backstop for a mis-route, not the happy path.
fn not_an_operator(destination: &str, plan: &LogicalPlan) -> QuernError {
    let variant = match plan {
        LogicalPlan::Insert { .. } => "INSERT",
        LogicalPlan::Update { .. } => "UPDATE",
        LogicalPlan::Delete { .. } => "DELETE",
        LogicalPlan::CreateTable { .. } => "CREATE TABLE",
        LogicalPlan::DropTable { .. } => "DROP TABLE",
        _ => "statement",
    };
    QuernError::Type(format!(
        "{variant} produces no rows and has no operator: route it to {destination}"
    ))
}

/// `Some(k)` iff `predicate` is exactly `<pk column> = <int literal>` (either
/// way round) for this table's declared `INTEGER PRIMARY KEY`.
///
/// Everything else returns `None` and lowers as `Scan` + `Filter`. The
/// narrowness is the point, because each rejected case would otherwise be a
/// wrong answer rather than a slow one:
///
/// * `<`/`>`/`<>` — a point probe answers a different question.
/// * a non-`Int` literal — `Value::eq` makes `pk = '5'` an *error* (§1), and
///   `lookup_pk` would have silently answered it.
/// * column-to-column, or an expression like `pk = 2 + 3` — no constant to
///   probe with; constant folding is not this bead.
/// * a non-PK column, or a PK column that is not `Type::Int` — the index is
///   keyed by the INTEGER primary key and by nothing else.
fn pk_equality(predicate: &Expr, schema: &Schema) -> Option<i64> {
    let Expr::Binary {
        op: BinOp::Eq,
        left,
        right,
    } = predicate
    else {
        return None;
    };
    let pk = schema.primary_key()?;
    if schema.columns[pk].ty != Type::Int {
        return None;
    }
    match (left.as_ref(), right.as_ref()) {
        (Expr::ColumnRef(i), Expr::Literal(Value::Int(k)))
        | (Expr::Literal(Value::Int(k)), Expr::ColumnRef(i))
            if *i == pk =>
        {
            Some(*k)
        }
        _ => None,
    }
}

/// One-or-zero-row source: the row `Storage::lookup_pk` finds for one key.
///
/// The row is fetched in the constructor for the same reason `Scan`'s are — the
/// borrow of storage must end before the `Box<dyn Operator>` escapes.
struct PkLookup {
    schema: Vec<Column>,
    row: Option<Row>,
}

impl PkLookup {
    fn new(storage: &dyn Storage, table: &str, schema: &Schema, key: i64) -> Result<PkLookup> {
        Ok(PkLookup {
            schema: schema.columns.clone(),
            row: storage.lookup_pk(table, key)?.map(|(_id, row)| row),
        })
    }
}

impl Operator for PkLookup {
    fn schema(&self) -> &[Column] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>> {
        Ok(self.row.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{AggExpr, AggFunc};
    use crate::storage::Db;
    use tempfile::TempDir;

    fn col(name: &str, ty: Type, primary_key: bool) -> Column {
        Column {
            name: name.to_string(),
            ty,
            primary_key,
        }
    }

    /// `t(id INT PRIMARY KEY, b INT, s TEXT)`.
    fn schema() -> Schema {
        Schema {
            table: "t".to_string(),
            columns: vec![
                col("id", Type::Int, true),
                col("b", Type::Int, false),
                col("s", Type::Text, false),
            ],
        }
    }

    fn int(i: i64) -> Expr {
        Expr::Literal(Value::Int(i))
    }

    /// A `Db` in a fresh tempdir holding `t` with the given rows. The TempDir
    /// comes back so it outlives the Db.
    fn db(rows: &[(i64, i64, &str)]) -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let mut db = Db::open(dir.path()).unwrap();
        db.create_table(&schema()).unwrap();
        for (id, b, s) in rows {
            db.insert(
                "t",
                &vec![Value::Int(*id), Value::Int(*b), Value::Text(s.to_string())],
            )
            .unwrap();
        }
        (dir, db)
    }

    fn scan() -> LogicalPlan {
        LogicalPlan::Scan {
            table: "t".to_string(),
            schema: schema(),
        }
    }

    fn drain(plan: &LogicalPlan, storage: &dyn Storage) -> Vec<Row> {
        let mut op = build(plan, storage).unwrap();
        let mut out = Vec::new();
        while let Some(row) = op.next().unwrap() {
            out.push(row);
        }
        out
    }

    /// Just the INT columns of each row, so assertions stay readable.
    fn ints(rows: &[Row]) -> Vec<Vec<i64>> {
        rows.iter()
            .map(|r| {
                r.iter()
                    .filter_map(|v| match v {
                        Value::Int(i) => Some(*i),
                        _ => None,
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn scan_yields_every_row() {
        let (_dir, db) = db(&[(1, 10, "a"), (2, 20, "b")]);
        assert_eq!(ints(&drain(&scan(), &db)), vec![vec![1, 10], vec![2, 20]]);
    }

    #[test]
    fn filter_project_sort_limit_each_build_and_run() {
        let (_dir, db) = db(&[(1, 30, "a"), (2, 10, "b"), (3, 20, "c")]);

        // Filter: not a PK equality, so a real Filter over a real Scan.
        let filter = LogicalPlan::Filter {
            input: Box::new(scan()),
            predicate: Expr::Binary {
                op: BinOp::Gt,
                left: Box::new(Expr::ColumnRef(1)),
                right: Box::new(int(15)),
            },
        };
        assert_eq!(ints(&drain(&filter, &db)), vec![vec![1, 30], vec![3, 20]]);

        // Project: one expression per output column, in exprs order.
        let project = LogicalPlan::Project {
            input: Box::new(scan()),
            exprs: vec![(Expr::ColumnRef(1), "b".to_string())],
        };
        assert_eq!(
            ints(&drain(&project, &db)),
            vec![vec![30], vec![10], vec![20]]
        );

        // Sort: descending on the projected column.
        let sort = LogicalPlan::Sort {
            input: Box::new(project.clone()),
            keys: vec![(Expr::ColumnRef(0), true)],
        };
        assert_eq!(ints(&drain(&sort, &db)), vec![vec![30], vec![20], vec![10]]);

        // Limit: above the sort, two rows.
        let limit = LogicalPlan::Limit {
            input: Box::new(sort),
            n: 2,
        };
        assert_eq!(ints(&drain(&limit, &db)), vec![vec![30], vec![20]]);
    }

    #[test]
    fn join_builds_with_left_then_right_columns() {
        let (_dir, db) = db(&[(1, 10, "a"), (2, 20, "b")]);
        // t JOIN t ON left.id = right.id — right column j is index 3 + j.
        let join = LogicalPlan::Join {
            left: Box::new(scan()),
            right: Box::new(scan()),
            on: Expr::Binary {
                op: BinOp::Eq,
                left: Box::new(Expr::ColumnRef(0)),
                right: Box::new(Expr::ColumnRef(3)),
            },
        };
        assert_eq!(
            ints(&drain(&join, &db)),
            vec![vec![1, 10, 1, 10], vec![2, 20, 2, 20]]
        );
    }

    #[test]
    fn aggregate_builds_eagerly_and_emits_keys_then_aggs() {
        let (_dir, db) = db(&[(1, 10, "a"), (2, 10, "b"), (3, 20, "c")]);
        let agg = LogicalPlan::Aggregate {
            input: Box::new(scan()),
            group_by: vec![Expr::ColumnRef(1)],
            aggs: vec![AggExpr::count_star()],
        };
        // [key b, COUNT(*)] per group, groups ascending by key.
        assert_eq!(ints(&drain(&agg, &db)), vec![vec![10, 2], vec![20, 1]]);
    }

    #[test]
    fn the_full_nested_select_shape_builds_and_runs() {
        // SELECT b, COUNT(*) FROM t WHERE b > 5 GROUP BY b ORDER BY b DESC LIMIT 1
        let (_dir, db) = db(&[(1, 10, "a"), (2, 10, "b"), (3, 20, "c"), (4, 1, "d")]);
        let plan = LogicalPlan::Limit {
            input: Box::new(LogicalPlan::Sort {
                input: Box::new(LogicalPlan::Project {
                    input: Box::new(LogicalPlan::Aggregate {
                        input: Box::new(LogicalPlan::Filter {
                            input: Box::new(scan()),
                            predicate: Expr::Binary {
                                op: BinOp::Gt,
                                left: Box::new(Expr::ColumnRef(1)),
                                right: Box::new(int(5)),
                            },
                        }),
                        group_by: vec![Expr::ColumnRef(1)],
                        aggs: vec![AggExpr::of(AggFunc::Sum, Expr::ColumnRef(0))],
                    }),
                    exprs: vec![
                        (Expr::ColumnRef(0), "b".to_string()),
                        (Expr::ColumnRef(1), "SUM(id)".to_string()),
                    ],
                }),
                keys: vec![(Expr::ColumnRef(0), true)],
            }),
            n: 1,
        };
        // Groups b=10 (SUM(id)=3) and b=20 (SUM(id)=3); DESC, first only.
        assert_eq!(ints(&drain(&plan, &db)), vec![vec![20, 3]]);
    }

    #[test]
    fn a_pk_equality_over_a_scan_takes_the_lookup_and_answers_the_same() {
        let (_dir, db) = db(&[(1, 10, "a"), (5, 50, "e"), (9, 90, "i")]);
        for pred in [
            Expr::Binary {
                op: BinOp::Eq,
                left: Box::new(Expr::ColumnRef(0)),
                right: Box::new(int(5)),
            },
            // Literal on the left is the same lookup.
            Expr::Binary {
                op: BinOp::Eq,
                left: Box::new(int(5)),
                right: Box::new(Expr::ColumnRef(0)),
            },
        ] {
            assert_eq!(pk_equality(&pred, &schema()), Some(5));
            let plan = LogicalPlan::Filter {
                input: Box::new(scan()),
                predicate: pred,
            };
            let rows = drain(&plan, &db);
            assert_eq!(ints(&rows), vec![vec![5, 50]]);
            // Same shape as a Scan's rows: the lookup is a drop-in leaf.
            assert_eq!(build(&plan, &db).unwrap().schema(), schema().columns);
        }

        // A missing key is zero rows, and the drained lookup stays drained.
        let plan = LogicalPlan::Filter {
            input: Box::new(scan()),
            predicate: Expr::Binary {
                op: BinOp::Eq,
                left: Box::new(Expr::ColumnRef(0)),
                right: Box::new(int(4)),
            },
        };
        let mut op = build(&plan, &db).unwrap();
        assert_eq!(op.next(), Ok(None));
        assert_eq!(op.next(), Ok(None));
    }

    #[test]
    fn the_fast_path_is_not_taken_for_anything_but_a_pk_equality() {
        let s = schema();
        let pk = || Box::new(Expr::ColumnRef(0));
        let nonpk = || Box::new(Expr::ColumnRef(1));
        let cases: Vec<(&str, Expr)> = vec![
            (
                "non-PK column",
                Expr::Binary {
                    op: BinOp::Eq,
                    left: nonpk(),
                    right: Box::new(int(5)),
                },
            ),
            (
                "greater-than on the PK",
                Expr::Binary {
                    op: BinOp::Gt,
                    left: pk(),
                    right: Box::new(int(5)),
                },
            ),
            (
                "not-equal on the PK",
                Expr::Binary {
                    op: BinOp::Ne,
                    left: pk(),
                    right: Box::new(int(5)),
                },
            ),
            (
                "column to column",
                Expr::Binary {
                    op: BinOp::Eq,
                    left: pk(),
                    right: nonpk(),
                },
            ),
            (
                "non-Int literal against the PK",
                Expr::Binary {
                    op: BinOp::Eq,
                    left: pk(),
                    right: Box::new(Expr::Literal(Value::Text("5".to_string()))),
                },
            ),
            (
                "an unfolded constant expression",
                Expr::Binary {
                    op: BinOp::Eq,
                    left: pk(),
                    right: Box::new(Expr::Binary {
                        op: BinOp::Add,
                        left: Box::new(int(2)),
                        right: Box::new(int(3)),
                    }),
                },
            ),
            (
                "a conjunction that merely contains a PK equality",
                Expr::Binary {
                    op: BinOp::And,
                    left: Box::new(Expr::Binary {
                        op: BinOp::Eq,
                        left: pk(),
                        right: Box::new(int(5)),
                    }),
                    right: Box::new(Expr::Literal(Value::Bool(true))),
                },
            ),
        ];
        for (why, pred) in &cases {
            assert_eq!(pk_equality(pred, &s), None, "must not fast-path: {why}");
        }

        // A table with no PK never fast-paths, whatever the predicate.
        let mut no_pk = schema();
        no_pk.columns[0].primary_key = false;
        assert_eq!(
            pk_equality(
                &Expr::Binary {
                    op: BinOp::Eq,
                    left: pk(),
                    right: Box::new(int(5)),
                },
                &no_pk
            ),
            None
        );
    }

    /// A `Db` whose `scan` refuses to run, so "the fast path was taken" is
    /// observable instead of merely plausible: a plan that builds and answers
    /// through this cannot have scanned, and one that needs a scan errors.
    struct NoScan<'a>(&'a Db);

    impl Storage for NoScan<'_> {
        fn scan(&self, _table: &str) -> Result<Box<dyn Iterator<Item = Result<(u64, Row)>> + '_>> {
            Err(QuernError::Storage("scan attempted".to_string()))
        }
        fn lookup_pk(&self, table: &str, key: i64) -> Result<Option<(u64, Row)>> {
            self.0.lookup_pk(table, key)
        }
        fn create_table(&mut self, _schema: &Schema) -> Result<()> {
            unreachable!("read-only")
        }
        fn drop_table(&mut self, _table: &str) -> Result<()> {
            unreachable!("read-only")
        }
        fn insert(&mut self, _table: &str, _row: &Row) -> Result<u64> {
            unreachable!("read-only")
        }
        fn delete(&mut self, _table: &str, _id: u64) -> Result<()> {
            unreachable!("read-only")
        }
        fn update(&mut self, _table: &str, _id: u64, _row: &Row) -> Result<()> {
            unreachable!("read-only")
        }
        fn begin(&mut self) -> Result<()> {
            unreachable!("read-only")
        }
        fn commit(&mut self) -> Result<()> {
            unreachable!("read-only")
        }
        fn rollback(&mut self) -> Result<()> {
            unreachable!("read-only")
        }
    }

    /// `WHERE <column i> = k` directly above the scan of `t`.
    fn eq_filter(i: usize, k: i64) -> LogicalPlan {
        LogicalPlan::Filter {
            input: Box::new(scan()),
            predicate: Expr::Binary {
                op: BinOp::Eq,
                left: Box::new(Expr::ColumnRef(i)),
                right: Box::new(int(k)),
            },
        }
    }

    #[test]
    fn the_fast_path_really_replaces_the_scan_and_only_for_the_pk() {
        let (_dir, db) = db(&[(1, 10, "a"), (5, 50, "e")]);
        let no_scan = NoScan(&db);

        // `id = 5` answers with the scan unavailable: it is a lookup, not a
        // scan plus a filter.
        assert_eq!(ints(&drain(&eq_filter(0, 5), &no_scan)), vec![vec![5, 50]]);

        // `b = 50` is not the PK, so it must still be a Scan — which this
        // storage refuses. If the pattern match ever widens, this fails here
        // rather than silently returning a row from the wrong column.
        assert!(matches!(
            build(&eq_filter(1, 50), &no_scan),
            Err(QuernError::Storage(m)) if m == "scan attempted"
        ));
    }

    /// The wrong-answer test: with `id` and `b` swapped between the rows, a
    /// `b = 1` predicate that wrongly probed the PK index would return the
    /// other row entirely, not merely a slow-but-right answer.
    #[test]
    fn a_non_pk_equality_still_answers_from_the_scan() {
        let (_dir, db) = db(&[(1, 2, "a"), (2, 1, "b")]);
        let plan = LogicalPlan::Filter {
            input: Box::new(scan()),
            predicate: Expr::Binary {
                op: BinOp::Eq,
                left: Box::new(Expr::ColumnRef(1)),
                right: Box::new(int(1)),
            },
        };
        assert_eq!(ints(&drain(&plan, &db)), vec![vec![2, 1]]);
    }

    /// A Filter that is not *directly* above a Scan keeps its Filter, even when
    /// the predicate looks like a PK equality: the index space above a Project
    /// or a Join is not the table's.
    #[test]
    fn only_a_filter_directly_above_a_scan_is_a_candidate() {
        let (_dir, db) = db(&[(1, 10, "a"), (5, 50, "e")]);
        let plan = LogicalPlan::Filter {
            // Project reverses the columns, so ColumnRef(0) is `b`, not `id`.
            input: Box::new(LogicalPlan::Project {
                input: Box::new(scan()),
                exprs: vec![
                    (Expr::ColumnRef(1), "b".to_string()),
                    (Expr::ColumnRef(0), "id".to_string()),
                ],
            }),
            predicate: Expr::Binary {
                op: BinOp::Eq,
                left: Box::new(Expr::ColumnRef(0)),
                right: Box::new(int(50)),
            },
        };
        assert_eq!(ints(&drain(&plan, &db)), vec![vec![50, 5]]);
    }

    #[test]
    fn dml_and_ddl_are_refused_rather_than_built() {
        let (_dir, db) = db(&[]);
        let plans = [
            LogicalPlan::Insert {
                table: "t".to_string(),
                rows: vec![vec![int(1), int(2), Expr::Literal(Value::Text("a".into()))]],
            },
            LogicalPlan::Update {
                table: "t".to_string(),
                sets: vec![("b".to_string(), int(1))],
                predicate: None,
            },
            LogicalPlan::Delete {
                table: "t".to_string(),
                predicate: None,
            },
            LogicalPlan::CreateTable { schema: schema() },
            LogicalPlan::DropTable {
                table: "t".to_string(),
            },
        ];
        for plan in &plans {
            match build(plan, &db) {
                Err(QuernError::Type(m)) => assert!(
                    m.contains("no operator"),
                    "unexpected message for {plan:?}: {m}"
                ),
                other => panic!(
                    "expected a Type error for {plan:?}, got {:?}",
                    other.is_ok()
                ),
            }
        }
    }
}
