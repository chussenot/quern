//! bead: quern-heap — slotted-page heap file. See docs/quern.md §4.
//!
//! One [`Heap`] is one table's rows, held in a chain of pages borrowed from the
//! [`Pager`]. A page carries a slot directory of `(offset, len)` growing
//! forward from the page start and row bytes growing backward from the page
//! end; `RowId` is `(page_idx << 16) | slot_idx`.
//!
//! The heap does not own the pager: every method takes it, because the catalog,
//! the btree and every table's heap all live in the same file. A `Heap` is two
//! page numbers plus the code that reads them, so the storage layer only has to
//! persist [`Heap::first_page`] per table and can call [`Heap::open`] to
//! recover the rest.
//!
//! **Everything below the pager is untrusted input.** A slot directory longer
//! than the page, an `(offset, len)` pair pointing outside it, a value cut
//! short, bad UTF-8 in a `Text`, an unknown tag byte: all
//! [`QuernError::Storage`], never a panic and never an out-of-bounds slice
//! (§1). Every field is validated before it is used to index anything.

use crate::storage::pager::{Page, PageIdx, Pager, PAGE_SIZE};
use crate::types::{QuernError, Result, Row, RowId, Value};

/// Page header: `u16 slot_count | u16 used_bytes | u32 next_page`.
///
/// Every field is chosen so that all-zero — what the pager hands back for a
/// freshly allocated page — is the correct empty page: no slots, no row bytes,
/// no next page (0 is the pager's header page, so it can never be a data page).
const HEADER: usize = 8;

/// A slot directory entry: `u16 offset | u16 len`. `offset == 0` is a
/// tombstone, since offset 0 lands inside the header and can never be a row.
const SLOT: usize = 4;

/// The largest row that can exist: an empty page, minus its one slot.
pub const MAX_ROW_BYTES: usize = PAGE_SIZE - HEADER - SLOT;

/// A table's rows: a chain of slotted pages, in increasing page order.
///
/// Not `Copy`: `last` is live state that [`Heap::insert`] advances, and a copy
/// left behind would go on filling a page that is no longer the tail.
#[derive(Debug, Clone)]
pub struct Heap {
    first: PageIdx,
    /// The tail of the chain — the only page inserts try.
    last: PageIdx,
}

impl Heap {
    /// Start a new heap: one empty page, whose index the caller must persist.
    pub fn create(pager: &mut Pager) -> Result<Heap> {
        // An allocated page reads back zeroed, and a zeroed page is already a
        // valid empty one, so there is nothing to write here.
        let first = pager.allocate_page()?;
        Ok(Heap { first, last: first })
    }

    /// Reopen a heap from its first page, walking to the tail.
    ///
    /// The chain is strictly increasing — `Pager::allocate_page` only ever
    /// hands out higher indices — so a `next` that does not increase is
    /// corruption, and enforcing that is also what makes the walk terminate.
    pub fn open(pager: &Pager, first: PageIdx) -> Result<Heap> {
        if first == 0 {
            return Err(corrupt("heap first page is 0, which is the pager header"));
        }
        let mut last = first;
        while let Some(next) = next_page(pager, last)? {
            last = next;
        }
        Ok(Heap { first, last })
    }

    /// The page to persist so [`Heap::open`] can find this heap again.
    pub fn first_page(&self) -> PageIdx {
        self.first
    }

    /// Append a row. Allocates a page when the tail cannot fit it.
    ///
    /// A row larger than [`MAX_ROW_BYTES`] is an `Err`: there are no overflow
    /// pages, so no number of allocations would ever fit it.
    pub fn insert(&mut self, pager: &mut Pager, row: &Row) -> Result<RowId> {
        let bytes = encode_row(row);
        check_row_len(bytes.len())?;

        // ponytail: only the tail page is ever tried, so free space left by a
        // delete or a relocated update is never reused, and a page before the
        // tail only ever fills once. The ceiling is a heap that grows with
        // deletes; add a free-space map (or compaction) if that starts to hurt.
        let mut page = pager.read_page(self.last)?;
        let mut header = PageHeader::parse(&page)?;
        if header.free() < bytes.len() + SLOT {
            let fresh = pager.allocate_page()?;
            header.next = fresh;
            header.write(&mut page);
            pager.write_page(self.last, &page)?;
            self.last = fresh;
            page = pager.read_page(fresh)?;
            header = PageHeader::parse(&page)?;
        }

        let slot = header.push(&mut page, &bytes);
        pager.write_page(self.last, &page)?;
        Ok(row_id(self.last, slot))
    }

    /// The row, or `Ok(None)` if that slot is a tombstone or was never used.
    pub fn get(&self, pager: &Pager, id: RowId) -> Result<Option<Row>> {
        let (idx, slot) = split_row_id(id)?;
        let page = pager.read_page(idx)?;
        let header = PageHeader::parse(&page)?;
        match header.slot(&page, slot)? {
            None => Ok(None),
            Some((off, len)) => decode_row(&page[off..off + len]).map(Some),
        }
    }

    /// Overwrite a row, returning the `RowId` it now lives at.
    ///
    /// The id is preserved whenever the new bytes fit in the row's own page —
    /// shorter than before, or longer with room left over. Only a page with no
    /// room forces a move, and then the returned id is a new one and the caller
    /// (whoever owns the pk index) has to follow it.
    pub fn update(&mut self, pager: &mut Pager, id: RowId, row: &Row) -> Result<RowId> {
        let bytes = encode_row(row);
        check_row_len(bytes.len())?;

        let (idx, slot) = split_row_id(id)?;
        let mut page = pager.read_page(idx)?;
        let mut header = PageHeader::parse(&page)?;
        let (off, len) = header
            .slot(&page, slot)?
            .ok_or_else(|| QuernError::Storage(format!("no such row: {id}")))?;

        if bytes.len() <= len {
            // ponytail: the tail of the old row is left as a gap, because §4
            // says no compaction. Upgrade path is the same free-space map the
            // insert path wants.
            page[off..off + bytes.len()].copy_from_slice(&bytes);
            header.set_slot(&mut page, slot, off, bytes.len());
            pager.write_page(idx, &page)?;
            return Ok(id);
        }

        if header.free() >= bytes.len() {
            let new_off = header.place(&mut page, &bytes);
            header.set_slot(&mut page, slot, new_off, bytes.len());
            pager.write_page(idx, &page)?;
            return Ok(id);
        }

        header.tombstone(&mut page, slot);
        pager.write_page(idx, &page)?;
        self.insert(pager, row)
    }

    /// Tombstone a row. `Ok(false)` if it was already gone.
    pub fn delete(&mut self, pager: &mut Pager, id: RowId) -> Result<bool> {
        let (idx, slot) = split_row_id(id)?;
        let mut page = pager.read_page(idx)?;
        let header = PageHeader::parse(&page)?;
        if header.slot(&page, slot)?.is_none() {
            return Ok(false);
        }
        header.tombstone(&mut page, slot);
        pager.write_page(idx, &page)?;
        Ok(true)
    }

    /// Every live row, in heap order: pages in chain order, slots ascending.
    ///
    /// Deterministic by construction (§5), and it borrows only the pager, not
    /// the heap, so holding a scan does not lock out a `&mut self` mutator.
    pub fn scan<'a>(&self, pager: &'a Pager) -> HeapScan<'a> {
        HeapScan {
            pager,
            page_idx: self.first,
            loaded: None,
            slot: 0,
            done: false,
        }
    }
}

fn check_row_len(len: usize) -> Result<()> {
    if len > MAX_ROW_BYTES {
        return Err(QuernError::Storage(format!(
            "row of {len} bytes exceeds the {MAX_ROW_BYTES}-byte maximum for a {PAGE_SIZE}-byte page"
        )));
    }
    Ok(())
}

/// Iterator over the live rows of one heap. See [`Heap::scan`].
pub struct HeapScan<'a> {
    pager: &'a Pager,
    page_idx: PageIdx,
    /// The current page and its validated header, loaded on demand.
    loaded: Option<(Page, PageHeader)>,
    slot: u16,
    done: bool,
}

/// What the current position says to do next. Decided under a `&self` borrow
/// of the loaded page, then acted on with `&mut self`.
enum Step {
    Load,
    Skip,
    Row(RowId, Row),
    NextPage(PageIdx),
    End,
    Fail(QuernError),
}

impl Iterator for HeapScan<'_> {
    type Item = Result<(RowId, Row)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            match self.step() {
                Step::Load => {
                    if let Err(e) = self.load() {
                        self.done = true;
                        return Some(Err(e));
                    }
                }
                Step::Skip => self.slot += 1,
                Step::Row(id, row) => {
                    self.slot += 1;
                    return Some(Ok((id, row)));
                }
                Step::NextPage(next) => {
                    self.page_idx = next;
                    self.loaded = None;
                    self.slot = 0;
                }
                Step::End => {
                    self.done = true;
                    return None;
                }
                // A corrupt page ends the scan: one error, then `None`, so a
                // caller that keeps calling cannot spin.
                Step::Fail(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

impl HeapScan<'_> {
    fn step(&self) -> Step {
        let Some((page, header)) = &self.loaded else {
            return Step::Load;
        };
        if self.slot >= header.slot_count {
            return match header.next {
                0 => Step::End,
                next => Step::NextPage(next),
            };
        }
        match header.slot(page, self.slot) {
            Ok(None) => Step::Skip, // tombstone
            Ok(Some((off, len))) => match decode_row(&page[off..off + len]) {
                Ok(row) => Step::Row(row_id(self.page_idx, self.slot), row),
                Err(e) => Step::Fail(e),
            },
            Err(e) => Step::Fail(e),
        }
    }

    fn load(&mut self) -> Result<()> {
        let page = self.pager.read_page(self.page_idx)?;
        let header = PageHeader::parse(&page)?;
        if header.next != 0 && header.next <= self.page_idx {
            return Err(corrupt(&format!(
                "heap chain does not increase: page {} points to {}",
                self.page_idx, header.next
            )));
        }
        self.loaded = Some((page, header));
        Ok(())
    }
}

// --- RowId ------------------------------------------------------------------

/// §4: `RowId` is `(page_idx << 16) | slot_idx`.
pub fn row_id(page: PageIdx, slot: u16) -> RowId {
    (u64::from(page) << 16) | u64::from(slot)
}

/// Inverse of [`row_id`]. `Err` for an id that cannot name a data page: one
/// whose page part overflows a `PageIdx`, or names the pager's header page.
pub fn split_row_id(id: RowId) -> Result<(PageIdx, u16)> {
    let page = id >> 16;
    if page == 0 || page > u64::from(PageIdx::MAX) {
        return Err(QuernError::Storage(format!("not a valid row id: {id}")));
    }
    Ok((page as PageIdx, id as u16))
}

// --- page layout ------------------------------------------------------------

/// A parsed, validated page header. Its existence is the proof that
/// `HEADER + slot_count * SLOT <= PAGE_SIZE - used <= PAGE_SIZE`, which is what
/// lets the rest of this file index the directory without further checks.
#[derive(Debug, Clone, Copy)]
struct PageHeader {
    slot_count: u16,
    /// Row bytes occupied at the page tail, so `0` is an empty page.
    used: u16,
    /// Next page in the chain, `0` for none.
    next: PageIdx,
}

impl PageHeader {
    fn parse(page: &Page) -> Result<PageHeader> {
        // Fixed offsets into a `[u8; PAGE_SIZE]`: no index here can be out of
        // bounds, whatever the bytes say.
        let slot_count = u16::from_le_bytes([page[0], page[1]]);
        let used = usize::from(u16::from_le_bytes([page[2], page[3]]));
        let next = u32::from_le_bytes([page[4], page[5], page[6], page[7]]);

        // `slot_count` is a u16, so the product cannot overflow a usize.
        let dir_end = HEADER + usize::from(slot_count) * SLOT;
        if used > PAGE_SIZE - HEADER || dir_end > PAGE_SIZE - used {
            return Err(corrupt(&format!(
                "a {slot_count}-slot directory and {used} bytes of rows do not fit a {PAGE_SIZE}-byte page"
            )));
        }
        Ok(PageHeader {
            slot_count,
            used: used as u16,
            next,
        })
    }

    fn write(&self, page: &mut Page) {
        page[0..2].copy_from_slice(&self.slot_count.to_le_bytes());
        page[2..4].copy_from_slice(&self.used.to_le_bytes());
        page[4..8].copy_from_slice(&self.next.to_le_bytes());
    }

    fn dir_end(&self) -> usize {
        HEADER + usize::from(self.slot_count) * SLOT
    }

    /// First byte of the row-bytes region, which grows backward from the end.
    fn data_start(&self) -> usize {
        PAGE_SIZE - usize::from(self.used)
    }

    /// Bytes between the directory and the row bytes. Cannot underflow: not
    /// overlapping is exactly the invariant [`PageHeader::parse`] checked.
    fn free(&self) -> usize {
        self.data_start() - self.dir_end()
    }

    /// `(offset, len)` of a live row, or `None` for a tombstone or a slot that
    /// does not exist. `Err` if the pair does not lie in the row-bytes region.
    fn slot(&self, page: &Page, slot: u16) -> Result<Option<(usize, usize)>> {
        if slot >= self.slot_count {
            return Ok(None);
        }
        let at = HEADER + usize::from(slot) * SLOT;
        let off = usize::from(u16::from_le_bytes([page[at], page[at + 1]]));
        let len = usize::from(u16::from_le_bytes([page[at + 2], page[at + 3]]));
        if off == 0 {
            return Ok(None);
        }
        if off < self.data_start() || len > PAGE_SIZE - off {
            return Err(corrupt(&format!(
                "slot {slot} spans {off}..{} but the row bytes are {}..{PAGE_SIZE}",
                off.saturating_add(len),
                self.data_start()
            )));
        }
        Ok(Some((off, len)))
    }

    /// Both `off` and `len` are bounded by `PAGE_SIZE`, and `slot` is inside
    /// the directory this header already validated.
    fn set_slot(&self, page: &mut Page, slot: u16, off: usize, len: usize) {
        let at = HEADER + usize::from(slot) * SLOT;
        page[at..at + 2].copy_from_slice(&(off as u16).to_le_bytes());
        page[at + 2..at + 4].copy_from_slice(&(len as u16).to_le_bytes());
        self.write(page);
    }

    fn tombstone(&self, page: &mut Page, slot: u16) {
        self.set_slot(page, slot, 0, 0);
    }

    /// Copy `bytes` into the free region and return their offset. The caller
    /// must have checked `free()`.
    fn place(&mut self, page: &mut Page, bytes: &[u8]) -> usize {
        let off = self.data_start() - bytes.len();
        page[off..off + bytes.len()].copy_from_slice(bytes);
        self.used += bytes.len() as u16;
        off
    }

    /// Place a row and give it a fresh slot. The caller must have checked that
    /// `free() >= bytes.len() + SLOT`.
    fn push(&mut self, page: &mut Page, bytes: &[u8]) -> u16 {
        let off = self.place(page, bytes);
        let slot = self.slot_count;
        self.slot_count += 1;
        self.set_slot(page, slot, off, bytes.len());
        slot
    }
}

/// `next` of a page, validated to increase so a chain walk terminates.
fn next_page(pager: &Pager, idx: PageIdx) -> Result<Option<PageIdx>> {
    let header = PageHeader::parse(&pager.read_page(idx)?)?;
    match header.next {
        0 => Ok(None),
        next if next > idx => Ok(Some(next)),
        next => Err(corrupt(&format!(
            "heap chain does not increase: page {idx} points to {next}"
        ))),
    }
}

// --- row bytes --------------------------------------------------------------
//
//   row   := value*        (the slot length delimits it; no count prefix)
//   value := u8 tag, u32 len, len bytes
//   tag   := 0 Null (len 0) | 1 Int (len 8, i64) | 2 Text (utf8)
//          | 3 Bool (len 1, 0 or 1)
//
// All integers little-endian.

const VALUE_HEADER: usize = 5;

fn encode_row(row: &Row) -> Vec<u8> {
    let mut out = Vec::new();
    for value in row {
        let (tag, payload): (u8, Vec<u8>) = match value {
            Value::Null => (0, Vec::new()),
            Value::Int(i) => (1, i.to_le_bytes().to_vec()),
            Value::Text(s) => (2, s.as_bytes().to_vec()),
            Value::Bool(b) => (3, vec![u8::from(*b)]),
        };
        out.push(tag);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
    }
    out
}

fn decode_row(mut bytes: &[u8]) -> Result<Row> {
    let mut row = Row::new();
    while !bytes.is_empty() {
        if bytes.len() < VALUE_HEADER {
            return Err(corrupt(&format!(
                "truncated value header: {} byte(s) left, need {VALUE_HEADER}",
                bytes.len()
            )));
        }
        let tag = bytes[0];
        let len = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        let rest = &bytes[VALUE_HEADER..];
        if rest.len() < len {
            return Err(corrupt(&format!(
                "truncated value: tag {tag} claims {len} byte(s), {} left",
                rest.len()
            )));
        }
        let (payload, tail) = rest.split_at(len);
        row.push(decode_value(tag, payload)?);
        bytes = tail;
    }
    Ok(row)
}

fn decode_value(tag: u8, payload: &[u8]) -> Result<Value> {
    let want = |n: usize| {
        if payload.len() == n {
            Ok(())
        } else {
            Err(corrupt(&format!(
                "value tag {tag} carries {} byte(s), expected {n}",
                payload.len()
            )))
        }
    };
    match tag {
        0 => want(0).map(|()| Value::Null),
        1 => {
            want(8)?;
            let mut b = [0u8; 8];
            b.copy_from_slice(payload);
            Ok(Value::Int(i64::from_le_bytes(b)))
        }
        2 => String::from_utf8(payload.to_vec())
            .map(Value::Text)
            .map_err(|e| corrupt(&format!("TEXT value is not utf-8: {e}"))),
        3 => {
            want(1)?;
            match payload[0] {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                b => Err(corrupt(&format!("BOOL value is {b}, expected 0 or 1"))),
            }
        }
        _ => Err(corrupt(&format!("unknown value tag {tag}"))),
    }
}

fn corrupt(what: &str) -> QuernError {
    QuernError::Storage(format!("corrupt heap page: {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use tempfile::tempdir;

    /// A pager on a fresh temp database, plus the tempdir keeping it alive.
    fn fixture() -> (tempfile::TempDir, Pager) {
        let dir = tempdir().unwrap();
        let pager = Pager::open(&dir.path().join("db")).unwrap();
        (dir, pager)
    }

    fn insert(heap: &mut Heap, pager: &mut Pager, row: Row) -> RowId {
        heap.insert(pager, &row).unwrap()
    }

    fn rows(heap: &Heap, pager: &Pager) -> Vec<(RowId, Row)> {
        heap.scan(pager).collect::<Result<Vec<_>>>().unwrap()
    }

    fn txt(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    #[test]
    fn round_trips_every_value_variant() {
        let (_dir, mut pager) = fixture();
        let mut heap = Heap::create(&mut pager).unwrap();

        let row = vec![
            Value::Null,
            Value::Int(0),
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            txt(""),
            txt("a tab\tand a ' quote"),
            txt("héllo → 世界"),
            Value::Bool(true),
            Value::Bool(false),
        ];
        let id = insert(&mut heap, &mut pager, row.clone());
        assert_eq!(heap.get(&pager, id).unwrap(), Some(row.clone()));

        // An empty row is a row: zero values, and it is still live.
        let empty = insert(&mut heap, &mut pager, Vec::new());
        assert_eq!(heap.get(&pager, empty).unwrap(), Some(Vec::new()));
        assert_eq!(rows(&heap, &pager), vec![(id, row), (empty, Vec::new())]);
    }

    #[test]
    fn a_row_too_large_for_an_empty_page_is_an_error_not_a_loop() {
        let (_dir, mut pager) = fixture();
        let mut heap = Heap::create(&mut pager).unwrap();
        let before = pager.page_count();

        let huge = vec![txt(&"x".repeat(MAX_ROW_BYTES))];
        assert!(matches!(
            heap.insert(&mut pager, &huge),
            Err(QuernError::Storage(_))
        ));
        assert_eq!(
            pager.page_count(),
            before,
            "a rejected row must not have allocated anything"
        );

        // The largest row that does fit, fits — in the page we already have.
        let fits = vec![txt(&"x".repeat(MAX_ROW_BYTES - VALUE_HEADER))];
        let id = insert(&mut heap, &mut pager, fits.clone());
        assert_eq!(heap.get(&pager, id).unwrap(), Some(fits));
        assert_eq!(pager.page_count(), before);
    }

    #[test]
    fn rows_spill_onto_a_second_page_in_heap_order() {
        let (_dir, mut pager) = fixture();
        let mut heap = Heap::create(&mut pager).unwrap();

        // 1022 bytes with its slot, so exactly four fit a page.
        let ids: Vec<RowId> = (0..5)
            .map(|i| {
                let row = vec![Value::Int(i), txt(&"y".repeat(1000))];
                insert(&mut heap, &mut pager, row)
            })
            .collect();

        let pages: Vec<PageIdx> = ids.iter().map(|id| split_row_id(*id).unwrap().0).collect();
        assert_eq!(pages, vec![1, 1, 1, 1, 2], "the fifth row spills");
        assert_eq!(pager.page_count(), 3);

        let scanned = rows(&heap, &pager);
        assert_eq!(scanned.len(), 5);
        for (i, (id, row)) in scanned.iter().enumerate() {
            assert_eq!(*id, ids[i], "scan order is insert order");
            assert_eq!(row[0], Value::Int(i as i64));
        }
    }

    #[test]
    fn update_keeps_the_id_in_place_and_when_it_grows() {
        let (_dir, mut pager) = fixture();
        let mut heap = Heap::create(&mut pager).unwrap();
        let id = insert(&mut heap, &mut pager, vec![txt("medium")]);

        // Shorter: same slot, written over the old bytes.
        let shorter = vec![txt("s")];
        assert_eq!(heap.update(&mut pager, id, &shorter).unwrap(), id);
        assert_eq!(heap.get(&pager, id).unwrap(), Some(shorter));

        // Longer, but the page has room: same slot, new offset.
        let longer = vec![txt(&"g".repeat(500)), Value::Bool(true)];
        assert_eq!(heap.update(&mut pager, id, &longer).unwrap(), id);
        assert_eq!(heap.get(&pager, id).unwrap(), Some(longer));

        // Longer than what is left of the page: the row moves, id and all.
        let filler = vec![txt(&"z".repeat(3000))];
        insert(&mut heap, &mut pager, filler.clone());
        let big = vec![txt(&"G".repeat(2000))];
        let moved = heap.update(&mut pager, id, &big).unwrap();
        assert_ne!(moved, id, "no room left, so the row relocated");
        assert_eq!(heap.get(&pager, moved).unwrap(), Some(big.clone()));
        assert_eq!(heap.get(&pager, id).unwrap(), None, "old slot tombstoned");

        // Exactly one copy of it survives, and the scan agrees.
        assert_eq!(
            rows(&heap, &pager),
            vec![(row_id(1, 1), filler), (moved, big)]
        );

        // Updating a dead row, or a slot that never existed, is an error.
        let null = vec![Value::Null];
        assert!(matches!(
            heap.update(&mut pager, id, &null),
            Err(QuernError::Storage(_))
        ));
        assert!(matches!(
            heap.update(&mut pager, row_id(1, 900), &null),
            Err(QuernError::Storage(_))
        ));
    }

    #[test]
    fn delete_tombstones_and_the_scan_skips_it() {
        let (_dir, mut pager) = fixture();
        let mut heap = Heap::create(&mut pager).unwrap();
        let ids: Vec<RowId> = (0..4)
            .map(|i| insert(&mut heap, &mut pager, vec![Value::Int(i)]))
            .collect();

        assert!(heap.delete(&mut pager, ids[1]).unwrap());
        assert!(
            !heap.delete(&mut pager, ids[1]).unwrap(),
            "deleting twice is not an error, just false"
        );
        assert!(!heap.delete(&mut pager, row_id(1, 500)).unwrap());
        assert_eq!(heap.get(&pager, ids[1]).unwrap(), None);

        let scanned = rows(&heap, &pager);
        assert_eq!(
            scanned.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![ids[0], ids[2], ids[3]]
        );
        // A slot is never reused, so the next insert gets slot 4, not 1.
        assert_eq!(
            insert(&mut heap, &mut pager, vec![Value::Int(9)]),
            row_id(1, 4)
        );
    }

    #[test]
    fn row_ids_encode_and_decode() {
        assert_eq!(row_id(1, 0), 1 << 16);
        assert_eq!(row_id(3, 7), (3 << 16) | 7);
        assert_eq!(split_row_id(row_id(3, 7)).unwrap(), (3, 7));
        assert_eq!(
            split_row_id(row_id(PageIdx::MAX, u16::MAX)).unwrap(),
            (PageIdx::MAX, u16::MAX)
        );
        // Page 0 is the pager header, and nothing above u32 pages can exist.
        assert!(split_row_id(0).is_err());
        assert!(split_row_id(0xFFFF).is_err());
        assert!(split_row_id(u64::MAX).is_err());
    }

    #[test]
    fn persists_across_a_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db");
        let (first, ids) = {
            let mut pager = Pager::open(&path).unwrap();
            let mut heap = Heap::create(&mut pager).unwrap();
            let ids: Vec<RowId> = (0..200)
                .map(|i| {
                    let row = vec![
                        Value::Int(i),
                        txt(&format!("row {i}")),
                        Value::Bool(i % 2 == 0),
                    ];
                    insert(&mut heap, &mut pager, row)
                })
                .collect();
            heap.delete(&mut pager, ids[7]).unwrap();
            pager.flush().unwrap();
            (heap.first_page(), ids)
        };

        let pager = Pager::open(&path).unwrap();
        let heap = Heap::open(&pager, first).unwrap();
        let scanned = rows(&heap, &pager);
        assert_eq!(scanned.len(), 199);
        assert_eq!(scanned[0].0, ids[0]);
        assert_eq!(scanned[7].0, ids[8], "the deleted row stays deleted");
        assert_eq!(
            scanned.last().unwrap().1[1],
            txt("row 199"),
            "including the rows that spilled onto later pages"
        );
        assert!(scanned.last().unwrap().0 > row_id(1, u16::MAX), "spilled");
        assert_eq!(heap.get(&pager, ids[7]).unwrap(), None);
    }

    #[test]
    fn reopening_a_heap_with_a_bad_chain_is_an_error() {
        let (_dir, mut pager) = fixture();
        let mut heap = Heap::create(&mut pager).unwrap();
        insert(&mut heap, &mut pager, vec![Value::Int(1)]);

        assert!(matches!(Heap::open(&pager, 0), Err(QuernError::Storage(_))));

        // A `next` that does not increase would make a chain walk loop forever.
        let mut page = pager.read_page(1).unwrap();
        page[4..8].copy_from_slice(&1u32.to_le_bytes());
        pager.write_page(1, &page).unwrap();
        assert!(matches!(Heap::open(&pager, 1), Err(QuernError::Storage(_))));
        // ...and a scan refuses it too, rather than spinning.
        let scanned: Vec<_> = heap.scan(&pager).collect();
        assert_eq!(scanned.len(), 1);
        assert!(matches!(scanned[0], Err(QuernError::Storage(_))));
    }

    /// Insert one row, corrupt page 1 with `edit`, then assert that both `get`
    /// and `scan` report a storage error instead of panicking.
    fn assert_corrupt(row: Row, edit: impl Fn(&mut Page)) {
        let (_dir, mut pager) = fixture();
        let mut heap = Heap::create(&mut pager).unwrap();
        let id = insert(&mut heap, &mut pager, row);

        let mut page = pager.read_page(1).unwrap();
        edit(&mut page);
        pager.write_page(1, &page).unwrap();

        assert!(
            matches!(heap.get(&pager, id), Err(QuernError::Storage(_))),
            "get accepted a corrupt page"
        );
        assert!(
            heap.scan(&pager)
                .any(|r| matches!(r, Err(QuernError::Storage(_)))),
            "scan accepted a corrupt page"
        );
    }

    #[test]
    fn a_corrupt_page_is_an_error_never_a_panic() {
        // One `Value::Int(1)` occupies the last 13 bytes of the page.
        let int_at = PAGE_SIZE - 13;

        // A slot directory that cannot fit the page.
        assert_corrupt(vec![Value::Int(1)], |p| {
            p[0..2].copy_from_slice(&u16::MAX.to_le_bytes())
        });
        // More row bytes claimed than the page holds.
        assert_corrupt(vec![Value::Int(1)], |p| {
            p[2..4].copy_from_slice(&(PAGE_SIZE as u16).to_le_bytes())
        });
        // A slot whose (offset, len) runs off the end of the page.
        assert_corrupt(vec![Value::Int(1)], |p| {
            p[HEADER + 2..HEADER + 4].copy_from_slice(&4000u16.to_le_bytes())
        });
        // A slot pointing into the free space ahead of the row bytes.
        assert_corrupt(vec![Value::Int(1)], |p| {
            p[HEADER..HEADER + 2].copy_from_slice(&100u16.to_le_bytes())
        });
        // A value header cut short by the slot length.
        assert_corrupt(vec![Value::Int(1)], |p| {
            p[HEADER + 2..HEADER + 4].copy_from_slice(&3u16.to_le_bytes())
        });
        // A value cut short: an Int claiming 3 of its 8 bytes.
        assert_corrupt(vec![Value::Int(1)], move |p| {
            p[int_at + 1..int_at + 5].copy_from_slice(&3u32.to_le_bytes())
        });
        // An unknown tag byte.
        assert_corrupt(vec![Value::Int(1)], move |p| p[int_at] = 9);
        // Bad UTF-8 in a TEXT.
        assert_corrupt(vec![txt("ok")], |p| p[PAGE_SIZE - 1] = 0xFF);
        // A BOOL that is neither 0 nor 1.
        assert_corrupt(vec![Value::Bool(true)], |p| p[PAGE_SIZE - 1] = 7);
    }

    #[test]
    fn every_random_row_reads_back() {
        let (_dir, mut pager) = fixture();
        let mut heap = Heap::create(&mut pager).unwrap();
        // Seeded, so a failure is reproducible (§5 wants determinism).
        let mut rng = StdRng::seed_from_u64(0x5EED_0FA0_BEEF);

        let mut expected: Vec<(RowId, Row)> = Vec::new();
        for _ in 0..3000 {
            let row: Row = (0..rng.gen_range(1..5))
                .map(|_| match rng.gen_range(0..4) {
                    0 => Value::Null,
                    1 => Value::Int(rng.gen()),
                    2 => Value::Text(
                        (0..rng.gen_range(0..40))
                            .map(|_| char::from(b'a' + rng.gen_range(0..26)))
                            .collect(),
                    ),
                    _ => Value::Bool(rng.gen()),
                })
                .collect();
            let id = insert(&mut heap, &mut pager, row.clone());
            expected.push((id, row));
        }

        for (id, row) in &expected {
            assert_eq!(heap.get(&pager, *id).unwrap().as_ref(), Some(row));
        }
        assert_eq!(rows(&heap, &pager), expected, "scan order is heap order");

        // And again from disk, with the chain rebuilt by `open`.
        pager.flush().unwrap();
        let reopened = Heap::open(&pager, heap.first_page()).unwrap();
        assert_eq!(rows(&reopened, &pager), expected);
    }
}
