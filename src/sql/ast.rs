//! bead: quern-ast — HOT: Statement, Expr, SelectStmt
//!
//! The AST the three parsers build and `plan::logical` lowers. The shapes here
//! are the ones docs/quern.md §3 grew for `AggExpr`/`AggFunc` (bead .39) and
//! the ones bead .38 pinned for `Expr` — implemented as proposed, not
//! redesigned. Everything else is shaped so lowering into the frozen
//! `LogicalPlan` is a move rather than a translation.
//!
//! Two halves of one enum, which is the thing to understand before using it:
//!
//! * **`Expr::Column` is parser output, `Expr::ColumnRef` is planner output.**
//!   Parsers only ever produce `Column { table, name }` (`table` is `Some` for
//!   `t.a`). `plan::logical` resolves it against the input schema and rewrites
//!   it to `ColumnRef(i)`, so `exec` never does a name lookup per row. Above a
//!   join the index space is left columns then right columns, concatenated.
//! * **`Expr::Agg` is lifted out, not evaluated.** An aggregate call parses as
//!   `Agg(Box<AggExpr>)` inside a `SELECT` item; `plan::logical` lifts every
//!   one into `LogicalPlan::Aggregate::aggs` and leaves a `ColumnRef` behind.
//!
//! So `exec::eval` sees only `Literal`, `ColumnRef`, `Binary` and `Unary` in a
//! well-formed plan, and owes `Err(QuernError::Type(..))` — never a panic — on
//! a surviving `Column` or `Agg`.

use crate::types::{Schema, Value};
use std::fmt;

/// One statement. Variants match `LogicalPlan`'s fields where they can, so
/// `lower` mostly moves them across. `Insert` is the one that cannot: it still
/// carries the optional column list that lowering resolves into the positional
/// order `LogicalPlan::Insert` wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    CreateTable {
        schema: Schema,
    },
    DropTable {
        table: String,
    },
    Insert {
        table: String,
        /// `None` = positional `INSERT INTO t VALUES (..)`.
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    },
    Select(SelectStmt),
    Update {
        table: String,
        sets: Vec<(String, Expr)>,
        predicate: Option<Expr>,
    },
    Delete {
        table: String,
        predicate: Option<Expr>,
    },
    Begin,
    Commit,
    Rollback,
}

/// `SELECT` per §1: projection, one `FROM`, at most one inner `JOIN .. ON`,
/// `WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT`. No subqueries, no `HAVING`, no
/// `DISTINCT` — those are non-goals, so there is no field for them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectStmt {
    pub projection: Vec<SelectItem>,
    pub from: String,
    pub join: Option<Join>,
    /// `WHERE`. Named to match `LogicalPlan::Filter::predicate`.
    pub predicate: Option<Expr>,
    pub group_by: Vec<Expr>,
    /// `bool` is descending, so this moves straight into
    /// `LogicalPlan::Sort::keys` with no conversion.
    pub order_by: Vec<(Expr, bool)>,
    pub limit: Option<usize>,
}

/// Inner join only, and at most one — §1's whole join surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub table: String,
    pub on: Expr,
}

/// One item in a projection list. An aggregate is an ordinary `Expr::Agg` item;
/// only `*` needs its own variant, because it expands against the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    /// `SELECT *`
    Star,
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}

impl SelectItem {
    /// Unaliased expression item, the common case.
    pub fn expr(expr: Expr) -> SelectItem {
        SelectItem::Expr { expr, alias: None }
    }

    /// `expr AS name`.
    pub fn aliased(expr: Expr, alias: impl Into<String>) -> SelectItem {
        SelectItem::Expr {
            expr,
            alias: Some(alias.into()),
        }
    }

    /// The output column name for `LogicalPlan::Project::exprs`: the `AS` alias
    /// if there was one, else the expression as written. `None` for `Star`,
    /// which expands to the input schema's own names.
    pub fn output_name(&self) -> Option<String> {
        match self {
            SelectItem::Star => None,
            SelectItem::Expr { expr, alias } => {
                Some(alias.clone().unwrap_or_else(|| expr.to_string()))
            }
        }
    }
}

/// The five aggregates of §1, defined here because `LogicalPlan::Aggregate`
/// references `AggExpr` without defining it. `plan/logical.rs` imports this;
/// it does not define a second copy.
///
/// `arg` is `None` for exactly one thing: `COUNT(*)`, the only aggregate §1
/// lets you write without a column and the only one that counts `Null` rows.
/// `COUNT(a)` is `Some` and skips `Null` like every other aggregate.
///
/// `alias` is the output name — the `AS` alias if one was given, else the
/// source spelling (`"COUNT(*)"`, `"SUM(a)"`). The constructors fill it in, so
/// use them rather than the struct literal and the two never disagree. §5
/// compares no headers, so nothing is graded on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggExpr {
    pub func: AggFunc,
    pub arg: Option<Expr>,
    pub alias: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl AggFunc {
    pub fn name(self) -> &'static str {
        match self {
            AggFunc::Count => "COUNT",
            AggFunc::Sum => "SUM",
            AggFunc::Min => "MIN",
            AggFunc::Max => "MAX",
            AggFunc::Avg => "AVG",
        }
    }
}

impl AggExpr {
    /// `COUNT(*)` — the one aggregate with no argument.
    pub fn count_star() -> AggExpr {
        AggExpr {
            func: AggFunc::Count,
            arg: None,
            alias: "COUNT(*)".to_string(),
        }
    }

    /// `func(arg)`, with `alias` defaulted to the source spelling.
    pub fn of(func: AggFunc, arg: Expr) -> AggExpr {
        AggExpr {
            alias: format!("{}({arg})", func.name()),
            func,
            arg: Some(arg),
        }
    }

    /// Override the output name, for `func(arg) AS name`.
    pub fn with_alias(mut self, alias: impl Into<String>) -> AggExpr {
        self.alias = alias.into();
        self
    }

    /// True for `COUNT(*)` only — the §1 case that counts rows instead of
    /// skipping `Null` inputs.
    pub fn is_count_star(&self) -> bool {
        self.func == AggFunc::Count && self.arg.is_none()
    }
}

/// A scalar expression. This is exactly what `LogicalPlan`'s `predicate`,
/// `exprs`, `on`, `keys` and `group_by` hold, and what `exec::eval` evaluates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Literal(Value),
    /// A name as written; `table` is `Some` for `t.a`. Parser output only.
    Column {
        table: Option<String>,
        name: String,
    },
    /// A resolved position in the operator's input row. Planner output only.
    ColumnRef(usize),
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    /// An aggregate call in a `SELECT` list. `plan::logical` lifts these into
    /// `LogicalPlan::Aggregate` and replaces them with a `ColumnRef`, so one
    /// reaching `exec::eval` is an internal error.
    Agg(Box<AggExpr>),
}

impl Expr {
    /// Unqualified column reference, `a`.
    pub fn col(name: impl Into<String>) -> Expr {
        Expr::Column {
            table: None,
            name: name.into(),
        }
    }

    /// Qualified column reference, `t.a`.
    pub fn qcol(table: impl Into<String>, name: impl Into<String>) -> Expr {
        Expr::Column {
            table: Some(table.into()),
            name: name.into(),
        }
    }

    /// The boxing the parsers would otherwise write at every call site.
    pub fn bin(left: Expr, op: BinOp, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    pub fn un(op: UnOp, expr: Expr) -> Expr {
        Expr::Unary {
            op,
            expr: Box::new(expr),
        }
    }

    pub fn agg(agg: AggExpr) -> Expr {
        Expr::Agg(Box::new(agg))
    }

    /// Whether this expression contains an aggregate call anywhere. The parsers
    /// use it to reject `WHERE COUNT(*) > 1` (no `HAVING` in quern), and
    /// `plan::logical` to decide whether a `SELECT` needs an `Aggregate` node
    /// at all — a `GROUP BY`-less projection of aggregates still does.
    pub fn contains_agg(&self) -> bool {
        match self {
            Expr::Agg(_) => true,
            Expr::Binary { left, right, .. } => left.contains_agg() || right.contains_agg(),
            Expr::Unary { expr, .. } => expr.contains_agg(),
            Expr::Literal(_) | Expr::Column { .. } | Expr::ColumnRef(_) => false,
        }
    }
}

/// §1's binary operators, and nothing else. `Add`..`Div` are INT-only,
/// `Eq`..`Gt` are INT/TEXT/BOOL, `And`/`Or` are BOOL.
///
/// `Add`, `Sub`, `Mul`, `Div`, `Eq`, `Ne`, `Lt`, `Gt` each have a same-named
/// NULL-rule helper on `Value`, so `exec::eval` is a dispatch table.
/// `And`/`Or` deliberately do not: `types.rs` has no helper for them, so their
/// NULL behaviour is `exec::eval`'s to implement from §1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Gt,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::Lt => "<",
            BinOp::Gt => ">",
            BinOp::And => "AND",
            BinOp::Or => "OR",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// `NOT x`
    Not,
    /// `-x`
    Neg,
}

// --- Display: the source-ish rendering used for output column names ---------

impl fmt::Display for AggFunc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl fmt::Display for AggExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.arg {
            None => write!(f, "{}(*)", self.func),
            Some(e) => write!(f, "{}({e})", self.func),
        }
    }
}

/// Renders close enough to the SQL it came from to be an output column name or
/// an error message. Binaries are always parenthesised — this is not a
/// minimal-parentheses pretty printer and does not need to be.
impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Value's own Display is the .slt print rule (bare text); an
            // expression re-quotes text so `b <> 'q'` reads as SQL.
            Expr::Literal(Value::Text(s)) => write!(f, "'{}'", s.replace('\'', "''")),
            Expr::Literal(v) => write!(f, "{v}"),
            Expr::Column {
                table: Some(t),
                name,
            } => write!(f, "{t}.{name}"),
            Expr::Column { table: None, name } => f.write_str(name),
            Expr::ColumnRef(i) => write!(f, "#{i}"),
            Expr::Binary { op, left, right } => write!(f, "({left} {} {right})", op.symbol()),
            Expr::Unary {
                op: UnOp::Not,
                expr,
            } => write!(f, "NOT {expr}"),
            Expr::Unary {
                op: UnOp::Neg,
                expr,
            } => write!(f, "-{expr}"),
            Expr::Agg(a) => write!(f, "{a}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Column, Type};

    /// SELECT t.b, COUNT(*), SUM(t.a) AS total FROM t JOIN u ON t.a = u.a
    ///   WHERE u.b <> 'q' GROUP BY t.b ORDER BY t.b DESC LIMIT 5
    fn representative_select() -> SelectStmt {
        SelectStmt {
            projection: vec![
                SelectItem::expr(Expr::qcol("t", "b")),
                SelectItem::expr(Expr::agg(AggExpr::count_star())),
                SelectItem::aliased(
                    Expr::agg(AggExpr::of(AggFunc::Sum, Expr::qcol("t", "a")).with_alias("total")),
                    "total",
                ),
            ],
            from: "t".into(),
            join: Some(Join {
                table: "u".into(),
                on: Expr::bin(Expr::qcol("t", "a"), BinOp::Eq, Expr::qcol("u", "a")),
            }),
            predicate: Some(Expr::bin(
                Expr::qcol("u", "b"),
                BinOp::Ne,
                Expr::Literal(Value::Text("q".into())),
            )),
            group_by: vec![Expr::qcol("t", "b")],
            order_by: vec![(Expr::qcol("t", "b"), true)],
            limit: Some(5),
        }
    }

    #[test]
    fn select_tree_carries_everything_logical_needs() {
        let s = representative_select();
        assert_eq!(
            Statement::Select(s.clone()),
            Statement::Select(representative_select())
        );

        // plan-logical lifts aggregates by walking the projection exprs.
        let aggs: Vec<&AggExpr> = s
            .projection
            .iter()
            .filter_map(|i| match i {
                SelectItem::Expr {
                    expr: Expr::Agg(a), ..
                } => Some(&**a),
                _ => None,
            })
            .collect();
        assert_eq!(aggs.len(), 2);
        assert!(aggs[0].is_count_star(), "COUNT(*) has no arg");
        assert_eq!(aggs[0].alias, "COUNT(*)");
        assert!(!aggs[1].is_count_star());
        assert_eq!(aggs[1].func, AggFunc::Sum);
        assert_eq!(aggs[1].arg, Some(Expr::qcol("t", "a")));
        assert_eq!(aggs[1].alias, "total");

        // contains_agg is how a projection is told apart from a predicate.
        assert!(s.projection[1].output_name().is_some());
        assert!(!s.predicate.as_ref().unwrap().contains_agg());
        assert!(!s.group_by[0].contains_agg());
        assert!(Expr::bin(
            Expr::agg(AggExpr::count_star()),
            BinOp::Gt,
            Expr::Literal(Value::Int(1))
        )
        .contains_agg());

        // Output names: alias wins, else the expression as written.
        let names: Vec<Option<String>> = s.projection.iter().map(|i| i.output_name()).collect();
        assert_eq!(
            names,
            vec![
                Some("t.b".to_string()),
                Some("COUNT(*)".to_string()),
                Some("total".to_string()),
            ]
        );

        // ORDER BY moves into LogicalPlan::Sort::keys unconverted: bool = desc.
        let keys: Vec<(Expr, bool)> = s.order_by.clone();
        assert_eq!(keys, vec![(Expr::qcol("t", "b"), true)]);
        assert_eq!(s.group_by, vec![Expr::qcol("t", "b")]);
        assert_eq!(s.limit, Some(5));
        assert_eq!(s.join.as_ref().unwrap().table, "u");
        assert_eq!(
            s.predicate.as_ref().unwrap().to_string(),
            "(u.b <> 'q')",
            "predicate renders as the SQL it came from"
        );
    }

    #[test]
    fn star_projection_has_no_output_name() {
        let s = SelectStmt {
            projection: vec![SelectItem::Star],
            from: "t".into(),
            ..SelectStmt::default()
        };
        assert_eq!(s.projection[0].output_name(), None);
        assert!(s.predicate.is_none() && s.join.is_none() && s.limit.is_none());
        assert!(s.group_by.is_empty() && s.order_by.is_empty());
    }

    /// UPDATE t SET b = 'w', c = FALSE WHERE a = 2
    #[test]
    fn update_tree_matches_the_logical_plan_variant() {
        let stmt = Statement::Update {
            table: "t".into(),
            sets: vec![
                ("b".into(), Expr::Literal(Value::Text("w".into()))),
                ("c".into(), Expr::Literal(Value::Bool(false))),
            ],
            predicate: Some(Expr::bin(
                Expr::col("a"),
                BinOp::Eq,
                Expr::Literal(Value::Int(2)),
            )),
        };
        let Statement::Update {
            table,
            sets,
            predicate,
        } = &stmt
        else {
            panic!("expected Update");
        };
        assert_eq!(table, "t");
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].0, "b");
        assert_eq!(sets[1].1, Expr::Literal(Value::Bool(false)));
        assert_eq!(predicate.as_ref().unwrap().to_string(), "(a = 2)");
    }

    #[test]
    fn dml_and_ddl_variants_round_trip() {
        // INSERT INTO t (a, b) VALUES (1, 'x'), (2, NULL)
        let insert = Statement::Insert {
            table: "t".into(),
            columns: Some(vec!["a".into(), "b".into()]),
            rows: vec![
                vec![
                    Expr::Literal(Value::Int(1)),
                    Expr::Literal(Value::Text("x".into())),
                ],
                vec![Expr::Literal(Value::Int(2)), Expr::Literal(Value::Null)],
            ],
        };
        let Statement::Insert { columns, rows, .. } = &insert else {
            panic!("expected Insert");
        };
        assert_eq!(columns.as_deref().map(<[String]>::len), Some(2));
        assert_eq!(rows.len(), 2);

        // Positional INSERT is the same variant with columns: None.
        assert!(matches!(
            Statement::Insert {
                table: "t".into(),
                columns: None,
                rows: vec![],
            },
            Statement::Insert { columns: None, .. }
        ));

        // CREATE TABLE carries a Schema, which is what LogicalPlan::CreateTable
        // and Storage::create_table both want.
        let create = Statement::CreateTable {
            schema: Schema {
                table: "t".into(),
                columns: vec![Column {
                    name: "a".into(),
                    ty: Type::Int,
                    primary_key: true,
                }],
            },
        };
        let Statement::CreateTable { schema } = &create else {
            panic!("expected CreateTable");
        };
        assert_eq!(schema.primary_key(), Some(0));

        for s in [Statement::Begin, Statement::Commit, Statement::Rollback] {
            assert_eq!(s.clone(), s);
        }
    }

    #[test]
    fn display_covers_every_expr_shape() {
        assert_eq!(Expr::Literal(Value::Null).to_string(), "NULL");
        assert_eq!(Expr::Literal(Value::Int(-3)).to_string(), "-3");
        assert_eq!(
            Expr::Literal(Value::Text("it's".into())).to_string(),
            "'it''s'"
        );
        assert_eq!(Expr::col("a").to_string(), "a");
        assert_eq!(Expr::qcol("t", "a").to_string(), "t.a");
        assert_eq!(Expr::ColumnRef(2).to_string(), "#2");
        assert_eq!(
            Expr::un(UnOp::Not, Expr::col("c")).to_string(),
            "NOT c",
            "NOT keeps its keyword spelling"
        );
        assert_eq!(Expr::un(UnOp::Neg, Expr::col("a")).to_string(), "-a");
        assert_eq!(
            Expr::bin(
                Expr::bin(Expr::col("b"), BinOp::Add, Expr::Literal(Value::Int(1))),
                BinOp::Gt,
                Expr::col("a")
            )
            .to_string(),
            "((b + 1) > a)"
        );
        // An aliased aggregate still renders as its source spelling; the alias
        // is the output name, not the rendering.
        let sum = AggExpr::of(AggFunc::Avg, Expr::col("a"));
        assert_eq!(sum.alias, "AVG(a)");
        assert_eq!(Expr::agg(sum.with_alias("mean")).to_string(), "AVG(a)");
    }
}
