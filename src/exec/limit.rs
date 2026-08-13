//! bead: quern-exec-limit
//!
//! `LIMIT n`: yield at most `n` rows from the child, then report exhausted.
//!
//! The point of LIMIT is the work it *avoids*, so this never drains the child
//! and discards — once `n` rows are out (or the child is dry) the child is
//! never polled again, and `LIMIT 0` never polls it at all. There is no
//! buffering and no OFFSET: it is not in §1's SQL surface.

use super::Operator;
use crate::types::{Column, Result, Row};

pub struct Limit {
    input: Box<dyn Operator>,
    /// Rows still allowed out. Also latched to 0 when the child runs dry, so a
    /// drained `Limit` stops re-polling a drained child.
    remaining: usize,
}

impl Limit {
    pub fn new(input: Box<dyn Operator>, n: usize) -> Self {
        Self {
            input,
            remaining: n,
        }
    }
}

impl Operator for Limit {
    fn schema(&self) -> &[Column] {
        self.input.schema()
    }

    fn next(&mut self) -> Result<Option<Row>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        match self.input.next()? {
            Some(row) => {
                self.remaining -= 1;
                Ok(Some(row))
            }
            None => {
                self.remaining = 0;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Type, Value};
    use std::cell::Cell;
    use std::rc::Rc;

    /// A child that counts how many times it was polled, so the tests can
    /// assert on the work LIMIT did *not* ask for.
    struct Counting {
        schema: Vec<Column>,
        rows: std::vec::IntoIter<Row>,
        polls: Rc<Cell<usize>>,
    }

    impl Operator for Counting {
        fn schema(&self) -> &[Column] {
            &self.schema
        }
        fn next(&mut self) -> Result<Option<Row>> {
            self.polls.set(self.polls.get() + 1);
            Ok(self.rows.next())
        }
    }

    fn child(n: i64) -> (Box<dyn Operator>, Rc<Cell<usize>>) {
        let polls = Rc::new(Cell::new(0));
        let op = Counting {
            schema: vec![Column {
                name: "a".to_string(),
                ty: Type::Int,
                primary_key: false,
            }],
            rows: (0..n)
                .map(|i| vec![Value::Int(i)])
                .collect::<Vec<_>>()
                .into_iter(),
            polls: Rc::clone(&polls),
        };
        (Box::new(op), polls)
    }

    /// Drain an operator, returning the first column of every row.
    fn drain(op: &mut dyn Operator) -> Vec<i64> {
        let mut out = Vec::new();
        while let Some(row) = op.next().expect("no expression to fail") {
            match row[0] {
                Value::Int(i) => out.push(i),
                ref v => panic!("unexpected {v:?}"),
            }
        }
        out
    }

    #[test]
    fn limit_below_input_stops_pulling_from_the_child() {
        let (c, polls) = child(100);
        let mut limit = Limit::new(c, 3);
        assert_eq!(drain(&mut limit), vec![0, 1, 2]);
        // Three rows out, three polls in: the other 97 were never asked for,
        // and the exhaustion was reported without a fourth poll.
        assert_eq!(polls.get(), 3);
        // Still exhausted, still not touching the child.
        assert_eq!(limit.next(), Ok(None));
        assert_eq!(polls.get(), 3);
    }

    #[test]
    fn limit_equal_to_input_yields_everything_without_a_trailing_poll() {
        let (c, polls) = child(3);
        let mut limit = Limit::new(c, 3);
        assert_eq!(drain(&mut limit), vec![0, 1, 2]);
        assert_eq!(polls.get(), 3);
    }

    #[test]
    fn limit_above_input_yields_what_there_is() {
        let (c, polls) = child(2);
        let mut limit = Limit::new(c, 5);
        assert_eq!(drain(&mut limit), vec![0, 1]);
        // Two rows plus the one poll that reported the child dry.
        assert_eq!(polls.get(), 3);
        assert_eq!(limit.next(), Ok(None));
        assert_eq!(polls.get(), 3);
    }

    #[test]
    fn limit_zero_never_polls_the_child() {
        let (c, polls) = child(100);
        let mut limit = Limit::new(c, 0);
        assert_eq!(drain(&mut limit), Vec::<i64>::new());
        assert_eq!(polls.get(), 0);
    }

    #[test]
    fn empty_child_is_exhausted_after_one_poll() {
        let (c, polls) = child(0);
        let mut limit = Limit::new(c, 3);
        assert_eq!(limit.next(), Ok(None));
        assert_eq!(limit.next(), Ok(None));
        assert_eq!(polls.get(), 1);
    }

    #[test]
    fn schema_is_the_childs_schema_unchanged() {
        let (c, _) = child(1);
        let expected = c.schema().to_vec();
        let limit = Limit::new(c, 0);
        assert_eq!(limit.schema(), expected.as_slice());
    }
}
