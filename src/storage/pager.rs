//! bead: quern-pager — 4096-byte pages, file-backed. See docs/quern.md §4.
//!
//! Page 0 is the header (magic `QUERN\0\0\0`, u32 page count, u32 catalog
//! root) and is owned by the pager: it is rebuilt from live state on every
//! [`Pager::flush`], so callers may read it but not write it.
//!
//! Everything here parses bytes that came off a disk, so every field is
//! validated before it is used to size or index anything. A file that is not
//! a quern database, is truncated, or is asked for a page it does not have is
//! a [`QuernError::Storage`], never a panic (§1: a panic anywhere is a bug).
//!
//! No `unsafe`: `#![forbid(unsafe_code)]` stays as-is. §4 permits one block
//! here with a measurement behind it; there is no measurement, and seek plus
//! `read_exact` into a stack page is not the bottleneck at this scale.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::types::{QuernError, Result};

/// Every page is exactly this many bytes, header included.
pub const PAGE_SIZE: usize = 4096;

/// One page, by value. 4 KiB memcpy per read.
///
/// ponytail: pages are copied out rather than borrowed from a cache, so a
/// read is a 4 KiB memcpy. That buys `read_page(&self)`, which the frozen
/// `Storage::scan(&self)` signature needs. Hand out `&Page` from a pinned
/// buffer pool if profiling ever puts the copy on top.
pub type Page = [u8; PAGE_SIZE];

const MAGIC: &[u8; 8] = b"QUERN\0\0\0";
const OFF_PAGE_COUNT: usize = 8;
const OFF_CATALOG_ROOT: usize = 12;

/// A page index. Page 0 is always the header, so `1..page_count` is data.
pub type PageIdx = u32;

pub struct Pager {
    /// `RefCell` because reads go through `&self`: the frozen `Storage` trait
    /// has `scan(&self)`/`lookup_pk(&self)`, and seeking mutates file state.
    /// The borrow never escapes a method, so it can never conflict.
    file: RefCell<std::fs::File>,
    page_count: PageIdx,
    catalog_root: PageIdx,
    /// ponytail: no eviction policy, by design (§4). The ceiling is that
    /// resident memory grows with the write set between flushes — a
    /// transaction touching N pages holds N * 4 KiB until `flush()`. Add an
    /// LRU with a page limit if a single transaction ever outgrows RAM.
    dirty: HashMap<PageIdx, Page>,
}

impl Pager {
    /// Open (or create) a database file. Creating one writes a valid header
    /// immediately, so a fresh file is never left in the "truncated" state.
    pub fn open(path: &Path) -> Result<Pager> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| io_err(&format!("open {}", path.display()), e))?;
        let len = file
            .metadata()
            .map_err(|e| io_err("stat database", e))?
            .len();

        if len == 0 {
            let mut pager = Pager {
                file: RefCell::new(file),
                page_count: 1,
                catalog_root: 0,
                dirty: HashMap::new(),
            };
            pager.flush()?;
            return Ok(pager);
        }

        if len % PAGE_SIZE as u64 != 0 {
            return Err(QuernError::Storage(format!(
                "truncated database: {len} bytes is not a whole number of {PAGE_SIZE}-byte pages"
            )));
        }

        let mut header = [0u8; PAGE_SIZE];
        file.seek(SeekFrom::Start(0))
            .map_err(|e| io_err("seek to header", e))?;
        file.read_exact(&mut header)
            .map_err(|e| io_err("read header", e))?;

        if &header[..MAGIC.len()] != MAGIC {
            return Err(QuernError::Storage(
                "not a quern database: bad magic in page 0".into(),
            ));
        }
        let page_count = le_u32(&header, OFF_PAGE_COUNT);
        if page_count == 0 {
            return Err(QuernError::Storage(
                "corrupt header: page count 0, but page 0 is the header".into(),
            ));
        }
        if u64::from(page_count) * PAGE_SIZE as u64 > len {
            return Err(QuernError::Storage(format!(
                "truncated database: header claims {page_count} pages, file holds {}",
                len / PAGE_SIZE as u64
            )));
        }

        Ok(Pager {
            file: RefCell::new(file),
            page_count,
            catalog_root: le_u32(&header, OFF_CATALOG_ROOT),
            dirty: HashMap::new(),
        })
    }

    /// Total pages, header included. Valid indices are `0..page_count()`.
    pub fn page_count(&self) -> PageIdx {
        self.page_count
    }

    /// Root page of the catalog, or 0 if the catalog has none yet.
    pub fn catalog_root(&self) -> PageIdx {
        self.catalog_root
    }

    /// Record the catalog root. Reaches disk on the next [`Pager::flush`].
    pub fn set_catalog_root(&mut self, idx: PageIdx) -> Result<()> {
        self.check_range(idx)?;
        self.catalog_root = idx;
        Ok(())
    }

    /// A page that was allocated but never written reads back zeroed.
    pub fn read_page(&self, idx: PageIdx) -> Result<Page> {
        self.check_range(idx)?;
        if let Some(page) = self.dirty.get(&idx) {
            return Ok(*page);
        }
        let mut buf = [0u8; PAGE_SIZE];
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(u64::from(idx) * PAGE_SIZE as u64))
            .map_err(|e| io_err(&format!("seek to page {idx}"), e))?;
        file.read_exact(&mut buf)
            .map_err(|e| io_err(&format!("read page {idx}"), e))?;
        Ok(buf)
    }

    /// Buffer a whole page. `data` must be exactly [`PAGE_SIZE`] bytes, and
    /// page 0 is off limits — it is the header the pager maintains itself.
    pub fn write_page(&mut self, idx: PageIdx, data: &[u8]) -> Result<()> {
        if idx == 0 {
            return Err(QuernError::Storage(
                "page 0 is the header and is written by the pager".into(),
            ));
        }
        self.check_range(idx)?;
        if data.len() != PAGE_SIZE {
            return Err(QuernError::Storage(format!(
                "page write is {} bytes, expected {PAGE_SIZE}",
                data.len()
            )));
        }
        let mut page = [0u8; PAGE_SIZE];
        page.copy_from_slice(data);
        self.dirty.insert(idx, page);
        Ok(())
    }

    /// Grow the file by one zeroed page and return its index.
    pub fn allocate_page(&mut self) -> Result<PageIdx> {
        let idx = self.page_count;
        self.page_count = idx.checked_add(1).ok_or_else(|| {
            QuernError::Storage(format!("cannot allocate past page {}", PageIdx::MAX))
        })?;
        // Zeroed and dirty: reads see zeros before the flush, and the flush
        // extends the file so reads see zeros after it too.
        self.dirty.insert(idx, [0u8; PAGE_SIZE]);
        Ok(idx)
    }

    /// Write the header and every dirty page, then `fsync`.
    pub fn flush(&mut self) -> Result<()> {
        let mut header = [0u8; PAGE_SIZE];
        header[..MAGIC.len()].copy_from_slice(MAGIC);
        header[OFF_PAGE_COUNT..OFF_PAGE_COUNT + 4].copy_from_slice(&self.page_count.to_le_bytes());
        header[OFF_CATALOG_ROOT..OFF_CATALOG_ROOT + 4]
            .copy_from_slice(&self.catalog_root.to_le_bytes());

        {
            let mut file = self.file.borrow_mut();
            write_page_at(&mut file, 0, &header)?;
            // Sorted: front-to-back writes, and `HashMap` order is not
            // deterministic (§5 bans it from any output path).
            let mut indices: Vec<PageIdx> = self.dirty.keys().copied().collect();
            indices.sort_unstable();
            for idx in indices {
                write_page_at(&mut file, idx, &self.dirty[&idx])?;
            }
            file.sync_all().map_err(|e| io_err("fsync database", e))?;
        }
        self.dirty.clear();
        Ok(())
    }

    fn check_range(&self, idx: PageIdx) -> Result<()> {
        if idx >= self.page_count {
            return Err(QuernError::Storage(format!(
                "page index {idx} out of range: database has {} page(s)",
                self.page_count
            )));
        }
        Ok(())
    }
}

fn write_page_at(file: &mut std::fs::File, idx: PageIdx, page: &Page) -> Result<()> {
    // Seeking past EOF and writing zero-fills the gap, which is what makes an
    // allocated-but-unwritten page read back as zeros after a flush.
    file.seek(SeekFrom::Start(u64::from(idx) * PAGE_SIZE as u64))
        .map_err(|e| io_err(&format!("seek to page {idx}"), e))?;
    file.write_all(page)
        .map_err(|e| io_err(&format!("write page {idx}"), e))
}

fn io_err(what: &str, e: std::io::Error) -> QuernError {
    QuernError::Storage(format!("{what}: {e}"))
}

/// Little-endian u32 at a constant offset. No `unwrap`, no panic path.
fn le_u32(page: &Page, at: usize) -> u32 {
    u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn filled(byte: u8) -> Page {
        [byte; PAGE_SIZE]
    }

    #[test]
    fn create_write_read_round_trip() {
        let dir = tempdir().unwrap();
        let mut pager = Pager::open(&dir.path().join("db")).unwrap();
        assert_eq!(pager.page_count(), 1, "a fresh database is header-only");

        let idx = pager.allocate_page().unwrap();
        assert_eq!(idx, 1);
        pager.write_page(idx, &filled(0xAB)).unwrap();
        assert_eq!(pager.read_page(idx).unwrap(), filled(0xAB));

        // Still correct once it has left the dirty map.
        pager.flush().unwrap();
        assert_eq!(pager.read_page(idx).unwrap(), filled(0xAB));
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let mut pager = Pager::open(&path).unwrap();
            let a = pager.allocate_page().unwrap();
            let b = pager.allocate_page().unwrap();
            pager.write_page(a, &filled(1)).unwrap();
            pager.write_page(b, &filled(2)).unwrap();
            pager.set_catalog_root(b).unwrap();
            pager.flush().unwrap();
        }
        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.page_count(), 3);
        assert_eq!(pager.catalog_root(), 2);
        assert_eq!(pager.read_page(1).unwrap(), filled(1));
        assert_eq!(pager.read_page(2).unwrap(), filled(2));
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            3 * PAGE_SIZE as u64,
            "flush extends the file to cover every allocated page"
        );
    }

    #[test]
    fn allocated_but_unwritten_page_reads_zeroed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let mut pager = Pager::open(&path).unwrap();
            let idx = pager.allocate_page().unwrap();
            assert_eq!(pager.read_page(idx).unwrap(), filled(0), "before flush");
            pager.flush().unwrap();
            assert_eq!(pager.read_page(idx).unwrap(), filled(0), "after flush");
        }
        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.read_page(1).unwrap(), filled(0), "after reopen");
    }

    #[test]
    fn bad_magic_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db");
        fs::write(&path, vec![b'x'; PAGE_SIZE]).unwrap();
        match Pager::open(&path) {
            Err(QuernError::Storage(m)) => assert!(m.contains("magic"), "{m}"),
            Err(e) => panic!("wrong error: {e:?}"),
            Ok(_) => panic!("expected a storage error, the file opened cleanly"),
        }
    }

    #[test]
    fn out_of_range_index_is_rejected() {
        let dir = tempdir().unwrap();
        let mut pager = Pager::open(&dir.path().join("db")).unwrap();
        assert!(matches!(pager.read_page(1), Err(QuernError::Storage(_))));
        assert!(matches!(
            pager.write_page(1, &filled(0)),
            Err(QuernError::Storage(_))
        ));
        // Page 0 exists but belongs to the pager.
        assert!(pager.read_page(0).is_ok());
        assert!(matches!(
            pager.write_page(0, &filled(0)),
            Err(QuernError::Storage(_))
        ));
        // And a page write has to be a whole page.
        pager.allocate_page().unwrap();
        assert!(matches!(
            pager.write_page(1, &[0u8; 10]),
            Err(QuernError::Storage(_))
        ));
    }

    #[test]
    fn truncated_file_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let mut pager = Pager::open(&path).unwrap();
            pager.allocate_page().unwrap();
            pager.allocate_page().unwrap();
            pager.flush().unwrap();
        }
        // Whole pages lost: the header still claims three.
        let full = fs::read(&path).unwrap();
        fs::write(&path, &full[..PAGE_SIZE]).unwrap();
        match Pager::open(&path) {
            Err(QuernError::Storage(m)) => assert!(m.contains("truncated"), "{m}"),
            Err(e) => panic!("wrong error: {e:?}"),
            Ok(_) => panic!("expected a storage error, the file opened cleanly"),
        }
        // A partial page is not a whole number of pages either.
        fs::write(&path, &full[..PAGE_SIZE + 7]).unwrap();
        match Pager::open(&path) {
            Err(QuernError::Storage(m)) => assert!(m.contains("truncated"), "{m}"),
            Err(e) => panic!("wrong error: {e:?}"),
            Ok(_) => panic!("expected a storage error, the file opened cleanly"),
        }
    }
}
