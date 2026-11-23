//! Tokens spent today by the coding tools installed on this machine.
//!
//! Nothing here talks to a server. Claude Code, Codex, Antigravity and the
//! Grok CLI each leave a usage record on disk as a side effect of running;
//! these modules add it up for one calendar day. A busy day of Claude Code
//! transcripts runs to hundreds of megabytes, and the files are appended to
//! while we read them, so every source is incremental: the first call in a
//! process reads what today's files hold, later calls read only what was
//! appended since.
//!
//! Each row fails on its own. A missing folder gives that row the status
//! "unavailable" and a reason; it never turns the whole panel into an error.

mod antigravity;
mod claude_code;
mod codex;
mod grok;

use chrono::{FixedOffset, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

const DAY_SECONDS: i64 = 24 * 60 * 60;
const NO_HOME: &str = "找不到用户主目录";
const CRASHED: &str = "统计时出错，下次刷新会重新读取";
const ROWS: [(&str, &str); 4] = [
    ("claude-code", "Claude Code"),
    ("codex", "Codex"),
    ("gemini", "Gemini"),
    ("grok", "Grok"),
];

/// One day's token use, as the dashboard renders it.
#[derive(Debug, Clone, Serialize)]
pub struct TokenDay {
    /// RFC 3339 in the offset that defines the day, e.g. `2026-11-23T00:00:00+08:00`.
    pub day_start: String,
    pub utc_offset_minutes: i32,
    /// When this snapshot was computed, in the same offset.
    pub observed_at: String,
    /// Always four, in the order of `ROWS`.
    pub rows: Vec<TokenRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenRow {
    pub id: String,
    pub label: String,
    /// "ok" or "unavailable".
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Every token the model processed, cache reads included.
    pub total: u64,
    /// `total` minus cache reads.
    pub fresh: u64,
    pub cache_read: u64,
    /// Model calls counted today.
    pub requests: u64,
    pub sources: Vec<TokenSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_points: Option<WeeklyPoints>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TokenSource {
    pub label: String,
    pub total: u64,
    pub fresh: u64,
    pub cache_read: u64,
    pub requests: u64,
}

/// Grok Bot reports no tokens at all, only how much of a weekly allowance is
/// gone. What it spent today is therefore the climb of that percentage since
/// the first reading of the day — see grok.rs.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WeeklyPoints {
    pub product: String,
    /// Percentage points of the weekly allowance used today.
    pub points: f64,
    /// When today's first reading was taken.
    pub since: String,
    /// False when the first reading came well after midnight, i.e. the deck
    /// was not running and anything spent before it is missing.
    pub from_day_start: bool,
}

/// Counts already translated into the dashboard's vocabulary. Each source
/// knows whether its own "input" includes cache reads; nothing downstream has
/// to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub total: u64,
    pub fresh: u64,
    pub cache_read: u64,
    pub requests: u64,
}

impl std::ops::AddAssign for Tally {
    fn add_assign(&mut self, other: Self) {
        self.total += other.total;
        self.fresh += other.fresh;
        self.cache_read += other.cache_read;
        self.requests += other.requests;
    }
}

impl TokenSource {
    fn new(label: &str, tally: Tally) -> Self {
        Self {
            label: label.to_string(),
            total: tally.total,
            fresh: tally.fresh,
            cache_read: tally.cache_read,
            requests: tally.requests,
        }
    }
}

impl TokenRow {
    fn ok(id: &str, label: &str, tally: Tally, sources: Vec<TokenSource>) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            status: "ok".to_string(),
            reason: None,
            total: tally.total,
            fresh: tally.fresh,
            cache_read: tally.cache_read,
            requests: tally.requests,
            sources,
            weekly_points: None,
        }
    }

    fn unavailable(id: &str, label: &str, reason: impl Into<String>) -> Self {
        Self {
            status: "unavailable".to_string(),
            reason: Some(reason.into()),
            ..Self::ok(id, label, Tally::default(), Vec::new())
        }
    }

    fn from_result(index: usize, result: Result<(Tally, Vec<TokenSource>), String>) -> Self {
        let (id, label) = ROWS[index];
        match result {
            Ok((tally, sources)) => Self::ok(id, label, tally, sources),
            Err(reason) => Self::unavailable(id, label, reason),
        }
    }
}

/// `[start, end)` in unix seconds. Both ends are whole seconds, so comparing a
/// timestamp's whole seconds against them gives the same answer as comparing
/// the exact instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: i64,
    pub end: i64,
}

impl Window {
    pub fn contains(&self, unix_seconds: i64) -> bool {
        (self.start..self.end).contains(&unix_seconds)
    }
}

/// Where everything lives. Spelled out rather than derived on the spot so the
/// tests can point every path somewhere harmless.
#[derive(Debug, Clone, Default)]
struct Paths {
    home: Option<PathBuf>,
    settings: Option<PathBuf>,
    grok_bot_cache: Option<PathBuf>,
    day_state: Option<PathBuf>,
}

impl Paths {
    fn system() -> Self {
        let app = dirs::data_local_dir().map(|dir| dir.join("ai-quota-deck"));
        Self {
            home: dirs::home_dir(),
            settings: app.as_ref().map(|dir| dir.join("token-usage.json")),
            grok_bot_cache: app
                .as_ref()
                .map(|dir| dir.join("provider-cache").join("grok-bot.json")),
            day_state: app.as_ref().map(|dir| dir.join("token-day-state.json")),
        }
    }

    /// The machine clock's zone is not necessarily the one the user lives in
    /// (a laptop left on another continent's time), so "today" can be pinned
    /// to an offset in a settings file. Read on every call, so editing it
    /// takes effect without a restart.
    fn offset_minutes(&self) -> i32 {
        self.settings
            .as_deref()
            .and_then(configured_offset)
            .unwrap_or_else(local_offset_minutes)
    }
}

fn configured_offset(path: &Path) -> Option<i32> {
    #[derive(Deserialize)]
    struct Settings {
        day_utc_offset_minutes: Option<i32>,
    }
    let raw = std::fs::read(path).ok()?;
    // Notepad and PowerShell both like to start the file with a byte-order
    // mark, which serde_json rejects.
    let bytes = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&raw);
    let settings: Settings = serde_json::from_slice(bytes).ok()?;
    settings
        .day_utc_offset_minutes
        .filter(|minutes| (-12 * 60..=14 * 60).contains(minutes))
}

/// The offset in force right now. On the day a daylight-saving change happens
/// this puts midnight an hour off; one fixed offset per day is what the
/// contract can express, and it is right on every other day.
fn local_offset_minutes() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

fn day_start(now: i64, offset_minutes: i32) -> i64 {
    let offset = i64::from(offset_minutes) * 60;
    (now + offset).div_euclid(DAY_SECONDS) * DAY_SECONDS - offset
}

fn rfc3339(unix_seconds: i64, offset_minutes: i32) -> String {
    FixedOffset::east_opt(offset_minutes * 60)
        .and_then(|offset| offset.timestamp_opt(unix_seconds, 0).single())
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, false))
        .unwrap_or_default()
}

/// Everything remembered between calls: how far into each file we have read,
/// which responses are already counted, and the running totals.
#[derive(Default)]
struct Engine {
    day_start: Option<i64>,
    /// No byte read so far was written after this instant (unix seconds).
    read_until: i64,
    claude: claude_code::Scanner,
    codex: codex::Scanner,
    gemini: antigravity::Scanner,
    /// Loaded from disk on first use, so Grok Bot's tally survives a restart.
    grok_bot: Option<grok::BotDay>,
}

impl Engine {
    /// Move the tallies to the day starting at `day_start`.
    fn roll_to(&mut self, day_start: i64) {
        match self.day_start {
            Some(current) if current == day_start => return,
            // An ordinary midnight. Every byte consumed so far was written
            // before the new day began, so none of it can belong to the new
            // day: keep the file positions and forget only yesterday's totals.
            Some(_) if self.read_until <= day_start => {
                self.claude.new_day();
                self.codex.new_day();
                self.gemini.new_day(day_start);
            }
            // The day moved over bytes already consumed — the offset in the
            // settings file changed. Those bytes were skipped as "not today"
            // and would never be looked at again, so start over from scratch.
            Some(_) => {
                let grok_bot = self.grok_bot.take();
                *self = Self {
                    grok_bot,
                    ..Self::default()
                };
            }
            None => {}
        }
        self.day_start = Some(day_start);
    }

    fn snapshot(
        &mut self,
        paths: &Paths,
        offset_minutes: i32,
        window: Window,
        now: i64,
    ) -> TokenDay {
        let started = Instant::now();
        self.roll_to(window.start);
        let home = paths.home.as_deref();

        let claude = isolated(&mut self.claude, |scanner| {
            scanner.scan(home.ok_or(NO_HOME)?, window)
        });
        let codex = isolated(&mut self.codex, |scanner| {
            scanner.scan(home.ok_or(NO_HOME)?, window)
        });
        let gemini = isolated(&mut self.gemini, |scanner| {
            scanner.scan(home.ok_or(NO_HOME)?, window)
        });
        let grok_cli = isolated(&mut (), |_| grok::cli_usage(home.ok_or(NO_HOME)?, window));
        let weekly_points = self.grok_bot_points(paths, window.start, offset_minutes);
        // Rounded up: a line appended during the last fraction of a second of
        // the scan must still count as "read before" the next midnight check.
        self.read_until = now + started.elapsed().as_secs() as i64 + 1;

        let rows = vec![
            TokenRow::from_result(0, claude.map(|tally| (tally, Vec::new()))),
            TokenRow::from_result(1, codex.map(|tally| (tally, Vec::new()))),
            TokenRow::from_result(
                2,
                gemini.map(|sources| {
                    let mut total = Tally::default();
                    let rows = sources
                        .into_iter()
                        .map(|(label, tally)| {
                            total += tally;
                            TokenSource::new(label, tally)
                        })
                        .collect();
                    (total, rows)
                }),
            ),
            grok_row(grok_cli, weekly_points),
        ];

        TokenDay {
            day_start: rfc3339(window.start, offset_minutes),
            utc_offset_minutes: offset_minutes,
            observed_at: rfc3339(now, offset_minutes),
            rows,
        }
    }

    fn grok_bot_points(
        &mut self,
        paths: &Paths,
        day_start: i64,
        offset_minutes: i32,
    ) -> Option<WeeklyPoints> {
        let day = self.grok_bot.get_or_insert_with(|| {
            paths
                .day_state
                .as_deref()
                .map(grok::load_day)
                .unwrap_or_default()
        });
        let reading = paths.grok_bot_cache.as_deref().and_then(grok::bot_reading);
        if day.observe(day_start, reading.as_ref()) {
            if let Some(path) = paths.day_state.as_deref() {
                // A failed save costs only the points gathered since the last
                // one, and only if the deck restarts today; not worth a row.
                if let Err(error) = grok::save_day(path, day) {
                    eprintln!("token day state save failed: {error}");
                }
            }
        }
        day.weekly_points(day_start, offset_minutes)
    }
}

/// Grok has two halves with nothing in common: the CLI logs tokens, Grok Bot
/// only a weekly percentage. The row is worth showing if either is there.
fn grok_row(cli: Result<Tally, String>, weekly_points: Option<WeeklyPoints>) -> TokenRow {
    let (id, label) = ROWS[3];
    match (cli, weekly_points) {
        (Ok(tally), weekly_points) => TokenRow {
            weekly_points,
            ..TokenRow::ok(id, label, tally, vec![TokenSource::new("命令行", tally)])
        },
        (Err(_), Some(points)) => TokenRow {
            weekly_points: Some(points),
            ..TokenRow::ok(id, label, Tally::default(), Vec::new())
        },
        (Err(reason), None) => {
            let reason = format!("{reason}；Grok Bot 今天也还没有读数");
            TokenRow::unavailable(id, label, reason)
        }
    }
}

/// Run one source so that a panic costs that row and nothing else. The
/// source's state is thrown away, since a panic can leave it half-updated;
/// the next call simply reads its files again.
fn isolated<S: Default, T>(
    state: &mut S,
    work: impl FnOnce(&mut S) -> Result<T, String>,
) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(|| work(&mut *state))) {
        Ok(result) => result,
        Err(_) => {
            *state = S::default();
            Err(CRASHED.to_string())
        }
    }
}

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

#[tauri::command]
pub async fn token_usage_today() -> TokenDay {
    // The first call reads hundreds of megabytes. That is blocking file IO and
    // must not sit on an async worker, where it would stall every other
    // command the dashboard sends meanwhile.
    match tauri::async_runtime::spawn_blocking(today).await {
        Ok(day) => day,
        Err(_) => failed_day(CRASHED),
    }
}

fn today() -> TokenDay {
    let paths = Paths::system();
    let offset = paths.offset_minutes();
    let now = Utc::now().timestamp();
    let start = day_start(now, offset);
    // Nothing on disk can be stamped later than now, so running the window to
    // the next midnight counts exactly [start, now). It also keeps a line that
    // races the clock from being consumed without ever being counted.
    let window = Window {
        start,
        end: start + DAY_SECONDS,
    };
    // One scan at a time: a second caller waits, then finds almost nothing new.
    let mut engine = ENGINE.lock().unwrap_or_else(|poisoned| {
        ENGINE.clear_poison();
        let mut engine = poisoned.into_inner();
        *engine = None;
        engine
    });
    engine
        .get_or_insert_with(Engine::default)
        .snapshot(&paths, offset, window, now)
}

fn failed_day(reason: &str) -> TokenDay {
    let offset = Paths::system().offset_minutes();
    let now = Utc::now().timestamp();
    TokenDay {
        day_start: rfc3339(day_start(now, offset), offset),
        utc_offset_minutes: offset,
        observed_at: rfc3339(now, offset),
        rows: ROWS
            .iter()
            .map(|(id, label)| TokenRow::unavailable(id, label, reason))
            .collect(),
    }
}

/// Every `*.jsonl` under `root`, sorted. Junctions and symlinks are followed:
/// `~/.claude/projects` is often a junction to another drive.
fn jsonl_files(root: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_jsonl(root, recursive, 0, &mut files);
    files.sort();
    files
}

fn collect_jsonl(dir: &Path, recursive: bool, depth: usize, files: &mut Vec<PathBuf>) {
    // Deep enough for every layout seen; a link pointing back up the tree
    // would otherwise recurse forever.
    const MAX_DEPTH: usize = 8;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let (is_dir, is_file) = if kind.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(target) => (target.is_dir(), target.is_file()),
                Err(_) => continue,
            }
        } else {
            (kind.is_dir(), kind.is_file())
        };
        if is_dir {
            if recursive && depth < MAX_DEPTH {
                collect_jsonl(&path, true, depth + 1, files);
            }
        } else if is_file && path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
}

/// Seconds since the epoch, or infinity when the platform cannot say — an
/// unknown age has to be treated as "possibly today".
fn modified_seconds(metadata: &std::fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(f64::INFINITY, |age| age.as_secs_f64())
}

/// Feed every complete line from byte `start` onwards to `on_line`, and return
/// the offset just past the last one. A final line without its `\n` is still
/// being written; it is left for the next call instead of being parsed
/// half-finished and then lost.
fn read_complete_lines(
    path: &Path,
    start: u64,
    mut on_line: impl FnMut(&str),
) -> std::io::Result<u64> {
    const CHUNK: usize = 4 << 20;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buffer: Vec<u8> = Vec::with_capacity(CHUNK);
    let mut offset = start;
    loop {
        let filled = buffer.len();
        buffer.resize(filled + CHUNK, 0);
        let read = file.read(&mut buffer[filled..])?;
        buffer.truncate(filled + read);
        if read == 0 {
            return Ok(offset);
        }
        if let Some(last) = buffer.iter().rposition(|&byte| byte == b'\n') {
            each_line(&buffer[..last], &mut on_line);
            offset += (last + 1) as u64;
            buffer.drain(..=last);
        }
    }
}

fn each_line(bytes: &[u8], on_line: &mut impl FnMut(&str)) {
    // Validating a whole chunk at once is far cheaper than line by line, and
    // `str` gets the fast substring search the prefilters lean on.
    match std::str::from_utf8(bytes) {
        Ok(text) => text.split('\n').for_each(on_line),
        Err(_) => bytes
            .split(|&byte| byte == b'\n')
            .filter_map(|line| std::str::from_utf8(line).ok())
            .for_each(on_line),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A fresh, empty directory under the system temp folder.
    pub fn scratch_dir(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ai-quota-deck-tokens-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn unix(rfc3339: &str) -> i64 {
        crate::quota::rfc3339_to_unix(rfc3339).expect("valid timestamp")
    }

    pub fn append(path: &std::path::Path, text: &str) {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    pub fn codex_record(response_id: &str, at: &str, usage: [u64; 4]) -> String {
        let [input, cached, output, total] = usage;
        format!(
            r#"{{"timestamp":"{at}","type":"token_usage_record","payload":{{"response_id":"{response_id}","usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"total_tokens":{total}}}}}}}"#
        ) + "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{append, codex_record, scratch_dir, unix};
    use super::*;

    const BEIJING: i32 = 480;

    fn paths_for(home: &Path) -> Paths {
        Paths {
            home: Some(home.to_path_buf()),
            settings: None,
            grok_bot_cache: None,
            day_state: None,
        }
    }

    #[test]
    fn day_start_is_local_midnight_under_the_offset() {
        // 04:00 in Beijing on the 23rd is still the 22nd in UTC.
        let now = unix("2026-11-22T20:00:00Z");
        assert_eq!(day_start(now, BEIJING), unix("2026-11-22T16:00:00Z"));
        assert_eq!(day_start(now, 0), unix("2026-11-22T00:00:00Z"));
        assert_eq!(day_start(now, -480), unix("2026-11-22T08:00:00Z"));
        assert_eq!(
            rfc3339(day_start(now, BEIJING), BEIJING),
            "2026-11-23T00:00:00+08:00"
        );
        assert_eq!(rfc3339(day_start(now, 0), 0), "2026-11-22T00:00:00+00:00");
    }

    #[test]
    fn reads_the_offset_setting_even_with_a_byte_order_mark() {
        let dir = scratch_dir("settings");
        let path = dir.join("token-usage.json");
        std::fs::write(&path, b"\xEF\xBB\xBF{\"day_utc_offset_minutes\": 480}").unwrap();
        assert_eq!(configured_offset(&path), Some(480));
        std::fs::write(&path, b"{\"day_utc_offset_minutes\": 9999}").unwrap();
        assert_eq!(configured_offset(&path), None, "out of range is ignored");
        assert_eq!(configured_offset(&dir.join("missing.json")), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn serializes_the_wire_contract() {
        let day = TokenDay {
            day_start: "2026-11-23T00:00:00+08:00".into(),
            utc_offset_minutes: 480,
            observed_at: "2026-11-23T10:00:00+08:00".into(),
            rows: vec![
                TokenRow::ok(
                    "gemini",
                    "Gemini",
                    Tally {
                        total: 3,
                        fresh: 2,
                        cache_read: 1,
                        requests: 1,
                    },
                    vec![TokenSource::new("IDE", Tally::default())],
                ),
                TokenRow::unavailable("codex", "Codex", "没找到 ~/.codex/sessions"),
            ],
        };
        let json = serde_json::to_value(&day).unwrap();
        assert_eq!(json["utc_offset_minutes"], 480);
        let ok = &json["rows"][0];
        assert_eq!(ok["status"], "ok");
        assert!(ok.get("reason").is_none());
        assert!(ok.get("weekly_points").is_none());
        assert_eq!(ok["sources"][0]["label"], "IDE");
        assert_eq!(ok["cache_read"], 1);
        let missing = &json["rows"][1];
        assert_eq!(missing["status"], "unavailable");
        assert_eq!(missing["reason"], "没找到 ~/.codex/sessions");
    }

    #[test]
    fn every_row_is_present_and_unavailable_without_any_data() {
        let home = scratch_dir("empty-home");
        let now = unix("2026-11-23T02:00:00Z");
        let start = day_start(now, BEIJING);
        let day = Engine::default().snapshot(
            &paths_for(&home),
            BEIJING,
            Window {
                start,
                end: start + DAY_SECONDS,
            },
            now,
        );
        let ids: Vec<_> = day.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["claude-code", "codex", "gemini", "grok"]);
        assert!(day.rows.iter().all(|row| row.status == "unavailable"));
        assert!(day.rows.iter().all(|row| row.reason.is_some()));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn midnight_keeps_file_positions_but_not_yesterdays_totals() {
        let home = scratch_dir("rollover");
        let file = home.join(".codex/sessions/2026/11/22/rollout-a.jsonl");
        append(
            &file,
            &codex_record("resp_1", "2026-11-22T10:00:00.000Z", [10, 0, 1, 11]),
        );
        let paths = paths_for(&home);
        let mut engine = Engine::default();

        let day1_now = unix("2026-11-22T12:00:00Z");
        let day1 = day_start(day1_now, BEIJING);
        let first = engine.snapshot(
            &paths,
            BEIJING,
            Window {
                start: day1,
                end: day1 + DAY_SECONDS,
            },
            day1_now,
        );
        assert_eq!(first.rows[1].requests, 1);

        // Rewrite the already-read record in place as if it were tomorrow's.
        // Only a reader that went back to byte 0 would see it.
        let original = std::fs::read_to_string(&file).unwrap();
        let rewritten = original
            .replace("resp_1", "resp_X")
            .replace("2026-11-22T10", "2026-11-23T10");
        assert_eq!(rewritten.len(), original.len());
        std::fs::write(&file, rewritten).unwrap();
        append(
            &file,
            &codex_record("resp_2", "2026-11-23T01:00:00.000Z", [20, 5, 2, 22]),
        );

        let day2_now = unix("2026-11-23T02:00:00Z");
        let day2 = day_start(day2_now, BEIJING);
        let second = engine.snapshot(
            &paths,
            BEIJING,
            Window {
                start: day2,
                end: day2 + DAY_SECONDS,
            },
            day2_now,
        );
        let codex = &second.rows[1];
        assert_eq!((codex.requests, codex.total, codex.cache_read), (1, 22, 5));

        // Switching the offset moves the day start over bytes already
        // consumed, so everything is read again from zero — which this time
        // does find the rewritten record.
        let third = engine.snapshot(
            &paths,
            0,
            Window {
                start: unix("2026-11-23T00:00:00Z"),
                end: unix("2026-11-24T00:00:00Z"),
            },
            day2_now,
        );
        assert_eq!(third.rows[1].requests, 2);
        std::fs::remove_dir_all(home).unwrap();
    }

    /// Prints today's totals per source for a fixed window `[day start, T)`,
    /// for comparison with an independent script run with the same T.
    ///
    ///   TOKEN_CUTOFF=<unix seconds> [TOKEN_STATE_FILE=<scratch path>]
    ///   cargo test --manifest-path src-tauri/Cargo.toml live_today -- --ignored --nocapture
    ///
    /// Reads the real home folder and settings, but never writes the real
    /// Grok Bot day state.
    #[test]
    #[ignore]
    fn live_today_fixed_window() {
        let cutoff: i64 = std::env::var("TOKEN_CUTOFF")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| Utc::now().timestamp());
        let system = Paths::system();
        let state = std::env::var_os("TOKEN_STATE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| scratch_dir("live").join("token-day-state.json"));
        let paths = Paths {
            day_state: Some(state),
            ..system
        };
        let offset = paths.offset_minutes();
        let window = Window {
            start: day_start(cutoff, offset),
            end: cutoff,
        };
        println!(
            "window [{}, {}) offset {offset}",
            rfc3339(window.start, offset),
            rfc3339(window.end, offset)
        );

        let mut engine = Engine::default();
        let clock = Instant::now();
        let first = engine.snapshot(&paths, offset, window, cutoff);
        let first_time = clock.elapsed();
        let clock = Instant::now();
        let second = engine.snapshot(&paths, offset, window, cutoff);
        let second_time = clock.elapsed();

        for row in &first.rows {
            println!(
                "{:<12} {:<11} total={} fresh={} cache_read={} requests={} reason={:?}",
                row.id, row.status, row.total, row.fresh, row.cache_read, row.requests, row.reason
            );
            for source in &row.sources {
                println!(
                    "    {:<8} total={} fresh={} cache_read={} requests={}",
                    source.label, source.total, source.fresh, source.cache_read, source.requests
                );
            }
            if let Some(points) = &row.weekly_points {
                println!("    weekly_points {points:?}");
            }
        }
        println!("first scan {first_time:?}, second (incremental) {second_time:?}");
        println!("{}", serde_json::to_string(&first).unwrap());
        assert_same_counts(&first, &second);
    }

    fn assert_same_counts(a: &TokenDay, b: &TokenDay) {
        for (a, b) in a.rows.iter().zip(&b.rows) {
            assert_eq!(
                (a.total, a.fresh, a.cache_read, a.requests, &a.sources),
                (b.total, b.fresh, b.cache_read, b.requests, &b.sources),
                "{}",
                a.id
            );
        }
    }

    /// Real files keep growing while this runs. The window ends a minute
    /// after the first call, so the second call has to pick up everything
    /// appended in between — and must then agree with a fresh full scan.
    #[test]
    #[ignore]
    fn live_incremental_matches_a_fresh_scan() {
        let paths = Paths {
            day_state: Some(scratch_dir("live-incremental").join("state.json")),
            ..Paths::system()
        };
        let offset = paths.offset_minutes();
        let now = Utc::now().timestamp();
        let window = Window {
            start: day_start(now, offset),
            end: now + 45,
        };
        let mut engine = Engine::default();
        let first = engine.snapshot(&paths, offset, window, now);
        std::thread::sleep(std::time::Duration::from_secs(60));
        let clock = Instant::now();
        let second = engine.snapshot(&paths, offset, window, Utc::now().timestamp());
        let incremental = clock.elapsed();
        let fresh = Engine::default().snapshot(&paths, offset, window, Utc::now().timestamp());
        for (a, b) in first.rows.iter().zip(&second.rows) {
            println!(
                "{:<12} +{} tokens +{} requests in the minute between calls",
                a.id,
                b.total - a.total,
                b.requests - a.requests
            );
        }
        println!("incremental call took {incremental:?}");
        assert_same_counts(&second, &fresh);
    }
}
