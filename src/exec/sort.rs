//! bead: quern-exec-sort — `ORDER BY`. See docs/quern.md §1 and §3.
//!
//! `Sort` is the one blocking operator in the tree: it drains its child in
//! full, sorts, then hands the rows back one at a time. §1's NULL rule for
//! ordering lives in [`Value::sort_cmp`] and is *not* re-implemented here —
//! that comparator is already reversed for `descending`, so this file never
//! calls `.reverse()` and never matches on `Value::Null`.
//!
//! ponytail: the whole input is materialised in memory (one `Vec` of rows plus
//! their key values). Upgrade path if a table ever outgrows RAM: an external
//! merge sort — spill sorted runs of N rows to temp files, then k-way merge in
//! `next()`. Nothing above `Sort` has to change for that; it is all behind
//! `materialise`.

use std::cmp::Ordering;

use super::{eval, Operator};
use crate::sql::ast::Expr;
use crate::types::{Column, Result, Row, Value};

pub struct Sort {
    input: Box<dyn Operator>,
    /// `LogicalPlan::Sort::keys`: the `bool` is **descending**.
    keys: Vec<(Expr, bool)>,
    /// `None` until the first `next()` materialises and sorts the child.
    sorted: Option<std::vec::IntoIter<Row>>,
}

impl Sort {
    pub fn new(input: Box<dyn Operator>, keys: Vec<(Expr, bool)>) -> Sort {
        Sort {
            input,
            keys,
            sorted: None,
        }
    }

    /// Drain the child, evaluate every sort key once per row, sort, and return
    /// the rows in order.
    ///
    /// The keys are evaluated up front rather than inside the comparator for
    /// two reasons: a comparator cannot return an error (so an eval failure has
    /// to surface before the sort starts), and `O(n log n)` comparisons would
    /// otherwise re-evaluate the same expression over and over.
    ///
    /// `sort_by` — not `sort_unstable_by` — because §1 wants ties to keep the
    /// child's order so the corpus is deterministic.
    fn materialise(&mut self) -> Result<std::vec::IntoIter<Row>> {
        let mut keyed: Vec<(Vec<Value>, Row)> = Vec::new();
        while let Some(row) = self.input.next()? {
            let key = self
                .keys
                .iter()
                .map(|(expr, _)| eval(expr, &row))
                .collect::<Result<Vec<Value>>>()?;
            keyed.push((key, row));
        }

        let descending: Vec<bool> = self.keys.iter().map(|(_, d)| *d).collect();
        keyed.sort_by(|(a, _), (b, _)| {
            a.iter()
                .zip(b)
                .zip(&descending)
                .map(|((a, b), &desc)| Value::sort_cmp(a, b, desc))
                .find(|o| *o != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        });

        Ok(keyed
            .into_iter()
            .map(|(_, row)| row)
            .collect::<Vec<Row>>()
            .into_iter())
    }
}

impl Operator for Sort {
    /// Sorting permutes rows; it does not change their shape.
    fn schema(&self) -> &[Column] {
        self.input.schema()
    }

    fn next(&mut self) -> Result<Option<Row>> {
        if self.sorted.is_none() {
            self.sorted = Some(self.materialise()?);
        }
        Ok(self.sorted.as_mut().and_then(Iterator::next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::BinOp;
    use crate::types::{QuernError, Type};

    /// A child operator over a fixed row list. `Err` rows are not a thing an
    /// operator can hold, so eval errors are provoked with a bad key expression
    /// instead (see `eval_error_propagates`).
    struct Mock {
        schema: Vec<Column>,
        rows: std::vec::IntoIter<Row>,
    }

    impl Mock {
        fn boxed(cols: &[(&str, Type)], rows: Vec<Row>) -> Box<dyn Operator> {
            Box::new(Mock {
                schema: cols
                    .iter()
                    .map(|(name, ty)| Column {
                        name: (*name).to_string(),
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
    fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    /// `ORDER BY` the given keys over single-column rows, drained to a Vec.
    fn drain(op: &mut dyn Operator) -> Vec<Row> {
        let mut out = Vec::new();
        while let Some(row) = op.next().expect("no eval error expected") {
            out.push(row);
        }
        out
    }

    /// One INT column, sorted on it.
    fn sort_one(values: Vec<Value>, descending: bool) -> Vec<Value> {
        let rows: Vec<Row> = values.into_iter().map(|v| vec![v]).collect();
        let mut sort = Sort::new(
            Mock::boxed(&[("a", Type::Int)], rows),
            vec![(Expr::ColumnRef(0), descending)],
        );
        drain(&mut sort).into_iter().map(|r| r[0].clone()).collect()
    }

    #[test]
    fn asc_and_desc_on_int_and_text() {
        assert_eq!(
            sort_one(vec![int(3), int(1), int(2)], false),
            vec![int(1), int(2), int(3)]
        );
        assert_eq!(
            sort_one(vec![int(3), int(1), int(2)], true),
            vec![int(3), int(2), int(1)]
        );
        assert_eq!(
            sort_one(vec![text("b"), text("c"), text("a")], false),
            vec![text("a"), text("b"), text("c")]
        );
        assert_eq!(
            sort_one(vec![text("b"), text("c"), text("a")], true),
            vec![text("c"), text("b"), text("a")]
        );
    }

    #[test]
    fn null_sorts_last_ascending_and_first_descending() {
        // §1's rule, on INT and on TEXT: one NULL-last order, reversed for
        // DESC — not "NULL last in both directions".
        assert_eq!(
            sort_one(vec![Value::Null, int(2), int(1)], false),
            vec![int(1), int(2), Value::Null]
        );
        assert_eq!(
            sort_one(vec![int(2), Value::Null, int(1)], true),
            vec![Value::Null, int(2), int(1)]
        );
        assert_eq!(
            sort_one(vec![text("b"), Value::Null, text("a")], false),
            vec![text("a"), text("b"), Value::Null]
        );
        assert_eq!(
            sort_one(vec![text("b"), Value::Null, text("a")], true),
            vec![Value::Null, text("b"), text("a")]
        );
        // Several NULLs, and nothing but NULLs, are both fine.
        assert_eq!(
            sort_one(vec![Value::Null, int(1), Value::Null], false),
            vec![int(1), Value::Null, Value::Null]
        );
        assert_eq!(
            sort_one(vec![Value::Null, Value::Null], true),
            vec![Value::Null, Value::Null]
        );
    }

    #[test]
    fn multi_key_with_mixed_asc_and_desc() {
        // ORDER BY a ASC, b DESC over (a, b).
        let rows = vec![
            vec![int(1), int(10)],
            vec![int(2), int(20)],
            vec![int(1), int(30)],
            vec![int(2), int(5)],
        ];
        let mut sort = Sort::new(
            Mock::boxed(&[("a", Type::Int), ("b", Type::Int)], rows),
            vec![(Expr::ColumnRef(0), false), (Expr::ColumnRef(1), true)],
        );
        assert_eq!(
            drain(&mut sort),
            vec![
                vec![int(1), int(30)],
                vec![int(1), int(10)],
                vec![int(2), int(20)],
                vec![int(2), int(5)],
            ]
        );
        // The second key only breaks ties on the first: a DESC, b ASC.
        let rows = vec![
            vec![int(1), int(10)],
            vec![int(2), int(20)],
            vec![int(1), Value::Null],
            vec![int(2), int(5)],
        ];
        let mut sort = Sort::new(
            Mock::boxed(&[("a", Type::Int), ("b", Type::Int)], rows),
            vec![(Expr::ColumnRef(0), true), (Expr::ColumnRef(1), false)],
        );
        assert_eq!(
            drain(&mut sort),
            vec![
                vec![int(2), int(5)],
                vec![int(2), int(20)],
                vec![int(1), int(10)],
                vec![int(1), Value::Null], // NULL last: key b is ASC
            ]
        );
    }

    #[test]
    fn equal_keys_keep_child_order() {
        // Every row has key 7, so a stable sort must return the input verbatim.
        let rows: Vec<Row> = (0..6).map(|i| vec![int(7), int(i)]).collect();
        let mut sort = Sort::new(
            Mock::boxed(&[("k", Type::Int), ("tag", Type::Int)], rows.clone()),
            vec![(Expr::ColumnRef(0), false)],
        );
        assert_eq!(drain(&mut sort), rows);
        // Same input, DESC: stability is not direction-dependent.
        let mut sort = Sort::new(
            Mock::boxed(&[("k", Type::Int), ("tag", Type::Int)], rows.clone()),
            vec![(Expr::ColumnRef(0), true)],
        );
        assert_eq!(drain(&mut sort), rows);
    }

    #[test]
    fn sorts_by_an_expression_not_only_a_column() {
        // ORDER BY -a  ==  a DESC, via the shared evaluator.
        let rows: Vec<Row> = vec![int(1), int(3), int(2)]
            .into_iter()
            .map(|v| vec![v])
            .collect();
        let mut sort = Sort::new(
            Mock::boxed(&[("a", Type::Int)], rows),
            vec![(
                Expr::bin(Expr::Literal(int(0)), BinOp::Sub, Expr::ColumnRef(0)),
                false,
            )],
        );
        assert_eq!(
            drain(&mut sort),
            vec![vec![int(3)], vec![int(2)], vec![int(1)]]
        );
    }

    #[test]
    fn eval_error_propagates() {
        // ColumnRef past the end of the row is QuernError::Type in eval, and
        // Sort must surface it rather than sorting a partial result.
        let mut sort = Sort::new(
            Mock::boxed(&[("a", Type::Int)], vec![vec![int(1)]]),
            vec![(Expr::ColumnRef(9), false)],
        );
        assert!(matches!(sort.next(), Err(QuernError::Type(_))));
        // A type error inside the key expression surfaces the same way.
        let mut sort = Sort::new(
            Mock::boxed(&[("a", Type::Int)], vec![vec![text("x")]]),
            vec![(
                Expr::bin(Expr::ColumnRef(0), BinOp::Add, Expr::Literal(int(1))),
                false,
            )],
        );
        assert!(matches!(sort.next(), Err(QuernError::Type(_))));
    }

    #[test]
    fn empty_child_yields_nothing_and_stays_drained() {
        let mut sort = Sort::new(
            Mock::boxed(&[("a", Type::Int)], Vec::new()),
            vec![(Expr::ColumnRef(0), false)],
        );
        assert_eq!(sort.next(), Ok(None));
        assert_eq!(sort.next(), Ok(None));
        // Schema passes through unchanged, even with no rows.
        assert_eq!(sort.schema().len(), 1);
        assert_eq!(sort.schema()[0].name, "a");

        // And a drained non-empty sort keeps returning Ok(None).
        let mut sort = Sort::new(
            Mock::boxed(&[("a", Type::Int)], vec![vec![int(1)]]),
            vec![(Expr::ColumnRef(0), true)],
        );
        assert_eq!(sort.next(), Ok(Some(vec![int(1)])));
        assert_eq!(sort.next(), Ok(None));
        assert_eq!(sort.next(), Ok(None));
    }
}
