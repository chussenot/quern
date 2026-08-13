//! bead: quern-exec-aggregate — `GROUP BY` with the five aggregates.
//!
//! A pipeline breaker: the whole child is consumed before the first output row
//! exists, so [`Aggregate::new`] drains it and `next()` just hands back the
//! rows it built. That also keeps the operator `'static` (see `exec` module
//! docs: nothing here may outlive a borrow of `Storage`).
//!
//! Three contracts worth stating out loud, because each one fails silently:
//!
//! * **Row shape** is `group_by.len() + aggs.len()` values: the group keys in
//!   `group_by` order FIRST, then the aggregates in `aggs` order (docs/quern.md
//!   §3, bead .39). `plan::logical` puts a `Project` above us to permute into
//!   `SELECT`-list order, and it assumes exactly this.
//! * **Groups are emitted in sorted key order**, never hash order — §5 grades
//!   real output and the corpus compares it. The groups live in a `BTreeMap`
//!   keyed by the key `Vec<Value>`, so the ordering is `Value`'s derived `Ord`
//!   (the *determinism* order documented in `types.rs`) applied
//!   lexicographically. Deliberately NOT `Value::sort_cmp`: that is the
//!   `ORDER BY` comparator with §1's NULL-placement rules, which belongs to
//!   `exec::sort` and would make group order depend on a sort direction we do
//!   not have.
//! * **NULLs**: per §1 every aggregate skips `Null` inputs, and `COUNT(*)`
//!   counts rows regardless. So in a group whose column is entirely `Null`,
//!   `SUM`/`MIN`/`MAX`/`AVG` are `Null` while `COUNT(*)` is the row count (and
//!   `COUNT(col)` is 0).
//!
//! `AVG` over `INT` yields `INT`, truncating toward zero — quern has no float
//! type, and §1 leaves the choice to this bead. It is the running `SUM` divided
//! by the number of counted (non-`Null`) rows, through `Value::div`, so it is
//! plain i64 division: `AVG` of 1 and 2 is 1, of -1 and -2 is -1.

use std::collections::BTreeMap;

use super::{eval, Operator};
use crate::sql::ast::{AggExpr, AggFunc, BinOp, Expr, UnOp};
use crate::types::{Column, Result, Row, Type, Value};

/// Hash-free grouping aggregate. Group keys first, then aggregates; see the
/// module docs.
pub struct Aggregate {
    schema: Vec<Column>,
    rows: std::vec::IntoIter<Row>,
}

impl Aggregate {
    /// Drain `input`, group it, and compute the aggregates. `group_by` empty
    /// means one group over all rows, which is emitted even when the child is
    /// empty — `SELECT COUNT(*) FROM empty` is one row, `0`.
    pub fn new(
        mut input: Box<dyn Operator>,
        group_by: &[Expr],
        aggs: &[AggExpr],
    ) -> Result<Aggregate> {
        let schema = output_schema(input.schema(), group_by, aggs);

        let mut groups: BTreeMap<Vec<Value>, Vec<Acc>> = BTreeMap::new();
        // The no-GROUP-BY group exists before any row does, so it survives an
        // empty child.
        if group_by.is_empty() {
            groups.insert(Vec::new(), vec![Acc::default(); aggs.len()]);
        }
        while let Some(row) = input.next()? {
            let key = group_by
                .iter()
                .map(|e| eval(e, &row))
                .collect::<Result<Vec<Value>>>()?;
            let accs = groups
                .entry(key)
                .or_insert_with(|| vec![Acc::default(); aggs.len()]);
            for (acc, agg) in accs.iter_mut().zip(aggs) {
                acc.push(agg, &row)?;
            }
        }

        let rows = groups
            .into_iter()
            .map(|(key, accs)| {
                let mut row = key;
                for (acc, agg) in accs.iter().zip(aggs) {
                    row.push(acc.finish(agg.func)?);
                }
                Ok(row)
            })
            .collect::<Result<Vec<Row>>>()?;

        Ok(Aggregate {
            schema,
            rows: rows.into_iter(),
        })
    }
}

impl Operator for Aggregate {
    fn schema(&self) -> &[Column] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>> {
        Ok(self.rows.next()) // drained stays drained: IntoIter keeps yielding None
    }
}

/// Running state for ONE aggregate in ONE group.
///
/// `n` is what this aggregate counted: every row for `COUNT(*)`, only the
/// non-`Null` inputs for everything else — which is the whole of §1's NULL rule
/// for aggregates, and why `COUNT(*)` and `COUNT(a)` differ.
#[derive(Clone, Default)]
struct Acc {
    n: i64,
    /// `SUM`/`AVG` total. `None` until the first non-`Null` input, so an
    /// all-`Null` group stays `Null` instead of becoming `0`.
    sum: Option<Value>,
    /// `MIN`/`MAX` extreme so far, `None` until the first non-`Null` input.
    best: Option<Value>,
}

impl Acc {
    fn push(&mut self, agg: &AggExpr, row: &Row) -> Result<()> {
        // COUNT(*) is the one aggregate with no argument, and the one that
        // counts a row whatever it holds.
        let Some(arg) = &agg.arg else {
            self.n += 1;
            return Ok(());
        };
        let v = eval(arg, row)?;
        if v == Value::Null {
            return Ok(());
        }
        self.n += 1;
        match agg.func {
            AggFunc::Count => {}
            // Delegated to types.rs on purpose: `Value::add` is where §1 lives,
            // and it is already `checked_add` — i64 overflow comes back as
            // Err(QuernError::Type), never a panic, and SUM of a TEXT column is
            // its type error rather than a second one written here.
            AggFunc::Sum | AggFunc::Avg => {
                let total = self.sum.take().unwrap_or(Value::Int(0));
                self.sum = Some(Value::add(&total, &v)?);
            }
            // `Value::sort_cmp` ascending — the SAME comparator `exec::sort`
            // uses, so `MIN(f)` can never disagree with `ORDER BY f` (bead .47:
            // FALSE < TRUE for BOOL, which is Rust's own bool order). Its
            // NULL-placement half is unreachable here: Null inputs were skipped
            // above, so this only ever compares non-Null values.
            AggFunc::Min => {
                if self
                    .best
                    .as_ref()
                    .is_none_or(|b| Value::sort_cmp(&v, b, false).is_lt())
                {
                    self.best = Some(v);
                }
            }
            AggFunc::Max => {
                if self
                    .best
                    .as_ref()
                    .is_none_or(|b| Value::sort_cmp(&v, b, false).is_gt())
                {
                    self.best = Some(v);
                }
            }
        }
        Ok(())
    }

    fn finish(&self, func: AggFunc) -> Result<Value> {
        Ok(match func {
            AggFunc::Count => Value::Int(self.n),
            AggFunc::Sum => self.sum.clone().unwrap_or(Value::Null),
            AggFunc::Min | AggFunc::Max => self.best.clone().unwrap_or(Value::Null),
            // `sum` is `Some` exactly when `n > 0`, so the division cannot be
            // by zero: an all-Null group is Null, not an error.
            AggFunc::Avg => match &self.sum {
                None => Value::Null,
                Some(total) => Value::div(total, &Value::Int(self.n))?,
            },
        })
    }
}

/// Group-key columns then aggregate columns, matching the emitted row.
///
/// Types follow bead .43's stated convention and nothing more: nothing in the
/// engine reads `ty` on an operator's output schema (§5 compares no headers),
/// so this is a match, not a type-inference pass. Do not grow one.
fn output_schema(input: &[Column], group_by: &[Expr], aggs: &[AggExpr]) -> Vec<Column> {
    let derived = |name: String, ty: Type| Column {
        name,
        ty,
        primary_key: false,
    };
    group_by
        .iter()
        .map(|e| {
            let name = match e {
                Expr::ColumnRef(i) => input
                    .get(*i)
                    .map_or_else(|| e.to_string(), |c| c.name.clone()),
                _ => e.to_string(),
            };
            derived(name, ty_of(e, input))
        })
        .chain(aggs.iter().map(|a| {
            let ty = match (a.func, &a.arg) {
                // MIN/MAX keep their argument's type; the rest are INT, AVG
                // included (it truncates).
                (AggFunc::Min | AggFunc::Max, Some(arg)) => ty_of(arg, input),
                _ => Type::Int,
            };
            derived(a.alias.clone(), ty)
        }))
        .collect()
}

fn ty_of(e: &Expr, input: &[Column]) -> Type {
    match e {
        Expr::ColumnRef(i) => input.get(*i).map_or(Type::Int, |c| c.ty),
        Expr::Literal(Value::Text(_)) => Type::Text,
        Expr::Literal(Value::Bool(_)) => Type::Bool,
        Expr::Binary {
            op: BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::And | BinOp::Or,
            ..
        }
        | Expr::Unary {
            op: UnOp::Not,
            expr: _,
        } => Type::Bool,
        _ => Type::Int,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child operator built from literal rows — the only thing these tests
    /// need from the rest of `exec`.
    struct Mock {
        schema: Vec<Column>,
        rows: std::vec::IntoIter<Row>,
    }

    impl Mock {
        /// One column per `types`, named `c0..`, over the given rows.
        fn boxed(types: &[Type], rows: Vec<Row>) -> Box<dyn Operator> {
            Box::new(Mock {
                schema: types
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| Column {
                        name: format!("c{i}"),
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
    fn txt(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    /// `func(#col)` for every aggregate but `COUNT(*)`.
    fn agg(func: AggFunc, col: usize) -> AggExpr {
        AggExpr::of(func, Expr::ColumnRef(col))
    }

    fn run(child: Box<dyn Operator>, group_by: &[Expr], aggs: &[AggExpr]) -> Result<Vec<Row>> {
        let mut op = Aggregate::new(child, group_by, aggs)?;
        let mut out = Vec::new();
        while let Some(row) = op.next()? {
            out.push(row);
        }
        // A drained operator keeps saying so.
        assert_eq!(op.next(), Ok(None));
        Ok(out)
    }

    #[test]
    fn all_five_aggregates_per_group_keys_first() {
        // (group, value): group 1 -> 10, 30; group 2 -> 7.
        let child = Mock::boxed(
            &[Type::Int, Type::Int],
            vec![
                vec![int(1), int(10)],
                vec![int(2), int(7)],
                vec![int(1), int(30)],
            ],
        );
        let aggs = [
            AggExpr::count_star(),
            agg(AggFunc::Count, 1),
            agg(AggFunc::Sum, 1),
            agg(AggFunc::Min, 1),
            agg(AggFunc::Max, 1),
            agg(AggFunc::Avg, 1),
        ];
        let mut op = Aggregate::new(child, &[Expr::ColumnRef(0)], &aggs).unwrap();
        // Shape: one key column then six aggregates, in that order.
        assert_eq!(op.schema().len(), 7);
        assert_eq!(op.schema()[0].name, "c0");
        assert_eq!(op.schema()[1].name, "COUNT(*)");
        let mut out = Vec::new();
        while let Some(r) = op.next().unwrap() {
            out.push(r);
        }
        assert_eq!(
            out,
            vec![
                vec![int(1), int(2), int(2), int(40), int(10), int(30), int(20)],
                vec![int(2), int(1), int(1), int(7), int(7), int(7), int(7)],
            ]
        );
    }

    #[test]
    fn groups_are_emitted_in_sorted_key_order() {
        // Deliberately unsorted input, with a repeat, so hash order would show.
        let rows = [30, 10, 20, 10, -5, 0]
            .iter()
            .map(|k| vec![int(*k)])
            .collect();
        let out = run(
            Mock::boxed(&[Type::Int], rows),
            &[Expr::ColumnRef(0)],
            &[AggExpr::count_star()],
        )
        .unwrap();
        let keys: Vec<&Value> = out.iter().map(|r| &r[0]).collect();
        assert_eq!(keys, vec![&int(-5), &int(0), &int(10), &int(20), &int(30)]);
        assert_eq!(out[2][1], int(2)); // key 10 appeared twice

        // Two-column key: lexicographic, and NULL first (the determinism order
        // from types.rs, not ORDER BY's NULL-last).
        let rows = vec![
            vec![int(2), txt("a")],
            vec![int(1), txt("b")],
            vec![int(1), txt("a")],
            vec![Value::Null, txt("z")],
        ];
        let out = run(
            Mock::boxed(&[Type::Int, Type::Text], rows),
            &[Expr::ColumnRef(0), Expr::ColumnRef(1)],
            &[AggExpr::count_star()],
        )
        .unwrap();
        let keys: Vec<Row> = out.iter().map(|r| r[..2].to_vec()).collect();
        assert_eq!(
            keys,
            vec![
                vec![Value::Null, txt("z")],
                vec![int(1), txt("a")],
                vec![int(1), txt("b")],
                vec![int(2), txt("a")],
            ]
        );
    }

    #[test]
    fn grouping_by_text_and_by_bool() {
        let rows = vec![
            vec![txt("b"), int(1)],
            vec![txt("a"), int(2)],
            vec![txt("b"), int(3)],
        ];
        let out = run(
            Mock::boxed(&[Type::Text, Type::Int], rows),
            &[Expr::ColumnRef(0)],
            &[agg(AggFunc::Sum, 1)],
        )
        .unwrap();
        assert_eq!(
            out,
            vec![vec![txt("a"), int(2)], vec![txt("b"), int(4)]],
            "TEXT keys sort lexicographically"
        );

        let rows = vec![
            vec![Value::Bool(true), int(1)],
            vec![Value::Bool(false), int(2)],
            vec![Value::Bool(true), int(4)],
        ];
        let out = run(
            Mock::boxed(&[Type::Bool, Type::Int], rows),
            &[Expr::ColumnRef(0)],
            &[agg(AggFunc::Sum, 1), agg(AggFunc::Max, 1)],
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                vec![Value::Bool(false), int(2), int(2)],
                vec![Value::Bool(true), int(5), int(4)],
            ],
            "FALSE before TRUE"
        );
        // MIN/MAX over TEXT works too, and keeps the argument's type.
        let rows = vec![vec![txt("m")], vec![txt("a")], vec![txt("z")]];
        let out = run(
            Mock::boxed(&[Type::Text], rows),
            &[],
            &[agg(AggFunc::Min, 0), agg(AggFunc::Max, 0)],
        )
        .unwrap();
        assert_eq!(out, vec![vec![txt("a"), txt("z")]]);

        // MIN/MAX over BOOL: FALSE is the lesser (bead .47, and 060_group.slt
        // case 18 expects `6 FALSE TRUE`).
        let rows = [true, false, true, true]
            .iter()
            .map(|b| vec![Value::Bool(*b)])
            .collect();
        let out = run(
            Mock::boxed(&[Type::Bool], rows),
            &[],
            &[
                AggExpr::count_star(),
                agg(AggFunc::Min, 0),
                agg(AggFunc::Max, 0),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            vec![vec![int(4), Value::Bool(false), Value::Bool(true)]]
        );
    }

    #[test]
    fn null_inputs_are_skipped_but_count_star_counts_rows() {
        // Group 1 has one NULL among three rows; group 2 is ALL NULL.
        let rows = vec![
            vec![int(1), int(4)],
            vec![int(1), Value::Null],
            vec![int(1), int(6)],
            vec![int(2), Value::Null],
            vec![int(2), Value::Null],
        ];
        let aggs = [
            AggExpr::count_star(),
            agg(AggFunc::Count, 1),
            agg(AggFunc::Sum, 1),
            agg(AggFunc::Min, 1),
            agg(AggFunc::Max, 1),
            agg(AggFunc::Avg, 1),
        ];
        let out = run(
            Mock::boxed(&[Type::Int, Type::Int], rows),
            &[Expr::ColumnRef(0)],
            &aggs,
        )
        .unwrap();
        // AVG(4, NULL, 6) is 5: the NULL is skipped by the divisor too.
        assert_eq!(
            out[0],
            vec![int(1), int(3), int(2), int(10), int(4), int(6), int(5)]
        );
        // The all-NULL group: COUNT(*) is the row count, COUNT(col) is 0, and
        // SUM/MIN/MAX/AVG are all NULL — not 0, which is the trap.
        assert_eq!(
            out[1],
            vec![
                int(2),
                int(2),
                int(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null
            ]
        );
        assert_ne!(out[1][1], out[1][3], "COUNT(*) and SUM differ on all-NULL");
    }

    #[test]
    fn no_group_by_is_one_group_even_over_an_empty_child() {
        let aggs = [
            AggExpr::count_star(),
            agg(AggFunc::Sum, 0),
            agg(AggFunc::Min, 0),
            agg(AggFunc::Max, 0),
            agg(AggFunc::Avg, 0),
        ];
        let rows = vec![vec![int(5)], vec![int(1)], vec![int(9)]];
        let out = run(Mock::boxed(&[Type::Int], rows), &[], &aggs).unwrap();
        assert_eq!(out, vec![vec![int(3), int(15), int(1), int(9), int(5)]]);

        // Empty child: still exactly one row. COUNT(*) of nothing is 0.
        let out = run(Mock::boxed(&[Type::Int], Vec::new()), &[], &aggs).unwrap();
        assert_eq!(
            out,
            vec![vec![
                int(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null
            ]]
        );
        // With a GROUP BY, an empty child is zero groups.
        let out = run(
            Mock::boxed(&[Type::Int], Vec::new()),
            &[Expr::ColumnRef(0)],
            &[AggExpr::count_star()],
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn avg_over_int_truncates_toward_zero() {
        let avg = |vals: &[i64]| {
            let rows = vals.iter().map(|v| vec![int(*v)]).collect();
            run(
                Mock::boxed(&[Type::Int], rows),
                &[],
                &[agg(AggFunc::Avg, 0)],
            )
            .unwrap()[0][0]
                .clone()
        };
        assert_eq!(avg(&[1, 2]), int(1)); // 3/2 -> 1, not 2
        assert_eq!(avg(&[7, 7, 8]), int(7)); // 22/3 -> 7
        assert_eq!(avg(&[-1, -2]), int(-1)); // -3/2 -> -1, toward zero
        assert_eq!(avg(&[-5]), int(-5));
    }

    #[test]
    fn sum_overflow_is_an_error_not_a_panic() {
        let rows = vec![vec![int(i64::MAX)], vec![int(1)]];
        // `.map(|_| ())` because an Operator is not Debug and expect_err wants it.
        let err = Aggregate::new(
            Mock::boxed(&[Type::Int], rows),
            &[],
            &[agg(AggFunc::Sum, 0)],
        )
        .map(|_| ())
        .expect_err("i64 overflow must be an Err");
        assert!(matches!(err, crate::types::QuernError::Type(_)), "{err:?}");

        // AVG shares the running SUM, so it overflows the same way.
        let rows = vec![vec![int(i64::MIN)], vec![int(-1)]];
        assert!(Aggregate::new(
            Mock::boxed(&[Type::Int], rows),
            &[],
            &[agg(AggFunc::Avg, 0)]
        )
        .is_err());

        // SUM of a TEXT column is types.rs's type error, not a panic either.
        let rows = vec![vec![txt("x")]];
        assert!(Aggregate::new(
            Mock::boxed(&[Type::Text], rows),
            &[],
            &[agg(AggFunc::Sum, 0)]
        )
        .is_err());
    }
}
