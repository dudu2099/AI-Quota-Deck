import {
  applyClaudeLoginStatus,
  enabledProviders,
  isUserAway,
  missedRefreshCycle,
  providerObservationMs,
  providerRequestFloor,
  providerRetryDelay,
  resumeGraceDeadline,
  settleProviders,
} from "./polling.js";
import { displayedPercent, usageDisplayLabel } from "./widget-model.js";
const { invoke } = window.__TAURI__.core;
const { emitTo, listen } = window.__TAURI__.event;

const PROVIDERS = [
  {
    id: "claude",
    name: "Claude",
    command: "claude_quota",
    setup: "运行 Claude Code 并登录一次。",
    // Claude's undocumented endpoint is more sensitive than the other sources.
    // Poll conservatively while active; the 429 cooldown itself is owned by the
    // Rust side (claude-rate-limit.json) and arrives as retry_after_seconds.
    pollMs: 360_000,
    pauseWhileAway: true,
  },
  {
    id: "codex",
    name: "Codex",
    command: "codex_quota",
    setup: "打开 Codex 应用并登录一次。",
  },
  {
    id: "antigravity",
    name: "Antigravity",
    command: "antigravity_quota",
    setup: "安装 Google Antigravity IDE 并登录；读取额度时要保持 IDE 打开。",
  },
  {
    id: "gemini",
    name: "Gemini",
    command: "gemini_quota",
    setup: "安装可选的 Browser Bridge 扩展，打开 Gemini，再点一次扩展图标。",
    optional: true,
  },
  {
    id: "grok",
    name: "Grok",
    command: "grok_quota",
    setup: "安装可选的 Browser Bridge 扩展（推荐），或用 Grok Build 登录。",
    optional: true,
  },
  {
    id: "grok_bot",
    name: "Grok Bot",
    mark: "GB",
    command: "grok_bot_quota",
    setup: "安装 Grok Bot 桌面应用并登录一次。",
  },
];

// Three minutes, matching the sibling Claude tray tool's default. Reading a
// quota costs no quota, so the only cost is request volume against six
// undocumented endpoints — and one of them answers 429 if pushed.
const POLL_MS = 180_000;

// Defensive floor for duplicate lifecycle events or future scheduler changes.
// The visible countdown is the only normal refresh trigger.
const MIN_GAP_MS = 20_000;

// Per provider, and escalating: a failure that repeats usually needs time, not
// another attempt. One rate-limited provider must never slow the others.
const BACKOFF_MS = [60_000, 120_000, 300_000];

// The reference Claude monitor stops all network traffic after five minutes
// without keyboard/mouse input, and immediately on workstation lock.
const IDLE_PAUSE_SECONDS = 5 * 60;

// Let Windows networking and provider apps settle before Claude checks resume.
// This avoids racing Claude Desktop/Code immediately after a long sleep.
const CLAUDE_RESUME_GRACE_MS = 120_000;

// "This account has no such quota" only changes if someone subscribes, so there
// is nothing to gain from asking every three minutes.
const UNAVAILABLE_RECHECK_MS = 1_800_000;

// A few points of slack before calling a window "ahead of pace", so a row does
// not flicker in and out of the warning as the clock ticks.
const PACE_TOLERANCE = 4;

const THEME_KEY = "quota-deck-theme";

const providersEl = document.getElementById("providers");
const providerControlsEl = document.getElementById("provider-controls");
const updatedEl = document.getElementById("updated");
const countdownEl = document.getElementById("countdown");
const widgetModeEl = document.getElementById("widget-mode");
const stripModeEl = document.getElementById("strip-mode");
const themeEl = document.getElementById("theme");

let widgetPreferences = {
  visible: false,
  locked: false,
  strip: false,
  hidden_providers: [],
  usage_display: "used",
};

const nowSec = () => Math.floor(Date.now() / 1000);

// ── Theme ───────────────────────────────────────────────────────────────────

const darkQuery = matchMedia("(prefers-color-scheme: dark)");

themeEl.addEventListener("click", () => {
  const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  localStorage.setItem(THEME_KEY, next);
  document.documentElement.dataset.theme = next;
  void emitTo("widget", "widget-theme-changed", next).catch(() => {});
});

// Follow the system only while the user has expressed no preference of their own.
darkQuery.addEventListener("change", (event) => {
  if (localStorage.getItem(THEME_KEY)) return;
  document.documentElement.dataset.theme = event.matches ? "dark" : "light";
  void emitTo("widget", "widget-theme-changed", document.documentElement.dataset.theme).catch(
    () => {},
  );
});

// ── Companion views ──────────────────────────────────────────────────────────

function applyWidgetPreferences(preferences) {
  widgetPreferences = { ...widgetPreferences, ...preferences };
  const widgetActive = widgetPreferences.visible && !widgetPreferences.strip;
  const stripActive = widgetPreferences.visible && widgetPreferences.strip;
  widgetModeEl.setAttribute("aria-pressed", widgetActive.toString());
  stripModeEl.setAttribute("aria-pressed", stripActive.toString());
  widgetModeEl.title = widgetActive ? "返回主面板" : "显示桌面小窗";
  stripModeEl.title = stripActive ? "返回主面板" : "显示长条";
}

async function setDisplayMode(mode, source) {
  try {
    applyWidgetPreferences(await invoke("set_display_mode", { mode }));
  } catch (error) {
    source.title = `无法切换显示模式：${error}`;
  }
}

widgetModeEl.addEventListener("click", () => {
  const active = widgetPreferences.visible && !widgetPreferences.strip;
  void setDisplayMode(active ? "dashboard" : "widget", widgetModeEl);
});

stripModeEl.addEventListener("click", () => {
  const active = widgetPreferences.visible && widgetPreferences.strip;
  void setDisplayMode(active ? "dashboard" : "strip", stripModeEl);
});

// ── Formatting ──────────────────────────────────────────────────────────────

function duration(seconds) {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days) return `${days}天${hours}小时`;
  if (hours) return `${hours}小时${minutes}分钟`;
  if (minutes) return `${minutes}分钟`;
  return `${seconds}秒`;
}

function untilReset(resetsAt) {
  if (!resetsAt) return null;
  const left = resetsAt - nowSec();
  return left <= 0 ? "正在重置" : `${duration(left)}后重置`;
}

function ago(unixSeconds) {
  const past = nowSec() - unixSeconds;
  return past < 60 ? "刚刚" : `${duration(past)}前`;
}

function updateAge(date) {
  const seconds = Math.max(0, Math.floor((Date.now() - date.getTime()) / 1000));
  if (seconds < 60) return "刚刚";

  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}分钟前`;

  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}小时前`;

  const days = Math.floor(hours / 24);
  return `${days}天前`;
}

/** The provider's own severity wins where it offers one — it knows what "close
 *  to the limit" means for a given plan better than a threshold guessed here.
 *  The 70/90 fallbacks match the sibling extensions. */
function meterVar(percent, severity) {
  if (severity === "critical" || severity === "severe") return "var(--crit)";
  if (severity === "warning") return "var(--warn)";
  if (severity === "normal") return percent >= 90 ? "var(--crit)" : "var(--ok)";
  if (percent >= 90) return "var(--crit)";
  if (percent >= 70) return "var(--warn)";
  return "var(--ok)";
}

/** How far through the window we are, as a percentage. Needs both a duration
 *  and an end; a row lacking either gets no pace mark. */
function pacePercent(window_) {
  const { window_seconds: length, resets_at: end } = window_;
  if (!length || !end) return null;
  const elapsed = (nowSec() - (end - length)) / length;
  return Math.min(100, Math.max(0, elapsed * 100));
}

// ── Rendering ───────────────────────────────────────────────────────────────

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function renderWindow(window_) {
  const percent = displayedPercent(window_.percent, widgetPreferences.usage_display);
  const pace = pacePercent(window_);
  const displayedPace =
    pace === null ? null : displayedPercent(pace, widgetPreferences.usage_display);

  const row = el("div", "row");
  row.style.setProperty("--meter", meterVar(window_.percent, window_.severity));
  row.style.setProperty("--fill", `${percent}%`);

  const head = el("div", "row-head");
  head.append(el("span", "row-label", window_.label));

  const figure = el("span", "row-figure", Math.round(percent).toString());
  figure.append(el("span", "unit", `% ${usageDisplayLabel(widgetPreferences.usage_display)}`));
  head.append(figure);
  row.append(head);

  const track = el("div", "track");
  track.append(el("div", "track-fill"));
  if (displayedPace !== null) {
    const mark = el("div", "track-pace");
    mark.style.setProperty("--pace-at", `${displayedPace}%`);
    track.append(mark);
  }
  row.append(track);

  const reset = untilReset(window_.resets_at);
  if (reset) {
    const note = el("p", "row-note", reset);
    if (pace !== null && window_.percent > pace + PACE_TOLERANCE) {
      note.append(document.createTextNode(" · "));
      note.append(el("span", "ahead", "消耗偏快"));
    }
    row.append(note);
  }

  return row;
}

function renderProvider(provider, quota) {
  const section = el("section", "provider");

  const head = el("div", "provider-head");
  head.append(el("h2", "provider-name", provider.name));

  if (quota?.status === "ok" && quota.plan) {
    head.append(el("span", "plan", quota.plan));
  }
  if (quota?.status === "ok" && quota.stale) {
    head.append(el("span", "badge", "缓存"));
  }
  section.append(head);

  if (!quota) {
    section.append(el("p", "note", "读取中…"));
    return section;
  }

  // Nothing is wrong here — the account just has no quota of this kind. Say it
  // plainly and quietly; most people who install the deck will see at least one.
  if (quota.status === "unavailable") {
    section.append(el("p", "note quiet", quota.message));
    return section;
  }

  // Another application owns this credential and must renew it. Treating this
  // as a transient error would promise retries that cannot possibly help.
  if (quota.status === "action_required") {
    section.append(el("p", "note", quota.message));
    return section;
  }

  if (quota.status === "error") {
    // The Rust side phrases these as something to do, not as a stack trace.
    const note = el("p", "note", quota.message);
    // A provider we are deliberately leaving alone is waiting, not broken. Say
    // which, or the card looks stuck.
    const waitMs = slot(provider.id).backoffUntil - Date.now();
    if (waitMs > 0) {
      note.append(document.createTextNode(`（${duration(Math.ceil(waitMs / 1000))}后重试）`));
    }
    section.append(note);
    return section;
  }

  for (const window_ of quota.windows) {
    section.append(renderWindow(window_));
  }

  if (quota.breakdown?.length) {
    const list = el("ul", "slices");
    for (const slice of quota.breakdown) {
      const item = el("li", null, slice.label);
      item.append(el("span", null, `${Math.round(slice.percent)}%`));
      list.append(item);
    }
    section.append(list);
  }

  if (quota.stale) {
    const when = quota.stale.observed_at ? ago(quota.stale.observed_at) : "时间未知";
    const note = el("p", "note stale", `来自${quota.stale.source} · ${when}`);
    if (quota.stale.reason) {
      note.append(document.createElement("br"), document.createTextNode(quota.stale.reason));
      const waitMs = slot(provider.id).backoffUntil - Date.now();
      if (waitMs > 0) {
        note.append(
          document.createTextNode(`（${duration(Math.ceil(waitMs / 1000))}后重试）`),
        );
      }
    }
    section.append(note);
  }

  return section;
}

function renderEmptyState(checked, allHidden) {
  const section = el("section", "provider-empty");
  if (allHidden) {
    section.append(el("h2", null, "已隐藏全部来源"));
    section.append(el("p", null, "在上方「AI 来源与显示」里可以重新打开。"));
    return section;
  }
  section.append(el("h2", null, checked ? "没有检测到任何来源" : "正在检测来源…"));
  section.append(
    el(
      "p",
      null,
      checked
        ? "先运行一次 Claude Code、Codex 或 Antigravity IDE，或为 Gemini 和 Grok 安装可选的 Browser Bridge 扩展。"
        : "正在查找 Claude Code、Codex、Antigravity、Gemini 和 Grok 已有的登录。",
    ),
  );
  return section;
}

function renderProviderControls(missingProviders) {
  providerControlsEl.replaceChildren();

  const activeIds = new Set(activeProviders().map(({ id }) => id));
  const displayName = widgetPreferences.usage_display === "remaining" ? "剩余" : "已用";
  const button = el("button", "sources-trigger");
  button.type = "button";
  button.setAttribute("aria-expanded", sourcesExpanded.toString());
  button.setAttribute("aria-controls", "source-picker");
  button.setAttribute(
    "aria-label",
    `选择 AI 来源和额度显示方式。已显示 ${activeIds.size}/${PROVIDERS.length} 个来源，数值显示${displayName}额度。`,
  );

  const orbit = el("span", "source-orbit");
  orbit.setAttribute("aria-hidden", "true");
  for (const provider of PROVIDERS) {
    const dot = el("span", "source-dot");
    dot.dataset.source = provider.id;
    if (!activeIds.has(provider.id)) dot.classList.add("is-hidden");
    orbit.append(dot);
  }

  const copy = el("span", "sources-trigger-copy");
  copy.append(
    el("span", "sources-trigger-label", "AI 来源与显示"),
    el(
      "span",
      "sources-trigger-meta",
      `已显示 ${activeIds.size}/${PROVIDERS.length} · ${displayName}`,
    ),
  );
  button.append(orbit, copy, el("span", "sources-chevron"));
  button.addEventListener("click", () => {
    sourcesExpanded = !sourcesExpanded;
    render();
    if (sourcesExpanded) {
      document.querySelector(".source-toggle input")?.focus();
    }
  });
  providerControlsEl.append(button);

  if (!sourcesExpanded) return;

  const panel = el("div", "source-picker");
  panel.id = "source-picker";
  const heading = el("div", "source-picker-head");
  heading.append(
    el("strong", null, "显示的来源"),
    el("span", null, "隐藏的来源不会去查询。"),
  );
  panel.append(heading);

  const list = el("div", "source-list");
  for (const provider of PROVIDERS) {
    const visible = activeIds.has(provider.id);
    const item = el("label", "source-option");
    item.dataset.source = provider.id;
    if (!visible) item.classList.add("is-hidden");

    const mark = el("span", "source-mark", provider.mark ?? provider.name.slice(0, 1));
    mark.dataset.source = provider.id;
    mark.setAttribute("aria-hidden", "true");

    const details = el("span", "source-copy");
    const name = el("span", "source-name", provider.name);
    if (provider.optional) name.append(el("span", "source-optional", "扩展"));
    details.append(name, el("span", "source-status", providerStatus(provider, visible)));

    const toggle = el("span", "source-toggle");
    const checkbox = el("input");
    checkbox.type = "checkbox";
    checkbox.checked = visible;
    checkbox.dataset.provider = provider.id;
    checkbox.setAttribute("aria-label", `显示 ${provider.name}`);
    checkbox.addEventListener("change", () => {
      void setProviderHidden(provider, !checkbox.checked, checkbox);
    });
    toggle.append(checkbox, el("span", "source-switch"));
    item.append(mark, details, toggle);
    list.append(item);
  }
  panel.append(list);
  panel.append(renderUsageDisplay());
  if (bridgePath && missingProviders.some(({ optional }) => optional)) {
    panel.append(renderBridgeHelp());
  }
  providerControlsEl.append(panel);
}

function renderUsageDisplay() {
  const row = el("div", "usage-display-option");
  const copy = el("span", "usage-display-copy");
  copy.append(
    el("strong", null, "额度数值"),
    el("span", null, "显示已用掉的额度，还是剩余的额度。"),
  );

  const choices = el("div", "usage-display-choices");
  choices.setAttribute("role", "radiogroup");
  choices.setAttribute("aria-label", "额度数值显示方式");
  for (const [mode, label] of [
    ["used", "已用"],
    ["remaining", "剩余"],
  ]) {
    const choice = el("label", "usage-display-choice");
    const input = el("input");
    input.type = "radio";
    input.name = "usage-display";
    input.value = mode;
    input.checked = widgetPreferences.usage_display === mode;
    input.dataset.usageDisplay = mode;
    input.addEventListener("change", () => {
      if (input.checked) void setUsageDisplay(mode, input);
    });
    choice.append(input, el("span", null, label));
    choices.append(choice);
  }
  row.append(copy, choices);
  return row;
}

async function setUsageDisplay(mode, input) {
  try {
    applyWidgetPreferences(await invoke("set_usage_display", { mode }));
  } catch (error) {
    input.checked = false;
    const current = document.querySelector(
      `[data-usage-display="${widgetPreferences.usage_display}"]`,
    );
    if (current) current.checked = true;
    input.title = `无法切换额度显示：${error}`;
    input.focus();
    return;
  }
  render();
  document.querySelector(`[data-usage-display="${mode}"]`)?.focus();
}

function providerStatus(provider, visible) {
  if (!visible) return "已隐藏 · 暂停查询";
  const quota = results[provider.id];
  if (!quota) return "正在检查连接…";
  if (quota.status === "not_configured") return provider.setup;
  if (quota.status === "error") return "已显示 · 正在自动重试";
  if (quota.status === "action_required") return "已显示 · 需要处理";
  if (quota.status === "unavailable") return "已显示 · 没有可用额度";
  return "在主面板、小窗和长条中显示";
}

async function setProviderHidden(provider, hidden, checkbox) {
  try {
    applyWidgetPreferences(await invoke("set_provider_hidden", { id: provider.id, hidden }));
  } catch (error) {
    checkbox.checked = !hidden;
    checkbox.title = `无法切换来源显示：${error}`;
    return;
  }
  render();
  document.querySelector(`[data-provider="${provider.id}"]`)?.focus();
  if (hidden) return;

  // Go through fetchProvider directly rather than refresh([provider]): the
  // cycle-level inFlight guard would silently drop a re-tick made while a
  // cycle is running. The provider's own floor and backoff gates still decide
  // whether a request is actually sent.
  const attempted = await fetchProvider(provider);
  if (attempted) noteProviderObservation(results[provider.id]);
  render();
  document.querySelector(`[data-provider="${provider.id}"]`)?.focus();
}

// The one thing the setup steps cannot be written down without: where the app
// actually staged the bridge. The MSI and NSIS bundles install to different
// roots, so this has to be asked for rather than assumed.
function renderBridgeHelp() {
  const block = el("div", "bridge-help");
  block.append(
    el(
      "p",
      "bridge-step",
      "Browser Bridge 扩展：打开 chrome://extensions，开启「开发者模式」，" +
        "点「加载已解压的扩展程序」并选中这个文件夹，然后点一次扩展图标。",
    ),
    el("code", "bridge-path", bridgePath),
  );

  const copy = el("button", "bridge-action", "复制路径");
  copy.type = "button";
  copy.addEventListener("click", async () => {
    let label = "已复制";
    try {
      await navigator.clipboard.writeText(bridgePath);
    } catch {
      label = "复制失败";
    }
    copy.textContent = label;
    setTimeout(() => (copy.textContent = "复制路径"), 1500);
  });

  const open = el("button", "bridge-action", "打开文件夹");
  open.type = "button";
  open.addEventListener("click", async () => {
    try {
      await invoke("reveal_bridge_dir");
    } catch {
      open.textContent = "无法打开";
      setTimeout(() => (open.textContent = "打开文件夹"), 1500);
    }
  });

  const actions = el("div", "bridge-actions");
  actions.append(copy, open);
  block.append(actions);
  return block;
}

// The providers the user has left ticked. Only these are scheduled or drawn;
// a hidden provider keeps its results and schedule slot untouched.
function activeProviders() {
  return enabledProviders(PROVIDERS, widgetPreferences.hidden_providers);
}

function allProvidersChecked() {
  return activeProviders().every(({ id }) => results[id]);
}

function configuredProviders() {
  return activeProviders().filter(
    ({ id }) => results[id] && results[id].status !== "not_configured",
  );
}

function render() {
  renderProviderResults();
  const active = activeProviders();
  const checked = allProvidersChecked();
  const missing = checked
    ? active.filter(({ id }) => results[id].status === "not_configured")
    : [];
  renderProviderControls(missing);
}

// Provider calls finish independently. Update cards and the companion as each
// one lands without rebuilding the source picker under a user's keyboard focus.
function renderProviderResults() {
  const active = activeProviders();
  const checked = allProvidersChecked();
  const configured = configuredProviders();

  providersEl.replaceChildren(
    ...(configured.length
      ? configured.map((provider) => renderProvider(provider, results[provider.id]))
      : [renderEmptyState(checked, active.length === 0)]),
  );
  publishWidgetSnapshot();
}

function noteProviderObservation(result) {
  const observedAt = providerObservationMs(result);
  if (observedAt == null) return;
  if (!lastUpdatedAt || observedAt > lastUpdatedAt.getTime()) {
    lastUpdatedAt = new Date(observedAt);
  }
}

function publishWidgetSnapshot() {
  const backoffUntil = Object.fromEntries(
    PROVIDERS.map(({ id }) => [id, schedule[id]?.backoffUntil ?? 0]),
  );
  void emitTo("widget", "quota-snapshot", {
    results,
    backoffUntil,
    updatedAt: lastUpdatedAt?.getTime() ?? null,
    theme: document.documentElement.dataset.theme,
  }).catch(() => {});
}

// ── Polling ─────────────────────────────────────────────────────────────────

const results = {};
const schedule = {};
let inFlight = false;
let lastUpdatedAt = null;
let nextRefreshAt = null;
let refreshTimer = null;
let sourcesExpanded = false;
let bridgePath = null;

document.addEventListener("pointerdown", (event) => {
  if (!sourcesExpanded || providerControlsEl.contains(event.target)) return;
  sourcesExpanded = false;
  render();
});

document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape" || !sourcesExpanded) return;
  sourcesExpanded = false;
  render();
  document.querySelector(".sources-trigger")?.focus();
});

function slot(id) {
  return (schedule[id] ??= {
    lastAttempt: 0,
    backoffUntil: 0,
    recheckAfter: 0,
    failures: 0,
    retryingRateLimit: false,
  });
}

async function fetchProvider(provider, { userAway = false } = {}) {
  const state = slot(provider.id);
  const now = Date.now();

  if (provider.pauseWhileAway && userAway) return false;

  if (provider.id === "claude") {
    try {
      const issue = applyClaudeLoginStatus(state, await invoke("claude_login_status"));
      if (issue) {
        results[provider.id] = { status: "action_required", provider: "claude", message: issue };
        return true;
      }
    } catch {
      // A local metadata read failure must not bypass the HTTP cooldown.
    }
  }

  // A provider that asked us to slow down stays untouched until its backoff ends.
  if (now < state.backoffUntil) return false;
  if (now < state.recheckAfter) return false;
  const providerFloor = providerRequestFloor(provider, state.retryingRateLimit, MIN_GAP_MS);
  if (now - state.lastAttempt < providerFloor) return false;
  state.lastAttempt = now;

  try {
    results[provider.id] = await invoke(provider.command);
  } catch (error) {
    // A command that throws is a bug in the deck, not a provider outage; say so
    // rather than blaming the vendor.
    results[provider.id] = {
      status: "error",
      message: `本程序无法运行 ${provider.name} 查询：${error}`,
    };
  }

  const status = results[provider.id]?.status;
  const retryAfterSeconds = results[provider.id]?.retry_after_seconds;
  const rateLimited =
    typeof retryAfterSeconds === "number" && Number.isFinite(retryAfterSeconds);
  state.backoffUntil = 0;
  state.recheckAfter = 0;

  const retryDelay = providerRetryDelay(
    results[provider.id],
    provider,
    state.failures,
    BACKOFF_MS,
  );

  if (retryDelay !== null) {
    state.backoffUntil = Date.now() + retryDelay;
    state.failures += 1;
    state.retryingRateLimit = rateLimited;
  } else {
    // An unavailable quota is a settled answer, not a failure: no escalation,
    // but no point asking again soon either.
    if (status === "unavailable") state.recheckAfter = Date.now() + UNAVAILABLE_RECHECK_MS;
    state.failures = 0;
    state.retryingRateLimit = false;
  }
  return true;
}

function updateRefreshStatus() {
  if (activeProviders().length === 0) {
    updatedEl.textContent = "已隐藏全部来源";
    countdownEl.textContent = "";
    return;
  }

  const checked = allProvidersChecked();
  const hasConfiguredProvider = configuredProviders().length > 0;

  if (checked && !hasConfiguredProvider) {
    updatedEl.textContent = "等待来源接入";
    if (inFlight) {
      countdownEl.textContent = "正在检查来源…";
      return;
    }
    if (!nextRefreshAt) {
      countdownEl.textContent = "已安排来源检查";
      return;
    }
    const seconds = Math.max(0, Math.ceil((nextRefreshAt - Date.now()) / 1000));
    countdownEl.textContent = `${duration(seconds)}后再次检查`;
    return;
  }

  updatedEl.textContent = lastUpdatedAt
    ? `更新于 ${updateAge(lastUpdatedAt)}`
    : "更新于 —";

  if (inFlight) {
    countdownEl.textContent = "刷新中…";
    return;
  }
  if (!nextRefreshAt) {
    countdownEl.textContent = "已安排刷新";
    return;
  }
  const seconds = Math.max(0, Math.ceil((nextRefreshAt - Date.now()) / 1000));
  countdownEl.textContent = `${duration(seconds)}后刷新`;
}

async function refresh(providers = activeProviders(), { markUpdated = true } = {}) {
  if (inFlight) return;
  inFlight = true;
  updateRefreshStatus();

  const checksActivity = providers.some((provider) => provider.pauseWhileAway);
  const activity = checksActivity ? await invoke("system_activity").catch(() => null) : null;
  const userAway = isUserAway(activity, IDLE_PAUSE_SECONDS);
  await settleProviders(
    providers,
    (provider) => fetchProvider(provider, { userAway }),
    (provider) => {
      if (markUpdated) noteProviderObservation(results[provider.id]);
      renderProviderResults();
      updateRefreshStatus();
    },
  );

  render();
  inFlight = false;
  updateRefreshStatus();
}

function scheduleNextRefresh() {
  clearTimeout(refreshTimer);
  nextRefreshAt = Date.now() + POLL_MS;
  const scheduledAt = nextRefreshAt;
  updateRefreshStatus();
  refreshTimer = setTimeout(async () => {
    if (missedRefreshCycle(Date.now(), scheduledAt, POLL_MS)) deferClaudeAfterResume();
    await refresh();
    scheduleNextRefresh();
  }, POLL_MS);
}

await listen("widget-ready", publishWidgetSnapshot);
await listen("widget-preferences-changed", ({ payload }) => {
  applyWidgetPreferences(payload);
  render();
});
applyWidgetPreferences(await invoke("widget_preferences").catch(() => widgetPreferences));

render();
// Resolved once: the staging path only changes if the user moves their profile.
// A failure here just hides the copy/open shortcuts; the README still has the path.
bridgePath = await invoke("bridge_dir").catch(() => null);

// Rust emits this tick outside the WebView. It recovers an overdue global cycle
// when Chromium throttles the hidden dashboard and still lets Claude retry an
// expired 429 independently between those cycles.
function refreshFromNativeTick() {
  if (inFlight) return;
  if (nextRefreshAt && Date.now() >= nextRefreshAt) {
    void (async () => {
      await refresh();
      scheduleNextRefresh();
    })();
    return;
  }
  const claude = activeProviders().find(({ id }) => id === "claude");
  if (claude) void refresh([claude], { markUpdated: false });
}

function deferClaudeAfterResume() {
  const state = slot("claude");
  state.recheckAfter = resumeGraceDeadline(
    Date.now(),
    state.recheckAfter,
    CLAUDE_RESUME_GRACE_MS,
  );
}

await listen("system-activity-resumed", deferClaudeAfterResume);
await listen("active-refresh-tick", refreshFromNativeTick);

await refresh();
scheduleNextRefresh();
setInterval(updateRefreshStatus, 1000);

// Countdowns and backoff timers are relative, so they go stale between polls
// even when the data does not.
setInterval(() => {
  // A render rebuilds the providers panel, so it would take focus off a
  // checkbox someone is on. Relative times can wait for the next tick.
  if (providerControlsEl.contains(document.activeElement)) return;
  if (Object.keys(results).length) render();
}, 30_000);

// Chromium may throttle this WebView while the tray window is hidden. On
  // reveal, refresh browser-backed caches, notice an Antigravity IDE or Grok
  // Bot sign-in that appeared since, and let Claude catch up after an idle or
  // locked stretch.
// Provider floors and backoff still suppress duplicate calls.
function resumeProviders() {
  return activeProviders().filter(
    ({ id }) =>
      id === "claude" ||
      id === "antigravity" ||
      id === "gemini" ||
      id === "grok" ||
      id === "grok_bot",
  );
}

function refreshAfterReveal() {
  if (document.hidden) return;
  if (missedRefreshCycle(Date.now(), nextRefreshAt, POLL_MS)) deferClaudeAfterResume();
  void refresh(resumeProviders(), { markUpdated: false });
}

document.addEventListener("visibilitychange", refreshAfterReveal);
window.addEventListener("focus", refreshAfterReveal);
