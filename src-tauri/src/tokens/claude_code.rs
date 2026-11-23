//! Claude Code keeps one JSONL transcript per session under
//! `~/.claude/projects/<project>/<session>.jsonl`, with sub-agent runs beside
//! it in `<session>/subagents/agent-*.jsonl`. Each API response lands as one or
//! more `assistant` lines carrying that response's `message.usage`.

use super::{jsonl_files, modified_seconds, read_complete_lines, Tally, Window};
use crate::quota::rfc3339_to_unix;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Scanner {
    /// Bytes already consumed, per transcript.
    offsets: HashMap<PathBuf, u64>,
    /// Message ids counted today.
    seen: HashSet<String>,
    today: Counts,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    requests: u64,
}

impl Counts {
    /// Anthropic reports `input_tokens` without the cached part, so every
    /// category is disjoint and the total is their sum. Writing to the cache is
    /// real processing and stays in "fresh".
    fn tally(&self) -> Tally {
        let fresh = self.input + self.output + self.cache_write;
        Tally {
            total: fresh + self.cache_read,
            fresh,
            cache_read: self.cache_read,
            requests: self.requests,
        }
    }

    fn add(&mut self, usage: &Usage) {
        self.input += usage.input_tokens.unwrap_or(0);
        self.output += usage.output_tokens.unwrap_or(0);
        self.cache_read += usage.cache_read_input_tokens.unwrap_or(0);
        self.cache_write += usage.cache_creation_input_tokens.unwrap_or(0);
        self.requests += 1;
    }
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

impl Scanner {
    pub fn new_day(&mut self) {
        self.seen.clear();
        self.today = Counts::default();
    }

    pub fn scan(&mut self, home: &Path, window: Window) -> Result<Tally, String> {
        let root = home.join(".claude").join("projects");
        if !root.is_dir() {
            return Err("没找到 ~/.claude/projects".to_string());
        }
        let files = jsonl_files(&root, true);
        for path in &files {
            let Ok(metadata) = std::fs::metadata(path) else {
                continue;
            };
            let size = metadata.len();
            let start = match self.offsets.get(path) {
                Some(&offset) if offset <= size => offset,
                // Shorter than what was already read means rewritten. Read it
                // again; message ids keep anything from counting twice.
                Some(_) => 0,
                // First sight. Claude Code appends with ordinary writes, which
                // move the modified time, so a file last touched before today
                // holds nothing from today. Start at its end.
                None if modified_seconds(&metadata) < window.start as f64 => size,
                None => 0,
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
    // Nearly all of a transcript is prompts and tool output. Every usage line
    // contains both of these words, and checking for them is far cheaper than
    // parsing the line.
    if !text.contains("\"usage\"") || !text.contains("assistant") {
        return;
    }
    let Ok(line) = serde_json::from_str::<Line>(text) else {
        return;
    };
    if line.kind.as_deref() != Some("assistant") {
        return;
    }
    let Some(message) = line.message else {
        return;
    };
    let Some(usage) = message.usage else {
        return;
    };
    // Claude Code invents `<synthetic>` messages locally (API errors,
    // interruptions). No model was called for them.
    if message.model.as_deref() == Some("<synthetic>") {
        return;
    }
    // Bucket by the line's own time, not the file's: one session file can run
    // across midnight, and a resumed session copies yesterday's lines.
    let Some(at) = line.timestamp.as_deref().and_then(rfc3339_to_unix) else {
        return;
    };
    if !window.contains(at) {
        return;
    }
    // A response with several content blocks is written as several lines,
    // each repeating the full usage; forked sessions copy whole histories into
    // a new file. Either way it is one call, so count each message id once.
    if let Some(id) = message.id {
        if !seen.insert(id) {
            return;
        }
    }
    today.add(&usage);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::test_support::{append, scratch_dir, unix};
    use std::time::{Duration, UNIX_EPOCH};

    fn assistant(id: &str, at: &str, model: &str, usage: [u64; 4]) -> String {
        let [input, output, cache_read, cache_write] = usage;
        format!(
            r#"{{"type":"assistant","timestamp":"{at}","message":{{"id":"{id}","model":"{model}","role":"assistant","content":[{{"type":"text","text":"x"}}],"usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_write}}}}}}}"#
        ) + "\n"
    }

    /// Beijing's 23 November: 2026-11-22T16:00Z up to 2026-11-23T16:00Z.
    fn window() -> Window {
        Window {
            start: unix("2026-11-22T16:00:00Z"),
            end: unix("2026-11-23T16:00:00Z"),
        }
    }

    #[test]
    fn counts_each_message_once_and_buckets_by_line_time_across_midnight() {
        let home = scratch_dir("claude");
        let project = home.join(".claude/projects/F--demo");
        let main = project.join("session-a.jsonl");
        let sub = project.join("session-a/subagents/agent-1.jsonl");
        let fork = project.join("session-b.jsonl");

        let mut text = String::new();
        // Yesterday by a tenth of a second: excluded, even written twice.
        text += &assistant("msg_y", "2026-11-22T15:59:59.900Z", "m", [100, 10, 1000, 50]);
        text += &assistant("msg_y", "2026-11-22T15:59:59.900Z", "m", [100, 10, 1000, 50]);
        // One response split over three content-block lines.
        for _ in 0..3 {
            text += &assistant("msg_a", "2026-11-22T16:00:00.000Z", "m", [1, 2, 3, 4]);
        }
        text += r#"{"type":"user","timestamp":"2026-11-22T16:00:01Z","message":{"role":"user","content":"what does \"usage\" mean to the assistant"}}"#;
        text += "\n";
        text += &assistant("msg_s", "2026-11-22T16:00:02Z", "<synthetic>", [9, 9, 9, 9]);
        append(&main, &text);
        append(&sub, &assistant("msg_b", "2026-11-22T18:00:00Z", "m", [10, 20, 30, 40]));
        // A fork repeats msg_a, adds the day's last call and tomorrow's first.
        let mut text = assistant("msg_a", "2026-11-22T16:00:00.000Z", "m", [1, 2, 3, 4]);
        text += &assistant("msg_c", "2026-11-23T15:59:59.999Z", "m", [5, 5, 5, 5]);
        text += &assistant("msg_t", "2026-11-23T16:00:00Z", "m", [7, 7, 7, 7]);
        append(&fork, &text);

        let mut scanner = Scanner::default();
        let tally = scanner.scan(&home, window()).unwrap();
        // msg_a + msg_b + msg_c
        assert_eq!(
            scanner.today,
            Counts {
                input: 16,
                output: 27,
                cache_read: 38,
                cache_write: 49,
                requests: 3
            }
        );
        assert_eq!(
            tally,
            Tally {
                total: 130,
                fresh: 92,
                cache_read: 38,
                requests: 3
            }
        );

        // A new call arrives, plus the first half of another.
        let partial = assistant("msg_e", "2026-11-22T20:00:00Z", "m", [0, 1, 0, 0]);
        let (head, tail) = partial.split_at(40);
        append(&main, &assistant("msg_d", "2026-11-22T19:00:00Z", "m", [1, 1, 1, 1]));
        append(&main, head);
        assert_eq!(scanner.scan(&home, window()).unwrap().requests, 4);
        let consumed = scanner.offsets[&main];
        assert_eq!(consumed + head.len() as u64, std::fs::metadata(&main).unwrap().len());

        // The rest of that line lands: counted now, and only now.
        append(&main, tail);
        let tally = scanner.scan(&home, window()).unwrap();
        assert_eq!(tally.requests, 5);
        assert_eq!(tally.total, 130 + 4 + 1);
        assert_eq!(scanner.scan(&home, window()).unwrap(), tally);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_file_untouched_since_yesterday_is_skipped_until_it_grows() {
        let home = scratch_dir("claude-mtime");
        let path = home.join(".claude/projects/p/old.jsonl");
        // Content stamped today in a file whose mtime says otherwise can only
        // be a clock oddity; trusting mtime is what makes the first scan cheap.
        append(&path, &assistant("msg_1", "2026-11-22T17:00:00Z", "m", [1, 0, 0, 0]));
        let long_ago = UNIX_EPOCH + Duration::from_secs(unix("2026-11-01T00:00:00Z") as u64);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();

        let mut scanner = Scanner::default();
        assert_eq!(scanner.scan(&home, window()).unwrap().requests, 0);
        append(&path, &assistant("msg_2", "2026-11-22T18:00:00Z", "m", [2, 0, 0, 0]));
        let tally = scanner.scan(&home, window()).unwrap();
        assert_eq!((tally.requests, tally.total), (1, 2));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_new_day_forgets_counted_ids_but_not_positions() {
        let home = scratch_dir("claude-day");
        let path = home.join(".claude/projects/p/s.jsonl");
        append(&path, &assistant("msg_1", "2026-11-22T17:00:00Z", "m", [1, 0, 0, 0]));
        let mut scanner = Scanner::default();
        scanner.scan(&home, window()).unwrap();
        scanner.new_day();
        let next = Window {
            start: window().end,
            end: window().end + 86_400,
        };
        assert_eq!(scanner.scan(&home, next).unwrap(), Tally::default());
        assert!(scanner.seen.is_empty());
        assert_eq!(
            scanner.offsets[&path],
            std::fs::metadata(&path).unwrap().len()
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_projects_folder_is_a_reason_not_a_zero() {
        let home = scratch_dir("claude-none");
        let error = Scanner::default().scan(&home, window()).unwrap_err();
        assert!(error.contains("~/.claude/projects"));
        std::fs::remove_dir_all(home).unwrap();
    }
}
