//! bead: quern-exec-trait — HOT: the Operator trait. See docs/quern.md §3.
//!
//! Execution is pull-based: a plan is a tree of [`Operator`]s and the root is
//! drained with [`Operator::next`] until it yields `Ok(None)`. Every operator
//! evaluates its expressions through the one [`eval`] in this module, so §1's
//! NULL rule is implemented exactly once (in `types.rs`) and dispatched to
//! exactly once (here).
//!
//! # An operator may NOT borrow Storage
//!
//! `Box<dyn Operator>` is `Box<dyn Operator + 'static>`, and the trait has no
//! lifetime parameter — frozen §3, not ours to widen. So an operator cannot
//! hold the iterator from `Storage::scan(&self)`, and `fn build(s: &dyn
//! Storage) -> Result<Box<dyn Operator>>` cannot compile if it tries
//! ("lifetime may not live long enough"). This was proven with rustc in bead
//! .35; do not rediscover it.
//!
//! The two consequences, both already decided:
//!
//! * `exec::scan` materialises its rows in its **constructor** — `Scan::new`
//!   takes `&dyn Storage`, collects, and the borrow ends there.
//! * DML is **not an Operator**. `exec::dml::execute(&LogicalPlan, &mut dyn
//!   Storage, &Schema) -> Result<usize>` is a free function, two-phase:
//!   collect the `(RowId, Row)` hits under the scan borrow, drop it, then
//!   mutate. A row count is not a `Row`, and `next()` cannot carry a `RowId`.
//!
//! Do not add methods to [`Operator`] to work around either of these.

pub mod aggregate;
pub mod dml;
pub mod filter;
pub mod join;
pub mod limit;
pub mod project;
pub mod scan;
pub mod sort;

use crate::sql::ast::{BinOp, Expr, UnOp};
use crate::types::{Column, QuernError, Result, Row, Value};

/// Frozen in docs/quern.md §3. `open`/`next` folded into a pull loop:
/// `Ok(None)` means exhausted, and an operator may not be polled for meaning
/// after that (drained operators should keep returning `Ok(None)`).
///
/// `schema()` describes the rows `next()` produces, so `schema().len()` is the
/// length of every `Row` yielded. See the module docs for why an implementor
/// cannot hold a borrow of `Storage`.
pub trait Operator {
    fn schema(&self) -> &[Column];
    fn next(&mut self) -> Result<Option<Row>>; // Ok(None) = exhausted
}

/// Evaluate a scalar expression against one input row.
///
/// `plan::logical` has already resolved every name, so this sees
/// `Expr::ColumnRef(i)` and indexes `row[i]` directly — there is no name lookup
/// and no schema argument. An `Expr::Column { .. }` or a bare `Expr::Agg`
/// reaching here means the plan was not lowered (or was hand-built wrong):
/// that is `QuernError::Type`, never a panic. Aggregates are computed by
/// `exec::aggregate`, which never calls `eval` on the `Agg` node itself.
///
/// NULL handling is **delegated**: the arithmetic and comparison operators call
/// the same-named `Value` helpers, which implement §1's rule (either operand
/// `Null` short-circuits to `Null`, before the divide-by-zero check). Only
/// `AND`/`OR`/`NOT` are implemented here, because `types.rs` deliberately ships
/// no helper for them — and they follow the identical shape: any `Null` operand
/// yields `Null`, any non-`BOOL` operand is an error. There is no short-circuit
/// evaluation: both sides of `AND`/`OR` are always evaluated, so
/// `FALSE AND 1/0` is an error rather than `FALSE`. §1 forbids inventing
/// three-valued refinements beyond the one rule, and eager evaluation is what
/// keeps a type error in a dead operand from depending on row order.
pub fn eval(expr: &Expr, row: &Row) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::ColumnRef(i) => row.get(*i).cloned().ok_or_else(|| {
            QuernError::Type(format!(
                "column reference #{i} out of range for a row of {} value(s)",
                row.len()
            ))
        }),
        Expr::Binary { op, left, right } => {
            let (l, r) = (eval(left, row)?, eval(right, row)?);
            match op {
                // Named associated-function form on purpose: `l.eq(&r)` is the
                // inherent NULL-rule helper, not `PartialEq::eq`, and reading
                // it as the latter is the one footgun in types.rs.
                BinOp::Add => Value::add(&l, &r),
                BinOp::Sub => Value::sub(&l, &r),
                BinOp::Mul => Value::mul(&l, &r),
                BinOp::Div => Value::div(&l, &r),
                BinOp::Eq => Value::eq(&l, &r),
                BinOp::Ne => Value::ne(&l, &r),
                BinOp::Lt => Value::lt(&l, &r),
                BinOp::Gt => Value::gt(&l, &r),
                BinOp::And | BinOp::Or => connective(*op, &l, &r),
            }
        }
        Expr::Unary { op, expr } => {
            let v = eval(expr, row)?;
            match (op, &v) {
                (_, Value::Null) => Ok(Value::Null),
                (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                (UnOp::Neg, Value::Int(i)) => i
                    .checked_neg()
                    .map(Value::Int)
                    .ok_or_else(|| QuernError::Type("integer overflow in unary -".to_string())),
                (UnOp::Not, _) => Err(QuernError::Type(format!(
                    "NOT expects BOOL, got {}",
                    v.type_name()
                ))),
                (UnOp::Neg, _) => Err(QuernError::Type(format!(
                    "unary - expects INT, got {}",
                    v.type_name()
                ))),
            }
        }
        Expr::Column { .. } => Err(QuernError::Type(format!(
            "unresolved column reference: {expr}"
        ))),
        Expr::Agg(agg) => Err(QuernError::Type(format!(
            "aggregate {agg} cannot be evaluated per row; it belongs in a GROUP BY node"
        ))),
    }
}

/// `AND`/`OR`, the two operators `types.rs` left to us. Same shape as its
/// helpers: `Null` first, then types, then the answer.
fn connective(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    for v in [l, r] {
        if !matches!(v, Value::Null | Value::Bool(_)) {
            return Err(QuernError::Type(format!(
                "{} expects BOOL, got {}",
                op.symbol(),
                v.type_name()
            )));
        }
    }
    match (l, r) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(match op {
            BinOp::And => *a && *b,
            _ => *a || *b,
        })),
        _ => unreachable!("guarded above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{AggExpr, AggFunc};

    fn int(i: i64) -> Expr {
        Expr::Literal(Value::Int(i))
    }
    fn boolean(b: bool) -> Expr {
        Expr::Literal(Value::Bool(b))
    }
    const NULL: Expr = Expr::Literal(Value::Null);

    /// `eval` against the empty row, for literal-only expressions.
    fn ev(e: Expr) -> Result<Value> {
        eval(&e, &Row::new())
    }
    fn bin(l: Expr, op: BinOp, r: Expr) -> Result<Value> {
        ev(Expr::bin(l, op, r))
    }

    #[test]
    fn every_binary_operator_delegates_to_the_value_helper() {
        use BinOp::*;
        assert_eq!(bin(int(2), Add, int(3)), Ok(Value::Int(5)));
        assert_eq!(bin(int(2), Sub, int(3)), Ok(Value::Int(-1)));
        assert_eq!(bin(int(2), Mul, int(3)), Ok(Value::Int(6)));
        assert_eq!(bin(int(7), Div, int(2)), Ok(Value::Int(3)));
        assert_eq!(bin(int(2), Eq, int(2)), Ok(Value::Bool(true)));
        assert_eq!(bin(int(2), Ne, int(2)), Ok(Value::Bool(false)));
        assert_eq!(bin(int(1), Lt, int(2)), Ok(Value::Bool(true)));
        assert_eq!(bin(int(1), Gt, int(2)), Ok(Value::Bool(false)));
        // Comparisons work on TEXT and BOOL too, not just INT.
        let text = |s: &str| Expr::Literal(Value::Text(s.to_string()));
        assert_eq!(bin(text("a"), Lt, text("b")), Ok(Value::Bool(true)));
        assert_eq!(
            bin(boolean(false), Lt, boolean(true)),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn null_propagates_through_every_binary_operator() {
        use BinOp::*;
        for op in [Add, Sub, Mul, Div, Eq, Ne, Lt, Gt, And, Or] {
            let operand = if matches!(op, And | Or) {
                boolean(true)
            } else {
                int(1)
            };
            assert_eq!(
                bin(NULL, op, operand.clone()),
                Ok(Value::Null),
                "NULL {} x",
                op.symbol()
            );
            assert_eq!(
                bin(operand, op, NULL),
                Ok(Value::Null),
                "x {} NULL",
                op.symbol()
            );
        }
        // NULL is checked before divide-by-zero, so this is Null, not Err.
        assert_eq!(bin(NULL, Div, int(0)), Ok(Value::Null));
        assert!(matches!(bin(int(1), Div, int(0)), Err(QuernError::Type(_))));
    }

    #[test]
    fn and_or_not_are_two_valued_plus_null() {
        use BinOp::{And, Or};
        for (a, b) in [(false, false), (false, true), (true, false), (true, true)] {
            assert_eq!(
                bin(boolean(a), And, boolean(b)),
                Ok(Value::Bool(a && b)),
                "{a} AND {b}"
            );
            assert_eq!(
                bin(boolean(a), Or, boolean(b)),
                Ok(Value::Bool(a || b)),
                "{a} OR {b}"
            );
        }
        // No short-circuit: FALSE AND <null> is Null, TRUE OR <null> is Null.
        assert_eq!(bin(boolean(false), And, NULL), Ok(Value::Null));
        assert_eq!(bin(boolean(true), Or, NULL), Ok(Value::Null));
        assert_eq!(bin(NULL, And, NULL), Ok(Value::Null));

        assert_eq!(
            ev(Expr::un(UnOp::Not, boolean(true))),
            Ok(Value::Bool(false))
        );
        assert_eq!(
            ev(Expr::un(UnOp::Not, boolean(false))),
            Ok(Value::Bool(true))
        );
        assert_eq!(ev(Expr::un(UnOp::Not, NULL)), Ok(Value::Null));

        // The WHERE rule keeps only Bool(true): Null is falsy, not an error.
        assert!(!Value::Null.is_true());
        assert!(!bin(NULL, And, boolean(true)).unwrap().is_true());
    }

    #[test]
    fn unary_neg_on_int_only_and_never_panics() {
        assert_eq!(ev(Expr::un(UnOp::Neg, int(3))), Ok(Value::Int(-3)));
        assert_eq!(ev(Expr::un(UnOp::Neg, NULL)), Ok(Value::Null));
        assert!(matches!(
            ev(Expr::un(UnOp::Neg, boolean(true))),
            Err(QuernError::Type(_))
        ));
        // i64::MIN has no positive: an error, not a debug-build panic.
        assert!(matches!(
            ev(Expr::un(UnOp::Neg, int(i64::MIN))),
            Err(QuernError::Type(_))
        ));
    }

    #[test]
    fn type_errors_are_errors_not_false() {
        use BinOp::*;
        let text = Expr::Literal(Value::Text("1".to_string()));
        // 1 = '1' is a statement error, per §1.
        assert!(matches!(
            bin(int(1), Eq, text.clone()),
            Err(QuernError::Type(_))
        ));
        // Arithmetic on non-INT.
        assert!(matches!(
            bin(text.clone(), Add, int(1)),
            Err(QuernError::Type(_))
        ));
        // AND/OR on non-BOOL, either side.
        assert!(matches!(
            bin(int(1), And, boolean(true)),
            Err(QuernError::Type(_))
        ));
        assert!(matches!(
            bin(boolean(true), Or, text),
            Err(QuernError::Type(_))
        ));
        // NOT on non-BOOL.
        assert!(matches!(
            ev(Expr::un(UnOp::Not, int(1))),
            Err(QuernError::Type(_))
        ));
    }

    #[test]
    fn column_ref_indexes_the_row_and_bounds_check_is_an_error() {
        let row: Row = vec![Value::Int(7), Value::Null, Value::Text("x".into())];
        assert_eq!(eval(&Expr::ColumnRef(0), &row), Ok(Value::Int(7)));
        assert_eq!(eval(&Expr::ColumnRef(1), &row), Ok(Value::Null));
        assert_eq!(eval(&Expr::ColumnRef(2), &row), Ok(Value::Text("x".into())));
        assert!(matches!(
            eval(&Expr::ColumnRef(3), &row),
            Err(QuernError::Type(_))
        ));
        assert!(matches!(
            eval(&Expr::ColumnRef(0), &Row::new()),
            Err(QuernError::Type(_))
        ));
        // Nested: (#0 + 1) > #0 over the same row.
        let e = Expr::bin(
            Expr::bin(Expr::ColumnRef(0), BinOp::Add, int(1)),
            BinOp::Gt,
            Expr::ColumnRef(0),
        );
        assert_eq!(eval(&e, &row), Ok(Value::Bool(true)));
    }

    #[test]
    fn unresolved_column_and_bare_agg_are_internal_errors() {
        let row: Row = vec![Value::Int(1)];
        for e in [Expr::col("a"), Expr::qcol("t", "a")] {
            let err = eval(&e, &row).expect_err("plan was not lowered");
            assert!(matches!(err, QuernError::Type(_)), "{err:?}");
        }
        assert!(matches!(
            eval(&Expr::agg(AggExpr::count_star()), &row),
            Err(QuernError::Type(_))
        ));
        assert!(matches!(
            eval(
                &Expr::agg(AggExpr::of(AggFunc::Sum, Expr::ColumnRef(0))),
                &row
            ),
            Err(QuernError::Type(_))
        ));
        // Buried in a subtree, still an error rather than a panic.
        assert!(matches!(
            eval(&Expr::bin(int(1), BinOp::Add, Expr::col("a")), &row),
            Err(QuernError::Type(_))
        ));
    }
}
