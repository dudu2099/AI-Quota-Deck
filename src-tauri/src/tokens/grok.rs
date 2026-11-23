//! Grok comes in two shapes with nothing in common on disk.
//!
//! The Grok CLI keeps `~/.grok/sessions/<encoded cwd>/<session id>/usage.json`,
//! rewritten after every turn, with per-turn token counts.
//!
//! Grok Bot records no tokens anywhere. All there is, is the weekly allowance
//! percentage the deck itself fetches and caches in
//! `provider-cache/grok-bot.json`. What was spent today is how far that
//! percentage climbed since the day's first reading, so this module keeps a
//! small per-day ledger of readings on disk.

use super::{rfc3339, Tally, WeeklyPoints, Window};
use crate::quota::rfc3339_to_unix;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How late the day's first reading may come and still count as covering the
/// whole day. The deck polls every few minutes while it runs.
const DAY_START_GRACE_SECONDS: i64 = 10 * 60;
const WEEK_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Deserialize)]
struct UsageFile {
    #[serde(default)]
    turns: Vec<Turn>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Turn {
    ended_at: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_read_tokens: Option<u64>,
    total_tokens: Option<u64>,
    model_calls: Option<u64>,
}

impl Turn {
    /// `inputTokens` already contains `cachedReadTokens`. Two independent
    /// checks on real files agree: `totalTokens` is exactly input plus output,
    /// with no room for cache reads on top; and the turn's `costUsdTicks`
    /// matches xAI's per-token prices only when the cached tokens are billed
    /// out of the input, not in addition to it.
    fn tally(&self) -> Tally {
        let input = self.input_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        let total = self.total_tokens.unwrap_or(input + output);
        let cache_read = self.cached_read_tokens.unwrap_or(0);
        Tally {
            total,
            fresh: total.saturating_sub(cache_read),
            cache_read,
            requests: self.model_calls.unwrap_or(1),
        }
    }
}

/// Every Grok CLI turn that ended inside `window`. The files are small and
/// rewritten whole, so they are simply read again each time.
pub fn cli_usage(home: &Path, window: Window) -> Result<Tally, String> {
    let root = home.join(".grok").join("sessions");
    if !root.is_dir() {
        return Err("没找到 ~/.grok/sessions".to_string());
    }
    let mut tally = Tally::default();
    for project in subdirectories(&root) {
        for session in subdirectories(&project) {
            let Ok(raw) = std::fs::read(session.join("usage.json")) else {
                continue;
            };
            let Ok(usage) = serde_json::from_slice::<UsageFile>(&raw) else {
                continue;
            };
            for turn in &usage.turns {
                let ended = turn.ended_at.as_deref().and_then(rfc3339_to_unix);
                if ended.is_some_and(|at| window.contains(at)) {
                    tally += turn.tally();
                }
            }
        }
    }
    Ok(tally)
}

fn subdirectories(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// One Grok Bot weekly-allowance reading, as cached by grok_bot.rs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BotReading {
    pub percent: f64,
    pub observed_at: i64,
    pub resets_at: Option<i64>,
}

pub fn bot_reading(cache: &Path) -> Option<BotReading> {
    #[derive(Deserialize)]
    struct Cache {
        windows: Vec<CacheWindow>,
        observed_at: i64,
    }
    #[derive(Deserialize)]
    struct CacheWindow {
        percent: f64,
        resets_at: Option<i64>,
        window_seconds: Option<i64>,
    }
    let cache: Cache = serde_json::from_slice(&std::fs::read(cache).ok()?).ok()?;
    let weekly = cache
        .windows
        .into_iter()
        .find(|window| window.window_seconds == Some(WEEK_SECONDS))?;
    Some(BotReading {
        percent: weekly.percent,
        observed_at: cache.observed_at,
        resets_at: weekly.resets_at,
    })
}

/// Today's Grok Bot ledger, persisted so a restart mid-day keeps it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BotDay {
    day_start: i64,
    /// Unix seconds of today's first reading; zero until there is one.
    first_observed_at: i64,
    last_observed_at: i64,
    last_percent: f64,
    last_resets_at: Option<i64>,
    points: f64,
}

impl BotDay {
    /// Fold the latest reading into the ledger for the day starting at
    /// `day_start`. Returns whether anything changed and needs saving.
    pub fn observe(&mut self, day_start: i64, reading: Option<&BotReading>) -> bool {
        let mut changed = false;
        if self.day_start != day_start {
            *self = Self {
                day_start,
                ..Self::default()
            };
            changed = true;
        }
        let Some(reading) = reading else {
            return changed;
        };
        // A reading from before midnight says nothing about today, and one
        // the deck has already folded in must not count twice.
        if reading.observed_at < day_start
            || reading.observed_at <= self.last_observed_at
            || !reading.percent.is_finite()
            || !(0.0..=100.0).contains(&reading.percent)
        {
            return changed;
        }
        if self.first_observed_at == 0 {
            // The baseline. Whatever was spent before it is unknowable.
            self.first_observed_at = reading.observed_at;
        } else if reading.percent >= self.last_percent {
            self.points += reading.percent - self.last_percent;
        } else if reading.resets_at.is_some() && reading.resets_at == self.last_resets_at {
            // Lower, yet the same window: the server rounding differently,
            // not a refund. Adding the whole percentage here would count the
            // week so far a second time.
        } else {
            // The weekly window rolled over between readings, so everything
            // in the new window was spent since the last reading.
            self.points += reading.percent;
        }
        self.last_observed_at = reading.observed_at;
        self.last_percent = reading.percent;
        self.last_resets_at = reading.resets_at;
        true
    }

    pub fn weekly_points(&self, day_start: i64, offset_minutes: i32) -> Option<WeeklyPoints> {
        if self.day_start != day_start || self.first_observed_at == 0 {
            return None;
        }
        Some(WeeklyPoints {
            product: "Grok Bot".to_string(),
            points: self.points,
            since: rfc3339(self.first_observed_at, offset_minutes),
            from_day_start: self.first_observed_at - day_start <= DAY_START_GRACE_SECONDS,
        })
    }
}

/// The day-state file is shared with whatever per-day ledgers come later, so
/// Grok Bot's lives under its own key.
#[derive(Default, Serialize, Deserialize)]
struct DayState {
    #[serde(default)]
    grok_bot: BotDay,
}

/// A missing or unreadable file is an empty ledger: the worst outcome is that
/// today's points restart from the next reading.
pub fn load_day(path: &Path) -> BotDay {
    std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<DayState>(&raw).ok())
        .map(|state| state.grok_bot)
        .unwrap_or_default()
}

pub fn save_day(path: &Path, day: &BotDay) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec(&DayState {
        grok_bot: day.clone(),
    })
    .map_err(|error| format!("cannot serialize token day state: {error}"))?;
    crate::quota::atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::test_support::{append, scratch_dir, unix};

    fn reading(observed_at: i64, percent: f64, resets_at: i64) -> BotReading {
        BotReading {
            percent,
            observed_at,
            resets_at: Some(resets_at),
        }
    }

    fn assert_points(day: &BotDay, day_start: i64, points: f64) {
        let got = day.weekly_points(day_start, 480).expect("points").points;
        assert!((got - points).abs() < 1e-9, "points {got}, expected {points}");
    }

    #[test]
    fn grok_bot_points_survive_a_weekly_reset_and_restart_on_a_new_day() {
        let day1 = unix("2026-11-22T16:00:00Z");
        let (week1, week2) = (day1 + 3 * 3600, day1 + 3 * 3600 + WEEK_SECONDS);
        let mut day = BotDay::default();

        // Nothing until the first reading of the day exists.
        assert!(day.observe(day1, None));
        assert_eq!(day.weekly_points(day1, 480), None);
        // Yesterday's cached reading is not today's baseline.
        assert!(!day.observe(day1, Some(&reading(day1 - 60, 9.0, week1))));
        assert_eq!(day.weekly_points(day1, 480), None);

        assert!(day.observe(day1, Some(&reading(day1 + 100, 10.0, week1))));
        let points = day.weekly_points(day1, 480).unwrap();
        assert_eq!(points.points, 0.0);
        assert_eq!(points.since, "2026-11-23T00:01:40+08:00");
        assert!(points.from_day_start);

        assert!(day.observe(day1, Some(&reading(day1 + 3600, 15.5, week1))));
        assert_points(&day, day1, 5.5);
        // The same cached reading seen again on the next poll.
        assert!(!day.observe(day1, Some(&reading(day1 + 3600, 15.5, week1))));
        assert_points(&day, day1, 5.5);
        // The week rolled over: 2% of the new week is all new spending.
        assert!(day.observe(day1, Some(&reading(day1 + 4 * 3600, 2.0, week2))));
        assert_points(&day, day1, 7.5);
        // A dip inside the same window is noise, not a reset.
        assert!(day.observe(day1, Some(&reading(day1 + 4 * 3600 + 60, 1.9, week2))));
        assert_points(&day, day1, 7.5);
        assert!(day.observe(day1, Some(&reading(day1 + 5 * 3600, 3.0, week2))));
        assert_points(&day, day1, 8.6);

        // The next day starts from zero, and from wherever the first reading
        // of that day happened to come.
        let day2 = day1 + 86_400;
        assert!(day.observe(day2, Some(&reading(day2 + 3000, 4.0, week2))));
        let points = day.weekly_points(day2, 480).unwrap();
        assert_eq!(points.points, 0.0);
        assert!(!points.from_day_start, "first reading 50 minutes in");
        assert_eq!(day.weekly_points(day1, 480), None);
        assert!(day.observe(day2, Some(&reading(day2 + 4000, 6.0, week2))));
        assert_points(&day, day2, 2.0);
    }

    #[test]
    fn the_ledger_round_trips_through_disk() {
        let dir = scratch_dir("grok-state");
        let path = dir.join("token-day-state.json");
        assert_eq!(load_day(&path), BotDay::default());
        let mut day = BotDay::default();
        let start = unix("2026-11-22T16:00:00Z");
        day.observe(start, Some(&reading(start + 10, 1.0, start + 9_999)));
        day.observe(start, Some(&reading(start + 20, 1.25, start + 9_999)));
        save_day(&path, &day).unwrap();
        assert_eq!(load_day(&path), day);
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load_day(&path), BotDay::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_the_weekly_window_from_the_grok_bot_cache() {
        let dir = scratch_dir("grok-cache");
        let path = dir.join("grok-bot.json");
        std::fs::write(
            &path,
            r#"{"plan":"x","windows":[{"label":"Daily","percent":50.0,"resets_at":5,"window_seconds":86400,"severity":null},{"label":"Weekly","percent":27.5,"resets_at":1795672786,"window_seconds":604800,"severity":null}],"observed_at":1795445834}"#,
        )
        .unwrap();
        assert_eq!(
            bot_reading(&path),
            Some(BotReading {
                percent: 27.5,
                observed_at: 1_795_445_834,
                resets_at: Some(1_795_672_786)
            })
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sums_cli_turns_that_ended_today() {
        let home = scratch_dir("grok-cli");
        let file = home.join(".grok/sessions/C%3A%5Cwork/0001/usage.json");
        append(
            &file,
            r#"{"sessionId":"s","session":{"inputTokens":1},"turns":[
                {"turnNumber":1,"endedAt":"2026-11-22T15:59:59.999999999+00:00","inputTokens":900,"outputTokens":9,"cachedReadTokens":0,"totalTokens":909,"modelCalls":1},
                {"turnNumber":2,"endedAt":"2026-11-23T09:39:59.521511100+00:00","inputTokens":28340,"outputTokens":209,"cachedReadTokens":1664,"reasoningTokens":208,"totalTokens":28549,"modelCalls":2}
            ]}"#,
        );
        append(&home.join(".grok/sessions/session_search.sqlite"), "x");
        let window = Window {
            start: unix("2026-11-22T16:00:00Z"),
            end: unix("2026-11-23T16:00:00Z"),
        };
        assert_eq!(
            cli_usage(&home, window).unwrap(),
            Tally {
                total: 28_549,
                fresh: 26_885,
                cache_read: 1_664,
                requests: 2
            }
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}
