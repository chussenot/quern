//! bead: quern-plan-logical — HOT: LogicalPlan. docs/quern.md §3
//!
//! The frozen `LogicalPlan` of §3, plus the lowering that produces it. This is
//! where **name resolution happens, exactly once**: every `Expr::Column
//! { table, name }` the parsers produced is rewritten into `Expr::ColumnRef(i)`
//! against the input schema of the node the expression hangs on, so no operator
//! ever does a name lookup per row. An unknown or ambiguous name is
//! `QuernError::Catalog` here, not a runtime surprise there.
//!
//! Two contracts the rest of the engine is written against:
//!
//! * **Join index space is left columns then right columns**, concatenated. A
//!   qualified `t.a` resolves within its own side; an unqualified `a` present on
//!   both sides is ambiguous and an error.
//! * **An `Aggregate` row is group keys first, then aggregates** —
//!   `group_by.len() + aggs.len()` values, in those two orders (bead .39). So a
//!   `Project` sits directly above every `Aggregate` and resolves against *that*
//!   shape, never the table's. Empty `group_by` is one group over all rows.
//!
//! Nesting order for a `SELECT`, innermost first:
//!
//! ```text
//! Scan | Join(Scan, Scan)  ->  Filter  ->  Aggregate  ->  Project  ->  Sort  ->  Limit
//!                               WHERE       GROUP BY      projection   ORDER BY   LIMIT
//! ```
//!
//! `Sort` is above `Project`, so `ORDER BY` resolves against the *projected*
//! output: `ORDER BY n` finds `b + 1 AS n`, and `ORDER BY COUNT(*)` finds the
//! projected aggregate. The flip side, and a deliberate limitation: a key that
//! was not projected (`SELECT a FROM t ORDER BY b`) is an unknown column.
//!
//! `SelectItem::Star` is expanded here into one `Project` expression per input
//! column; the star never reaches an operator. `Insert` is normalised into full
//! schema-ordered rows (bead .42), so `exec::dml` never sees a column list.
//! `BEGIN`/`COMMIT`/`ROLLBACK` have no `LogicalPlan` variant and are rejected —
//! transaction control is handled before planning.

use crate::catalog::Catalog;
use crate::sql::ast::{AggExpr, Expr, Join, SelectItem, SelectStmt, Statement};
use crate::types::{QuernError, Result, Schema, Value};

/// The logical plan tree, exactly as frozen in docs/quern.md §3. Every `Expr`
/// in here is resolved: `ColumnRef`, never `Column`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalPlan {
    Scan {
        table: String,
        schema: Schema,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: Expr,
    },
    Project {
        input: Box<LogicalPlan>,
        exprs: Vec<(Expr, String)>,
    },
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        on: Expr,
    },
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<Expr>,
        aggs: Vec<AggExpr>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<(Expr, bool)>,
    },
    Limit {
        input: Box<LogicalPlan>,
        n: usize,
    },
    Insert {
        table: String,
        rows: Vec<Vec<Expr>>,
    },
    Update {
        table: String,
        sets: Vec<(String, Expr)>,
        predicate: Option<Expr>,
    },
    Delete {
        table: String,
        predicate: Option<Expr>,
    },
    CreateTable {
        schema: Schema,
    },
    DropTable {
        table: String,
    },
}

/// Lower a parsed statement into a resolved logical plan.
///
/// Table names are looked up in `catalog` (case-insensitively, per §1) and the
/// canonical spelling from the `Schema` is what lands in the plan, so storage
/// keys agree no matter how the user typed the name.
pub fn lower(stmt: &Statement, catalog: &Catalog) -> Result<LogicalPlan> {
    match stmt {
        Statement::Select(select) => lower_select(select, catalog),
        Statement::Insert {
            table,
            columns,
            rows,
        } => lower_insert(table, columns.as_deref(), rows, catalog),
        Statement::Update {
            table,
            sets,
            predicate,
        } => {
            let schema = catalog.get(table)?;
            let scope = Scope::from_schema(schema);
            let mut resolved = Vec::with_capacity(sets.len());
            for (name, value) in sets {
                let i = schema.column_index(name).ok_or_else(|| {
                    QuernError::Catalog(format!("unknown column: {}.{name}", schema.table))
                })?;
                // Canonical spelling, so exec's one-off name lookup cannot miss.
                resolved.push((schema.columns[i].name.clone(), resolve_expr(value, &scope)?));
            }
            Ok(LogicalPlan::Update {
                table: schema.table.clone(),
                sets: resolved,
                predicate: resolve_opt(predicate.as_ref(), &scope)?,
            })
        }
        Statement::Delete { table, predicate } => {
            let schema = catalog.get(table)?;
            let scope = Scope::from_schema(schema);
            Ok(LogicalPlan::Delete {
                table: schema.table.clone(),
                predicate: resolve_opt(predicate.as_ref(), &scope)?,
            })
        }
        Statement::CreateTable { schema } => Ok(LogicalPlan::CreateTable {
            schema: schema.clone(),
        }),
        Statement::DropTable { table } => Ok(LogicalPlan::DropTable {
            // Fails here rather than at execution, and carries the canonical name.
            table: catalog.get(table)?.table.clone(),
        }),
        Statement::Begin | Statement::Commit | Statement::Rollback => Err(QuernError::Txn(
            "transaction control is not a plannable statement".to_string(),
        )),
    }
}

// --- name resolution --------------------------------------------------------

/// The shape an `Expr` resolves against: one `(qualifier, name)` per column, in
/// index order. `qualifier` is the table the column came from, `None` for a
/// derived column. `label` is what a miss is called in the error message.
struct Scope {
    cols: Vec<(Option<String>, String)>,
    label: &'static str,
}

impl Scope {
    fn from_schema(schema: &Schema) -> Scope {
        Scope {
            cols: schema
                .columns
                .iter()
                .map(|c| (Some(schema.table.clone()), c.name.clone()))
                .collect(),
            label: "column",
        }
    }

    /// Left columns then right columns — the join index space.
    fn concat(mut self, right: Scope) -> Scope {
        self.cols.extend(right.cols);
        self
    }

    fn resolve(&self, table: Option<&str>, name: &str) -> Result<usize> {
        let mut hit = None;
        for (i, (qualifier, col)) in self.cols.iter().enumerate() {
            if !col.eq_ignore_ascii_case(name) {
                continue;
            }
            if let Some(t) = table {
                match qualifier {
                    Some(q) if q.eq_ignore_ascii_case(t) => {}
                    _ => continue,
                }
            }
            if hit.is_some() {
                return Err(QuernError::Catalog(format!(
                    "ambiguous {}: {}",
                    self.label,
                    display(table, name)
                )));
            }
            hit = Some(i);
        }
        hit.ok_or_else(|| {
            QuernError::Catalog(format!("unknown {}: {}", self.label, display(table, name)))
        })
    }

    /// Name-only match, used by `ORDER BY` against the projected output so a key
    /// written as the whole expression (`COUNT(*)`, `b + 1`) finds its column.
    fn position_by_name(&self, name: &str) -> Option<usize> {
        self.cols
            .iter()
            .position(|(_, n)| n.eq_ignore_ascii_case(name))
    }
}

fn display(table: Option<&str>, name: &str) -> String {
    match table {
        Some(t) => format!("{t}.{name}"),
        None => name.to_string(),
    }
}

/// Rewrite every `Column` into a `ColumnRef` against `scope`. An aggregate here
/// is a bug in the input: quern has no `HAVING`, so `WHERE`, `ON` and `GROUP BY`
/// cannot contain one.
fn resolve_expr(e: &Expr, scope: &Scope) -> Result<Expr> {
    Ok(match e {
        Expr::Literal(v) => Expr::Literal(v.clone()),
        Expr::Column { table, name } => Expr::ColumnRef(scope.resolve(table.as_deref(), name)?),
        Expr::ColumnRef(i) => Expr::ColumnRef(*i),
        Expr::Binary { op, left, right } => {
            Expr::bin(resolve_expr(left, scope)?, *op, resolve_expr(right, scope)?)
        }
        Expr::Unary { op, expr } => Expr::un(*op, resolve_expr(expr, scope)?),
        Expr::Agg(a) => {
            return Err(QuernError::Parse(format!(
                "aggregate {a} is only allowed in a SELECT list"
            )))
        }
    })
}

fn resolve_opt(e: Option<&Expr>, scope: &Scope) -> Result<Option<Expr>> {
    match e {
        None => Ok(None),
        Some(e) => Ok(Some(resolve_expr(e, scope)?)),
    }
}

/// Lift every aggregate call out of a projection expression into `aggs`, leaving
/// a `ColumnRef` into the `Aggregate` output row behind. Arguments resolve
/// against the aggregate's *input* (`in_scope`); everything else against its
/// *output* group keys (`group_scope`), which is what makes a projected column
/// that is not a `GROUP BY` key an error.
fn lift_aggs(
    e: &Expr,
    in_scope: &Scope,
    group_scope: &Scope,
    aggs: &mut Vec<AggExpr>,
    base: usize,
) -> Result<Expr> {
    Ok(match e {
        Expr::Agg(a) => {
            let arg = match &a.arg {
                Some(arg) => Some(resolve_expr(arg, in_scope)?),
                None => None,
            };
            aggs.push(AggExpr {
                func: a.func,
                arg,
                alias: a.alias.clone(),
            });
            Expr::ColumnRef(base + aggs.len() - 1)
        }
        Expr::Binary { op, left, right } => Expr::bin(
            lift_aggs(left, in_scope, group_scope, aggs, base)?,
            *op,
            lift_aggs(right, in_scope, group_scope, aggs, base)?,
        ),
        Expr::Unary { op, expr } => {
            Expr::un(*op, lift_aggs(expr, in_scope, group_scope, aggs, base)?)
        }
        // Literal / Column / ColumnRef: leaves, so this cannot see an Agg.
        leaf => resolve_expr(leaf, group_scope)?,
    })
}

// --- SELECT -----------------------------------------------------------------

fn lower_select(select: &SelectStmt, catalog: &Catalog) -> Result<LogicalPlan> {
    let left = catalog.get(&select.from)?;
    let mut scope = Scope::from_schema(left);
    let mut plan = LogicalPlan::Scan {
        table: left.table.clone(),
        schema: left.clone(),
    };

    if let Some(Join { table, on }) = &select.join {
        let right = catalog.get(table)?;
        scope = scope.concat(Scope::from_schema(right));
        plan = LogicalPlan::Join {
            left: Box::new(plan),
            right: Box::new(LogicalPlan::Scan {
                table: right.table.clone(),
                schema: right.clone(),
            }),
            on: resolve_expr(on, &scope)?,
        };
    }

    if let Some(predicate) = &select.predicate {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: resolve_expr(predicate, &scope)?,
        };
    }

    // `*` expands against the input, qualifier included, so a join's duplicate
    // names stay resolvable.
    let mut items: Vec<(Expr, String)> = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        match item {
            SelectItem::Star => items.extend(scope.cols.iter().map(|(q, n)| {
                (
                    Expr::Column {
                        table: q.clone(),
                        name: n.clone(),
                    },
                    n.clone(),
                )
            })),
            SelectItem::Expr { expr, .. } => items.push((
                expr.clone(),
                item.output_name().unwrap_or_else(|| expr.to_string()),
            )),
        }
    }

    // GROUP BY, or a bare aggregate (one group over all rows).
    let needs_aggregate =
        !select.group_by.is_empty() || items.iter().any(|(e, _)| e.contains_agg());

    let (exprs, project_input) = if needs_aggregate {
        let mut group_by = Vec::with_capacity(select.group_by.len());
        let mut keys = Vec::with_capacity(select.group_by.len());
        for key in &select.group_by {
            let resolved = resolve_expr(key, &scope)?;
            // A key that is a plain column keeps its source qualifier, so
            // `GROUP BY t.b` is still addressable as `t.b` above the Aggregate.
            keys.push(match (&resolved, key) {
                (Expr::ColumnRef(i), Expr::Column { .. }) => scope.cols[*i].clone(),
                _ => (None, key.to_string()),
            });
            group_by.push(resolved);
        }
        let group_scope = Scope {
            cols: keys,
            label: "GROUP BY key",
        };

        let base = group_by.len();
        let mut aggs = Vec::new();
        let mut exprs = Vec::with_capacity(items.len());
        for (expr, name) in &items {
            exprs.push((
                lift_aggs(expr, &scope, &group_scope, &mut aggs, base)?,
                name.clone(),
            ));
        }
        plan = LogicalPlan::Aggregate {
            input: Box::new(plan),
            group_by,
            aggs,
        };
        (exprs, group_scope)
    } else {
        let mut exprs = Vec::with_capacity(items.len());
        for (expr, name) in &items {
            exprs.push((resolve_expr(expr, &scope)?, name.clone()));
        }
        (exprs, scope)
    };

    // ORDER BY resolves against the projected output, so build its scope before
    // `exprs` is moved into the Project.
    let out_scope = Scope {
        cols: exprs
            .iter()
            .zip(&items)
            .map(|((resolved, name), (source, _))| match (resolved, source) {
                (Expr::ColumnRef(i), Expr::Column { .. }) => {
                    (project_input.cols[*i].0.clone(), name.clone())
                }
                _ => (None, name.clone()),
            })
            .collect(),
        label: "output column",
    };
    plan = LogicalPlan::Project {
        input: Box::new(plan),
        exprs,
    };

    if !select.order_by.is_empty() {
        let mut keys = Vec::with_capacity(select.order_by.len());
        for (key, descending) in &select.order_by {
            // Whole-expression match first: `ORDER BY n`, `ORDER BY COUNT(*)`.
            let resolved = match out_scope.position_by_name(&key.to_string()) {
                Some(i) => Expr::ColumnRef(i),
                None => resolve_expr(key, &out_scope)?,
            };
            keys.push((resolved, *descending));
        }
        plan = LogicalPlan::Sort {
            input: Box::new(plan),
            keys,
        };
    }

    if let Some(n) = select.limit {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            n,
        };
    }

    Ok(plan)
}

// --- INSERT -----------------------------------------------------------------

/// Normalise every row into full, schema-ordered form (bead .42): omitted
/// columns become `NULL`, an explicit list reorders, and the primary key may not
/// be omitted. `exec::dml` therefore never sees a column list.
fn lower_insert(
    table: &str,
    columns: Option<&[String]>,
    rows: &[Vec<Expr>],
    catalog: &Catalog,
) -> Result<LogicalPlan> {
    let schema = catalog.get(table)?;
    let width = schema.columns.len();

    let targets: Vec<usize> = match columns {
        None => (0..width).collect(),
        Some(names) => {
            let mut targets = Vec::with_capacity(names.len());
            for name in names {
                let i = schema.column_index(name).ok_or_else(|| {
                    QuernError::Catalog(format!("unknown column: {}.{name}", schema.table))
                })?;
                if targets.contains(&i) {
                    return Err(QuernError::Catalog(format!(
                        "column {name} named twice in INSERT"
                    )));
                }
                targets.push(i);
            }
            targets
        }
    };

    if let Some(pk) = schema.primary_key() {
        if !targets.contains(&pk) {
            return Err(QuernError::Catalog(format!(
                "INSERT omits primary key column {}",
                schema.columns[pk].name
            )));
        }
    }

    // VALUES cannot reference columns, so resolve against nothing at all.
    let nothing = Scope {
        cols: Vec::new(),
        label: "column",
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != targets.len() {
            return Err(QuernError::Type(format!(
                "INSERT has {} values for {} columns",
                row.len(),
                targets.len()
            )));
        }
        let mut full = vec![Expr::Literal(Value::Null); width];
        for (value, &i) in row.iter().zip(&targets) {
            full[i] = resolve_expr(value, &nothing)?;
        }
        out.push(full);
    }

    Ok(LogicalPlan::Insert {
        table: schema.table.clone(),
        rows: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{AggFunc, BinOp, UnOp};
    use crate::types::{Column, Type};

    /// t(a INT PK, b TEXT, c BOOL) and u(a INT PK, d TEXT) — `a` is deliberately
    /// on both sides so ambiguity is testable.
    fn cat() -> Catalog {
        let mut c = Catalog::new();
        c.create(Schema {
            table: "t".into(),
            columns: vec![
                Column {
                    name: "a".into(),
                    ty: Type::Int,
                    primary_key: true,
                },
                Column {
                    name: "b".into(),
                    ty: Type::Text,
                    primary_key: false,
                },
                Column {
                    name: "c".into(),
                    ty: Type::Bool,
                    primary_key: false,
                },
            ],
        })
        .unwrap();
        c.create(Schema {
            table: "u".into(),
            columns: vec![
                Column {
                    name: "a".into(),
                    ty: Type::Int,
                    primary_key: true,
                },
                Column {
                    name: "d".into(),
                    ty: Type::Text,
                    primary_key: false,
                },
            ],
        })
        .unwrap();
        c
    }

    fn select(s: SelectStmt) -> Result<LogicalPlan> {
        lower(&Statement::Select(s), &cat())
    }

    fn project_exprs(plan: &LogicalPlan) -> &[(Expr, String)] {
        match plan {
            LogicalPlan::Project { exprs, .. } => exprs,
            other => panic!("expected Project, got {other:?}"),
        }
    }

    #[test]
    fn columns_resolve_to_indices_qualified_and_not() {
        // SELECT b, t.a FROM T  — table name case-insensitive, per §1.
        let plan = select(SelectStmt {
            projection: vec![
                SelectItem::expr(Expr::col("b")),
                SelectItem::expr(Expr::qcol("t", "a")),
            ],
            from: "T".into(),
            ..Default::default()
        })
        .unwrap();

        let exprs = project_exprs(&plan);
        assert_eq!(exprs[0].0, Expr::ColumnRef(1));
        assert_eq!(exprs[1].0, Expr::ColumnRef(0));
        // The canonical spelling from the catalog is what reaches storage.
        match &plan {
            LogicalPlan::Project { input, .. } => assert_eq!(
                **input,
                LogicalPlan::Scan {
                    table: "t".into(),
                    schema: cat().get("t").unwrap().clone()
                }
            ),
            other => panic!("expected Project, got {other:?}"),
        }
    }

    #[test]
    fn join_index_space_is_left_then_right() {
        // SELECT t.a, u.d FROM t JOIN u ON t.a = u.a
        let plan = select(SelectStmt {
            projection: vec![
                SelectItem::expr(Expr::qcol("t", "a")),
                SelectItem::expr(Expr::qcol("u", "d")),
            ],
            from: "t".into(),
            join: Some(Join {
                table: "u".into(),
                on: Expr::bin(Expr::qcol("t", "a"), BinOp::Eq, Expr::qcol("u", "a")),
            }),
            ..Default::default()
        })
        .unwrap();

        let exprs = project_exprs(&plan);
        assert_eq!(exprs[0].0, Expr::ColumnRef(0), "t.a is left column 0");
        assert_eq!(exprs[1].0, Expr::ColumnRef(4), "u.d is 3 + 1");
        match &plan {
            LogicalPlan::Project { input, .. } => match &**input {
                LogicalPlan::Join { on, .. } => assert_eq!(
                    *on,
                    Expr::bin(Expr::ColumnRef(0), BinOp::Eq, Expr::ColumnRef(3)),
                    "ON resolves in the combined space"
                ),
                other => panic!("expected Join, got {other:?}"),
            },
            other => panic!("expected Project, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_ambiguous_columns_are_catalog_errors() {
        let unknown = select(SelectStmt {
            projection: vec![SelectItem::expr(Expr::col("nope"))],
            from: "t".into(),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(
            unknown,
            QuernError::Catalog("unknown column: nope".to_string())
        );

        // A qualifier that is not in scope is just as unknown.
        assert!(matches!(
            select(SelectStmt {
                projection: vec![SelectItem::expr(Expr::qcol("u", "a"))],
                from: "t".into(),
                ..Default::default()
            }),
            Err(QuernError::Catalog(_))
        ));

        // Bare `a` after a join: on both sides, so ambiguous.
        let ambiguous = select(SelectStmt {
            projection: vec![SelectItem::expr(Expr::col("a"))],
            from: "t".into(),
            join: Some(Join {
                table: "u".into(),
                on: Expr::bin(Expr::qcol("t", "a"), BinOp::Eq, Expr::qcol("u", "a")),
            }),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(
            ambiguous,
            QuernError::Catalog("ambiguous column: a".to_string())
        );

        // Unknown table, and an aggregate where there is no HAVING.
        assert!(matches!(
            select(SelectStmt {
                projection: vec![SelectItem::Star],
                from: "gone".into(),
                ..Default::default()
            }),
            Err(QuernError::Catalog(_))
        ));
        assert!(matches!(
            select(SelectStmt {
                projection: vec![SelectItem::Star],
                from: "t".into(),
                predicate: Some(Expr::bin(
                    Expr::agg(AggExpr::count_star()),
                    BinOp::Gt,
                    Expr::Literal(Value::Int(1))
                )),
                ..Default::default()
            }),
            Err(QuernError::Parse(_))
        ));
    }

    #[test]
    fn star_expands_to_one_expr_per_column() {
        let plan = select(SelectStmt {
            projection: vec![SelectItem::Star],
            from: "t".into(),
            ..Default::default()
        })
        .unwrap();
        let exprs = project_exprs(&plan);
        assert_eq!(
            exprs,
            &[
                (Expr::ColumnRef(0), "a".to_string()),
                (Expr::ColumnRef(1), "b".to_string()),
                (Expr::ColumnRef(2), "c".to_string()),
            ]
        );

        // Across a join: left columns then right, duplicate `a` and all.
        let joined = select(SelectStmt {
            projection: vec![SelectItem::Star],
            from: "t".into(),
            join: Some(Join {
                table: "u".into(),
                on: Expr::bin(Expr::qcol("t", "a"), BinOp::Eq, Expr::qcol("u", "a")),
            }),
            ..Default::default()
        })
        .unwrap();
        let exprs: Vec<Expr> = project_exprs(&joined)
            .iter()
            .map(|(e, _)| e.clone())
            .collect();
        assert_eq!(
            exprs,
            (0..5).map(Expr::ColumnRef).collect::<Vec<_>>(),
            "no star survives, and nothing is dropped"
        );
    }

    #[test]
    fn every_clause_nests_in_spec_order() {
        // SELECT t.b, COUNT(*) AS n FROM t JOIN u ON t.a = u.a
        //   WHERE NOT t.c GROUP BY t.b ORDER BY n DESC LIMIT 5
        let plan = select(SelectStmt {
            projection: vec![
                SelectItem::expr(Expr::qcol("t", "b")),
                SelectItem::aliased(Expr::agg(AggExpr::count_star().with_alias("n")), "n"),
            ],
            from: "t".into(),
            join: Some(Join {
                table: "u".into(),
                on: Expr::bin(Expr::qcol("t", "a"), BinOp::Eq, Expr::qcol("u", "a")),
            }),
            predicate: Some(Expr::un(UnOp::Not, Expr::qcol("t", "c"))),
            group_by: vec![Expr::qcol("t", "b")],
            order_by: vec![(Expr::col("n"), true)],
            limit: Some(5),
        })
        .unwrap();

        // Limit -> Sort -> Project -> Aggregate -> Filter -> Join -> Scan, Scan
        let LogicalPlan::Limit { input, n } = &plan else {
            panic!("outermost node is Limit, got {plan:?}")
        };
        assert_eq!(*n, 5);
        let LogicalPlan::Sort { input, keys } = &**input else {
            panic!("expected Sort")
        };
        assert_eq!(
            keys,
            &[(Expr::ColumnRef(1), true)],
            "ORDER BY n is projected output column 1, descending"
        );
        let LogicalPlan::Project { input, exprs } = &**input else {
            panic!("expected Project")
        };
        assert_eq!(exprs[0], (Expr::ColumnRef(0), "t.b".to_string()));
        assert_eq!(exprs[1], (Expr::ColumnRef(1), "n".to_string()));
        let LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
        } = &**input
        else {
            panic!("expected Aggregate")
        };
        assert_eq!(group_by, &[Expr::ColumnRef(1)], "t.b in the join space");
        assert_eq!(aggs.len(), 1);
        assert!(aggs[0].is_count_star());
        let LogicalPlan::Filter { input, predicate } = &**input else {
            panic!("expected Filter")
        };
        assert_eq!(*predicate, Expr::un(UnOp::Not, Expr::ColumnRef(2)));
        let LogicalPlan::Join { left, right, .. } = &**input else {
            panic!("expected Join")
        };
        assert!(matches!(**left, LogicalPlan::Scan { .. }));
        assert!(matches!(**right, LogicalPlan::Scan { .. }));

        // No clauses at all: just Project over Scan, nothing invented.
        let bare = select(SelectStmt {
            projection: vec![SelectItem::Star],
            from: "t".into(),
            ..Default::default()
        })
        .unwrap();
        match &bare {
            LogicalPlan::Project { input, .. } => {
                assert!(matches!(**input, LogicalPlan::Scan { .. }))
            }
            other => panic!("expected Project over Scan, got {other:?}"),
        }
    }

    #[test]
    fn aggregate_output_is_group_keys_then_aggs() {
        // SELECT SUM(a), b, COUNT(*) FROM t GROUP BY b — deliberately not in
        // key order, so the Project has to permute.
        let plan = select(SelectStmt {
            projection: vec![
                SelectItem::expr(Expr::agg(AggExpr::of(AggFunc::Sum, Expr::col("a")))),
                SelectItem::expr(Expr::col("b")),
                SelectItem::expr(Expr::agg(AggExpr::count_star())),
            ],
            from: "t".into(),
            group_by: vec![Expr::col("b")],
            ..Default::default()
        })
        .unwrap();

        let exprs = project_exprs(&plan);
        assert_eq!(
            exprs.iter().map(|(e, _)| e.clone()).collect::<Vec<_>>(),
            vec![
                Expr::ColumnRef(1), // SUM(a): 1 key, then agg 0
                Expr::ColumnRef(0), // b: group key 0
                Expr::ColumnRef(2), // COUNT(*): agg 1
            ],
            "keys first, then aggs in aggs order"
        );

        let LogicalPlan::Project { input, .. } = &plan else {
            panic!("expected Project")
        };
        let LogicalPlan::Aggregate {
            group_by,
            aggs,
            input,
        } = &**input
        else {
            panic!("expected Aggregate directly under Project")
        };
        assert_eq!(group_by, &[Expr::ColumnRef(1)]);
        assert_eq!(aggs[0].func, AggFunc::Sum);
        assert_eq!(aggs[0].arg, Some(Expr::ColumnRef(0)), "SUM's arg is t.a");
        assert!(aggs[1].is_count_star());
        assert!(matches!(**input, LogicalPlan::Scan { .. }));

        // A bare aggregate with no GROUP BY is still one Aggregate node.
        let bare = select(SelectStmt {
            projection: vec![SelectItem::expr(Expr::agg(AggExpr::count_star()))],
            from: "t".into(),
            ..Default::default()
        })
        .unwrap();
        let LogicalPlan::Project { input, exprs } = &bare else {
            panic!("expected Project")
        };
        assert_eq!(
            exprs[0].0,
            Expr::ColumnRef(0),
            "no keys, so agg 0 is index 0"
        );
        assert!(matches!(
            &**input,
            LogicalPlan::Aggregate { group_by, .. } if group_by.is_empty()
        ));

        // A projected column that is not a group key cannot resolve.
        assert!(matches!(
            select(SelectStmt {
                projection: vec![
                    SelectItem::expr(Expr::col("a")),
                    SelectItem::expr(Expr::agg(AggExpr::count_star())),
                ],
                from: "t".into(),
                group_by: vec![Expr::col("b")],
                ..Default::default()
            }),
            Err(QuernError::Catalog(_))
        ));
    }

    #[test]
    fn order_by_resolves_against_the_projection() {
        // SELECT b + 1 AS n FROM t ORDER BY n
        let plan = select(SelectStmt {
            projection: vec![SelectItem::aliased(
                Expr::bin(Expr::col("b"), BinOp::Add, Expr::Literal(Value::Int(1))),
                "n",
            )],
            from: "t".into(),
            order_by: vec![(Expr::col("n"), false)],
            ..Default::default()
        })
        .unwrap();
        let LogicalPlan::Sort { keys, .. } = &plan else {
            panic!("expected Sort, got {plan:?}")
        };
        assert_eq!(keys, &[(Expr::ColumnRef(0), false)]);

        // ORDER BY on the aggregate as written, and on a qualified group key.
        let grouped = select(SelectStmt {
            projection: vec![
                SelectItem::expr(Expr::qcol("t", "b")),
                SelectItem::expr(Expr::agg(AggExpr::count_star())),
            ],
            from: "t".into(),
            group_by: vec![Expr::qcol("t", "b")],
            order_by: vec![
                (Expr::agg(AggExpr::count_star()), true),
                (Expr::qcol("t", "b"), false),
            ],
            ..Default::default()
        })
        .unwrap();
        let LogicalPlan::Sort { keys, .. } = &grouped else {
            panic!("expected Sort")
        };
        assert_eq!(
            keys,
            &[(Expr::ColumnRef(1), true), (Expr::ColumnRef(0), false)]
        );

        // A key that was not projected is an error, not a silent re-scan.
        assert!(matches!(
            select(SelectStmt {
                projection: vec![SelectItem::expr(Expr::col("a"))],
                from: "t".into(),
                order_by: vec![(Expr::col("b"), false)],
                ..Default::default()
            }),
            Err(QuernError::Catalog(_))
        ));
    }

    #[test]
    fn insert_column_list_reorders_and_null_fills() {
        // INSERT INTO t (c, a) VALUES (TRUE, 1) — schema order is a, b, c.
        let plan = lower(
            &Statement::Insert {
                table: "t".into(),
                columns: Some(vec!["c".into(), "a".into()]),
                rows: vec![vec![
                    Expr::Literal(Value::Bool(true)),
                    Expr::Literal(Value::Int(1)),
                ]],
            },
            &cat(),
        )
        .unwrap();
        assert_eq!(
            plan,
            LogicalPlan::Insert {
                table: "t".into(),
                rows: vec![vec![
                    Expr::Literal(Value::Int(1)),
                    Expr::Literal(Value::Null), // b omitted
                    Expr::Literal(Value::Bool(true)),
                ]],
            }
        );

        // Positional form is already schema-ordered.
        let positional = lower(
            &Statement::Insert {
                table: "t".into(),
                columns: None,
                rows: vec![vec![
                    Expr::Literal(Value::Int(2)),
                    Expr::Literal(Value::Text("x".into())),
                    Expr::Literal(Value::Null),
                ]],
            },
            &cat(),
        )
        .unwrap();
        let LogicalPlan::Insert { rows, .. } = &positional else {
            panic!("expected Insert")
        };
        assert_eq!(rows[0][1], Expr::Literal(Value::Text("x".into())));

        // Arity, unknown column, duplicate column, missing PK: all errors.
        for bad in [
            Statement::Insert {
                table: "t".into(),
                columns: None,
                rows: vec![vec![Expr::Literal(Value::Int(1))]],
            },
            Statement::Insert {
                table: "t".into(),
                columns: Some(vec!["a".into(), "zz".into()]),
                rows: vec![vec![
                    Expr::Literal(Value::Int(1)),
                    Expr::Literal(Value::Null),
                ]],
            },
            Statement::Insert {
                table: "t".into(),
                columns: Some(vec!["a".into(), "a".into()]),
                rows: vec![vec![
                    Expr::Literal(Value::Int(1)),
                    Expr::Literal(Value::Int(2)),
                ]],
            },
            Statement::Insert {
                table: "t".into(),
                columns: Some(vec!["b".into()]),
                rows: vec![vec![Expr::Literal(Value::Text("x".into()))]],
            },
            // A column reference in VALUES has nothing to resolve against.
            Statement::Insert {
                table: "t".into(),
                columns: Some(vec!["a".into()]),
                rows: vec![vec![Expr::col("a")]],
            },
        ] {
            assert!(lower(&bad, &cat()).is_err(), "should have rejected {bad:?}");
        }
    }

    #[test]
    fn dml_and_ddl_resolve_against_the_table() {
        // UPDATE t SET B = a + 1 WHERE c — SET names canonicalise to the schema.
        let update = lower(
            &Statement::Update {
                table: "t".into(),
                sets: vec![(
                    "A".into(),
                    Expr::bin(Expr::col("a"), BinOp::Add, Expr::Literal(Value::Int(1))),
                )],
                predicate: Some(Expr::col("c")),
            },
            &cat(),
        )
        .unwrap();
        assert_eq!(
            update,
            LogicalPlan::Update {
                table: "t".into(),
                sets: vec![(
                    "a".into(),
                    Expr::bin(Expr::ColumnRef(0), BinOp::Add, Expr::Literal(Value::Int(1)))
                )],
                predicate: Some(Expr::ColumnRef(2)),
            }
        );
        assert!(lower(
            &Statement::Update {
                table: "t".into(),
                sets: vec![("nope".into(), Expr::Literal(Value::Null))],
                predicate: None,
            },
            &cat()
        )
        .is_err());

        assert_eq!(
            lower(
                &Statement::Delete {
                    table: "t".into(),
                    predicate: Some(Expr::bin(
                        Expr::col("a"),
                        BinOp::Eq,
                        Expr::Literal(Value::Int(3))
                    )),
                },
                &cat()
            )
            .unwrap(),
            LogicalPlan::Delete {
                table: "t".into(),
                predicate: Some(Expr::bin(
                    Expr::ColumnRef(0),
                    BinOp::Eq,
                    Expr::Literal(Value::Int(3))
                )),
            }
        );

        let schema = cat().get("t").unwrap().clone();
        assert_eq!(
            lower(
                &Statement::CreateTable {
                    schema: schema.clone()
                },
                &Catalog::new()
            )
            .unwrap(),
            LogicalPlan::CreateTable { schema }
        );
        assert_eq!(
            lower(&Statement::DropTable { table: "T".into() }, &cat()).unwrap(),
            LogicalPlan::DropTable { table: "t".into() },
            "canonical spelling, and an unknown table fails here"
        );
        assert!(lower(
            &Statement::DropTable {
                table: "gone".into()
            },
            &cat()
        )
        .is_err());

        // Transaction control has no LogicalPlan variant; plan/mod.rs handles it.
        for stmt in [Statement::Begin, Statement::Commit, Statement::Rollback] {
            assert!(matches!(lower(&stmt, &cat()), Err(QuernError::Txn(_))));
        }
    }
}
