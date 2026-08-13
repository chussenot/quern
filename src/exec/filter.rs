//! bead: quern-exec-filter — `WHERE`, the one place §1's NULL rule is visible.
//!
//! A filter changes which rows flow, never their shape, so [`Filter::schema`]
//! is the child's schema verbatim.

use super::{eval, Operator};
use crate::sql::ast::Expr;
use crate::types::{Column, Result, Row};

/// Yields the child's rows for which the predicate evaluates to exactly
/// `Value::Bool(true)`.
///
/// §1: `Null` drops the row, and so does any other non-`Bool(true)` value —
/// [`Value::is_true`](crate::types::Value::is_true) is that test, and this
/// operator does not re-spell it. A non-`BOOL` predicate result (`WHERE 1`) is
/// therefore a DROP, not an error: the only values that can reach here are ones
/// `eval` already accepted, and §1 gives `WHERE` a single rule — keep exactly
/// `Bool(true)` — rather than a second type check on top of the evaluator's.
/// Genuinely mistyped predicates (`1 = 'x'`, `1 AND TRUE`) still fail inside
/// `eval`, and that `Err` propagates rather than silently dropping the row.
pub struct Filter {
    child: Box<dyn Operator>,
    predicate: Expr,
}

impl Filter {
    pub fn new(child: Box<dyn Operator>, predicate: Expr) -> Self {
        Self { child, predicate }
    }
}

impl Operator for Filter {
    fn schema(&self) -> &[Column] {
        self.child.schema()
    }

    fn next(&mut self) -> Result<Option<Row>> {
        while let Some(row) = self.child.next()? {
            if eval(&self.predicate, &row)?.is_true() {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::BinOp;
    use crate::types::{QuernError, Type, Value};

    /// Yields a fixed `Vec<Row>` once, then `Ok(None)` forever.
    struct Mock {
        schema: Vec<Column>,
        rows: std::vec::IntoIter<Row>,
    }

    impl Mock {
        /// One INT column `a`, one value per row.
        fn ints(vs: &[Value]) -> Box<dyn Operator> {
            Box::new(Mock {
                schema: vec![Column {
                    name: "a".to_string(),
                    ty: Type::Int,
                    primary_key: false,
                }],
                rows: vs
                    .iter()
                    .map(|v| vec![v.clone()])
                    .collect::<Vec<_>>()
                    .into_iter(),
            })
        }
    }

    impl Operator for Mock {
        fn schema(&self) -> &[Column] {
            &self.schema
        }
        fn next(&mut self) -> Result<Option<Row>> {
            Ok(self.rows.next())
        }
    }

    fn drain(child: Box<dyn Operator>, predicate: Expr) -> Result<Vec<Row>> {
        let mut f = Filter::new(child, predicate);
        let mut out = Vec::new();
        while let Some(row) = f.next()? {
            out.push(row);
        }
        Ok(out)
    }

    /// `#0 > 1`, the predicate every row test below runs.
    fn gt1() -> Expr {
        Expr::bin(Expr::ColumnRef(0), BinOp::Gt, Expr::Literal(Value::Int(1)))
    }

    #[test]
    fn keeps_true_drops_false_and_drops_null() {
        // 2 > 1 is true; 0 > 1 is false; NULL > 1 is Null — and Null drops.
        let rows = drain(
            Mock::ints(&[Value::Int(2), Value::Int(0), Value::Null, Value::Int(3)]),
            gt1(),
        )
        .unwrap();
        assert_eq!(rows, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);
    }

    #[test]
    fn a_non_bool_predicate_drops_every_row() {
        // `WHERE 1`: well-typed for eval, never Bool(true), so nothing passes.
        let rows = drain(
            Mock::ints(&[Value::Int(1), Value::Int(2)]),
            Expr::Literal(Value::Int(1)),
        )
        .unwrap();
        assert!(rows.is_empty());
        // A bare NULL predicate is the same story.
        let rows = drain(Mock::ints(&[Value::Int(1)]), Expr::Literal(Value::Null)).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn an_eval_error_propagates_instead_of_dropping_the_row() {
        // `1 = 'x'` is a type error in §1, not false.
        let mistyped = Expr::bin(
            Expr::ColumnRef(0),
            BinOp::Eq,
            Expr::Literal(Value::Text("x".to_string())),
        );
        assert!(matches!(
            drain(Mock::ints(&[Value::Int(1)]), mistyped),
            Err(QuernError::Type(_))
        ));
        // An unresolved column (plan not lowered) is an error too, not a drop.
        assert!(matches!(
            drain(Mock::ints(&[Value::Int(1)]), Expr::col("a")),
            Err(QuernError::Type(_))
        ));
    }

    #[test]
    fn an_empty_child_yields_nothing_and_stays_drained() {
        let mut f = Filter::new(Mock::ints(&[]), gt1());
        assert_eq!(f.next(), Ok(None));
        assert_eq!(f.next(), Ok(None));
        // Exhausting a non-empty child is equally repeatable.
        let mut f = Filter::new(Mock::ints(&[Value::Int(2)]), gt1());
        assert_eq!(f.next(), Ok(Some(vec![Value::Int(2)])));
        assert_eq!(f.next(), Ok(None));
        assert_eq!(f.next(), Ok(None));
    }

    #[test]
    fn schema_is_the_childs_schema_unchanged() {
        let child = Mock::ints(&[]);
        let expected = child.schema().to_vec();
        assert_eq!(Filter::new(child, gt1()).schema(), expected.as_slice());
    }
}
