//! bead: quern-exec-join — inner nested-loop `JOIN .. ON`. See docs/quern.md §1.
//!
//! There is no `LEFT`/`RIGHT`/`OUTER` join in this engine (§1), so a left row
//! that matches nothing emits nothing — there is no padded row to fall back to.
//!
//! The `ON` predicate is evaluated against the **concatenated** row, left
//! columns then right columns, because `plan::logical` resolved its
//! `ColumnRef(i)` indices against exactly that combined schema. [`Join::schema`]
//! returns that same concatenation, which is what the operator above us
//! resolved its own indices against.

use crate::exec::{eval, Operator};
use crate::sql::ast::Expr;
use crate::types::{Column, Result, Row};

/// ponytail: nested loop, O(left * right) predicate evaluations with the whole
/// right side resident in memory. Upgrade path when the corpus outgrows it: a
/// hash join on the equi-join key — build a `HashMap<Value, Vec<Row>>` from the
/// buffered right rows when `on` is a single `ColumnRef = ColumnRef`, probe it
/// per left row, and keep this loop as the fallback for every other predicate.
pub struct Join {
    left: Box<dyn Operator>,
    /// The right side, drained once in [`Join::new`]: `next` rescans it for
    /// every left row, so it cannot be a live child operator.
    right: Vec<Row>,
    on: Expr,
    schema: Vec<Column>,
    /// The left row currently being matched, and how far into `right` its scan
    /// has got. Held across calls because one left row can emit many rows.
    current: Option<Row>,
    probe: usize,
}

impl Join {
    /// Materialises the right side immediately; the left stays lazy.
    pub fn new(left: Box<dyn Operator>, mut right: Box<dyn Operator>, on: Expr) -> Result<Self> {
        let schema = [left.schema(), right.schema()].concat();
        let mut buffered = Vec::new();
        while let Some(row) = right.next()? {
            buffered.push(row);
        }
        Ok(Self {
            left,
            right: buffered,
            on,
            schema,
            current: None,
            probe: 0,
        })
    }
}

impl Operator for Join {
    fn schema(&self) -> &[Column] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>> {
        loop {
            let Some(left) = &self.current else {
                match self.left.next()? {
                    Some(row) => {
                        self.current = Some(row);
                        self.probe = 0;
                        continue;
                    }
                    // Left exhausted: exhausted for good, and staying that way
                    // because `current` is `None` and `left.next()` keeps
                    // yielding `None`.
                    None => return Ok(None),
                }
            };
            while self.probe < self.right.len() {
                let mut combined = left.clone();
                combined.extend_from_slice(&self.right[self.probe]);
                self.probe += 1;
                // Only Bool(true) matches: a Null predicate (a Null join key)
                // is falsy, per §1, and an inner join has nothing to emit for
                // it.
                if eval(&self.on, &combined)?.is_true() {
                    return Ok(Some(combined));
                }
            }
            self.current = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::BinOp;
    use crate::types::{QuernError, Type, Value};

    /// A canned child operator: fixed schema, fixed rows, then `Ok(None)`.
    struct Mock {
        schema: Vec<Column>,
        rows: std::vec::IntoIter<Row>,
    }

    fn mock(names: &[&str], rows: Vec<Row>) -> Box<dyn Operator> {
        Box::new(Mock {
            schema: names
                .iter()
                .map(|n| Column {
                    name: (*n).to_string(),
                    ty: Type::Int,
                    primary_key: false,
                })
                .collect(),
            rows: rows.into_iter(),
        })
    }

    impl Operator for Mock {
        fn schema(&self) -> &[Column] {
            &self.schema
        }
        fn next(&mut self) -> Result<Option<Row>> {
            Ok(self.rows.next())
        }
    }

    fn int_row(vs: &[i64]) -> Row {
        vs.iter().copied().map(Value::Int).collect()
    }

    /// `#l = #r` over the concatenated row.
    fn eq_on(l: usize, r: usize) -> Expr {
        Expr::bin(Expr::ColumnRef(l), BinOp::Eq, Expr::ColumnRef(r))
    }

    fn drain(mut join: Join) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        while let Some(row) = join.next()? {
            out.push(row);
        }
        // A drained operator keeps saying so.
        assert_eq!(join.next(), Ok(None));
        Ok(out)
    }

    /// left(a) ⨝ right(b, c) on a = b, one match per left row.
    #[test]
    fn one_to_one_join_concatenates_left_then_right() {
        let left = mock(&["a"], vec![int_row(&[1]), int_row(&[2])]);
        let right = mock(&["b", "c"], vec![int_row(&[1, 10]), int_row(&[2, 20])]);
        let join = Join::new(left, right, eq_on(0, 1)).unwrap();
        assert_eq!(
            drain(join).unwrap(),
            vec![int_row(&[1, 1, 10]), int_row(&[2, 2, 20])]
        );
    }

    /// Inner join: an unmatched left row is simply absent from the output.
    #[test]
    fn left_row_matching_nothing_emits_nothing() {
        let left = mock(&["a"], vec![int_row(&[1]), int_row(&[9]), int_row(&[2])]);
        let right = mock(&["b"], vec![int_row(&[1]), int_row(&[2])]);
        let join = Join::new(left, right, eq_on(0, 1)).unwrap();
        assert_eq!(
            drain(join).unwrap(),
            vec![int_row(&[1, 1]), int_row(&[2, 2])]
        );
    }

    /// Duplicates on both sides produce the full cross of the matches: 2x2 on
    /// key 1, plus the single 2-2 pair.
    #[test]
    fn many_to_many_join_emits_every_matching_pair() {
        let left = mock(&["a"], vec![int_row(&[1]), int_row(&[2]), int_row(&[1])]);
        let right = mock(
            &["b", "c"],
            vec![int_row(&[1, 10]), int_row(&[1, 11]), int_row(&[2, 20])],
        );
        let join = Join::new(left, right, eq_on(0, 1)).unwrap();
        assert_eq!(
            drain(join).unwrap(),
            vec![
                int_row(&[1, 1, 10]),
                int_row(&[1, 1, 11]),
                int_row(&[2, 2, 20]),
                int_row(&[1, 1, 10]),
                int_row(&[1, 1, 11]),
            ]
        );
    }

    #[test]
    fn an_empty_side_yields_nothing() {
        let empty_left = Join::new(
            mock(&["a"], vec![]),
            mock(&["b"], vec![int_row(&[1])]),
            eq_on(0, 1),
        )
        .unwrap();
        assert_eq!(drain(empty_left).unwrap(), Vec::<Row>::new());

        let empty_right = Join::new(
            mock(&["a"], vec![int_row(&[1]), int_row(&[2])]),
            mock(&["b"], vec![]),
            eq_on(0, 1),
        )
        .unwrap();
        assert_eq!(drain(empty_right).unwrap(), Vec::<Row>::new());
    }

    /// `NULL = NULL` is Null, which is not Bool(true): a Null key joins to
    /// nothing, including to another Null.
    #[test]
    fn null_join_key_matches_nothing() {
        let left = mock(&["a"], vec![vec![Value::Null], int_row(&[1])]);
        let right = mock(&["b"], vec![vec![Value::Null], int_row(&[1])]);
        let join = Join::new(left, right, eq_on(0, 1)).unwrap();
        assert_eq!(drain(join).unwrap(), vec![int_row(&[1, 1])]);
    }

    #[test]
    fn schema_is_the_left_schema_then_the_right_schema() {
        let join = Join::new(mock(&["a", "b"], vec![]), mock(&["c"], vec![]), eq_on(0, 2)).unwrap();
        let names: Vec<&str> = join.schema().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    /// An `ON` that cannot be evaluated is a statement error, not a non-match.
    #[test]
    fn eval_error_in_the_predicate_propagates() {
        let on = Expr::bin(
            Expr::ColumnRef(0),
            BinOp::Eq,
            Expr::Literal(Value::Text("1".to_string())),
        );
        let mut join = Join::new(
            mock(&["a"], vec![int_row(&[1])]),
            mock(&["b"], vec![int_row(&[1])]),
            on,
        )
        .unwrap();
        assert!(matches!(join.next(), Err(QuernError::Type(_))));

        // A ColumnRef past the end of the combined row is an error too, not a
        // panic — this is how a mis-lowered plan shows up.
        let mut wide = Join::new(
            mock(&["a"], vec![int_row(&[1])]),
            mock(&["b"], vec![int_row(&[1])]),
            eq_on(0, 2),
        )
        .unwrap();
        assert!(matches!(wide.next(), Err(QuernError::Type(_))));
    }
}
