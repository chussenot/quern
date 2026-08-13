//! bead: quern-btree — B+tree on INTEGER PRIMARY KEY. See docs/quern.md §4.
//!
//! Order-32 B+tree mapping the `INTEGER PRIMARY KEY` value (`i64`) to a
//! [`RowId`], one node per pager page. Internal nodes hold separator keys plus
//! child page indices; leaves hold keys plus row ids plus a link to the next
//! leaf, so walking from the leftmost leaf yields every entry in key order.
//! Only built for a table that declares a PK: `lookup_pk` uses it, `scan` does
//! not.
//!
//! The tree does not own the [`Pager`] — every method takes it as an argument
//! and the only state here is the root page index. That is deliberate: the
//! frozen `Storage` trait has `scan(&self)` and `lookup_pk(&self)`, one pager
//! is shared by every table's tree, and a tree holding a `&mut Pager` could
//! not coexist with either. Whoever owns the pager persists [`BTree::root`]
//! (it changes when the tree grows a level) and calls `Pager::flush`.
//!
//! Node bytes come off a disk, so decoding is a trust boundary: a bad tag, a
//! key count past the order, unsorted keys, a child index outside the file, a
//! leaf link that dangles — all [`QuernError::Storage`], never a panic (§1).
//! Structural loops are caught by a depth cap on descent and a page budget on
//! the leaf walk, so corrupt pointers cannot spin or overflow the stack.

use crate::storage::pager::{Page, PageIdx, Pager, PAGE_SIZE};
use crate::types::{QuernError, Result, RowId};

/// Order 32 (§4): an internal node holds up to 32 children, so up to 31 keys.
const ORDER: usize = 32;
const MAX_KEYS: usize = ORDER - 1;

const TAG_LEAF: u8 = 1;
const TAG_INTERNAL: u8 = 2;

const OFF_TAG: usize = 0;
const OFF_COUNT: usize = 1;
/// Next-leaf link, leaf nodes only. 0 means "no next leaf" (page 0 is the
/// pager header, so it can never be a node).
const OFF_NEXT: usize = 4;
const OFF_KEYS: usize = 16;
/// Row ids (leaf) or child indices (internal) follow the key array.
const OFF_VALS: usize = OFF_KEYS + MAX_KEYS * 8;

const _: () = assert!(
    OFF_VALS + MAX_KEYS * 8 <= PAGE_SIZE,
    "leaf node overflows a page"
);
const _: () = assert!(
    OFF_VALS + ORDER * 4 <= PAGE_SIZE,
    "internal node overflows a page"
);

/// A tree of order 32 reaches 2^160 entries at this depth; anything deeper is
/// a pointer cycle, not a tree.
const MAX_DEPTH: usize = 32;

/// A B+tree, identified by its root page. Cheap to copy and to reopen.
pub struct BTree {
    root: PageIdx,
}

impl BTree {
    /// Allocate an empty tree: one leaf page, which becomes the root.
    pub fn create(pager: &mut Pager) -> Result<BTree> {
        let root = pager.allocate_page()?;
        write_node(pager, root, &Node::empty_leaf())?;
        Ok(BTree { root })
    }

    /// Reopen an existing tree from a persisted root page index.
    pub fn open(root: PageIdx) -> Result<BTree> {
        if root == 0 {
            return Err(corrupt("btree root 0: page 0 is the pager header"));
        }
        Ok(BTree { root })
    }

    /// The root page. Persist this after any [`BTree::insert`] — a root split
    /// grows the tree's height and moves the root to a new page.
    pub fn root(&self) -> PageIdx {
        self.root
    }

    /// Point lookup. `Ok(None)` means the key is not in the tree.
    pub fn lookup(&self, pager: &Pager, key: i64) -> Result<Option<RowId>> {
        let leaf = find_leaf(pager, self.root, key)?;
        match read_node(pager, leaf)? {
            Node::Leaf { keys, rids, .. } => Ok(keys
                .binary_search(&key)
                .ok()
                .and_then(|i| rids.get(i).copied())),
            Node::Internal { .. } => Err(corrupt("descent ended on an internal node")),
        }
    }

    /// Insert a key. `Ok(false)` means the key was already present and nothing
    /// changed — that is the duplicate-PRIMARY-KEY signal `exec/dml.rs` turns
    /// into a user-facing error, since it is the caller that knows the table
    /// and column names to name in it.
    pub fn insert(&mut self, pager: &mut Pager, key: i64, rid: RowId) -> Result<bool> {
        match insert_at(pager, self.root, key, rid, 0)? {
            Ins::Duplicate => Ok(false),
            Ins::Done => Ok(true),
            Ins::Split { sep, right } => {
                // The root split: the only way the tree gets taller.
                let root = pager.allocate_page()?;
                let node = Node::Internal {
                    keys: vec![sep],
                    children: vec![self.root, right],
                };
                write_node(pager, root, &node)?;
                self.root = root;
                Ok(true)
            }
        }
    }

    /// Remove a key. `Ok(false)` means it was not there.
    ///
    /// ponytail: no rebalancing. A delete can leave a leaf underfull or even
    /// empty, and the page it occupies is never reclaimed (the pager has no
    /// free list, by §4). Both are harmless here: separator keys still route
    /// correctly, lookups land on the right leaf and find nothing, and the
    /// leaf walk skips an empty leaf. The ceiling is space and depth after a
    /// delete-heavy workload. The upgrade path is standard B+tree
    /// rebalancing — borrow from a sibling, else merge with it and drop the
    /// separator from the parent, collapsing the root when it falls to one
    /// child — which needs a pager free list to actually recover the pages.
    pub fn delete(&self, pager: &mut Pager, key: i64) -> Result<bool> {
        let leaf = find_leaf(pager, self.root, key)?;
        match read_node(pager, leaf)? {
            Node::Leaf {
                mut keys,
                mut rids,
                next,
            } => match keys.binary_search(&key) {
                Err(_) => Ok(false),
                Ok(pos) => {
                    keys.remove(pos);
                    rids.remove(pos);
                    write_node(pager, leaf, &Node::Leaf { keys, rids, next })?;
                    Ok(true)
                }
            },
            Node::Internal { .. } => Err(corrupt("descent ended on an internal node")),
        }
    }

    /// Every `(key, RowId)` in ascending key order, by walking the leaf links.
    pub fn iter<'p>(&self, pager: &'p Pager) -> Result<LeafIter<'p>> {
        Ok(LeafIter {
            pager,
            page: leftmost_leaf(pager, self.root)?,
            buf: Vec::new(),
            pos: 0,
            budget: pager.page_count(),
        })
    }
}

/// Iterator over the linked leaves. Yields `Err` once and then stops if the
/// chain is corrupt.
pub struct LeafIter<'p> {
    pager: &'p Pager,
    /// Next leaf to load; 0 means the walk is over.
    page: PageIdx,
    buf: Vec<(i64, RowId)>,
    pos: usize,
    /// A chain longer than the file has pages is a cycle.
    budget: PageIdx,
}

impl Iterator for LeafIter<'_> {
    type Item = Result<(i64, RowId)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.buf.get(self.pos) {
                self.pos += 1;
                return Some(Ok(*entry));
            }
            if self.page == 0 {
                return None;
            }
            if self.budget == 0 {
                self.page = 0;
                return Some(Err(corrupt("leaf chain does not terminate")));
            }
            self.budget -= 1;
            match read_node(self.pager, self.page) {
                Err(e) => {
                    self.page = 0;
                    return Some(Err(e));
                }
                Ok(Node::Internal { .. }) => {
                    self.page = 0;
                    return Some(Err(corrupt("leaf chain reached an internal node")));
                }
                Ok(Node::Leaf { keys, rids, next }) => {
                    self.buf = keys.into_iter().zip(rids).collect();
                    self.pos = 0;
                    self.page = next;
                }
            }
        }
    }
}

// --- descent ---------------------------------------------------------------

/// Which child of an internal node covers `key`. `keys[i]` is the smallest key
/// in `children[i + 1]`, so the slot is the number of separators `<= key`.
fn child_slot(keys: &[i64], key: i64) -> usize {
    keys.partition_point(|k| *k <= key)
}

fn find_leaf(pager: &Pager, root: PageIdx, key: i64) -> Result<PageIdx> {
    let mut idx = root;
    for _ in 0..MAX_DEPTH {
        match read_node(pager, idx)? {
            Node::Leaf { .. } => return Ok(idx),
            Node::Internal { keys, children } => idx = child_at(&children, child_slot(&keys, key))?,
        }
    }
    Err(corrupt(
        "btree descent exceeded the depth cap: cyclic child pointers",
    ))
}

fn leftmost_leaf(pager: &Pager, root: PageIdx) -> Result<PageIdx> {
    let mut idx = root;
    for _ in 0..MAX_DEPTH {
        match read_node(pager, idx)? {
            Node::Leaf { .. } => return Ok(idx),
            Node::Internal { children, .. } => idx = child_at(&children, 0)?,
        }
    }
    Err(corrupt(
        "btree descent exceeded the depth cap: cyclic child pointers",
    ))
}

fn child_at(children: &[PageIdx], slot: usize) -> Result<PageIdx> {
    children
        .get(slot)
        .copied()
        .ok_or_else(|| corrupt("internal node has no child for that slot"))
}

// --- insert ----------------------------------------------------------------

enum Ins {
    Done,
    Duplicate,
    /// The node split: `sep` and the new right page go to the parent.
    Split {
        sep: i64,
        right: PageIdx,
    },
}

fn insert_at(pager: &mut Pager, idx: PageIdx, key: i64, rid: RowId, depth: usize) -> Result<Ins> {
    if depth >= MAX_DEPTH {
        return Err(corrupt(
            "btree descent exceeded the depth cap: cyclic child pointers",
        ));
    }
    match read_node(pager, idx)? {
        Node::Leaf {
            mut keys,
            mut rids,
            next,
        } => {
            match keys.binary_search(&key) {
                Ok(_) => return Ok(Ins::Duplicate),
                Err(pos) => {
                    keys.insert(pos, key);
                    rids.insert(pos, rid);
                }
            }
            if keys.len() <= MAX_KEYS {
                write_node(pager, idx, &Node::Leaf { keys, rids, next })?;
                return Ok(Ins::Done);
            }
            // Split: the right half keeps its own copy of the separator, since
            // a leaf holds values, and the leaves stay linked left-to-right.
            let mid = keys.len() / 2;
            let rkeys = keys.split_off(mid);
            let rrids = rids.split_off(mid);
            let sep = *rkeys
                .first()
                .ok_or_else(|| corrupt("leaf split produced an empty right node"))?;
            let right = pager.allocate_page()?;
            write_node(
                pager,
                right,
                &Node::Leaf {
                    keys: rkeys,
                    rids: rrids,
                    next,
                },
            )?;
            write_node(
                pager,
                idx,
                &Node::Leaf {
                    keys,
                    rids,
                    next: right,
                },
            )?;
            Ok(Ins::Split { sep, right })
        }
        Node::Internal {
            mut keys,
            mut children,
        } => {
            let slot = child_slot(&keys, key);
            let child = child_at(&children, slot)?;
            // Done and Duplicate both leave this node untouched; only a split
            // of the child has to be absorbed here.
            let (sep, right) = match insert_at(pager, child, key, rid, depth + 1)? {
                Ins::Done => return Ok(Ins::Done),
                Ins::Duplicate => return Ok(Ins::Duplicate),
                Ins::Split { sep, right } => (sep, right),
            };
            keys.insert(slot, sep);
            children.insert(slot + 1, right);
            if keys.len() <= MAX_KEYS {
                write_node(pager, idx, &Node::Internal { keys, children })?;
                return Ok(Ins::Done);
            }
            // Split: unlike a leaf, the middle key moves up rather than being
            // copied — an internal node holds separators, not values.
            let mid = keys.len() / 2;
            let mut rkeys = keys.split_off(mid);
            let up = rkeys.remove(0);
            let rchildren = children.split_off(mid + 1);
            let right = pager.allocate_page()?;
            write_node(
                pager,
                right,
                &Node::Internal {
                    keys: rkeys,
                    children: rchildren,
                },
            )?;
            write_node(pager, idx, &Node::Internal { keys, children })?;
            Ok(Ins::Split { sep: up, right })
        }
    }
}

// --- node encoding ---------------------------------------------------------

enum Node {
    Leaf {
        keys: Vec<i64>,
        rids: Vec<RowId>,
        /// Next leaf in key order, or 0 for the last one.
        next: PageIdx,
    },
    Internal {
        keys: Vec<i64>,
        /// Always `keys.len() + 1` entries.
        children: Vec<PageIdx>,
    },
}

impl Node {
    fn empty_leaf() -> Node {
        Node::Leaf {
            keys: Vec::new(),
            rids: Vec::new(),
            next: 0,
        }
    }

    /// Decode a node, validating every field that came off disk before it is
    /// used to size or index anything. `page_count` bounds the page indices.
    fn decode(page: &Page, page_count: PageIdx) -> Result<Node> {
        let count = usize::from(u16::from_le_bytes([page[OFF_COUNT], page[OFF_COUNT + 1]]));
        if count > MAX_KEYS {
            return Err(corrupt(&format!(
                "btree node claims {count} keys, order {ORDER} allows {MAX_KEYS}"
            )));
        }
        let keys: Vec<i64> = (0..count)
            .map(|i| get_i64(page, OFF_KEYS + i * 8))
            .collect();
        if keys.windows(2).any(|w| w[0] >= w[1]) {
            return Err(corrupt("btree node keys are not strictly ascending"));
        }
        match page[OFF_TAG] {
            TAG_LEAF => {
                let next = get_u32(page, OFF_NEXT);
                if next >= page_count {
                    return Err(corrupt(&format!(
                        "leaf link to page {next}, database has {page_count} page(s)"
                    )));
                }
                Ok(Node::Leaf {
                    keys,
                    rids: (0..count)
                        .map(|i| get_u64(page, OFF_VALS + i * 8))
                        .collect(),
                    next,
                })
            }
            TAG_INTERNAL => {
                if count == 0 {
                    return Err(corrupt("internal btree node has no keys"));
                }
                let children: Vec<PageIdx> = (0..count + 1)
                    .map(|i| get_u32(page, OFF_VALS + i * 4))
                    .collect();
                if let Some(bad) = children
                    .iter()
                    .find(|c| **c == 0 || **c >= page_count)
                    .copied()
                {
                    return Err(corrupt(&format!(
                        "internal btree node points at page {bad}, database has {page_count} page(s)"
                    )));
                }
                Ok(Node::Internal { keys, children })
            }
            // Zero included: an allocated-but-unwritten page reads as zeros,
            // which is not a node.
            tag => Err(corrupt(&format!("btree node has invalid tag {tag}"))),
        }
    }

    fn encode(&self) -> Result<Page> {
        let mut page = [0u8; PAGE_SIZE];
        let keys = match self {
            Node::Leaf { keys, .. } | Node::Internal { keys, .. } => keys,
        };
        if keys.len() > MAX_KEYS {
            return Err(corrupt(&format!(
                "btree node of {} keys exceeds order {ORDER}",
                keys.len()
            )));
        }
        page[OFF_COUNT..OFF_COUNT + 2].copy_from_slice(&(keys.len() as u16).to_le_bytes());
        for (i, k) in keys.iter().enumerate() {
            put(&mut page, OFF_KEYS + i * 8, &k.to_le_bytes());
        }
        match self {
            Node::Leaf { rids, next, .. } => {
                page[OFF_TAG] = TAG_LEAF;
                put(&mut page, OFF_NEXT, &next.to_le_bytes());
                for (i, r) in rids.iter().enumerate() {
                    put(&mut page, OFF_VALS + i * 8, &r.to_le_bytes());
                }
            }
            Node::Internal { children, .. } => {
                page[OFF_TAG] = TAG_INTERNAL;
                if children.len() != keys.len() + 1 {
                    return Err(corrupt(&format!(
                        "internal btree node has {} keys but {} children",
                        keys.len(),
                        children.len()
                    )));
                }
                for (i, c) in children.iter().enumerate() {
                    put(&mut page, OFF_VALS + i * 4, &c.to_le_bytes());
                }
            }
        }
        Ok(page)
    }
}

fn read_node(pager: &Pager, idx: PageIdx) -> Result<Node> {
    Node::decode(&pager.read_page(idx)?, pager.page_count())
}

fn write_node(pager: &mut Pager, idx: PageIdx, node: &Node) -> Result<()> {
    pager.write_page(idx, &node.encode()?)
}

fn put(page: &mut Page, at: usize, bytes: &[u8]) {
    page[at..at + bytes.len()].copy_from_slice(bytes);
}

fn get_i64(page: &Page, at: usize) -> i64 {
    i64::from_le_bytes(eight(page, at))
}

fn get_u64(page: &Page, at: usize) -> u64 {
    u64::from_le_bytes(eight(page, at))
}

fn get_u32(page: &Page, at: usize) -> u32 {
    u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]])
}

/// Offsets are bounded by the validated key count, so this cannot go past the
/// page — and there is no `unwrap` here to panic if that ever stops holding.
fn eight(page: &Page, at: usize) -> [u8; 8] {
    let mut b = [0u8; 8];
    b.copy_from_slice(&page[at..at + 8]);
    b
}

fn corrupt(msg: &str) -> QuernError {
    QuernError::Storage(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::seq::SliceRandom;
    use rand::{Rng, SeedableRng};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, Pager, BTree) {
        let dir = tempfile::tempdir().unwrap();
        let mut pager = Pager::open(&dir.path().join("db")).unwrap();
        let tree = BTree::create(&mut pager).unwrap();
        (dir, pager, tree)
    }

    fn walk(pager: &Pager, tree: &BTree) -> Vec<(i64, RowId)> {
        tree.iter(pager)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
    }

    /// Levels from the root down to a leaf, following the leftmost child.
    fn height(pager: &Pager, tree: &BTree) -> usize {
        let mut idx = tree.root();
        let mut levels = 1;
        while let Node::Internal { children, .. } = read_node(pager, idx).unwrap() {
            idx = children[0];
            levels += 1;
        }
        levels
    }

    #[test]
    fn ten_thousand_random_keys_look_up_and_walk_sorted() {
        let (_dir, mut pager, mut tree) = fresh();
        // Seeded: same corpus every run (§5 determinism).
        let mut rng = rand::rngs::StdRng::seed_from_u64(0x9E3779B9);
        let mut model: BTreeMap<i64, RowId> = BTreeMap::new();

        for _ in 0..10_000 {
            let key: i64 = rng.gen_range(-5_000_000..5_000_000);
            let rid: RowId = rng.gen();
            let inserted = tree.insert(&mut pager, key, rid).unwrap();
            assert_eq!(
                inserted,
                !model.contains_key(&key),
                "insert of {key} disagreed with the model about duplication"
            );
            model.entry(key).or_insert(rid);
        }

        for (key, rid) in &model {
            assert_eq!(
                tree.lookup(&pager, *key).unwrap(),
                Some(*rid),
                "lookup of {key} after 10k inserts"
            );
        }

        let walked = walk(&pager, &tree);
        assert!(
            walked.windows(2).all(|w| w[0].0 < w[1].0),
            "the leaf walk is not sorted"
        );
        assert_eq!(
            walked,
            model.into_iter().collect::<Vec<_>>(),
            "the leaf walk is not complete"
        );
        assert!(height(&pager, &tree) >= 3, "10k keys should be multi-level");
    }

    #[test]
    fn forced_multi_level_split_keeps_every_key() {
        let (_dir, mut pager, mut tree) = fresh();
        assert_eq!(height(&pager, &tree), 1, "a fresh tree is one leaf");

        // Ascending: worst case for a B+tree, and the shortest route to a
        // root split of an internal node.
        for k in 0..2_000i64 {
            assert!(tree.insert(&mut pager, k, k as RowId * 7).unwrap());
        }
        assert!(
            height(&pager, &tree) >= 3,
            "expected the root to have split at least twice, got height {}",
            height(&pager, &tree)
        );
        assert!(matches!(
            read_node(&pager, tree.root()).unwrap(),
            Node::Internal { .. }
        ));
        for k in 0..2_000i64 {
            assert_eq!(tree.lookup(&pager, k).unwrap(), Some(k as RowId * 7));
        }
        assert_eq!(walk(&pager, &tree).len(), 2_000);

        // And in a shuffled order, for a different split shape.
        let (_dir2, mut pager2, mut tree2) = fresh();
        let mut keys: Vec<i64> = (0..2_000).collect();
        keys.shuffle(&mut rand::rngs::StdRng::seed_from_u64(7));
        for k in &keys {
            assert!(tree2.insert(&mut pager2, *k, *k as RowId).unwrap());
        }
        assert!(height(&pager2, &tree2) >= 3);
        assert_eq!(
            walk(&pager2, &tree2),
            (0..2_000).map(|k| (k, k as RowId)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn duplicate_key_is_rejected_and_leaves_the_first_value() {
        let (_dir, mut pager, mut tree) = fresh();
        assert!(tree.insert(&mut pager, 42, 100).unwrap());
        assert!(
            !tree.insert(&mut pager, 42, 999).unwrap(),
            "the duplicate must be signalled, not silently applied"
        );
        assert_eq!(tree.lookup(&pager, 42).unwrap(), Some(100));
        assert_eq!(walk(&pager, &tree).len(), 1);

        // Still true across a split boundary, deeper in the tree. 42 is
        // already in, and it must be the only key that refuses.
        for k in 0..200i64 {
            assert_eq!(
                tree.insert(&mut pager, k, k as RowId).unwrap(),
                k != 42,
                "insert of {k}"
            );
        }
        assert!(!tree.insert(&mut pager, 137, 1).unwrap());
        assert_eq!(tree.lookup(&pager, 137).unwrap(), Some(137));
        assert_eq!(
            tree.lookup(&pager, 42).unwrap(),
            Some(100),
            "first write wins"
        );
        assert_eq!(walk(&pager, &tree).len(), 200);
    }

    #[test]
    fn absent_key_looks_up_as_none() {
        let (_dir, mut pager, mut tree) = fresh();
        assert_eq!(tree.lookup(&pager, 1).unwrap(), None, "empty tree");
        for k in [2i64, 4, 6, 8] {
            tree.insert(&mut pager, k, k as RowId).unwrap();
        }
        for k in [-1i64, 1, 3, 5, 7, 9, i64::MIN, i64::MAX] {
            assert_eq!(tree.lookup(&pager, k).unwrap(), None, "absent key {k}");
        }
    }

    #[test]
    fn delete_removes_only_its_own_key() {
        let (_dir, mut pager, mut tree) = fresh();
        for k in 0..500i64 {
            tree.insert(&mut pager, k, k as RowId).unwrap();
        }
        assert!(!tree.delete(&mut pager, 10_000).unwrap(), "absent delete");

        for k in (0..500).step_by(2) {
            assert!(tree.delete(&mut pager, k).unwrap(), "delete of {k}");
            assert!(!tree.delete(&mut pager, k).unwrap(), "second delete of {k}");
        }
        for k in 0..500i64 {
            let want = (k % 2 == 1).then_some(k as RowId);
            assert_eq!(
                tree.lookup(&pager, k).unwrap(),
                want,
                "key {k} after delete"
            );
        }
        let walked = walk(&pager, &tree);
        assert!(walked.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(walked.len(), 250, "the walk must skip underfull leaves");

        // Emptying the tree entirely is legal, and it still walks.
        for k in (1..500).step_by(2) {
            assert!(tree.delete(&mut pager, k).unwrap());
        }
        assert!(walk(&pager, &tree).is_empty());
        assert!(
            tree.insert(&mut pager, 7, 7).unwrap(),
            "reusable after empty"
        );
        assert_eq!(walk(&pager, &tree), vec![(7, 7)]);
    }

    #[test]
    fn survives_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().join("db");
        let root = {
            let mut pager = Pager::open(&path).unwrap();
            let mut tree = BTree::create(&mut pager).unwrap();
            for k in 0..1_000i64 {
                tree.insert(&mut pager, k * 3, k as RowId).unwrap();
            }
            // A delete must survive the reopen too, not just the inserts.
            assert!(tree.delete(&mut pager, 9).unwrap());
            pager.flush().unwrap();
            tree.root()
        };

        let pager = Pager::open(&path).unwrap();
        let tree = BTree::open(root).unwrap();
        for k in 0..1_000i64 {
            let want = (k != 3).then_some(k as RowId);
            assert_eq!(tree.lookup(&pager, k * 3).unwrap(), want, "key {}", k * 3);
        }
        assert_eq!(walk(&pager, &tree).len(), 999);
        assert!(BTree::open(0).is_err(), "page 0 is the pager header");
    }

    /// Every corruption below is bytes an attacker or a bad disk could put in
    /// a node page; none of them may panic.
    #[test]
    fn corrupt_node_bytes_are_storage_errors() {
        fn broken(edit: impl FnOnce(&mut Page)) -> QuernError {
            let (_dir, mut pager, mut tree) = fresh();
            for k in 0..200i64 {
                tree.insert(&mut pager, k, k as RowId).unwrap();
            }
            let root = tree.root();
            let mut page = pager.read_page(root).unwrap();
            edit(&mut page);
            pager.write_page(root, &page).unwrap();
            // Whichever operation notices first, it must be an Err.
            let err = tree
                .lookup(&pager, 5)
                .err()
                .or_else(|| tree.iter(&pager).err())
                .or_else(|| tree.iter(&pager).ok()?.collect::<Result<Vec<_>>>().err())
                .or_else(|| tree.insert(&mut pager, 12_345, 1).err());
            err.expect("a corrupt node page was accepted")
        }

        // 1. A tag that is not leaf or internal — including the all-zero page
        //    an allocation hands out.
        assert!(matches!(
            broken(|p| p[OFF_TAG] = 0x7F),
            QuernError::Storage(_)
        ));
        assert!(matches!(broken(|p| p.fill(0)), QuernError::Storage(_)));

        // 2. A key count past the order: the classic "truncated node", where
        //    the header claims more entries than the page can hold.
        let e = broken(|p| p[OFF_COUNT..OFF_COUNT + 2].copy_from_slice(&999u16.to_le_bytes()));
        match e {
            QuernError::Storage(m) => assert!(m.contains("999"), "{m}"),
            other => panic!("wrong error: {other:?}"),
        }
        assert!(matches!(
            broken(|p| p[OFF_COUNT..OFF_COUNT + 2]
                .copy_from_slice(&(MAX_KEYS as u16 + 1).to_le_bytes())),
            QuernError::Storage(_)
        ));

        // 3. A child index outside the file, and one pointing at the header.
        let e = broken(|p| put(p, OFF_VALS, &9_999u32.to_le_bytes()));
        match e {
            QuernError::Storage(m) => assert!(m.contains("9999"), "{m}"),
            other => panic!("wrong error: {other:?}"),
        }
        assert!(matches!(
            broken(|p| put(p, OFF_VALS, &0u32.to_le_bytes())),
            QuernError::Storage(_)
        ));

        // 4. Keys out of order would make the binary search lie.
        assert!(matches!(
            broken(|p| put(p, OFF_KEYS, &i64::MAX.to_le_bytes())),
            QuernError::Storage(_)
        ));
    }

    #[test]
    fn corrupt_leaf_link_cannot_spin_the_walk() {
        let (_dir, mut pager, mut tree) = fresh();
        for k in 0..200i64 {
            tree.insert(&mut pager, k, k as RowId).unwrap();
        }
        // Point the first leaf's next-link at itself: a cycle, not a chain.
        let leaf = leftmost_leaf(&pager, tree.root()).unwrap();
        let mut page = pager.read_page(leaf).unwrap();
        put(&mut page, OFF_NEXT, &leaf.to_le_bytes());
        pager.write_page(leaf, &page).unwrap();
        assert!(
            tree.iter(&pager)
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .is_err(),
            "the walk must give up on a cyclic leaf chain, not loop"
        );

        // And a link past the end of the file is rejected on decode.
        let mut page = pager.read_page(leaf).unwrap();
        put(&mut page, OFF_NEXT, &(pager.page_count() + 5).to_le_bytes());
        pager.write_page(leaf, &page).unwrap();
        assert!(matches!(
            read_node(&pager, leaf),
            Err(QuernError::Storage(_))
        ));
    }
}
