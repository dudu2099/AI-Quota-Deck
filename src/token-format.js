// Formatting for the "today's tokens" section. Pure functions, so node --test
// can check them without a WebView.

const WAN = 10_000;
const YI = 100_000_000;

/** One decimal below ten of a unit, whole numbers above it: "2.9 万", "85 万",
 *  "8.3 亿". A trailing ".0" is dropped, so 30,000 reads "3 万", not "3.0 万". */
function scaled(value) {
  const text = value < 10 ? value.toFixed(1) : Math.round(value).toString();
  return text.endsWith(".0") ? text.slice(0, -2) : text;
}

/** A token count split into the figure and its unit, for a big number with a
 *  small unit beside it. Counts under 10,000 have no unit. */
export function tokenParts(count) {
  const n = Number.isFinite(count) && count > 0 ? count : 0;
  // 99,999,999 would round to "10000 万"; that is already one yi.
  if (n >= YI || Math.round(n / WAN) >= 10_000) return { value: scaled(n / YI), unit: "亿" };
  if (n >= WAN) return { value: scaled(n / WAN), unit: "万" };
  return { value: Math.round(n).toString(), unit: "" };
}

export function formatTokens(count) {
  const { value, unit } = tokenParts(count);
  return unit ? `${value} ${unit}` : value;
}

/** Today's figures added across every source that answered. A source that
 *  could not be read adds nothing and is named in `missing`, so a total that
 *  is short says why. Grok Bot's weekly points are a percentage, not tokens,
 *  and stay out of it. */
export function tokenTotals(rows = []) {
  const totals = { total: 0, fresh: 0, cache_read: 0, requests: 0, missing: [] };
  for (const row of rows) {
    if (row.status !== "ok") {
      totals.missing.push(row.label);
      continue;
    }
    totals.total += row.total || 0;
    totals.fresh += row.fresh || 0;
    totals.cache_read += row.cache_read || 0;
    totals.requests += row.requests || 0;
  }
  return totals;
}

/** Percentage points of a weekly allowance. Under 0.05 rounds to "0", which is
 *  what an untouched day should say, rather than "+0.0". */
export function formatPoints(points) {
  if (!Number.isFinite(points) || points < 0.05) return "0";
  return `+${points.toFixed(1).replace(/\.0$/, "")}`;
}

/** "HH:MM" of an RFC3339 instant, read in the offset that defines "today" — the
 *  machine's own clock may sit in another time zone. */
export function clockAt(isoString, offsetMinutes) {
  const ms = Date.parse(isoString);
  if (!Number.isFinite(ms)) return "";
  const shifted = new Date(ms + offsetMinutes * 60_000);
  const hh = String(shifted.getUTCHours()).padStart(2, "0");
  const mm = String(shifted.getUTCMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

/** What "midnight" means for the header. Beijing gets its name; any other
 *  offset is spelled out. */
export function dayZoneLabel(offsetMinutes) {
  if (offsetMinutes === 480) return "北京时间";
  const sign = offsetMinutes < 0 ? "−" : "+";
  const abs = Math.abs(offsetMinutes);
  const hours = Math.floor(abs / 60);
  const minutes = abs % 60;
  return `UTC${sign}${hours}${minutes ? `:${String(minutes).padStart(2, "0")}` : ""}`;
}
