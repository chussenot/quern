//! bead: quern-wal — append-only REDO log, replay on open. See docs/quern.md §4.
//!
//! # ROLLBACK and crash recovery are the same mechanism
//!
//! This is the whole design, and it is why there is no UNDO code anywhere in
//! this file. A mutation is appended as it happens; a transaction becomes
//! durable only when [`Wal::commit`] appends a commit record and `fsync`s.
//! [`Wal::replay`] returns the mutations of transactions that have a commit
//! record and *nothing else*.
//!
//! So `ROLLBACK` writes no commit record — and that is all it does. A crash
//! mid-transaction writes no commit record either. Both leave exactly the same
//! bytes on disk, both are undone by the same line of `replay` (the
//! `committed.contains` filter below), and neither needs a compensating write.
//! `txn.rs` must not call anything here to roll back, because there is nothing
//! to call.
//!
//! # This file parses bytes that came off a disk after a crash
//! Every length is validated against the bytes actually present before it is
//! used to slice or size anything. Garbage, truncation and an impossible
//! length are all [`QuernError::Storage`]; nothing here panics (§1).
//!
//! A record is framed `[u32 len][body][u32 crc32]`, with the body laid out as
//! §4 pins it: `lsn: u64, txn_id: u64, kind: u8, table_len: u32, table,
//! payload`. The payload length is implied by `len`.
//!
//! **Why a checksum and not just the length prefix.** The length alone
//! distinguishes "the record is not all here" from "it is", but not "the bytes
//! that are here are the ones we wrote": a partial sector write can leave a
//! full-length record with a half-written body, and the payload is opaque
//! bytes with no structure to sanity-check it against. The crc is ~10 lines
//! and no dependency, so the cheap version was not worth arguing for.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::types::{QuernError, Result};

/// `kind` values for the three mutators of the `Storage` trait. The WAL itself
/// interprets exactly one value — [`KIND_COMMIT`] — and treats every other
/// `kind` as opaque metadata belonging to whoever wrote the payload, so
/// callers may define their own (DDL, say) without touching this file.
pub const KIND_INSERT: u8 = 1;
pub const KIND_UPDATE: u8 = 2;
pub const KIND_DELETE: u8 = 3;
/// The commit record. 255 so callers can extend upward from 4 freely.
pub const KIND_COMMIT: u8 = 255;

const MAGIC: &[u8; 8] = b"QWAL\0\0\x00\x01";
const HEADER: usize = MAGIC.len();

/// lsn + txn_id + kind + table_len. A body shorter than this cannot be one.
const MIN_BODY: usize = 8 + 8 + 1 + 4;

/// ponytail: a hard 1 MiB ceiling on one record, so a corrupt length can
/// never become a huge allocation. Rows live in 4 KiB pages; if some future
/// payload legitimately needs more, raise this rather than removing it.
const MAX_RECORD: usize = 1 << 20;

/// One log record, as it comes back out of [`Wal::replay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    pub lsn: u64,
    pub txn_id: u64,
    pub kind: u8,
    pub table: String,
    pub payload: Vec<u8>,
}

/// The append-only log. Owns its own file and its own `fsync` ordering: it
/// does not go through the pager's dirty-page map, so a record made durable
/// here is on disk whether or not `Pager::flush` has run.
pub struct Wal {
    file: File,
    path: PathBuf,
    next_lsn: u64,
}

impl Wal {
    /// Open (or create) the log, positioned to append.
    ///
    /// A torn final record — the process died mid-append — is *truncated away*
    /// here, not just skipped: leaving it would put every later append behind
    /// a byte range that `replay` stops at, silently losing committed work.
    pub fn open(path: &Path) -> Result<Wal> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| io_err(&format!("open {}", path.display()), e))?;

        let len = file.metadata().map_err(|e| io_err("stat wal", e))?.len();

        let mut next_lsn = 0;
        if len == 0 {
            file.write_all(MAGIC)
                .map_err(|e| io_err("write wal header", e))?;
        } else {
            let bytes = std::fs::read(path).map_err(|e| io_err("read wal", e))?;
            let (records, valid_end) = decode(&bytes)?;
            next_lsn = records.iter().map(|r| r.lsn + 1).max().unwrap_or(0);
            if valid_end < bytes.len() {
                file.set_len(valid_end as u64)
                    .map_err(|e| io_err("truncate torn wal tail", e))?;
                file.sync_all().map_err(|e| io_err("sync wal", e))?;
            }
        }
        file.seek(SeekFrom::End(0))
            .map_err(|e| io_err("seek wal", e))?;

        Ok(Wal {
            file,
            path: path.to_path_buf(),
            next_lsn,
        })
    }

    /// Append one mutation and return its LSN. Does **not** `fsync`: only
    /// [`Wal::commit`] does, which is the whole point of grouping a
    /// transaction's writes behind one commit record.
    pub fn append(&mut self, txn_id: u64, kind: u8, table: &str, payload: &[u8]) -> Result<u64> {
        let body_len = MIN_BODY + table.len() + payload.len();
        if body_len > MAX_RECORD {
            return Err(QuernError::Storage(format!(
                "wal record of {body_len} bytes exceeds the {MAX_RECORD}-byte limit"
            )));
        }
        let lsn = self.next_lsn;

        let mut body = Vec::with_capacity(body_len);
        body.extend_from_slice(&lsn.to_le_bytes());
        body.extend_from_slice(&txn_id.to_le_bytes());
        body.push(kind);
        body.extend_from_slice(&(table.len() as u32).to_le_bytes());
        body.extend_from_slice(table.as_bytes());
        body.extend_from_slice(payload);

        // One buffer, one write: a partial write can then only ever truncate
        // the tail, which is exactly the case `decode` discards.
        let mut framed = Vec::with_capacity(4 + body.len() + 4);
        framed.extend_from_slice(&(body.len() as u32).to_le_bytes());
        framed.extend_from_slice(&body);
        framed.extend_from_slice(&crc32(&body).to_le_bytes());

        self.file
            .write_all(&framed)
            .map_err(|e| io_err("append wal record", e))?;
        self.next_lsn += 1;
        Ok(lsn)
    }

    /// Append the commit record for `txn_id` and `fsync`. Until this returns,
    /// none of the transaction's records will ever be replayed; after it
    /// returns, all of them will be.
    pub fn commit(&mut self, txn_id: u64) -> Result<u64> {
        let lsn = self.append(txn_id, KIND_COMMIT, "", &[])?;
        self.file
            .sync_all()
            .map_err(|e| io_err("fsync wal on commit", e))?;
        Ok(lsn)
    }

    /// The mutations of committed transactions, in LSN order.
    ///
    /// Records of a transaction with no commit record are discarded — whether
    /// it was rolled back or the process was killed. Commit records themselves
    /// are not returned; they carry no mutation.
    pub fn replay(&self) -> Result<Vec<WalRecord>> {
        let bytes = std::fs::read(&self.path).map_err(|e| io_err("read wal", e))?;
        let (records, _) = decode(&bytes)?;
        let committed: HashSet<u64> = records
            .iter()
            .filter(|r| r.kind == KIND_COMMIT)
            .map(|r| r.txn_id)
            .collect();
        Ok(records
            .into_iter()
            .filter(|r| r.kind != KIND_COMMIT && committed.contains(&r.txn_id))
            .collect())
    }

    /// Discard the whole log. Call this once the replayed mutations are
    /// durable in the heap (i.e. after `Pager::flush`) — otherwise the log
    /// grows forever, and a `txn_id` counter that restarts at 1 each run would
    /// eventually see an old committed id collide with a live uncommitted one.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.file
            .set_len(HEADER as u64)
            .map_err(|e| io_err("truncate wal", e))?;
        self.file
            .seek(SeekFrom::End(0))
            .map_err(|e| io_err("seek wal", e))?;
        self.file
            .sync_all()
            .map_err(|e| io_err("fsync wal on checkpoint", e))?;
        Ok(())
    }

    /// The LSN the next [`Wal::append`] will use.
    pub fn next_lsn(&self) -> u64 {
        self.next_lsn
    }
}

/// Every record in `bytes`, plus the offset one past the last valid one.
///
/// `valid_end < bytes.len()` means a torn tail was found and skipped. A
/// *garbage* file is a different thing and is an `Err`: no magic, an
/// impossible length, or a checksum mismatch with more records behind it —
/// none of which a crash mid-append can produce.
fn decode(bytes: &[u8]) -> Result<(Vec<WalRecord>, usize)> {
    if bytes.is_empty() {
        return Ok((Vec::new(), 0));
    }
    if bytes.len() < HEADER || &bytes[..HEADER] != MAGIC {
        return Err(QuernError::Storage(
            "not a quern wal: bad magic".to_string(),
        ));
    }

    let mut out = Vec::new();
    let mut cursor = HEADER;
    while cursor < bytes.len() {
        // A partial length prefix is a torn append, nothing more.
        let Some(len) = u32_at(bytes, cursor) else {
            break;
        };
        let len = len as usize;
        if !(MIN_BODY..=MAX_RECORD).contains(&len) {
            return Err(QuernError::Storage(format!(
                "wal: impossible record length {len} at offset {cursor}"
            )));
        }
        let end = cursor + 4 + len + 4;
        if end > bytes.len() {
            break; // body or checksum did not all make it to disk
        }
        let body = &bytes[cursor + 4..cursor + 4 + len];
        let Some(crc) = u32_at(bytes, cursor + 4 + len) else {
            break;
        };
        if crc != crc32(body) {
            if end == bytes.len() {
                break; // last record, half-written: same case as above
            }
            return Err(QuernError::Storage(format!(
                "wal: checksum mismatch at offset {cursor}, log is corrupt"
            )));
        }
        out.push(decode_body(body, cursor)?);
        cursor = end;
    }
    Ok((out, cursor))
}

fn decode_body(body: &[u8], offset: usize) -> Result<WalRecord> {
    // `decode` already checked body.len() >= MIN_BODY, so only table_len can
    // still be a lie.
    let (Some(lsn), Some(txn_id), Some(kind), Some(table_len)) = (
        u64_at(body, 0),
        u64_at(body, 8),
        body.get(16).copied(),
        u32_at(body, 17),
    ) else {
        return Err(QuernError::Storage(format!(
            "wal: truncated record header at offset {offset}"
        )));
    };
    let table_end = MIN_BODY + table_len as usize;
    if table_end > body.len() {
        return Err(QuernError::Storage(format!(
            "wal: record at offset {offset} claims a {table_len}-byte table name \
             but only has {} bytes left",
            body.len() - MIN_BODY
        )));
    }
    let table = String::from_utf8(body[MIN_BODY..table_end].to_vec()).map_err(|_| {
        QuernError::Storage(format!(
            "wal: record at offset {offset} has a non-UTF-8 table name"
        ))
    })?;
    Ok(WalRecord {
        lsn,
        txn_id,
        kind,
        table,
        payload: body[table_end..].to_vec(),
    })
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], i: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(i..i + 8)?.try_into().ok()?))
}

/// CRC-32 (IEEE, the zlib polynomial), bitwise. No table, no dependency; a
/// WAL record is at most a few KiB and this is not the bottleneck.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn io_err(what: &str, e: std::io::Error) -> QuernError {
    QuernError::Storage(format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn wal_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("quern.wal")
    }

    #[test]
    fn committed_records_replay_in_lsn_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        let mut wal = Wal::open(&path).unwrap();
        wal.append(1, KIND_INSERT, "t", b"one").unwrap();
        wal.append(1, KIND_UPDATE, "t", b"two").unwrap();
        wal.commit(1).unwrap();

        let got = wal.replay().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].payload, b"one");
        assert_eq!(got[0].kind, KIND_INSERT);
        assert_eq!(got[0].table, "t");
        assert_eq!(got[1].payload, b"two");
        assert!(got[0].lsn < got[1].lsn);

        // and across a reopen — a WAL that only works in-process is no WAL
        let reopened = Wal::open(&path).unwrap();
        assert_eq!(reopened.replay().unwrap(), got);
        assert_eq!(reopened.next_lsn(), 3); // two mutations + the commit record
    }

    #[test]
    fn uncommitted_records_are_never_replayed() {
        // This is ROLLBACK. It is also `kill -9`. Same bytes, same outcome.
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(&wal_path(&dir)).unwrap();
        wal.append(7, KIND_INSERT, "t", b"gone").unwrap();
        wal.append(7, KIND_DELETE, "t", b"gone too").unwrap();
        assert!(wal.replay().unwrap().is_empty());
    }

    #[test]
    fn interleaved_transactions_replay_only_the_committed_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(&wal_path(&dir)).unwrap();
        wal.append(1, KIND_INSERT, "t", b"keep-a").unwrap();
        wal.append(2, KIND_INSERT, "t", b"drop-a").unwrap();
        wal.append(1, KIND_INSERT, "t", b"keep-b").unwrap();
        wal.commit(1).unwrap();
        wal.append(2, KIND_INSERT, "t", b"drop-b").unwrap();

        let payloads: Vec<_> = wal
            .replay()
            .unwrap()
            .into_iter()
            .map(|r| r.payload)
            .collect();
        assert_eq!(payloads, vec![b"keep-a".to_vec(), b"keep-b".to_vec()]);
    }

    #[test]
    fn a_torn_final_record_is_discarded_and_the_rest_still_replays() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        let mut wal = Wal::open(&path).unwrap();
        wal.append(1, KIND_INSERT, "t", b"durable").unwrap();
        wal.commit(1).unwrap();
        let good_len = std::fs::metadata(&path).unwrap().len();
        wal.append(2, KIND_INSERT, "t", b"half-written").unwrap();
        drop(wal);

        // chop 5 bytes off the last record: a mid-append crash
        let torn = std::fs::metadata(&path).unwrap().len() - 5;
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(torn).unwrap();
        drop(f);

        let mut wal = Wal::open(&path).unwrap();
        let got = wal.replay().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, b"durable");
        // open() cut the torn tail off, so the next append is not stranded
        // behind bytes replay refuses to walk past.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
        wal.append(3, KIND_INSERT, "t", b"after").unwrap();
        wal.commit(3).unwrap();
        assert_eq!(wal.replay().unwrap().len(), 2);
    }

    #[test]
    fn a_garbage_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        std::fs::write(&path, b"this is not a wal, it is a haiku about one").unwrap();
        assert!(matches!(Wal::open(&path), Err(QuernError::Storage(_))));

        // valid magic, then nonsense: an impossible length, still an Err
        let path2 = dir.path().join("bad-len.wal");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes()); // < MIN_BODY
        bytes.extend_from_slice(b"abc\0\0\0\0");
        std::fs::write(&path2, &bytes).unwrap();
        assert!(matches!(Wal::open(&path2), Err(QuernError::Storage(_))));

        // a flipped byte inside a record that has another behind it
        let path3 = dir.path().join("flipped.wal");
        let mut wal = Wal::open(&path3).unwrap();
        wal.append(1, KIND_INSERT, "t", b"aaaa").unwrap();
        wal.commit(1).unwrap();
        drop(wal);
        let mut raw = Vec::new();
        File::open(&path3).unwrap().read_to_end(&mut raw).unwrap();
        raw[HEADER + 4] ^= 0xff;
        std::fs::write(&path3, &raw).unwrap();
        assert!(matches!(Wal::open(&path3), Err(QuernError::Storage(_))));
    }

    #[test]
    fn an_empty_file_replays_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        std::fs::write(&path, b"").unwrap();
        let wal = Wal::open(&path).unwrap();
        assert!(wal.replay().unwrap().is_empty());
        assert_eq!(wal.next_lsn(), 0);
    }

    #[test]
    fn checkpoint_empties_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_path(&dir);
        let mut wal = Wal::open(&path).unwrap();
        wal.append(1, KIND_INSERT, "t", b"applied").unwrap();
        wal.commit(1).unwrap();
        wal.checkpoint().unwrap();
        assert!(wal.replay().unwrap().is_empty());
        // still a usable log afterwards
        wal.append(2, KIND_INSERT, "t", b"next").unwrap();
        wal.commit(2).unwrap();
        assert_eq!(wal.replay().unwrap().len(), 1);
        assert_eq!(Wal::open(&path).unwrap().replay().unwrap().len(), 1);
    }
}
