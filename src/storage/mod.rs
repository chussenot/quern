//! bead: quern-storage-trait — HOT: the Storage trait. See docs/quern.md §3.
pub mod btree;
pub mod heap;
pub mod pager;
pub mod wal;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::txn::{TxnHost, TxnState};
use crate::types::{QuernError, Result, Row, RowId, Schema, Type, Value};
use btree::BTree;
use heap::Heap;
use pager::{Pager, PAGE_SIZE};
use wal::{Wal, WalRecord, KIND_DELETE, KIND_INSERT, KIND_UPDATE};

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

/// One table's storage: a heap, and a B+tree iff the table declares an
/// `INTEGER PRIMARY KEY`. `pk` is the column index of that key, cached from the
/// schema so a mutator never has to borrow the catalog and the tables at once.
struct Table {
    heap: Heap,
    index: Option<BTree>,
    pk: Option<usize>,
}

/// The database: one pager, one WAL, the catalog, and per-table storage.
///
/// The pager and the per-table `Heap`/`BTree` are SIBLING fields, not nested,
/// because heap and btree take `&mut Pager` per call — one file is shared by
/// every table.
///
/// PERSISTENT METADATA IS ONE PAGE: the page the pager header calls
/// `catalog_root`. It holds `u32 table_count`, then per table `u32 name_len |
/// lowercased name | u32 heap_first_page | u32 btree_root` (0 = no index),
/// then `Catalog::to_bytes()`. `Catalog::from_bytes` ignores trailing bytes, so
/// the whole 4096-byte page goes straight to it. It is rewritten after every
/// DDL and after every mutation, because a btree root moves when it splits.
pub struct Db {
    db_path: PathBuf,
    pager: Pager,
    wal: Wal,
    catalog: Catalog,
    /// Keyed by ASCII-lowercased table name (§1: names are case-insensitive).
    /// `BTreeMap`, not `HashMap`: the metadata page must encode deterministically.
    tables: BTreeMap<String, Table>,
    /// Transaction state and the commit ordering both live in `txn.rs`; this
    /// type only supplies the accessors [`TxnHost`] asks for.
    txn: TxnState,
    /// DDL done inside the open transaction, in order. §6: DDL takes effect
    /// immediately and ROLLBACK does not undo it — but rollback discards the
    /// pager's dirty map wholesale, DDL pages included, so `rollback` re-applies
    /// this list afterwards. See [`Storage::rollback`].
    txn_ddl: Vec<Ddl>,
}

/// A DDL statement, kept only long enough for `rollback` to re-apply it.
enum Ddl {
    Create(Schema),
    Drop(String),
}

impl Db {
    /// Open (creating if missing) the database directory: `quern.db` plus
    /// `quern.wal` (bead .37). Replays committed WAL records, makes them
    /// durable, then checkpoints — after which the log is empty and a stale
    /// txn id can never be mistaken for a live one.
    pub fn open(dir: &Path) -> Result<Db> {
        std::fs::create_dir_all(dir).map_err(|e| {
            QuernError::Storage(format!("create database directory {}: {e}", dir.display()))
        })?;
        let db_path = dir.join("quern.db");
        let pager = Pager::open(&db_path)?;
        let wal = Wal::open(&dir.join("quern.wal"))?;
        let (catalog, tables) = Self::load_meta(&pager)?;
        let mut db = Db {
            db_path,
            pager,
            wal,
            catalog,
            tables,
            txn: TxnState::new(),
            txn_ddl: Vec::new(),
        };
        // `TxnHost::recover` owns the sequence — replay, apply, flush,
        // checkpoint — and only the per-record work is ours. `replay()` returns
        // the mutations of committed transactions only: a rolled-back or crashed
        // txn wrote no commit record and is discarded by the same filter.
        db.recover(|db, rec| {
            db.reapply(rec)?;
            db.persist_meta()
        })?;
        Ok(db)
    }

    /// The tables the catalog knows about, for callers that need a schema.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    fn reapply(&mut self, rec: &WalRecord) -> Result<()> {
        // ponytail: DDL is not WAL-logged, so a table CREATEd and populated
        // inside one explicit transaction that commits and then crashes before
        // the flush comes back with neither the table nor its rows — atomic,
        // but not durable. Log the catalog page under a new record kind if that
        // ever matters. Everything else re-applies exactly.
        if !self.tables.contains_key(&Self::key(&rec.table)) {
            return Ok(());
        }
        let mut at = 0;
        match rec.kind {
            KIND_INSERT => {
                let row = decode_row(&rec.payload, &mut at)?;
                self.apply_insert(&rec.table, &row)?;
            }
            KIND_UPDATE => {
                let id = take_u64(&rec.payload, &mut at)?;
                let row = decode_row(&rec.payload, &mut at)?;
                self.apply_update(&rec.table, id, &row)?;
            }
            KIND_DELETE => {
                let id = take_u64(&rec.payload, &mut at)?;
                self.apply_delete(&rec.table, id)?;
            }
            kind => {
                return Err(QuernError::Storage(format!(
                    "wal record {} has unknown kind {kind}",
                    rec.lsn
                )))
            }
        }
        Ok(())
    }

    fn key(table: &str) -> String {
        table.to_ascii_lowercase()
    }

    fn table(&self, table: &str) -> Result<&Table> {
        self.tables
            .get(&Self::key(table))
            .ok_or_else(|| QuernError::Catalog(format!("no such table: {table}")))
    }

    fn load_meta(pager: &Pager) -> Result<(Catalog, BTreeMap<String, Table>)> {
        let root = pager.catalog_root();
        if root == 0 {
            return Ok((Catalog::new(), BTreeMap::new()));
        }
        let page = pager.read_page(root)?;
        let mut at = 0;
        let count = take_u32(&page, &mut at)?;
        let mut roots = Vec::new();
        for _ in 0..count {
            let len = take_u32(&page, &mut at)? as usize;
            let name = std::str::from_utf8(take(&page, &mut at, len)?)
                .map_err(|e| QuernError::Storage(format!("table name in metadata page: {e}")))?
                .to_string();
            let first = take_u32(&page, &mut at)?;
            let index_root = take_u32(&page, &mut at)?;
            roots.push((name, first, index_root));
        }
        let catalog = Catalog::from_bytes(&page[at..])?;
        let mut tables = BTreeMap::new();
        for (name, first, index_root) in roots {
            let pk = catalog.get(&name)?.primary_key();
            let index = if index_root == 0 {
                None
            } else {
                Some(BTree::open(index_root)?)
            };
            tables.insert(
                name,
                Table {
                    heap: Heap::open(pager, first)?,
                    index,
                    pk,
                },
            );
        }
        Ok((catalog, tables))
    }

    fn persist_meta(&mut self) -> Result<()> {
        let mut buf = Vec::with_capacity(PAGE_SIZE);
        buf.extend_from_slice(&(self.tables.len() as u32).to_le_bytes());
        for (name, t) in &self.tables {
            buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
            buf.extend_from_slice(name.as_bytes());
            buf.extend_from_slice(&t.heap.first_page().to_le_bytes());
            let index_root = t.index.as_ref().map_or(0, BTree::root);
            buf.extend_from_slice(&index_root.to_le_bytes());
        }
        buf.extend_from_slice(&self.catalog.to_bytes());
        if buf.len() > PAGE_SIZE {
            // ponytail: the catalog plus the table roots live in exactly one
            // page (§4 gives the header a single catalog root). Chain a second
            // metadata page if a schema ever outgrows 4 KiB.
            return Err(QuernError::Storage(format!(
                "database metadata is {} bytes, one page holds {PAGE_SIZE}",
                buf.len()
            )));
        }
        buf.resize(PAGE_SIZE, 0);
        let root = match self.pager.catalog_root() {
            0 => {
                let idx = self.pager.allocate_page()?;
                self.pager.set_catalog_root(idx)?;
                idx
            }
            idx => idx,
        };
        self.pager.write_page(root, &buf)
    }

    /// Discard everything the pager is holding and rebuild from the file. The
    /// whole of `rollback`: uncommitted pages never reached disk, and the WAL
    /// has no commit record for them, which is exactly what a crash leaves.
    fn reload(&mut self) -> Result<()> {
        self.pager = Pager::open(&self.db_path)?;
        let (catalog, tables) = Self::load_meta(&self.pager)?;
        self.catalog = catalog;
        self.tables = tables;
        Ok(())
    }

    /// Heap + index write, with no WAL append: shared by the mutators and by
    /// recovery, so replay cannot drift from the live path.
    ///
    /// ponytail: a `RowId` handed to `update`/`delete` is NOT checked against
    /// the table's page chain — the heap decodes whatever page the id names, so
    /// an id from another table (or a hand-made one) corrupts that page. Every
    /// caller gets its ids from `scan`/`lookup_pk` on the same table. Validate
    /// the page against the chain in `split_row_id`'s caller if that changes.
    fn apply_insert(&mut self, table: &str, row: &Row) -> Result<RowId> {
        let key = Self::table_pk(self.table(table)?, table, row)?;
        if let (Some(k), Some(index)) = (key, self.table(table)?.index.as_ref()) {
            // Pre-checked, so a duplicate never leaves an orphan heap row.
            if index.lookup(&self.pager, k)?.is_some() {
                return Err(duplicate_pk(table, k));
            }
        }
        let t = self
            .tables
            .get_mut(&Self::key(table))
            .ok_or_else(|| QuernError::Catalog(format!("no such table: {table}")))?;
        let id = t.heap.insert(&mut self.pager, row)?;
        if let (Some(k), Some(index)) = (key, t.index.as_mut()) {
            if !index.insert(&mut self.pager, k, id)? {
                return Err(duplicate_pk(table, k));
            }
        }
        Ok(id)
    }

    fn apply_update(&mut self, table: &str, id: RowId, row: &Row) -> Result<()> {
        let t = self.table(table)?;
        let new_key = Self::table_pk(t, table, row)?;
        let old = t
            .heap
            .get(&self.pager, id)?
            .ok_or_else(|| QuernError::Storage(format!("no row {id} in table {table}")))?;
        let old_key = Self::table_pk(t, table, &old)?;
        if let (Some(nk), Some(ok), Some(index)) = (new_key, old_key, t.index.as_ref()) {
            if nk != ok && index.lookup(&self.pager, nk)?.is_some() {
                return Err(duplicate_pk(table, nk));
            }
        }
        let t = self
            .tables
            .get_mut(&Self::key(table))
            .ok_or_else(|| QuernError::Catalog(format!("no such table: {table}")))?;
        // A row that outgrows the space left on its page relocates, and the
        // RowId encodes page and slot — so this is where the index is repointed.
        let new_id = t.heap.update(&mut self.pager, id, row)?;
        if let (Some(nk), Some(ok), Some(index)) = (new_key, old_key, t.index.as_mut()) {
            if nk != ok || new_id != id {
                index.delete(&mut self.pager, ok)?;
                if !index.insert(&mut self.pager, nk, new_id)? {
                    return Err(duplicate_pk(table, nk));
                }
            }
        }
        Ok(())
    }

    fn apply_delete(&mut self, table: &str, id: RowId) -> Result<()> {
        let t = self.table(table)?;
        let old = t
            .heap
            .get(&self.pager, id)?
            .ok_or_else(|| QuernError::Storage(format!("no row {id} in table {table}")))?;
        let old_key = Self::table_pk(t, table, &old)?;
        let t = self
            .tables
            .get_mut(&Self::key(table))
            .ok_or_else(|| QuernError::Catalog(format!("no such table: {table}")))?;
        t.heap.delete(&mut self.pager, id)?;
        if let (Some(k), Some(index)) = (old_key, t.index.as_ref()) {
            index.delete(&mut self.pager, k)?;
        }
        Ok(())
    }

    /// The row's PK value, or `None` when the table has no PK.
    fn table_pk(t: &Table, table: &str, row: &Row) -> Result<Option<i64>> {
        let Some(i) = t.pk else { return Ok(None) };
        match row.get(i) {
            Some(Value::Int(k)) => Ok(Some(*k)),
            Some(Value::Null) => Err(QuernError::Type(format!(
                "PRIMARY KEY of table {table} may not be NULL"
            ))),
            Some(v) => Err(QuernError::Type(format!(
                "PRIMARY KEY of table {table} must be an INTEGER, got {}",
                v.type_name()
            ))),
            None => Err(QuernError::Type(format!(
                "row for table {table} has {} value(s), no column {i}",
                row.len()
            ))),
        }
    }
}

/// The four accessors `txn.rs` needs. Everything transactional — the commit
/// ordering, the implicit transaction, the recovery checkpoint — lives there and
/// is reached through this impl, so there is exactly one copy of each rule.
impl TxnHost for Db {
    fn txn(&mut self) -> &mut TxnState {
        &mut self.txn
    }

    fn wal(&mut self) -> &mut Wal {
        &mut self.wal
    }

    /// Pages durable, and then — because they are — the log's redo is spent, so
    /// truncate it. The checkpoint belongs here rather than at the `commit` call
    /// site: `flush` is exactly the moment its precondition becomes true, and
    /// `txn.rs` calls this from both `commit` and `recover`. Without it a clean
    /// commit leaves records in the log that the next `open` would re-apply on
    /// top of the pages that already hold them, and re-applying an insert
    /// allocates a fresh `RowId`, so replay is not idempotent.
    fn flush(&mut self) -> Result<()> {
        self.pager.flush()?;
        self.wal.checkpoint()
    }

    /// Reopen the pager: drops the dirty-page map and the `page_count` that
    /// `allocate_page` bumped, and rebuilds the catalog and per-table roots from
    /// the file — the heap and B-tree state above the pager, which would
    /// otherwise still point into discarded pages.
    fn discard(&mut self) -> Result<()> {
        self.reload()
    }
}

impl Storage for Db {
    fn create_table(&mut self, schema: &Schema) -> Result<()> {
        if let Some(i) = schema.primary_key() {
            if schema.columns[i].ty != Type::Int {
                return Err(QuernError::Type(format!(
                    "PRIMARY KEY column {} of table {} must be INTEGER (§4)",
                    schema.columns[i].name, schema.table
                )));
            }
        }
        // Whether the DDL has to be replayed on ROLLBACK depends on there being
        // a user transaction around it — which `statement` is about to open one
        // if there isn't, so ask before entering it.
        let in_txn = self.txn.is_open();
        self.statement(|db, _txn| {
            db.catalog.create(schema.clone())?;
            let pk = schema.primary_key();
            let heap = Heap::create(&mut db.pager)?;
            let index = match pk {
                Some(_) => Some(BTree::create(&mut db.pager)?),
                None => None,
            };
            db.tables
                .insert(Self::key(&schema.table), Table { heap, index, pk });
            db.persist_meta()?;
            if in_txn {
                db.txn_ddl.push(Ddl::Create(schema.clone()));
            }
            Ok(())
        })
    }

    fn drop_table(&mut self, table: &str) -> Result<()> {
        // ponytail: the heap and index pages of a dropped table are not
        // reclaimed — the pager has no free list (§4). Needs one, plus a page
        // walk, if a workload ever churns tables.
        let in_txn = self.txn.is_open();
        self.statement(|db, _txn| {
            db.catalog.drop(table)?;
            db.tables.remove(&Self::key(table));
            db.persist_meta()?;
            if in_txn {
                db.txn_ddl.push(Ddl::Drop(table.to_string()));
            }
            Ok(())
        })
    }

    fn insert(&mut self, table: &str, row: &Row) -> Result<RowId> {
        self.statement(|db, txn| {
            // Apply first, log second: a rejected mutation must not leave a
            // record that recovery would try — and fail — to re-apply. Within a
            // txn the order is free; only `commit` fsyncs the log before the
            // pages, and that order lives in `txn.rs`.
            let id = db.apply_insert(table, row)?;
            db.wal.append(txn, KIND_INSERT, table, &encode_row(row))?;
            db.persist_meta()?;
            Ok(id)
        })
    }

    fn delete(&mut self, table: &str, id: RowId) -> Result<()> {
        self.statement(|db, txn| {
            db.apply_delete(table, id)?;
            db.wal.append(txn, KIND_DELETE, table, &id.to_le_bytes())?;
            db.persist_meta()
        })
    }

    fn update(&mut self, table: &str, id: RowId, row: &Row) -> Result<()> {
        self.statement(|db, txn| {
            db.apply_update(table, id, row)?;
            let mut payload = id.to_le_bytes().to_vec();
            payload.extend_from_slice(&encode_row(row));
            db.wal.append(txn, KIND_UPDATE, table, &payload)?;
            db.persist_meta()
        })
    }

    fn scan(&self, table: &str) -> Result<Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_>> {
        // The iterator borrows the pager, not the heap, so this is a plain
        // `&self` call — and the borrow of `self` it carries is why no mutator
        // can run while it is alive (bead .35).
        Ok(Box::new(self.table(table)?.heap.scan(&self.pager)))
    }

    fn lookup_pk(&self, table: &str, key: i64) -> Result<Option<(RowId, Row)>> {
        let t = self.table(table)?;
        let index = t
            .index
            .as_ref()
            .ok_or_else(|| QuernError::Type(format!("table {table} has no INTEGER PRIMARY KEY")))?;
        let Some(id) = index.lookup(&self.pager, key)? else {
            return Ok(None);
        };
        match t.heap.get(&self.pager, id)? {
            Some(row) => Ok(Some((id, row))),
            None => Err(QuernError::Storage(format!(
                "index of table {table} points at missing row {id} for key {key}"
            ))),
        }
    }

    fn begin(&mut self) -> Result<()> {
        TxnHost::begin(self).map(|_| ())
    }

    fn commit(&mut self) -> Result<()> {
        self.txn_ddl.clear();
        TxnHost::commit(self)
    }

    fn rollback(&mut self) -> Result<()> {
        let ddl = std::mem::take(&mut self.txn_ddl);
        // Ends the transaction and discards the in-memory work; the WAL is
        // deliberately untouched, because the absence of a commit record IS the
        // rollback. Both rules live in `txn.rs`.
        TxnHost::rollback(self)?;
        // §6: DDL takes effect immediately and ROLLBACK does not undo it. The
        // discard above threw it away with the row work, because a DDL inside an
        // open transaction has only ever reached the pager's dirty map — so
        // re-do it here instead. Flushing it at DDL time is what does NOT work:
        // that would flush the transaction's row pages with it, and those must
        // die here. Now that the txn is closed each of these runs in its own
        // implicit transaction, and the only dirty pages it can flush are the
        // ones it just made.
        for op in ddl {
            match op {
                Ddl::Create(schema) => self.create_table(&schema)?,
                Ddl::Drop(table) => self.drop_table(&table)?,
            }
        }
        // The aborted records are dead weight in the log, and a later process
        // starting its txn ids at 1 could collide with them.
        self.wal.checkpoint()
    }
}

fn duplicate_pk(table: &str, key: i64) -> QuernError {
    // QuernError has no Constraint variant and types.rs is frozen (§3); Type is
    // where a value that violates the schema belongs.
    QuernError::Type(format!(
        "duplicate PRIMARY KEY value {key} in table {table}"
    ))
}

/// §4's row encoding, used for WAL payloads: `u32 count`, then per value a tag
/// byte (`0`=Null `1`=Int `2`=Text `3`=Bool) and its bytes.
fn encode_row(row: &Row) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(row.len() as u32).to_le_bytes());
    for v in row {
        match v {
            Value::Null => buf.push(0),
            Value::Int(i) => {
                buf.push(1);
                buf.extend_from_slice(&i.to_le_bytes());
            }
            Value::Text(s) => {
                buf.push(2);
                buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            Value::Bool(b) => {
                buf.push(3);
                buf.push(u8::from(*b));
            }
        }
    }
    buf
}

// Everything below parses bytes that came off a disk, so every length is
// checked before it is used to slice (§1: a panic anywhere is a bug).

fn decode_row(bytes: &[u8], at: &mut usize) -> Result<Row> {
    let count = take_u32(bytes, at)? as usize;
    let mut row = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let tag = take(bytes, at, 1)?[0];
        row.push(match tag {
            0 => Value::Null,
            1 => Value::Int(take_u64(bytes, at)? as i64),
            2 => {
                let len = take_u32(bytes, at)? as usize;
                Value::Text(
                    std::str::from_utf8(take(bytes, at, len)?)
                        .map_err(|e| QuernError::Storage(format!("text value: {e}")))?
                        .to_string(),
                )
            }
            3 => Value::Bool(take(bytes, at, 1)?[0] != 0),
            t => return Err(QuernError::Storage(format!("value has unknown tag {t}"))),
        });
    }
    Ok(row)
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = at.checked_add(n).ok_or_else(|| {
        QuernError::Storage(format!("record offset {at} + {n} overflows", at = *at))
    })?;
    let slice = bytes
        .get(*at..end)
        .ok_or_else(|| QuernError::Storage(format!("record wants {n} byte(s) at {}", *at)))?;
    *at = end;
    Ok(slice)
}

fn take_u32(bytes: &[u8], at: &mut usize) -> Result<u32> {
    let mut arr = [0u8; 4];
    arr.copy_from_slice(take(bytes, at, 4)?);
    Ok(u32::from_le_bytes(arr))
}

fn take_u64(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(take(bytes, at, 8)?);
    Ok(u64::from_le_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Column;
    use tempfile::TempDir;

    fn col(name: &str, ty: Type, primary_key: bool) -> Column {
        Column {
            name: name.into(),
            ty,
            primary_key,
        }
    }

    /// `t(id INTEGER PRIMARY KEY, name TEXT)`.
    fn schema() -> Schema {
        Schema {
            table: "T".into(),
            columns: vec![col("id", Type::Int, true), col("name", Type::Text, false)],
        }
    }

    fn row(id: i64, name: &str) -> Row {
        vec![Value::Int(id), Value::Text(name.into())]
    }

    fn fresh() -> (TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(dir.path()).unwrap();
        db.create_table(&schema()).unwrap();
        (dir, db)
    }

    fn rows(db: &Db, table: &str) -> Vec<(RowId, Row)> {
        db.scan(table).unwrap().collect::<Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn create_insert_scan_lookup_round_trip() {
        let (_dir, mut db) = fresh();
        let a = db.insert("t", &row(1, "ada")).unwrap();
        let b = db.insert("T", &row(2, "bob")).unwrap();
        assert_ne!(a, b);

        let scanned = rows(&db, "t");
        assert_eq!(scanned, vec![(a, row(1, "ada")), (b, row(2, "bob"))]);
        assert_eq!(db.lookup_pk("t", 1).unwrap(), Some((a, row(1, "ada"))));
        assert_eq!(db.lookup_pk("t", 2).unwrap(), Some((b, row(2, "bob"))));
        assert_eq!(db.lookup_pk("t", 3).unwrap(), None);
        // Unknown table is a Catalog error, not a panic.
        assert!(matches!(
            db.scan("nope").err(),
            Some(QuernError::Catalog(_))
        ));
    }

    #[test]
    fn update_that_relocates_the_row_is_still_found_by_pk() {
        let (_dir, mut db) = fresh();
        let id = db.insert("t", &row(1, &"x".repeat(3000))).unwrap();
        let grown = row(1, &"y".repeat(4000));
        db.update("t", id, &grown).unwrap();

        let (new_id, found) = db.lookup_pk("t", 1).unwrap().expect("still indexed");
        // The point of the test: the row really did move, so the index entry
        // had to be repointed rather than left alone.
        assert_ne!(new_id, id, "row should have relocated to a new page");
        assert_eq!(found, grown);
        assert_eq!(rows(&db, "t"), vec![(new_id, grown)]);
    }

    #[test]
    fn update_that_changes_the_pk_moves_the_index_entry() {
        let (_dir, mut db) = fresh();
        let id = db.insert("t", &row(1, "ada")).unwrap();
        db.update("t", id, &row(7, "ada")).unwrap();
        assert_eq!(db.lookup_pk("t", 1).unwrap(), None);
        assert_eq!(db.lookup_pk("t", 7).unwrap().unwrap().1, row(7, "ada"));
    }

    #[test]
    fn delete_removes_from_both_scan_and_index() {
        let (_dir, mut db) = fresh();
        let a = db.insert("t", &row(1, "ada")).unwrap();
        let b = db.insert("t", &row(2, "bob")).unwrap();
        db.delete("t", a).unwrap();

        assert_eq!(rows(&db, "t"), vec![(b, row(2, "bob"))]);
        assert_eq!(db.lookup_pk("t", 1).unwrap(), None);
        assert_eq!(db.lookup_pk("t", 2).unwrap(), Some((b, row(2, "bob"))));
        // A second delete of the same id is an error, not a silent no-op.
        assert!(matches!(
            db.delete("t", a).err(),
            Some(QuernError::Storage(_))
        ));
    }

    #[test]
    fn duplicate_pk_is_surfaced_and_writes_nothing() {
        let (_dir, mut db) = fresh();
        let id = db.insert("t", &row(1, "ada")).unwrap();
        let err = db.insert("t", &row(1, "eve")).unwrap_err();
        match err {
            QuernError::Type(m) => assert!(m.contains("duplicate PRIMARY KEY"), "{m}"),
            other => panic!("expected a Type error, got {other:?}"),
        }
        // No orphan heap row: the index is checked before the heap is touched.
        assert_eq!(rows(&db, "t"), vec![(id, row(1, "ada"))]);
        assert_eq!(db.lookup_pk("t", 1).unwrap().unwrap().1, row(1, "ada"));

        // NULL and non-INTEGER keys are rejected too.
        assert!(matches!(
            db.insert("t", &vec![Value::Null, Value::Text("x".into())])
                .err(),
            Some(QuernError::Type(_))
        ));
    }

    /// §6: "CREATE TABLE and DROP TABLE take effect immediately and are not
    /// undone by ROLLBACK" — including inside an open explicit transaction,
    /// while the row work in that same transaction still dies.
    #[test]
    fn rollback_keeps_ddl_but_still_discards_the_rows() {
        let (dir, mut db) = fresh();
        let ada = db.insert("t", &row(1, "ada")).unwrap();
        Storage::begin(&mut db).unwrap();
        db.insert("t", &row(2, "bob")).unwrap();
        db.create_table(&Schema {
            table: "dtxn".into(),
            columns: vec![col("a", Type::Int, true)],
        })
        .unwrap();
        Storage::rollback(&mut db).unwrap();

        // The table survived, and its index is USABLE — the failure this guards
        // against is a table that comes back with a btree root pointing at a
        // page the rollback discarded, which reads as a zeroed page and errors.
        let id = db.insert("dtxn", &vec![Value::Int(9)]).unwrap();
        assert_eq!(
            db.lookup_pk("dtxn", 9).unwrap(),
            Some((id, vec![Value::Int(9)]))
        );
        // ... and the row work of the rolled-back transaction did not.
        assert_eq!(rows(&db, "t"), vec![(ada, row(1, "ada"))]);

        // A DROP inside a transaction is not undone either.
        Storage::begin(&mut db).unwrap();
        db.drop_table("dtxn").unwrap();
        Storage::rollback(&mut db).unwrap();
        assert!(matches!(
            db.scan("dtxn").err(),
            Some(QuernError::Catalog(_))
        ));
        drop(db);

        // All of it durable, not just in memory.
        let db = Db::open(dir.path()).unwrap();
        assert!(matches!(
            db.scan("dtxn").err(),
            Some(QuernError::Catalog(_))
        ));
        assert_eq!(rows(&db, "t").len(), 1);
    }

    #[test]
    fn rollback_discards_the_transaction() {
        let (dir, mut db) = fresh();
        let id = db.insert("t", &row(1, "ada")).unwrap();
        Storage::begin(&mut db).unwrap();
        db.insert("t", &row(2, "bob")).unwrap();
        assert_eq!(rows(&db, "t").len(), 2);
        Storage::rollback(&mut db).unwrap();

        assert_eq!(rows(&db, "t"), vec![(id, row(1, "ada"))]);
        assert_eq!(db.lookup_pk("t", 2).unwrap(), None);
        assert!(matches!(
            Storage::rollback(&mut db).err(),
            Some(QuernError::Txn(_))
        ));
        assert!(matches!(
            Storage::commit(&mut db).err(),
            Some(QuernError::Txn(_))
        ));
        Storage::begin(&mut db).unwrap();
        assert!(matches!(
            Storage::begin(&mut db).err(),
            Some(QuernError::Txn(_))
        ));
        drop(db);

        // And it is gone from the file too, not just from memory.
        let db = Db::open(dir.path()).unwrap();
        assert_eq!(rows(&db, "t").len(), 1);
    }

    #[test]
    fn commit_persists_across_a_reopen() {
        let (dir, mut db) = fresh();
        Storage::begin(&mut db).unwrap();
        db.insert("t", &row(1, "ada")).unwrap();
        db.insert("t", &row(2, "bob")).unwrap();
        Storage::commit(&mut db).unwrap();
        drop(db);

        let db = Db::open(dir.path()).unwrap();
        assert_eq!(
            rows(&db, "t")
                .into_iter()
                .map(|(_, r)| r)
                .collect::<Vec<_>>(),
            vec![row(1, "ada"), row(2, "bob")]
        );
        assert!(db.lookup_pk("t", 2).unwrap().is_some());
        assert_eq!(db.catalog().get("t").unwrap().table, "T");
    }

    #[test]
    fn drop_table_removes_it_for_good() {
        let (dir, mut db) = fresh();
        db.insert("t", &row(1, "ada")).unwrap();
        db.drop_table("T").unwrap();
        assert!(matches!(db.scan("t").err(), Some(QuernError::Catalog(_))));
        assert!(matches!(
            db.drop_table("t").err(),
            Some(QuernError::Catalog(_))
        ));
        drop(db);

        let mut db = Db::open(dir.path()).unwrap();
        assert!(matches!(db.scan("t").err(), Some(QuernError::Catalog(_))));
        // The name is free again.
        db.create_table(&schema()).unwrap();
        assert_eq!(rows(&db, "t"), vec![]);
    }

    #[test]
    fn reopen_replays_a_committed_txn_whose_pages_never_reached_disk() {
        let (dir, mut db) = fresh();
        let id = db.insert("t", &row(1, "ada")).unwrap();
        Storage::begin(&mut db).unwrap();
        db.insert("t", &row(2, "bob")).unwrap();
        db.update("t", id, &row(1, "ADA")).unwrap();
        let txn = db.txn.end("COMMIT").unwrap();
        // The crash window: the WAL is fsynced by commit(), and then the
        // process dies before pager.flush() — so drop the Db without flushing.
        db.wal.commit(txn).unwrap();
        drop(db);

        let db = Db::open(dir.path()).unwrap();
        let names: Vec<Row> = rows(&db, "t").into_iter().map(|(_, r)| r).collect();
        assert_eq!(names, vec![row(1, "ADA"), row(2, "bob")]);
        assert_eq!(db.lookup_pk("t", 2).unwrap().unwrap().1, row(2, "bob"));
        assert_eq!(db.lookup_pk("t", 1).unwrap().unwrap().1, row(1, "ADA"));
        // Recovery checkpointed, so the next open has nothing left to replay.
        assert!(db.wal.replay().unwrap().is_empty());
        drop(db);
        let db = Db::open(dir.path()).unwrap();
        assert_eq!(rows(&db, "t").len(), 2);
    }

    #[test]
    fn an_uncommitted_txn_is_never_replayed() {
        let (dir, mut db) = fresh();
        Storage::begin(&mut db).unwrap();
        db.insert("t", &row(1, "ada")).unwrap();
        // No commit record: identical bytes to a kill -9, discarded the same way.
        drop(db);

        let db = Db::open(dir.path()).unwrap();
        assert_eq!(rows(&db, "t"), vec![]);
    }

    #[test]
    fn a_table_without_a_pk_scans_but_has_no_index() {
        let (_dir, mut db) = fresh();
        db.create_table(&Schema {
            table: "logs".into(),
            columns: vec![col("msg", Type::Text, false)],
        })
        .unwrap();
        let id = db.insert("logs", &vec![Value::Text("hi".into())]).unwrap();
        assert_eq!(
            rows(&db, "logs"),
            vec![(id, vec![Value::Text("hi".into())])]
        );
        assert!(matches!(
            db.lookup_pk("logs", 1).err(),
            Some(QuernError::Type(_))
        ));
        // A TEXT primary key is rejected at CREATE, not at INSERT.
        assert!(matches!(
            db.create_table(&Schema {
                table: "bad".into(),
                columns: vec![col("k", Type::Text, true)],
            })
            .err(),
            Some(QuernError::Type(_))
        ));
    }

    #[test]
    fn row_encoding_round_trips_every_value_shape() {
        let r = vec![
            Value::Null,
            Value::Int(-9_000_000_000),
            Value::Text("héllo".into()),
            Value::Bool(true),
            Value::Bool(false),
        ];
        let bytes = encode_row(&r);
        let mut at = 0;
        assert_eq!(decode_row(&bytes, &mut at).unwrap(), r);
        assert_eq!(at, bytes.len());
        // Truncation is an error, never a panic.
        for n in 0..bytes.len() {
            let mut at = 0;
            assert!(decode_row(&bytes[..n], &mut at).is_err(), "len {n}");
        }
    }
}
