/**
 * WorkBuddy 逆向渠道 · 独立监控面板（统计页第 4 个 Tab）。
 *
 * 为什么独立成页而不是并入「调用与模型」：那个页面服务的是 AutoClaw / Loomy /
 * TRAE Work CN 账号口径（按账号归因）；本页服务的是 **WorkBuddy 逆向渠道**
 * （`channel=workbuddy` / `workbuddy-native` 两条链路），两者的数据源、口径、
 * 失效模式都不同——混在一页会让「这是谁的数据」变模糊，故单开一页。
 *
 * 数据来源（**复用既有后端接口，未新增端点**）：
 * - `getGlmUsageSummary("workbuddy")`  → 按模型的调用数 / 失败数 / Token / 积分
 * - `getGlmUsageRecords(500,"workbuddy")` → 明细，用于链路拆分、失败归因、时间桶
 *
 * ⚠️ Token 口径（勿误读）：`workbuddy-native`（ACP 直连）链路上游**不返回 usage**，
 * 其 Token 恒为 0 且不是「消耗为 0」。本页显式区分「已上报 / 未上报」计数，
 * 覆盖率低是结构性的，不代表统计缺失。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertCircle, Loader2, RefreshCw } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { PoolHealthPanel } from "@/components/pool-health-panel";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type { GlmUsageRecord, GlmUsageStatistics } from "@/lib/types";
import {
  formatCompactNumber as formatTokens,
  formatGroupedNumber as formatNumber,
  formatLatency,
  formatClock as formatTime,
  formatRatioPercent as formatPercent,
} from "@/lib/stats-format";

/** 本页固定的渠道前缀（后端前缀匹配，一次拿全两条链路）。 */
const CHANNEL = "workbuddy";

/** 明细拉取上限（后端上限 500）。 */
const RECORD_LIMIT = 500;

type LinkKey = "native-acp" | "opencode-borrow";

const LINK_LABEL: Record<LinkKey, string> = {
  "native-acp": "ACP 直连",
  "opencode-borrow": "OpenCode 借道",
};

function classifyLink(record: GlmUsageRecord): LinkKey {
  return (record.channel ?? "").endsWith("native") ? "native-acp" : "opencode-borrow";
}

/**
 * 失败归因。顺序即优先级：先匹配更具体的模式。
 * 依据 `~/.wb-switch/glm_usage_records.json` 中 workbuddy* 渠道真实 error 样本归纳。
 */
const ERR_RULES: { re: RegExp; kind: string }[] = [
  { re: /无效的 API Key/i, kind: "invalid-key" },
  { re: /代理未启用/i, kind: "proxy-disabled" },
  { re: /messages 不能为空/i, kind: "empty-messages" },
  { re: /会话未隔离/i, kind: "session-not-isolated" },
  { re: /提交消息失败\s*500|Unexpected server error/i, kind: "upstream-500" },
  { re: /提交消息失败[\s\S]*error sending request|transport:|error sending request/i, kind: "transport" },
  { re: /429|rate.?limit/i, kind: "rate-limit" },
  { re: /timeout|timed out/i, kind: "timeout" },
  { re: /refresh_failed|re-login/i, kind: "auth" },
];

function classifyError(error: string | null | undefined): string {
  if (!error) return "unknown";
  for (const rule of ERR_RULES) {
    if (rule.re.test(error)) return rule.kind;
  }
  return "other";
}

/** 百分位（升序数组）。纯函数，便于用例与复用。 */
function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const idx = Math.min(sorted.length - 1, Math.round(p * (sorted.length - 1)));
  return sorted[idx];
}

type LinkStat = {
  calls: number;
  ok: number;
  fail: number;
  total: number;
  prompt: number;
  completion: number;
  reportedCalls: number;
  unreportedCalls: number;
  cost: number;
  latencies: number[];
};

function emptyLinkStat(): LinkStat {
  return {
    calls: 0, ok: 0, fail: 0, total: 0, prompt: 0, completion: 0,
    reportedCalls: 0, unreportedCalls: 0, cost: 0, latencies: [],
  };
}

function StatCell({ label, value, accent, title }: {
  label: string; value: string; accent?: boolean; title?: string;
}) {
  return (
    <div className="min-w-0">
      <p className="text-[11px] text-muted-foreground">{label}</p>
      <p
        className={cn(
          "truncate font-semibold tabular-nums leading-6",
          accent ? "text-primary" : "text-foreground",
        )}
        title={title ?? value}
      >
        {value}
      </p>
    </div>
  );
}

/** 横向占比条：宽度按 max 归一，最小可见宽度 2%。 */
function Bar({ value, max }: { value: number; max: number }) {
  const w = max > 0 ? Math.max(2, (value / max) * 100) : 0;
  return (
    <div className="mt-1.5 h-1.5 w-full overflow-hidden rounded-full bg-muted">
      <div className="h-full rounded-full bg-primary/70" style={{ width: `${w}%` }} />
    </div>
  );
}

export default function WorkBuddyMonitorPage() {
  const [summary, setSummary] = useState<GlmUsageStatistics | null>(null);
  const [records, setRecords] = useState<GlmUsageRecord[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [updatedAt, setUpdatedAt] = useState<number>(0);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [s, r] = await Promise.all([
        api.getGlmUsageSummary(CHANNEL),
        api.getGlmUsageRecords(RECORD_LIMIT, CHANNEL).catch(() => ({ records: [] })),
      ]);
      setSummary(s);
      setRecords(r.records ?? []);
      setUpdatedAt(Date.now());
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
    // 60s 轮询：本页是「持续监控」视图，无需手动刷新也能跟上。
    const timer = window.setInterval(() => { void load(); }, 60_000);
    return () => window.clearInterval(timer);
  }, [load]);

  const view = useMemo(() => {
    const all = summary?.windows?.all;
    const models = Object.entries(all?.models ?? {});

    // —— 明细侧聚合（链路 / 失败 / 延迟 / 时间桶）——
    const links: Record<LinkKey, LinkStat> = {
      "native-acp": emptyLinkStat(),
      "opencode-borrow": emptyLinkStat(),
    };
    const errKinds = new Map<string, number>();
    const buckets = new Map<number, { ts: number; calls: number; ok: number; fail: number; total: number }>();
    const bucketMs = 3600_000;

    for (const rec of records) {
      const link = classifyLink(rec);
      const stat = links[link];
      stat.calls += 1;
      const ok = rec.ok !== false;
      if (ok) stat.ok += 1; else stat.fail += 1;
      const total = rec.totalTokens ?? 0;
      if (total > 0) {
        stat.reportedCalls += 1;
        stat.prompt += rec.promptTokens ?? 0;
        stat.completion += rec.completionTokens ?? 0;
        stat.total += total;
      } else {
        stat.unreportedCalls += 1;
      }
      stat.cost += rec.credits ?? 0;
      const lat = rec.latencyMs ?? 0;
      if (lat > 0) stat.latencies.push(lat);
      if (!ok) {
        const kind = classifyError(rec.error);
        errKinds.set(kind, (errKinds.get(kind) ?? 0) + 1);
      }
      const key = Math.floor(rec.ts / bucketMs) * bucketMs;
      const bucket = buckets.get(key) ?? { ts: key, calls: 0, ok: 0, fail: 0, total: 0 };
      bucket.calls += 1;
      if (ok) bucket.ok += 1; else bucket.fail += 1;
      bucket.total += total;
      buckets.set(key, bucket);
    }

    for (const stat of Object.values(links)) stat.latencies.sort((a, b) => a - b);

    // —— 汇总侧口径（后端已按渠道过滤）——
    const calls = models.reduce((acc, [, m]) => acc + (m.calls ?? 0), 0);
    const failed = models.reduce((acc, [, m]) => acc + (m.failedCalls ?? 0), 0);
    const totalTokens = models.reduce((acc, [, m]) => acc + (m.totalTokens ?? 0), 0);
    const promptTokens = models.reduce((acc, [, m]) => acc + (m.promptTokens ?? 0), 0);
    const completionTokens = models.reduce((acc, [, m]) => acc + (m.completionTokens ?? 0), 0);
    const credits = models.reduce((acc, [, m]) => acc + (m.credits ?? 0), 0);
    const reportedCalls = records.filter((r) => (r.totalTokens ?? 0) > 0).length;

    const modelRows = models
      .map(([name, m]) => ({
        name,
        calls: m.calls ?? 0,
        failed: m.failedCalls ?? 0,
        total: m.totalTokens ?? 0,
        credits: m.credits ?? 0,
      }))
      .filter((r) => r.name !== "models" && r.name !== "—")
      .sort((a, b) => b.calls - a.calls);

    const tokenRows = [...modelRows].filter((r) => r.total > 0).sort((a, b) => b.total - a.total);
    const errRows = [...errKinds.entries()].sort((a, b) => b[1] - a[1]);
    const hourRows = [...buckets.values()].sort((a, b) => a.ts - b.ts).slice(-24);

    return {
      calls, failed, totalTokens, promptTokens, completionTokens, credits,
      reportedCalls, links, modelRows, tokenRows, errRows, hourRows,
      modelCount: modelRows.length,
    };
  }, [summary, records]);

  const successRate = view.calls > 0 ? (view.calls - view.failed) / view.calls : 0;
  const coverage = view.calls > 0 ? view.reportedCalls / view.calls : 0;
  const maxModelCalls = Math.max(1, ...view.modelRows.map((r) => r.calls));
  const maxToken = Math.max(1, ...view.tokenRows.map((r) => r.total));
  const maxErr = Math.max(1, ...view.errRows.map(([, n]) => n));
  const maxBucket = Math.max(1, ...view.hourRows.map((b) => b.calls));

  if (error) {
    return (
      <Card className="rounded-xl shadow-none">
        <CardContent className="flex items-start gap-2.5 p-5 text-sm">
          <AlertCircle className="mt-0.5 size-4 shrink-0 text-destructive" />
          <div className="min-w-0 flex-1">
            <p className="font-medium">加载 WorkBuddy 渠道统计失败</p>
            <p className="mt-0.5 break-all text-muted-foreground">{error}</p>
          </div>
          <Button size="sm" variant="outline" onClick={() => void load()}>重试</Button>
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-4">
      {/* 池健康面板置顶：本页一旦出现失败，第一个要回答的问题就是
          「是配额不足还是服务故障」——把答案放在症状上面。 */}
      <PoolHealthPanel />

      {/* 头部：口径说明 + 刷新 */}
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0">
          <h2 className="flex items-center gap-2 text-sm font-semibold">
            WorkBuddy 逆向渠道
            <Badge variant="secondary" className="font-normal">
              {formatNumber(view.calls)} 次调用
            </Badge>
          </h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            只统计 <code className="font-mono">channel={CHANNEL}</code> 前缀的记录
            （含 ACP 直连与 OpenCode 借道两条链路），与账号口径的统计互不混算。
          </p>
        </div>
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          {updatedAt > 0 && <span>更新于 {formatTime(updatedAt)}</span>}
          <Button size="sm" variant="outline" disabled={loading} onClick={() => void load()}>
            {loading
              ? <Loader2 className="size-3.5 animate-spin" />
              : <RefreshCw className="size-3.5" />}
            <span className="ml-1.5">刷新</span>
          </Button>
        </div>
      </div>

      {/* KPI */}
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
        <Card className="rounded-xl shadow-none">
          <CardContent className="p-3.5">
            <StatCell label="调用总数" value={formatNumber(view.calls)} accent
              title={`${view.calls} 次`} />
          </CardContent>
        </Card>
        <Card className="rounded-xl shadow-none">
          <CardContent className="p-3.5">
            <StatCell label="成功率" value={formatPercent(successRate)}
              title={`成功 ${view.calls - view.failed} / 失败 ${view.failed}`} />
          </CardContent>
        </Card>
        <Card className="rounded-xl shadow-none">
          <CardContent className="p-3.5">
            <StatCell label="Token（实测）" value={formatTokens(view.totalTokens)}
              title={`输入 ${formatNumber(view.promptTokens)} + 输出 ${formatNumber(view.completionTokens)}`} />
          </CardContent>
        </Card>
        <Card className="rounded-xl shadow-none">
          <CardContent className="p-3.5">
            <StatCell label="Token 覆盖率" value={formatPercent(coverage)}
              title="上游返回 usage 的调用占比；ACP 直连链路上游不返回 usage" />
          </CardContent>
        </Card>
        <Card className="rounded-xl shadow-none">
          <CardContent className="p-3.5">
            <StatCell label="成本（实耗）" value={view.credits.toFixed(6)} accent
              title={`${view.credits}`} />
          </CardContent>
        </Card>
        <Card className="rounded-xl shadow-none">
          <CardContent className="p-3.5">
            <StatCell label="模型种类" value={formatNumber(view.modelCount)} />
          </CardContent>
        </Card>
      </div>

      {/* Token 口径提示：避免把「未上报」误读成「0 消耗」 */}
      <p className="rounded-lg bg-muted/40 px-3 py-2 text-[11px] leading-relaxed text-muted-foreground">
        <strong className="font-medium text-foreground/80">Token 口径：</strong>
        ACP 直连链路的模型由上游托管，<strong>不返回 usage</strong>，其 Token 记为「未上报」而非 0。
        覆盖率低属结构性现象；已上报与未上报分开计数，两栏数相加即为总调用数。
      </p>

      {/* 按链路 */}
      <Card className="rounded-xl shadow-none">
        <CardContent className="p-4">
          <h3 className="mb-3 text-[13px] font-medium">按链路</h3>
          <div className="overflow-x-auto">
            <table className="w-full text-xs">
              <thead>
                <tr className="text-muted-foreground">
                  <th className="pb-2 pr-3 text-left font-normal">链路</th>
                  <th className="pb-2 pr-3 text-right font-normal">调用</th>
                  <th className="pb-2 pr-3 text-right font-normal">成功率</th>
                  <th className="pb-2 pr-3 text-right font-normal">Token 实测</th>
                  <th className="pb-2 pr-3 text-right font-normal">覆盖率</th>
                  <th className="pb-2 pr-3 text-right font-normal">p95 延迟</th>
                  <th className="pb-2 text-right font-normal">成本</th>
                </tr>
              </thead>
              <tbody className="tabular-nums">
                {(Object.keys(view.links) as LinkKey[]).map((key) => {
                  const s = view.links[key];
                  const lat = s.latencies;
                  const cov = s.calls > 0 ? s.reportedCalls / s.calls : 0;
                  return (
                    <tr key={key} className="border-t border-border/60">
                      <td className="py-2 pr-3">
                        <span className="font-medium">{LINK_LABEL[key]}</span>
                        <span className="ml-1.5 font-mono text-[10px] text-muted-foreground">{key}</span>
                      </td>
                      <td className="py-2 pr-3 text-right">{formatNumber(s.calls)}</td>
                      <td className={cn(
                        "py-2 pr-3 text-right",
                        s.calls > 0 && s.ok / s.calls >= 0.9 ? "text-emerald-600" : "text-amber-600",
                      )}>
                        {formatPercent(s.calls > 0 ? s.ok / s.calls : 0)}
                      </td>
                      <td className="py-2 pr-3 text-right">{formatNumber(s.total)}</td>
                      <td className="py-2 pr-3 text-right">{formatPercent(cov)}</td>
                      <td className="py-2 pr-3 text-right">{formatLatency(percentile(lat, 0.95))}</td>
                      <td className="py-2 text-right">{s.cost.toFixed(6)}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </CardContent>
      </Card>

      {/* 模型使用频率 */}
      <Card className="rounded-xl shadow-none">
        <CardContent className="p-4">
          <h3 className="mb-3 text-[13px] font-medium">模型使用频率（Top 20）</h3>
          {view.modelRows.length === 0 ? (
            <p className="py-6 text-center text-xs text-muted-foreground">该渠道暂无调用记录</p>
          ) : (
            <div className="space-y-2.5">
              {view.modelRows.slice(0, 20).map((row) => (
                <div key={row.name} className="min-w-0">
                  <div className="flex items-baseline justify-between gap-3 text-xs">
                    <span className="min-w-0 truncate font-mono" title={row.name}>{row.name}</span>
                    <span className="shrink-0 tabular-nums text-muted-foreground">
                      {formatNumber(row.calls)} 次
                      <span className="mx-1.5 text-border">|</span>
                      {formatPercent(view.calls > 0 ? row.calls / view.calls : 0)}
                      {row.failed > 0 && (
                        <span className="ml-1.5 text-destructive/80">失败 {row.failed}</span>
                      )}
                    </span>
                  </div>
                  <Bar value={row.calls} max={maxModelCalls} />
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>

      {/* Token 消耗 */}
      <Card className="rounded-xl shadow-none">
        <CardContent className="p-4">
          <h3 className="mb-1 text-[13px] font-medium">Token 消耗（仅上游已上报）</h3>
          <p className="mb-3 text-[11px] text-muted-foreground">
            仅含返回 usage 的调用；ACP 直连链路不参与排名（未上报 ≠ 零消耗）。
          </p>
          {view.tokenRows.length === 0 ? (
            <p className="py-6 text-center text-xs text-muted-foreground">暂无可统计的 Token 数据</p>
          ) : (
            <div className="space-y-2.5">
              {view.tokenRows.slice(0, 12).map((row) => (
                <div key={row.name} className="min-w-0">
                  <div className="flex items-baseline justify-between gap-3 text-xs">
                    <span className="min-w-0 truncate font-mono" title={row.name}>{row.name}</span>
                    <span className="shrink-0 tabular-nums text-muted-foreground">
                      {formatNumber(row.total)}
                    </span>
                  </div>
                  <Bar value={row.total} max={maxToken} />
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>

      {/* 失败归因 + 时间桶 */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card className="rounded-xl shadow-none">
          <CardContent className="p-4">
            <h3 className="mb-3 text-[13px] font-medium">失败原因分布</h3>
            {view.errRows.length === 0 ? (
              <p className="py-6 text-center text-xs text-muted-foreground">无失败记录</p>
            ) : (
              <div className="space-y-2.5">
                {view.errRows.map(([kind, count]) => (
                  <div key={kind} className="min-w-0">
                    <div className="flex items-baseline justify-between gap-3 text-xs">
                      <span className="font-mono">{kind}</span>
                      <span className="tabular-nums text-muted-foreground">{formatNumber(count)}</span>
                    </div>
                    <Bar value={count} max={maxErr} />
                  </div>
                ))}
              </div>
            )}
          </CardContent>
        </Card>

        <Card className="rounded-xl shadow-none">
          <CardContent className="p-4">
            <h3 className="mb-3 text-[13px] font-medium">最近 24 个时间桶（每小时）</h3>
            {view.hourRows.length === 0 ? (
              <p className="py-6 text-center text-xs text-muted-foreground">暂无数据</p>
            ) : (
              <div className="space-y-2">
                {view.hourRows.map((b) => (
                  <div key={b.ts} className="flex items-center gap-2.5 text-xs tabular-nums">
                    <span className="w-16 shrink-0 text-muted-foreground">{formatTime(b.ts)}</span>
                    <span className="w-10 shrink-0 text-right">{b.calls}</span>
                    <div className="h-1.5 min-w-0 flex-1 overflow-hidden rounded-full bg-muted">
                      <div
                        className="h-full rounded-full bg-primary/70"
                        style={{ width: `${Math.max(2, (b.calls / maxBucket) * 100)}%` }}
                      />
                    </div>
                    {b.fail > 0
                      ? <span className="w-14 shrink-0 text-right text-destructive/80">失败 {b.fail}</span>
                      : <span className="w-14 shrink-0" />}
                    <span className="w-20 shrink-0 text-right text-muted-foreground">
                      {formatTokens(b.total)}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </CardContent>
        </Card>
      </div>

      <p className="text-[11px] text-muted-foreground">
        数据源：服务端每次调用后写入的 <code className="font-mono">glm_usage_records.json</code>
        （请求级明细），本页 60 秒自动刷新，也可手动刷新。
      </p>
    </div>
  );
}
