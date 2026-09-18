/**
 * 三个统计页（模型用量 / Token / 积分）共用的**展示**格式化。
 *
 * 规则：
 * - 只做展示换算（数量级缩写、日期布局），**不做任何口径换算**
 *   （token ↔ 积分、调用次数口径等一律在上游决定）；
 * - 输出必须与三页原先各自的本地实现**逐字符一致**——函数从页面原样搬迁而来，
 *   页面侧用 `import { x as 原名 }` 保持调用点不动。
 */

/** 千分位分组（整数不带小数，小数最多 2 位）。 */
export function formatGroupedNumber(value: number | undefined | null): string {
  const v = value ?? 0;
  return Number.isInteger(v)
    ? v.toLocaleString("en-US")
    : v.toLocaleString("en-US", { maximumFractionDigits: 2 });
}

/**
 * 紧凑数量级：≥1亿 → `X.X亿`，≥1万 → `X.X万`，其余千分位分组。
 * 非有限值与 0 一律输出 `0`。
 */
export function formatCompactNumber(value: number): string {
  const v = value ?? 0;
  const abs = Math.abs(v);
  const sign = v < 0 ? "-" : "";
  if (!Number.isFinite(abs) || abs === 0) return "0";
  if (abs >= 100_000_000) return `${sign}${(abs / 100_000_000).toFixed(1)}亿`;
  if (abs >= 10_000) return `${sign}${(abs / 10_000).toFixed(1)}万`;
  return `${sign}${formatGroupedNumber(abs)}`;
}

/** `MM/DD HH:mm:ss`（调用明细的时间列）。 */
export function formatClock(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/** 比率 → 百分比字符串（空值按 0 处理）。 */
export function formatRatioPercent(ratio: number | undefined | null, digits = 1): string {
  return `${((ratio ?? 0) * 100).toFixed(digits)}%`;
}

/** 延迟：`<1s` 显示 ms，否则显示 s（≥10s 去小数）。 */
export function formatLatency(ms: number | undefined | null): string {
  if (!ms) return "—";
  return ms >= 1000 ? `${(ms / 1000).toFixed(ms >= 10_000 ? 0 : 1)}s` : `${Math.round(ms)}ms`;
}

/** `MM/DD HH:mm`（zh-CN；空值显示 `—`）。 */
export function formatDateTimeShort(ts: number | null | undefined): string {
  if (!ts) return "—";
  return new Date(ts).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** `YYYY-MM-DD` → `MM/DD`（图表 X 轴标签）。 */
export function formatMonthDay(date: string): string {
  return date.slice(5).replace("-", "/");
}

/** `YYYY/MM/DD`（zh-CN；空值显示 `—`）。 */
export function formatDateCn(ts: number | null | undefined): string {
  if (ts === null || ts === undefined) return "—";
  return new Date(ts).toLocaleDateString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  });
}

/** 积分数值（zh-CN 分组，最多 2 位小数；空值显示 `—`）。 */
export function formatCredits(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return "—";
  return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 }).format(value);
}

/** 精确千分位（zh-CN，用于「次数」这类必须原样展示的整数）。 */
export const exactNumberFormatter = new Intl.NumberFormat("zh-CN");
