import {
  compactProviderName,
  compactWindowLabel,
  displayedPercent,
  quotaTone,
  usageDisplayLabel,
  visibleProviders,
  WIDGET_PROVIDERS,
  widgetWindows,
} from "./widget-model.js";

const { invoke } = window.__TAURI__.core;
const { emitTo, listen } = window.__TAURI__.event;

const providersEl = document.getElementById("widget-providers");
const statusEl = document.getElementById("widget-status");
const dragHandleEl = document.getElementById("widget-drag-handle");
const widgetEl = document.querySelector(".widget");
const openDashboardEl = document.getElementById("open-dashboard");
const taskbarOverlayEl = document.getElementById("taskbar-overlay");
const lockWidgetEl = document.getElementById("lock-widget");
const hideWidgetEl = document.getElementById("hide-widget");

let snapshot = { results: {}, backoffUntil: {}, updatedAt: null };
let preferences = {
  visible: false,
  locked: false,
  strip: false,
  taskbar_overlay: false,
  hidden_providers: [],
  usage_display: "used",
};
let resizeFrame = null;

const nowSec = () => Math.floor(Date.now() / 1000);

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function duration(seconds) {
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);
  if (days) return `${days}天${hours}小时`;
  if (hours) return `${hours}小时${minutes}分钟`;
  if (minutes) return `${minutes}分钟`;
  return `${Math.max(0, seconds)}秒`;
}

function age(timestampMs) {
  if (!timestampMs) return "等待数据";
  const seconds = Math.max(0, Math.floor((Date.now() - timestampMs) / 1_000));
  if (seconds < 60) return "刚刚更新";
  return `${duration(seconds)}前更新`;
}

function staleDetails(providerId, quota) {
  if (!quota?.stale) return null;
  const details = ["缓存数据"];
  if (quota.stale.observed_at) {
    details[0] = `缓存 · ${duration(Math.max(0, nowSec() - quota.stale.observed_at))}前`;
  }
  if (quota.stale.reason) details.push(quota.stale.reason);
  const waitMs = (snapshot.backoffUntil[providerId] ?? 0) - Date.now();
  if (waitMs > 0) details.push(`${duration(Math.ceil(waitMs / 1_000))}后重试`);
  return details.join(" · ");
}

function renderMetric(window_) {
  const metric = el("span", `widget-metric ${quotaTone(window_.percent, window_.severity)}`);
  const percent = displayedPercent(window_.percent, preferences.usage_display);
  const displayLabel = usageDisplayLabel(preferences.usage_display);
  const reset = window_.resets_at
    ? window_.resets_at > nowSec()
      ? ` · ${duration(window_.resets_at - nowSec())}后重置`
      : " · 正在重置"
    : "";
  metric.title = `${window_.label}：${Math.round(percent)}% ${displayLabel}${reset}`;
  metric.append(
    el("span", "metric-period", compactWindowLabel(window_)),
    el("span", "metric-value", `${Math.round(percent)}%`),
  );
  return metric;
}

function renderProvider(provider, quota) {
  const row = el("section", `widget-provider${quota?.stale ? " cached" : ""}`);
  const identity = el("div", "provider-identity");
  const name = el("span", "provider-name", compactProviderName(provider, preferences.strip));
  name.title = quota?.plan ? `${provider.name} · ${quota.plan}` : provider.name;
  identity.append(name);

  const stale = staleDetails(provider.id, quota);
  if (stale) {
    const cached = el("span", "cached-label", "缓存");
    cached.title = stale;
    identity.append(cached);
    row.title = stale;
  }
  row.append(identity);

  const metrics = el("div", "widget-metrics");
  if (!quota) {
    metrics.append(el("span", "provider-state", "…"));
  } else if (quota.status !== "ok") {
    const quiet = quota.status === "unavailable" || quota.status === "not_configured";
    const state = el("span", `provider-state${quiet ? "" : " problem"}`, quiet ? "—" : "!");
    if (quota.message) state.title = quota.message;
    metrics.append(state);
  } else {
    const windows = widgetWindows(provider.id, quota.windows);
    if (!windows.length) {
      const missing = el("span", "widget-metric unavailable");
      missing.title = `${provider.name} 不提供 7 天额度。`;
      missing.append(el("span", "metric-period", "7d"), el("span", "metric-value", "—"));
      metrics.append(missing);
    } else {
      metrics.append(...windows.map(renderMetric));
    }
  }
  row.append(metrics);
  return row;
}

function scheduleResize() {
  if (resizeFrame !== null) cancelAnimationFrame(resizeFrame);
  resizeFrame = requestAnimationFrame(() => {
    resizeFrame = null;
    const width = preferences.strip ? stripContentWidth() : widgetContentWidth();
    const height = preferences.strip
      ? 40
      : Math.ceil(widgetEl.getBoundingClientRect().height + 12);
    void invoke("resize_widget", { width, height }).catch(() => {});
  });
}

function number(value) {
  return Number.parseFloat(value) || 0;
}

function horizontalExtras(node) {
  const style = getComputedStyle(node);
  return (
    number(style.paddingLeft) +
    number(style.paddingRight) +
    number(style.borderLeftWidth) +
    number(style.borderRightWidth)
  );
}

function visibleChildrenWidth(node) {
  const children = [...node.children].filter(
    (child) => getComputedStyle(child).display !== "none",
  );
  const style = getComputedStyle(node);
  const gap = number(style.columnGap || style.gap);
  return children.reduce((total, child) => {
    const childStyle = getComputedStyle(child);
    return (
      total +
      Math.max(child.scrollWidth, child.getBoundingClientRect().width) +
      number(childStyle.marginLeft) +
      number(childStyle.marginRight)
    );
  }, Math.max(0, children.length - 1) * gap);
}

function metricsWidth(metrics) {
  const metricsStyle = getComputedStyle(metrics);
  const metricsNodes = [...metrics.children];
  const gap = number(metricsStyle.columnGap || metricsStyle.gap);
  return metricsNodes.reduce(
    (total, metric) =>
      total +
      (metric.children.length
        ? visibleChildrenWidth(metric)
        : Math.max(metric.scrollWidth, metric.getBoundingClientRect().width)),
    Math.max(0, metricsNodes.length - 1) * gap,
  );
}

function widgetContentWidth() {
  const headWidth = horizontalExtras(dragHandleEl) + visibleChildrenWidth(dragHandleEl);
  const rowWidths = [...providersEl.querySelectorAll(".widget-provider")].map((row) => {
    const style = getComputedStyle(row);
    const identityColumn = number(style.gridTemplateColumns.split(" ")[0]);
    return (
      horizontalExtras(row) +
      identityColumn +
      number(style.columnGap) +
      metricsWidth(row.querySelector(".widget-metrics"))
    );
  });
  const contentWidth = Math.max(headWidth, ...rowWidths, 188);
  return Math.ceil(contentWidth + horizontalExtras(widgetEl) + horizontalExtras(document.body));
}

function stripContentWidth() {
  const providerWidth = [...providersEl.querySelectorAll(".widget-provider")].reduce(
    (total, row) => {
      const style = getComputedStyle(row);
      return (
        total +
        horizontalExtras(row) +
        visibleChildrenWidth(row.querySelector(".provider-identity")) +
        number(style.columnGap) +
        metricsWidth(row.querySelector(".widget-metrics"))
      );
    },
    0,
  );
  const controlsWidth = horizontalExtras(dragHandleEl) + visibleChildrenWidth(dragHandleEl);
  return Math.ceil(
    providerWidth +
      controlsWidth +
      horizontalExtras(widgetEl) +
      horizontalExtras(document.body),
  );
}

function render() {
  const configured = visibleProviders(
    WIDGET_PROVIDERS,
    snapshot.results,
    preferences.hidden_providers,
  );
  const allHidden = WIDGET_PROVIDERS.every(({ id }) => preferences.hidden_providers.includes(id));
  providersEl.replaceChildren(
    ...(configured.length
      ? configured.map((provider) => renderProvider(provider, snapshot.results[provider.id]))
      : [el("p", "widget-empty", allHidden ? "已隐藏全部来源" : "没有检测到任何来源")]),
  );
  statusEl.textContent = age(snapshot.updatedAt);
  scheduleResize();
}

function applyPreferences(next) {
  preferences = { ...preferences, ...next };
  document.documentElement.dataset.locked = preferences.locked.toString();
  document.documentElement.dataset.mode = preferences.strip ? "strip" : "widget";
  document.documentElement.dataset.taskbarOverlay = preferences.taskbar_overlay.toString();
  widgetEl.setAttribute(
    "aria-label",
    preferences.strip ? "AI 额度长条" : "AI 额度小窗",
  );
  lockWidgetEl.title = preferences.locked ? "解锁小窗位置" : "锁定小窗位置";
  lockWidgetEl.setAttribute(
    "aria-label",
    preferences.locked ? "解锁小窗位置" : "锁定小窗位置",
  );
  taskbarOverlayEl.title = preferences.taskbar_overlay
    ? "不再让长条停在任务栏上方"
    : "让长条停在任务栏上方";
  taskbarOverlayEl.setAttribute("aria-label", taskbarOverlayEl.title);
  taskbarOverlayEl.setAttribute("aria-pressed", preferences.taskbar_overlay.toString());
  hideWidgetEl.title = preferences.strip ? "隐藏长条" : "隐藏小窗";
  hideWidgetEl.setAttribute("aria-label", hideWidgetEl.title);
  render();
}

widgetEl.addEventListener("pointerdown", (event) => {
  if (
    event.button !== 0 ||
    (!preferences.strip && preferences.locked) ||
    (!preferences.strip && !event.target.closest(".widget-head")) ||
    event.target.closest("button")
  )
    return;
  event.preventDefault();
  void invoke("start_widget_drag").catch(() => {});
});

widgetEl.addEventListener("dblclick", (event) => {
  if (event.target.closest("button")) return;
  // Strip uses its explicit dashboard button. Treating a quick drag/click as a
  // double-click could otherwise switch modes and make the strip appear to
  // vanish. Widget keeps the convenient header shortcut.
  if (preferences.strip || !event.target.closest(".widget-head")) return;
  void invoke("open_dashboard");
});

openDashboardEl.addEventListener("click", () => void invoke("open_dashboard"));
hideWidgetEl.addEventListener("click", () => void invoke("hide_companion"));
lockWidgetEl.addEventListener("click", () => {
  void invoke("set_widget_locked", { locked: !preferences.locked });
});

taskbarOverlayEl.addEventListener("click", () => {
  void invoke("set_taskbar_overlay", { enabled: !preferences.taskbar_overlay })
    .then(applyPreferences)
    .catch((error) => {
      taskbarOverlayEl.title = `无法切换任务栏置顶：${error}`;
    });
});

await listen("quota-snapshot", ({ payload }) => {
  snapshot = payload;
  if (payload.theme) document.documentElement.dataset.theme = payload.theme;
  render();
});
await listen("widget-theme-changed", ({ payload }) => {
  document.documentElement.dataset.theme = payload;
});
await listen("widget-preferences-changed", ({ payload }) => applyPreferences(payload));

applyPreferences(await invoke("widget_preferences").catch(() => preferences));
render();
await emitTo("main", "widget-ready").catch(() => {});
setInterval(() => {
  statusEl.textContent = age(snapshot.updatedAt);
  if (Object.keys(snapshot.results).length) render();
}, 30_000);
