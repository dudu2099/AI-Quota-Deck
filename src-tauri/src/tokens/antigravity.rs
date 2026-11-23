//! Antigravity keeps each conversation as a SQLite database under
//! `~/.gemini/<product>/conversations/*.db`: the IDE in `antigravity`, the
//! `agy` command line in `antigravity-cli`, and T3's integration in
//! `antigravity-acp` (often a link into T3's own data folder).
//!
//! Every model call is one row of the `steps` table. Its `metadata` column is
//! a protobuf `CortexStepMetadata`; the two fields read here are
//!
//! ```text
//! 1  created_at   google.protobuf.Timestamp { 1 seconds, 2 nanos }
//! 9  model_usage  { 2 input_tokens, 3 output_tokens, 5 cache_read_tokens,
//!                   7 message_id, ... }
//! ```
//!
//! There is no published schema, so the message is walked by hand rather than
//! generated: only wire types and field numbers are needed, and an unknown
//! field must be skipped, not rejected.

use super::{Tally, Window};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const SOURCES: [(&str, &str); 3] = [
    ("IDE", "antigravity"),
    ("agy", "antigravity-cli"),
    ("T3", "antigravity-acp"),
];

const CREATED_AT: u64 = 1;
const MODEL_USAGE: u64 = 9;
const TIMESTAMP_SECONDS: u64 = 1;
const INPUT_TOKENS: u64 = 2;
const OUTPUT_TOKENS: u64 = 3;
const CACHE_READ_TOKENS: u64 = 5;

#[derive(Default)]
pub struct Scanner {
    databases: HashMap<PathBuf, Database>,
}

/// What one conversation database held the last time it was read.
struct Database {
    stamp: Stamp,
    /// Calls made on or after the day start in force when it was read.
    steps: Vec<Step>,
}

/// Size and modified time of the database and its write-ahead log. A new
/// step lands in the log first and moves into the database at checkpoint, so
/// between them the two catch every write. The `-shm` index changes on every
/// read and is deliberately left out.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    database: (u64, Option<SystemTime>),
    log: Option<(u64, Option<SystemTime>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub created: i64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
}

impl Step {
    /// `input_tokens` excludes the cached part here, unlike OpenAI's, so the
    /// three categories add up to the total.
    fn tally(&self) -> Tally {
        let fresh = self.input + self.output;
        Tally {
            total: fresh + self.cache_read,
            fresh,
            cache_read: self.cache_read,
            requests: 1,
        }
    }
}

impl Scanner {
    /// A database that changes is read again in full, so the steps worth
    /// keeping are only those from the new day on.
    pub fn new_day(&mut self, day_start: i64) {
        for database in self.databases.values_mut() {
            database.steps.retain(|step| step.created >= day_start);
        }
    }

    pub fn scan(
        &mut self,
        home: &Path,
        window: Window,
    ) -> Result<Vec<(&'static str, Tally)>, String> {
        let gemini = home.join(".gemini");
        let mut sources = Vec::new();
        let mut live = HashSet::new();
        for (label, folder) in SOURCES {
            let dir = gemini.join(folder).join("conversations");
            if !dir.is_dir() {
                continue;
            }
            let mut tally = Tally::default();
            for path in database_files(&dir) {
                let Some(stamp) = stamp(&path) else {
                    continue;
                };
                let unchanged = self
                    .databases
                    .get(&path)
                    .is_some_and(|database| database.stamp == stamp);
                if !unchanged {
                    // On failure (the IDE holding a lock through a checkpoint,
                    // a half-copied file) keep the previous reading. Its stamp
                    // is the old one, so the next call tries again.
                    if let Ok(steps) = read_steps(&path, window.start) {
                        self.databases.insert(path.clone(), Database { stamp, steps });
                    }
                }
                if let Some(database) = self.databases.get(&path) {
                    for step in &database.steps {
                        if window.contains(step.created) {
                            tally += step.tally();
                        }
                    }
                }
                live.insert(path);
            }
            sources.push((label, tally));
        }
        self.databases.retain(|path, _| live.contains(path));
        if sources.is_empty() {
            return Err("没找到 ~/.gemini/antigravity 的对话记录".to_string());
        }
        Ok(sources)
    }
}

fn database_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "db") && path.is_file())
        .collect();
    files.sort();
    files
}

fn stamp(path: &Path) -> Option<Stamp> {
    let of = |metadata: std::fs::Metadata| (metadata.len(), metadata.modified().ok());
    let mut log = OsString::from(path.as_os_str());
    log.push("-wal");
    Some(Stamp {
        database: of(std::fs::metadata(path).ok()?),
        log: std::fs::metadata(PathBuf::from(log)).ok().map(of),
    })
}

/// Every call made at or after `since`.
///
/// Opened read-only but not `immutable`: the IDE may be writing, and a
/// reader that ignored the write-ahead log would miss the newest steps.
fn read_steps(path: &Path, since: i64) -> rusqlite::Result<Vec<Step>> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // Readers never wait on a WAL writer, but a checkpoint can hold the lock
    // for a moment. Wait briefly, then give up until the next call.
    connection.busy_timeout(Duration::from_millis(500))?;
    let mut statement = connection.prepare("SELECT metadata FROM steps")?;
    let mut rows = statement.query([])?;
    let mut steps = Vec::new();
    while let Some(row) = rows.next()? {
        let metadata = match row.get_ref(0)? {
            ValueRef::Blob(bytes) | ValueRef::Text(bytes) => bytes,
            _ => continue,
        };
        if let Some(step) = decode_step(metadata) {
            if step.created >= since {
                steps.push(step);
            }
        }
    }
    Ok(steps)
}

/// The usage of one step, or `None` for a step that called no model or whose
/// metadata does not parse.
pub fn decode_step(metadata: &[u8]) -> Option<Step> {
    let top = fields(metadata)?;
    let usage = fields(first_bytes(&top, MODEL_USAGE)?)?;
    let created = fields(first_bytes(&top, CREATED_AT)?)?;
    let seconds = i64::try_from(first_varint(&created, TIMESTAMP_SECONDS)?).ok()?;
    if seconds == 0 {
        return None;
    }
    Some(Step {
        created: seconds,
        input: first_varint(&usage, INPUT_TOKENS).unwrap_or(0),
        output: first_varint(&usage, OUTPUT_TOKENS).unwrap_or(0),
        cache_read: first_varint(&usage, CACHE_READ_TOKENS).unwrap_or(0),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    /// Fixed 32- and 64-bit values; nothing here reads them.
    Fixed,
}

/// Every top-level field of a protobuf message, in order, or `None` if the
/// bytes are not a well-formed message.
fn fields(bytes: &[u8]) -> Option<Vec<(u64, Wire<'_>)>> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let key = varint(bytes, &mut pos)?;
        let (number, wire) = (key >> 3, key & 7);
        if number == 0 {
            return None;
        }
        let value = match wire {
            0 => Wire::Varint(varint(bytes, &mut pos)?),
            1 | 5 => {
                let width = if wire == 1 { 8 } else { 4 };
                pos = pos.checked_add(width).filter(|&end| end <= bytes.len())?;
                Wire::Fixed
            }
            2 => {
                let length = usize::try_from(varint(bytes, &mut pos)?).ok()?;
                let end = pos.checked_add(length).filter(|&end| end <= bytes.len())?;
                let value = &bytes[pos..end];
                pos = end;
                Wire::Bytes(value)
            }
            // Groups (3, 4) are long deprecated and 6, 7 do not exist.
            _ => return None,
        };
        fields.push((number, value));
    }
    Some(fields)
}

fn varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// Protobuf lets a field repeat; for the scalars read here the first copy is
/// the one that counts.
fn first_varint(fields: &[(u64, Wire<'_>)], number: u64) -> Option<u64> {
    match fields.iter().find(|(n, _)| *n == number)?.1 {
        Wire::Varint(value) => Some(value),
        _ => None,
    }
}

fn first_bytes<'a>(fields: &[(u64, Wire<'a>)], number: u64) -> Option<&'a [u8]> {
    match fields.iter().find(|(n, _)| *n == number)?.1 {
        Wire::Bytes(value) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::test_support::{scratch_dir, unix};

    fn put_varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn uint(number: u64, value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        put_varint(number << 3, &mut out);
        put_varint(value, &mut out);
        out
    }

    fn bytes(number: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_varint(number << 3 | 2, &mut out);
        put_varint(payload.len() as u64, &mut out);
        out.extend_from_slice(payload);
        out
    }

    fn fixed(number: u64, wire: u64, width: usize) -> Vec<u8> {
        let mut out = Vec::new();
        put_varint(number << 3 | wire, &mut out);
        out.extend(std::iter::repeat_n(0xAB, width));
        out
    }

    fn metadata(created: i64, input: u64, output: u64, cache_read: u64) -> Vec<u8> {
        let timestamp = [uint(1, created as u64), uint(2, 123_456_789)].concat();
        let usage = [
            uint(1, 1026),
            uint(2, input),
            uint(3, output),
            uint(5, cache_read),
            bytes(7, b"bot-0e6f-message"),
            uint(9, 1),
        ]
        .concat();
        [
            uint(2, 15),
            bytes(1, &timestamp),
            fixed(4, 1, 8),
            fixed(5, 5, 4),
            bytes(9, &usage),
            bytes(20, b"free text that is not ours to read"),
        ]
        .concat()
    }

    #[test]
    fn decodes_a_hand_built_step_metadata_blob() {
        let created = unix("2026-11-23T01:02:03Z");
        let blob = metadata(created, 30_000, 1_500, 250_000);
        assert_eq!(
            decode_step(&blob),
            Some(Step {
                created,
                input: 30_000,
                output: 1_500,
                cache_read: 250_000
            })
        );
        assert_eq!(
            decode_step(&blob).unwrap().tally(),
            Tally {
                total: 281_500,
                fresh: 31_500,
                cache_read: 250_000,
                requests: 1
            }
        );
    }

    #[test]
    fn rejects_steps_without_usage_and_malformed_blobs() {
        let created = unix("2026-11-23T01:02:03Z");
        let blob = metadata(created, 1, 2, 3);
        // No model call: a step without field 9.
        let timestamp = [uint(1, created as u64)].concat();
        assert_eq!(decode_step(&bytes(1, &timestamp)), None);
        // Cut off mid-field.
        assert_eq!(decode_step(&blob[..blob.len() - 3]), None);
        // Field number zero is never valid.
        assert_eq!(decode_step(&[0x00, 0x01]), None);
        // A deprecated group wire type.
        assert_eq!(decode_step(&[0x0b]), None);
    }

    #[test]
    fn reads_steps_from_sqlite_and_rereads_only_changed_databases() {
        let home = scratch_dir("antigravity");
        let dir = home.join(".gemini/antigravity/conversations");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c1.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE steps (idx INTEGER, step_type INTEGER, metadata BLOB);",
            )
            .unwrap();
        let insert = |idx: i64, blob: Option<Vec<u8>>| {
            connection
                .execute("INSERT INTO steps VALUES (?1, 1, ?2)", (idx, blob))
                .unwrap();
        };
        insert(0, Some(metadata(unix("2026-11-22T15:59:59Z"), 9, 9, 9)));
        insert(1, Some(metadata(unix("2026-11-22T16:00:00Z"), 10, 2, 100)));
        insert(2, None);
        insert(3, Some(metadata(unix("2026-11-23T08:00:00Z"), 5, 1, 0)));

        let window = Window {
            start: unix("2026-11-22T16:00:00Z"),
            end: unix("2026-11-23T16:00:00Z"),
        };
        let mut scanner = Scanner::default();
        let sources = scanner.scan(&home, window).unwrap();
        let expected = Tally {
            total: 118,
            fresh: 18,
            cache_read: 100,
            requests: 2,
        };
        assert_eq!(sources, vec![("IDE", expected)]);

        // Unchanged files are served from memory: poison the cached copy and
        // the answer must not change.
        scanner.databases.get_mut(&path).unwrap().steps.clear();
        assert_eq!(scanner.scan(&home, window).unwrap()[0].1, Tally::default());

        // A new step changes the log, so the database is read again.
        insert(4, Some(metadata(unix("2026-11-23T09:00:00Z"), 1, 1, 1)));
        let sources = scanner.scan(&home, window).unwrap();
        assert_eq!(sources[0].1.requests, 3);
        assert_eq!(sources[0].1.total, 121);
        drop(connection);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_conversations_is_a_reason_not_a_zero() {
        let home = scratch_dir("antigravity-none");
        let window = Window { start: 0, end: 1 };
        assert!(Scanner::default().scan(&home, window).is_err());
        std::fs::remove_dir_all(home).unwrap();
    }
}
