//! bead: quern-exec-scan — the leaf operator: every live row of one table.

use super::Operator;
use crate::storage::Storage;
use crate::types::{Column, Result, Row, Schema};

/// Yields every live row of a table, in heap order.
///
/// # Why the rows are materialised in the constructor
///
/// `Storage::scan(&self)` returns `Box<dyn Iterator<Item = Result<(RowId,
/// Row)>> + '_>`, which borrows the storage for as long as it lives, while
/// `Box<dyn Operator>` is `'static` (frozen §3, and proven with rustc in bead
/// .35). An operator that *held* that iterator therefore cannot exist. So
/// [`Scan::new`] drains the iterator into a `Vec<Row>`, the borrow of storage
/// ends when the constructor returns, and [`Operator::next`] pops from the Vec.
///
/// Two consequences worth naming:
///
/// * An `Err` from any row is returned by `new`, not by `next` — a corrupt row
///   fails when the plan is built rather than half-way through the results.
/// * The `RowId` is dropped. The frozen `next() -> Result<Option<Row>>` cannot
///   carry one, and `exec::dml` collects its own `RowId`s straight from
///   `Storage::scan` (bead .17) rather than through an operator.
///
/// Order is whatever `Storage::scan` yields, preserved exactly — heap order,
/// which is insertion order with relocated rows moved to the end. Nothing here
/// sorts or reorders: the corpus depends on it being deterministic.
///
/// ponytail: full materialisation, so a scan costs the whole table in memory
/// and `SELECT ... LIMIT 1` still reads every row. Upgrade path is streaming —
/// which needs a lifetime on `Operator` or a storage handle that hands out an
/// owning cursor, i.e. a change to a frozen signature, not to this file.
pub struct Scan {
    schema: Vec<Column>,
    rows: std::vec::IntoIter<Row>,
}

impl Scan {
    /// Materialise `table` out of `storage`. The `Schema` comes from the
    /// caller's catalog (`LogicalPlan::Scan` carries it) because `Storage`
    /// deliberately has no `schema()` — bead .42.
    pub fn new(storage: &dyn Storage, table: &str, schema: &Schema) -> Result<Scan> {
        let rows = storage
            .scan(table)?
            .map(|r| r.map(|(_id, row)| row))
            .collect::<Result<Vec<Row>>>()?;
        Ok(Scan {
            schema: schema.columns.clone(),
            rows: rows.into_iter(),
        })
    }
}

impl Operator for Scan {
    fn schema(&self) -> &[Column] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>> {
        Ok(self.rows.next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{QuernError, RowId, Type, Value};

    /// A `Storage` that is nothing but a table of `scan` results. `Db` is a
    /// separate bead (.14) and this only needs the one method; the mutators
    /// exist to satisfy the trait and are never called.
    struct MockStorage {
        rows: Vec<Result<(RowId, Row)>>,
    }

    impl MockStorage {
        /// One INT column `a`, one row per value, `RowId`s 1..=n.
        fn ints(vs: &[i64]) -> MockStorage {
            MockStorage {
                rows: vs
                    .iter()
                    .enumerate()
                    .map(|(i, v)| Ok((i as RowId + 1, vec![Value::Int(*v)])))
                    .collect(),
            }
        }
    }

    impl Storage for MockStorage {
        fn scan(&self, table: &str) -> Result<Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_>> {
            if table != "t" {
                return Err(QuernError::Catalog(format!("no such table: {table}")));
            }
            Ok(Box::new(self.rows.iter().cloned()))
        }

        fn create_table(&mut self, _schema: &Schema) -> Result<()> {
            unreachable!("Scan never mutates")
        }
        fn drop_table(&mut self, _table: &str) -> Result<()> {
            unreachable!("Scan never mutates")
        }
        fn insert(&mut self, _table: &str, _row: &Row) -> Result<RowId> {
            unreachable!("Scan never mutates")
        }
        fn delete(&mut self, _table: &str, _id: RowId) -> Result<()> {
            unreachable!("Scan never mutates")
        }
        fn update(&mut self, _table: &str, _id: RowId, _row: &Row) -> Result<()> {
            unreachable!("Scan never mutates")
        }
        fn lookup_pk(&self, _table: &str, _key: i64) -> Result<Option<(RowId, Row)>> {
            unreachable!("Scan does not use the index")
        }
        fn begin(&mut self) -> Result<()> {
            unreachable!("Scan is not transactional")
        }
        fn commit(&mut self) -> Result<()> {
            unreachable!("Scan is not transactional")
        }
        fn rollback(&mut self) -> Result<()> {
            unreachable!("Scan is not transactional")
        }
    }

    fn schema() -> Schema {
        Schema {
            table: "t".to_string(),
            columns: vec![
                Column {
                    name: "a".to_string(),
                    ty: Type::Int,
                    primary_key: true,
                },
                Column {
                    name: "b".to_string(),
                    ty: Type::Text,
                    primary_key: false,
                },
            ],
        }
    }

    fn scan(storage: &MockStorage) -> Result<Scan> {
        Scan::new(storage, "t", &schema())
    }

    #[test]
    fn every_row_is_yielded_in_heap_order() {
        // Deliberately not sorted: whatever Storage::scan yields, verbatim.
        let storage = MockStorage::ints(&[3, 1, 2]);
        let mut s = scan(&storage).unwrap();
        let mut out = Vec::new();
        while let Some(row) = s.next().unwrap() {
            out.push(row);
        }
        assert_eq!(
            out,
            vec![
                vec![Value::Int(3)],
                vec![Value::Int(1)],
                vec![Value::Int(2)]
            ]
        );
    }

    #[test]
    fn an_empty_table_yields_nothing_and_a_drained_scan_stays_drained() {
        let mut s = scan(&MockStorage::ints(&[])).unwrap();
        assert_eq!(s.next(), Ok(None));
        assert_eq!(s.next(), Ok(None));

        let storage = MockStorage::ints(&[7]);
        let mut s = scan(&storage).unwrap();
        assert_eq!(s.next(), Ok(Some(vec![Value::Int(7)])));
        assert_eq!(s.next(), Ok(None));
        assert_eq!(s.next(), Ok(None));
    }

    #[test]
    fn schema_is_the_one_passed_to_the_constructor() {
        let s = scan(&MockStorage::ints(&[1])).unwrap();
        assert_eq!(s.schema(), schema().columns.as_slice());
    }

    #[test]
    fn a_bad_row_fails_the_constructor_not_next() {
        let mut storage = MockStorage::ints(&[1, 2]);
        storage
            .rows
            .insert(1, Err(QuernError::Storage("torn row".to_string())));
        assert!(matches!(
            scan(&storage),
            Err(QuernError::Storage(m)) if m == "torn row"
        ));
        // And a storage-level failure (unknown table) propagates the same way.
        assert!(matches!(
            Scan::new(&storage, "nope", &schema()),
            Err(QuernError::Catalog(_))
        ));
    }
}
