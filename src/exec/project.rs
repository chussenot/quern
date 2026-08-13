//! bead: quern-exec-project — the projection operator.
//!
//! One row in, one row out: for each `(Expr, String)` the planner put in
//! `LogicalPlan::Project`, evaluate the expression against the child row and
//! push the value, in order. `SELECT *` never reaches here — `plan::logical`
//! expands it into one expression per column, so there is no `Star` case.

use super::{eval, Operator};
use crate::sql::ast::Expr;
use crate::types::{Column, Result, Row, Type};

/// Projection over a child operator.
///
/// Unlike every other operator, `Project`'s output columns exist in no table,
/// so it cannot forward a borrowed schema: it **builds and owns** the `Vec<Column>`
/// in the constructor and hands out a slice of it.
pub struct Project {
    input: Box<dyn Operator>,
    exprs: Vec<Expr>,
    schema: Vec<Column>,
}

impl Project {
    /// `exprs` is `LogicalPlan::Project::exprs`: the expression and the output
    /// column name the planner resolved (the `AS` alias, else the source
    /// spelling).
    pub fn new(input: Box<dyn Operator>, exprs: Vec<(Expr, String)>) -> Self {
        let schema = exprs
            .iter()
            .map(|(expr, name)| Column {
                name: name.clone(),
                // No type inference, by decision (bead .43): §5 compares
                // tab-separated *values* and no headers, so nothing in the
                // engine ever reads this `ty`. The rule exists only because
                // `Operator::schema` is frozen at `&[Column]`. A bare column
                // carries its child's type through because that is free;
                // anything computed is stated to be `Int` and is not to be
                // believed. Do not grow an inference pass here.
                ty: match expr {
                    Expr::ColumnRef(i) => input.schema().get(*i).map_or(Type::Int, |col| col.ty),
                    _ => Type::Int,
                },
                primary_key: false,
            })
            .collect();
        Project {
            input,
            exprs: exprs.into_iter().map(|(expr, _)| expr).collect(),
            schema,
        }
    }
}

impl Operator for Project {
    fn schema(&self) -> &[Column] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>> {
        let Some(row) = self.input.next()? else {
            return Ok(None);
        };
        // `collect` into `Result<Row>` propagates the first eval error, so a
        // type error in one projection fails the statement rather than
        // yielding a half-built row.
        self.exprs
            .iter()
            .map(|expr| eval(expr, &row))
            .collect::<Result<Row>>()
            .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::BinOp;
    use crate::types::{QuernError, Value};

    /// A child operator over canned rows — the only way to unit-test an
    /// operator without dragging in storage.
    struct Mock {
        schema: Vec<Column>,
        rows: std::vec::IntoIter<Row>,
    }

    impl Mock {
        /// Columns are named `a`, `b`, `c`… and typed from `types`.
        fn boxed(types: &[Type], rows: Vec<Row>) -> Box<dyn Operator> {
            Box::new(Mock {
                schema: types
                    .iter()
                    .zip('a'..)
                    .map(|(ty, name)| Column {
                        name: name.to_string(),
                        ty: *ty,
                        primary_key: false,
                    })
                    .collect(),
                rows: rows.into_iter(),
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

    fn int(i: i64) -> Value {
        Value::Int(i)
    }

    /// Drain an operator into rows.
    fn drain(mut op: Project) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        while let Some(row) = op.next()? {
            out.push(row);
        }
        Ok(out)
    }

    /// `SELECT a, c` over three columns: a subset, child order preserved.
    #[test]
    fn projects_a_subset_of_columns() {
        let child = Mock::boxed(
            &[Type::Int, Type::Text, Type::Bool],
            vec![
                vec![int(1), Value::Text("x".into()), Value::Bool(true)],
                vec![int(2), Value::Text("y".into()), Value::Bool(false)],
            ],
        );
        let op = Project::new(
            child,
            vec![
                (Expr::ColumnRef(0), "a".to_string()),
                (Expr::ColumnRef(2), "c".to_string()),
            ],
        );
        assert_eq!(
            op.schema()
                .iter()
                .map(|c| (c.name.as_str(), c.ty))
                .collect::<Vec<_>>(),
            [("a", Type::Int), ("c", Type::Bool)]
        );
        assert_eq!(
            drain(op),
            Ok(vec![
                vec![int(1), Value::Bool(true)],
                vec![int(2), Value::Bool(false)],
            ])
        );
    }

    /// `SELECT b, a` — the output order is the projection's, not the child's,
    /// and a column may be repeated.
    #[test]
    fn reorders_and_repeats_columns() {
        let child = Mock::boxed(
            &[Type::Int, Type::Text],
            vec![vec![int(1), Value::Text("x".into())]],
        );
        let op = Project::new(
            child,
            vec![
                (Expr::ColumnRef(1), "b".to_string()),
                (Expr::ColumnRef(0), "a".to_string()),
                (Expr::ColumnRef(0), "a".to_string()),
            ],
        );
        assert_eq!(
            op.schema().iter().map(|c| c.ty).collect::<Vec<_>>(),
            [Type::Text, Type::Int, Type::Int]
        );
        assert_eq!(
            drain(op),
            Ok(vec![vec![Value::Text("x".into()), int(1), int(1)]])
        );
    }

    /// `SELECT a + 1 AS plus` — an arithmetic expression, under its alias.
    #[test]
    fn evaluates_an_arithmetic_expression_under_its_alias() {
        let child = Mock::boxed(&[Type::Int], vec![vec![int(1)], vec![int(41)]]);
        let op = Project::new(
            child,
            vec![(
                Expr::bin(Expr::ColumnRef(0), BinOp::Add, Expr::Literal(int(1))),
                "plus".to_string(),
            )],
        );
        assert_eq!(op.schema().len(), 1);
        assert_eq!(op.schema()[0].name, "plus");
        // Stated default: nothing reads it, but it must be *something*.
        assert_eq!(op.schema()[0].ty, Type::Int);
        assert_eq!(drain(op), Ok(vec![vec![int(2)], vec![int(42)]]));
    }

    /// NULL flows through an expression rather than erroring (§1).
    #[test]
    fn null_flows_through_an_expression() {
        let child = Mock::boxed(&[Type::Int], vec![vec![Value::Null]]);
        let op = Project::new(
            child,
            vec![
                (Expr::ColumnRef(0), "a".to_string()),
                (
                    Expr::bin(Expr::ColumnRef(0), BinOp::Mul, Expr::Literal(int(2))),
                    "double".to_string(),
                ),
            ],
        );
        assert_eq!(drain(op), Ok(vec![vec![Value::Null, Value::Null]]));
    }

    /// An eval error propagates out of `next` instead of being swallowed.
    #[test]
    fn an_eval_error_propagates() {
        let child = Mock::boxed(&[Type::Text], vec![vec![Value::Text("1".into())]]);
        // '1' + 1 is a type error, and #9 is out of range: either kills it.
        let mut op = Project::new(
            child,
            vec![
                (
                    Expr::bin(Expr::ColumnRef(0), BinOp::Add, Expr::Literal(int(1))),
                    "bad".to_string(),
                ),
                (Expr::ColumnRef(9), "worse".to_string()),
            ],
        );
        assert!(matches!(op.next(), Err(QuernError::Type(_))));
        // An out-of-range ColumnRef in the *schema* is not a panic either.
        assert_eq!(op.schema()[1].ty, Type::Int);
    }

    /// An empty child yields nothing, and stays exhausted.
    #[test]
    fn an_empty_child_yields_no_rows() {
        let child = Mock::boxed(&[Type::Int], vec![]);
        let mut op = Project::new(child, vec![(Expr::ColumnRef(0), "a".to_string())]);
        assert_eq!(op.next(), Ok(None));
        assert_eq!(op.next(), Ok(None));
        assert_eq!(op.schema().len(), 1);
    }
}
