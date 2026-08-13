//! bead: quern-txn — BEGIN/COMMIT/ROLLBACK, single writer. See docs/quern.md §4.
//!
//! # The two things in here that are correctness, not bookkeeping
//!
//! **1. WAL before pager, and it is not a comment this time.** The REDO
//! promise is "if the commit record is durable, the mutations will be
//! re-applied"; it is only true if the commit record reaches disk *before* the
//! data pages. `wal.rs` says so in prose, and prose does not run — so the
//! ordering lives in exactly one place that does: [`TxnHost::commit`], which
//! calls `wal().commit(id)` and then `flush()`, in that order, and is a
//! provided method so no `Storage` implementation writes the sequence itself
//! and gets it backwards.
//!
//! **2. `txn_id`s are only unique within a run, so recovery must checkpoint.**
//! The counter here restarts at 1 in every process. An old *committed* txn 1
//! left in the log and a live *uncommitted* txn 1 are then indistinguishable,
//! and `replay` would resurrect the live one's records — worse, if the live
//! txn 1 ever commits, an old *uncommitted* txn 1's records become "committed"
//! and get resurrected too. So [`TxnHost::recover`] ends with
//! `wal().checkpoint()`: an empty log cannot collide with anything.
//!
//! Why checkpointing and not a persisted counter: the collision with an old
//! *uncommitted* id cannot be closed by seeding the counter from `replay()`,
//! because `replay()` deliberately never shows uncommitted ids. Truncating the
//! log closes both halves at once, needs no second file to keep in sync, and
//! `wal.rs` wants it anyway so the log does not grow forever. Re-applying an
//! insert also allocates a *fresh* `RowId`, so replay is not idempotent and
//! must not happen twice — same conclusion, second reason.
//!
//! # ROLLBACK writes nothing, and that is the whole of it
//!
//! [`TxnHost::rollback`] never touches the WAL: the absence of a commit record
//! *is* the rollback, identically to a `kill -9`, so there is no UNDO here and
//! no second mechanism that could disagree with `replay`. What it must do is
//! throw away in-memory work, via [`TxnHost::discard`] — see that method for
//! why a blunt discard is the correct one.

use crate::storage::wal::{Wal, WalRecord};
use crate::types::{QuernError, Result};

/// Transaction identifier. Unique within one process run only — see the module
/// docs on why that forces [`TxnHost::recover`] to checkpoint.
pub type TxnId = u64;

/// The single-writer transaction state of §4: at most one open transaction,
/// ever. Held as a field by the storage layer, which exposes it through
/// [`TxnHost::txn`].
#[derive(Debug)]
pub struct TxnState {
    current: Option<TxnId>,
    next_id: TxnId,
}

impl Default for TxnState {
    fn default() -> Self {
        // From 1, so 0 is never a live txn id.
        TxnState {
            current: None,
            next_id: 1,
        }
    }
}

impl TxnState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The open transaction, if any.
    pub fn current(&self) -> Option<TxnId> {
        self.current
    }

    pub fn is_open(&self) -> bool {
        self.current.is_some()
    }

    /// `BEGIN`. Errors if a transaction is already open — quern has one writer
    /// and no nesting, so this is never a no-op that "joins" the outer txn.
    pub fn begin(&mut self) -> Result<TxnId> {
        if let Some(open) = self.current {
            return Err(QuernError::Txn(format!(
                "BEGIN inside a transaction: txn {open} is still open"
            )));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.current = Some(id);
        Ok(id)
    }

    /// End the open transaction and return its id. `what` names the statement
    /// so the error reads like the SQL that caused it.
    pub fn end(&mut self, what: &str) -> Result<TxnId> {
        self.current
            .take()
            .ok_or_else(|| QuernError::Txn(format!("{what} without an open transaction")))
    }
}

/// What a transaction needs from the storage layer, so that the commit
/// ordering and the recovery-checkpoint rule can live here rather than in
/// every call site.
///
/// The four required methods are accessors; the rest are provided and are the
/// point of the trait. `txn.rs` sits below `storage/` and cannot name the
/// concrete storage type, which is the only reason this is a trait at all.
///
/// A `Storage` implementation wires its frozen §3 methods straight through:
/// `Storage::begin` → [`TxnHost::begin`], `Storage::commit` →
/// [`TxnHost::commit`], `Storage::rollback` → [`TxnHost::rollback`], and every
/// statement runs inside [`TxnHost::statement`].
pub trait TxnHost {
    fn txn(&mut self) -> &mut TxnState;

    fn wal(&mut self) -> &mut Wal;

    /// Make the data pages durable — `Pager::flush`, which `fsync`s.
    fn flush(&mut self) -> Result<()>;

    /// Throw away every in-memory mutation that has not been flushed.
    ///
    /// This is the one way this design can silently leak uncommitted work, so
    /// it is a required method: `Pager` buffers writes in a dirty-page map and
    /// `read_page` *serves from it*, so a rolled-back statement's pages stay
    /// visible and would be written out by the next unrelated `flush()`.
    ///
    /// A blunt discard is correct and is what to implement: because
    /// [`TxnHost::commit`] always flushes, nothing dirty at rollback time
    /// belongs to a committed transaction. Reopening the `Pager` is the
    /// cheapest honest implementation — it drops the dirty map *and* the
    /// in-memory `page_count` that `allocate_page` bumped, which dropping the
    /// map alone would leave behind. Any heap or B-tree cache above the pager
    /// must be dropped here too.
    fn discard(&mut self) -> Result<()>;

    /// `BEGIN`.
    fn begin(&mut self) -> Result<TxnId> {
        self.txn().begin()
    }

    /// `COMMIT`. **The ordering below is the durability contract**: the WAL is
    /// made durable first (`wal.commit` appends the commit record and
    /// `fsync`s), and only then the data pages. Reversed, a crash between the
    /// two leaves flushed pages that no commit record vouches for, and
    /// recovery cannot tell them from a rolled-back transaction's. Nothing but
    /// this method enforces it, which is why no caller writes the sequence.
    fn commit(&mut self) -> Result<()> {
        let id = self.txn().end("COMMIT")?;
        self.wal().commit(id)?; // 1. REDO log durable
        self.flush() // 2. then the pages
    }

    /// `ROLLBACK`. Writes nothing to the WAL — deliberately; see the module
    /// docs. All it does is drop the in-memory work.
    fn rollback(&mut self) -> Result<()> {
        self.txn().end("ROLLBACK")?;
        self.discard()
    }

    /// Run one statement, in the open transaction if there is one, otherwise in
    /// an implicit transaction that commits on success and rolls back on
    /// failure (§4). `body` gets the `TxnId` to stamp its WAL records with.
    ///
    /// A failure inside an *explicit* transaction leaves that transaction open,
    /// as SQL expects: the client decides between `COMMIT` and `ROLLBACK`.
    /// quern has no statement-level savepoints, so the failed statement's
    /// partial work stays in the transaction until it ends either way.
    fn statement<T>(&mut self, body: impl FnOnce(&mut Self, TxnId) -> Result<T>) -> Result<T>
    where
        Self: Sized,
    {
        let mut implicit = false;
        let id = match self.txn().current() {
            Some(open) => open, // an explicit transaction: the statement joins it
            None => {
                implicit = true;
                self.txn().begin()?
            }
        };
        match body(self, id) {
            Ok(value) => {
                if implicit {
                    self.commit()?;
                }
                Ok(value)
            }
            Err(e) => {
                if implicit {
                    // The rollback error wins if there is one: a discard that
                    // failed means uncommitted pages may still reach disk,
                    // which is strictly worse news than the statement error.
                    self.rollback()?;
                }
                Err(e)
            }
        }
    }

    /// Crash recovery: re-apply every committed mutation, make it durable,
    /// then checkpoint. Call this once, right after opening the database.
    ///
    /// Returns the number of records re-applied. The `checkpoint` at the end is
    /// not tidiness — it is what keeps this process's fresh `txn_id` counter
    /// from colliding with an id still in the log. See the module docs.
    fn recover(
        &mut self,
        mut apply: impl FnMut(&mut Self, &WalRecord) -> Result<()>,
    ) -> Result<usize>
    where
        Self: Sized,
    {
        let records = self.wal().replay()?;
        for record in &records {
            apply(self, record)?;
        }
        self.flush()?; // durable in the heap before the log goes away
        self.wal().checkpoint()?;
        Ok(records.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::wal::KIND_INSERT;
    use std::path::Path;

    /// A minimal `TxnHost`: a real `Wal` (the ordering and checkpoint rules are
    /// about real bytes), a fake heap that is just a list of payloads.
    struct Host {
        txn: TxnState,
        wal: Wal,
        heap: Vec<Vec<u8>>,
        flushes: usize,
        discards: usize,
        /// Set by `flush()`: was the commit record already in the log when the
        /// pages were flushed? This is the ordering assertion.
        committed_before_flush: Option<bool>,
    }

    impl Host {
        fn open(path: &Path) -> Host {
            Host {
                txn: TxnState::new(),
                wal: Wal::open(path).unwrap(),
                heap: Vec::new(),
                flushes: 0,
                discards: 0,
                committed_before_flush: None,
            }
        }
    }

    impl TxnHost for Host {
        fn txn(&mut self) -> &mut TxnState {
            &mut self.txn
        }
        fn wal(&mut self) -> &mut Wal {
            &mut self.wal
        }
        fn flush(&mut self) -> Result<()> {
            self.flushes += 1;
            self.committed_before_flush = Some(!self.wal.replay().unwrap().is_empty());
            Ok(())
        }
        fn discard(&mut self) -> Result<()> {
            self.discards += 1;
            self.heap.clear();
            Ok(())
        }
    }

    fn wal_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("quern.wal")
    }

    /// Payloads a fresh reader of the same log would re-apply.
    fn replayable(path: &Path) -> Vec<Vec<u8>> {
        Wal::open(path)
            .unwrap()
            .replay()
            .unwrap()
            .into_iter()
            .map(|r| r.payload)
            .collect()
    }

    #[test]
    fn begin_then_commit_makes_the_work_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        let mut h = Host::open(&path);

        let id = h.begin().unwrap();
        assert_eq!(id, 1);
        assert_eq!(h.txn().current(), Some(1));
        h.wal().append(id, KIND_INSERT, "t", b"row").unwrap();
        h.commit().unwrap();

        assert_eq!(h.txn().current(), None);
        assert_eq!(h.flushes, 1);
        assert_eq!(replayable(&path), vec![b"row".to_vec()]);
        // The rule from wal.rs: the commit record was durable BEFORE the pages.
        assert_eq!(h.committed_before_flush, Some(true));
    }

    #[test]
    fn begin_inside_a_transaction_is_a_txn_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = Host::open(&wal_path(&dir));
        h.begin().unwrap();
        assert!(matches!(h.begin(), Err(QuernError::Txn(_))));
        assert_eq!(h.txn().current(), Some(1)); // and the open txn survives
    }

    #[test]
    fn commit_or_rollback_without_a_transaction_is_a_txn_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = Host::open(&wal_path(&dir));
        assert!(matches!(h.commit(), Err(QuernError::Txn(_))));
        assert!(matches!(h.rollback(), Err(QuernError::Txn(_))));
        assert_eq!(h.flushes, 0);
        assert_eq!(h.discards, 0);
    }

    #[test]
    fn an_implicit_transaction_commits_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        let mut h = Host::open(&path);

        let n = h
            .statement(|h, id| {
                h.wal().append(id, KIND_INSERT, "t", b"implicit")?;
                h.heap.push(b"implicit".to_vec());
                Ok(1)
            })
            .unwrap();

        assert_eq!(n, 1);
        assert_eq!(h.txn().current(), None); // committed, not left open
        assert_eq!(replayable(&path), vec![b"implicit".to_vec()]);
    }

    #[test]
    fn an_implicit_transaction_does_not_commit_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        let mut h = Host::open(&path);

        let r: Result<()> = h.statement(|h, id| {
            h.wal().append(id, KIND_INSERT, "t", b"doomed")?;
            h.heap.push(b"doomed".to_vec());
            Err(QuernError::Type("no".into()))
        });

        assert!(matches!(r, Err(QuernError::Type(_))));
        assert_eq!(h.txn().current(), None);
        assert_eq!(h.discards, 1); // the dirty work was thrown away...
        assert!(h.heap.is_empty()); // ...not flushed
        assert_eq!(h.flushes, 0);
        assert!(replayable(&path).is_empty()); // no commit record, so nothing replays
    }

    #[test]
    fn a_statement_inside_an_explicit_transaction_leaves_it_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        let mut h = Host::open(&path);
        h.begin().unwrap();

        h.statement(|h, id| {
            assert_eq!(id, 1); // the open txn's id, not a new one
            h.wal().append(id, KIND_INSERT, "t", b"a")
        })
        .unwrap();
        assert_eq!(h.txn().current(), Some(1));
        assert_eq!(h.flushes, 0); // nothing committed yet
        assert!(replayable(&path).is_empty());

        h.commit().unwrap();
        assert_eq!(replayable(&path), vec![b"a".to_vec()]);
    }

    #[test]
    fn rollback_leaves_no_commit_record_in_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        let mut h = Host::open(&path);

        let id = h.begin().unwrap();
        h.wal().append(id, KIND_INSERT, "t", b"gone").unwrap();
        h.heap.push(b"gone".to_vec());
        h.rollback().unwrap();

        assert_eq!(h.txn().current(), None);
        assert!(replayable(&path).is_empty()); // asserted via replay, per the bead
        assert_eq!(h.discards, 1);
        assert!(h.heap.is_empty());
        assert_eq!(h.flushes, 0); // rollback must never flush
    }

    #[test]
    fn recovery_checkpoints_so_a_reused_txn_id_cannot_resurrect_live_work() {
        // The hazard wal.rs warned about, both halves.
        let dir = tempfile::tempdir().unwrap();

        // --- half one: no checkpoint, and the live transaction is resurrected.
        let hazard = dir.path().join("hazard.wal");
        {
            // Run 1 commits txn 1 and dies before the pages are durable, so
            // the commit record for txn 1 is still in the log.
            let mut h = Host::open(&hazard);
            let id = h.begin().unwrap();
            h.wal().append(id, KIND_INSERT, "t", b"old").unwrap();
            h.wal().commit(id).unwrap(); // committed; no flush — the crash
        }
        let mut h = Host::open(&hazard); // run 2, counter back at 1
        let id = h.begin().unwrap();
        assert_eq!(id, 1); // same id as the committed txn in the log
        h.wal().append(id, KIND_INSERT, "t", b"live").unwrap();
        // Nothing was committed in this run, yet:
        assert_eq!(
            replayable(&hazard),
            vec![b"old".to_vec(), b"live".to_vec()],
            "without a checkpoint the live txn borrows the old txn's commit record"
        );

        // --- half two: recover() first, and it cannot happen.
        let fixed = dir.path().join("fixed.wal");
        {
            let mut h = Host::open(&fixed);
            let id = h.begin().unwrap();
            h.wal().append(id, KIND_INSERT, "t", b"old").unwrap();
            h.wal().commit(id).unwrap();
        }
        let mut h = Host::open(&fixed);
        let applied = h
            .recover(|h, rec| {
                h.heap.push(rec.payload.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(h.heap, vec![b"old".to_vec()]);
        assert!(replayable(&fixed).is_empty()); // log truncated by checkpoint

        let id = h.begin().unwrap();
        assert_eq!(id, 1); // still a fresh counter — that is fine now
        h.wal().append(id, KIND_INSERT, "t", b"live").unwrap();
        assert!(
            replayable(&fixed).is_empty(),
            "an empty log has no stale commit record for the live txn to borrow"
        );
        // and re-applying twice is impossible: the second recover sees nothing
        assert_eq!(h.recover(|_, _| Ok(())).unwrap(), 0);
    }
}
