import { test } from "node:test";
import assert from "node:assert/strict";

import {
  clockAt,
  dayZoneLabel,
  formatPoints,
  formatTokens,
  tokenParts,
  tokenTotals,
} from "../src/token-format.js";

test("totals add every source that answered and name the ones that did not", () => {
  const rows = [
    { label: "Claude Code", status: "ok", total: 850, fresh: 21, cache_read: 829, requests: 3868 },
    { label: "Codex", status: "ok", total: 96, fresh: 2, cache_read: 94, requests: 694 },
    { label: "Gemini", status: "unavailable", total: 0, fresh: 0, cache_read: 0, requests: 0 },
    {
      label: "Grok",
      status: "ok",
      total: 3,
      fresh: 3,
      cache_read: 0,
      requests: 1,
      weekly_points: { points: 4 },
    },
  ];
  assert.deepEqual(tokenTotals(rows), {
    total: 949,
    fresh: 26,
    cache_read: 923,
    requests: 4563,
    missing: ["Gemini"],
  });
  assert.deepEqual(tokenTotals(), { total: 0, fresh: 0, cache_read: 0, requests: 0, missing: [] });
});

test("counts under ten thousand print as they are", () => {
  assert.equal(formatTokens(0), "0");
  assert.equal(formatTokens(8414), "8414");
  assert.deepEqual(tokenParts(9999), { value: "9999", unit: "" });
});

test("wan keeps one decimal below ten and drops a trailing zero", () => {
  assert.equal(formatTokens(28_549), "2.9 万");
  assert.equal(formatTokens(30_000), "3 万");
  assert.equal(formatTokens(848_892), "85 万");
  assert.equal(formatTokens(20_800_000), "2080 万");
  assert.equal(formatTokens(95_711_660), "9571 万");
});

test("yi takes over at one hundred million", () => {
  assert.equal(formatTokens(99_999_999), "1 亿");
  assert.equal(formatTokens(99_994_999), "9999 万");
  assert.equal(formatTokens(100_000_000), "1 亿");
  assert.equal(formatTokens(830_100_000), "8.3 亿");
  assert.equal(formatTokens(1_930_035_577), "19 亿");
  assert.deepEqual(tokenParts(809_261_351), { value: "8.1", unit: "亿" });
});

test("missing or negative counts read as zero", () => {
  assert.equal(formatTokens(undefined), "0");
  assert.equal(formatTokens(-5), "0");
  assert.equal(formatTokens(Number.NaN), "0");
});

test("weekly points", () => {
  assert.equal(formatPoints(0), "0");
  assert.equal(formatPoints(0.04), "0");
  assert.equal(formatPoints(3.24), "+3.2");
  assert.equal(formatPoints(3), "+3");
  assert.equal(formatPoints(Number.NaN), "0");
});

test("clock reads in the day's offset, not the machine's", () => {
  // 14:51 UTC is 22:51 in Beijing.
  assert.equal(clockAt("2026-11-23T14:51:00Z", 480), "22:51");
  assert.equal(clockAt("2026-11-23T06:51:00-08:00", 480), "22:51");
  assert.equal(clockAt("2026-11-23T14:51:00Z", -480), "06:51");
  assert.equal(clockAt("not a date", 480), "");
});

test("zone label", () => {
  assert.equal(dayZoneLabel(480), "北京时间");
  assert.equal(dayZoneLabel(-480), "UTC−8");
  assert.equal(dayZoneLabel(330), "UTC+5:30");
  assert.equal(dayZoneLabel(0), "UTC+0");
});
