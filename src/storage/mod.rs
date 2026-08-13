//! bead: quern-storage-trait — HOT: the Storage trait. See docs/quern.md §3.
pub mod btree;
pub mod heap;
pub mod pager;
pub mod wal;

use crate::types::{Result, Row, RowId, Schema};

/// The frozen storage contract (docs/quern.md §3), verbatim.
///
/// Every exec/ and plan/ bead codes against this and nothing else. Note what
/// is deliberately absent: there is no `schema()` — the schema comes from the
/// `Catalog`, and exec-dml takes `&Schema` as a parameter (bead .42).
///
/// `scan(&self)` borrows the storage immutably for as long as the iterator
/// lives, so it cannot be held across a `&mut self` mutator (bead .35):
/// collect the `(RowId, Row)` hits, drop the iterator, then mutate.
pub trait Storage {
    fn create_table(&mut self, schema: &Schema) -> Result<()>;
    fn drop_table(&mut self, table: &str) -> Result<()>;
    fn insert(&mut self, table: &str, row: &Row) -> Result<RowId>;
    fn delete(&mut self, table: &str, id: RowId) -> Result<()>;
    fn update(&mut self, table: &str, id: RowId, row: &Row) -> Result<()>;
    // The signature is frozen by §3, so the `type_complexity` lint has nothing
    // to bite on: a type alias here would change the contract's spelling.
    #[allow(clippy::type_complexity)]
    fn scan(&self, table: &str) -> Result<Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_>>;
    fn lookup_pk(&self, table: &str, key: i64) -> Result<Option<(RowId, Row)>>;
    fn begin(&mut self) -> Result<()>;
    fn commit(&mut self) -> Result<()>;
    fn rollback(&mut self) -> Result<()>;
}
