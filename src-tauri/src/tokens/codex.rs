//! Codex writes one rollout per thread under
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` and moves finished ones to
//! `~/.codex/archived_sessions/`. After every model response it appends a
//! `token_usage_record` with that response's usage — the per-call figure.
//!
//! The `event_msg` / `token_count` lines carry running thread totals that
//! repeat and reset; they are not used.

use super::{jsonl_files, read_complete_lines, Tally, Window};
use crate::quota::rfc3339_to_unix;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Scanner {
    /// Bytes already consumed, per rollout.
    offsets: HashMap<PathBuf, u64>,
    /// Response ids counted today.
    seen: HashSet<String>,
    today: Counts,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    total: u64,
    cached: u64,
    requests: u64,
}

impl Counts {
    /// OpenAI's `input_tokens` already includes the cached part, and
    /// `total_tokens` is input plus output, so the total is taken as reported
    /// and the cached part is carved out of it.
    fn tally(&self) -> Tally {
        Tally {
            total: self.total,
            fresh: self.total.saturating_sub(self.cached),
            cache_read: self.cached,
            requests: self.requests,
        }
    }
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    response_id: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

impl Scanner {
    pub fn new_day(&mut self) {
        self.seen.clear();
        self.today = Counts::default();
    }

    pub fn scan(&mut self, home: &Path, window: Window) -> Result<Tally, String> {
        let codex = home.join(".codex");
        let sessions = codex.join("sessions");
        let archived = codex.join("archived_sessions");
        if !sessions.is_dir() && !archived.is_dir() {
            return Err("没找到 ~/.codex/sessions".to_string());
        }
        let mut files = jsonl_files(&sessions, true);
        // Archiving moves a rollout here unchanged, records and all. Response
        // ids stop the move from counting anything twice. Compressed `.zst`
        // archives are older than any day worth showing and are skipped.
        files.extend(jsonl_files(&archived, false));
        for path in &files {
            let Ok(metadata) = std::fs::metadata(path) else {
                continue;
            };
            let size = metadata.len();
            // Never decided by modified time: Codex appends through a handle
            // it keeps open, and Windows does not reliably move the timestamp
            // until that handle closes, so a live rollout can look days old.
            // The first scan reads every rollout once; after that, only the
            // bytes past where we stopped.
            let start = match self.offsets.get(path) {
                Some(&offset) if offset <= size => offset,
                _ => 0,
            };
            let offset = if size > start {
                let (seen, today) = (&mut self.seen, &mut self.today);
                read_complete_lines(path, start, |line| count_line(line, window, seen, today))
                    .unwrap_or(start)
            } else {
                start
            };
            self.offsets.insert(path.clone(), offset);
        }
        let live: HashSet<&PathBuf> = files.iter().collect();
        self.offsets.retain(|path, _| live.contains(path));
        Ok(self.today.tally())
    }
}

fn count_line(text: &str, window: Window, seen: &mut HashSet<String>, today: &mut Counts) {
    if !text.contains("token_usage_record") {
        return;
    }
    let Ok(line) = serde_json::from_str::<Line>(text) else {
        return;
    };
    if line.kind.as_deref() != Some("token_usage_record") {
        return;
    }
    let Some(at) = line.timestamp.as_deref().and_then(rfc3339_to_unix) else {
        return;
    };
    if !window.contains(at) {
        return;
    }
    let Some(payload) = line.payload else {
        return;
    };
    let Some(usage) = payload.usage else {
        return;
    };
    // A forked thread starts with a copy of its parent's records, and an
    // archived rollout is the same file under a new name.
    if let Some(id) = payload.response_id {
        if !seen.insert(id) {
            return;
        }
    }
    let input = usage.input_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    today.total += usage.total_tokens.unwrap_or(input + output);
    today.cached += usage.cached_input_tokens.unwrap_or(0);
    today.requests += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::test_support::{append, codex_record, scratch_dir, unix};
    use std::time::{Duration, UNIX_EPOCH};

    fn window() -> Window {
        Window {
            start: unix("2026-11-22T16:00:00Z"),
            end: unix("2026-11-23T16:00:00Z"),
        }
    }

    #[test]
    fn counts_each_response_once_and_reads_only_appended_bytes() {
        let home = scratch_dir("codex");
        let live = home.join(".codex/sessions/2026/11/23/rollout-a.jsonl");
        let archived = home.join(".codex/archived_sessions/rollout-b.jsonl");

        let mut text = String::from(
            r#"{"timestamp":"2026-11-22T15:00:00Z","type":"session_meta","payload":{"id":"t"}}"#,
        );
        text += "\n";
        text += &codex_record("resp_0", "2026-11-22T15:59:59.999Z", [500, 0, 5, 505]);
        text += &codex_record("resp_1", "2026-11-22T16:00:00.000Z", [100, 60, 10, 110]);
        // The running total that follows each call: not a call of its own.
        text += r#"{"timestamp":"2026-11-22T16:00:00.100Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":10,"total_tokens":110}}}}"#;
        text += "\n";
        text += &codex_record("resp_1", "2026-11-22T16:00:00.000Z", [100, 60, 10, 110]);
        append(&live, &text);
        // Codex leaves the timestamp alone while it appends; a stale one must
        // not hide the file.
        std::fs::File::options()
            .write(true)
            .open(&live)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000_000))
            .unwrap();
        let mut text = codex_record("resp_1", "2026-11-22T16:00:00.000Z", [100, 60, 10, 110]);
        text += &codex_record("resp_2", "2026-11-23T03:00:00Z", [50, 0, 5, 55]);
        append(&archived, &text);
        append(&archived.with_extension("jsonl.zst"), "\u{28}\u{b5}not json");

        let mut scanner = Scanner::default();
        let tally = scanner.scan(&home, window()).unwrap();
        assert_eq!(
            tally,
            Tally {
                total: 165,
                fresh: 105,
                cache_read: 60,
                requests: 2
            }
        );

        // Change a consumed record in place (same length, new id) and append a
        // call. A reader that started over would count the edited record too.
        let original = std::fs::read_to_string(&live).unwrap();
        let edited = original.replacen("resp_1", "resp_9", 1);
        assert_eq!(edited.len(), original.len());
        std::fs::write(&live, edited).unwrap();
        append(&live, &codex_record("resp_3", "2026-11-23T04:00:00Z", [20, 10, 2, 22]));

        let tally = scanner.scan(&home, window()).unwrap();
        assert_eq!(
            tally,
            Tally {
                total: 187,
                fresh: 117,
                cache_read: 70,
                requests: 3
            }
        );
        assert_eq!(scanner.offsets[&live], std::fs::metadata(&live).unwrap().len());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_codex_folder_is_a_reason_not_a_zero() {
        let home = scratch_dir("codex-none");
        let error = Scanner::default().scan(&home, window()).unwrap_err();
        assert!(error.contains("~/.codex/sessions"));
        std::fs::remove_dir_all(home).unwrap();
    }
}
