import { useEffect, useState } from "react";
import { BarChart3, CheckCircle2, Loader2, RefreshCw, Users, Zap } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { ModelMonitorPanel } from "@/components/model-monitor";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type {
  AccountUsageOverview,
  GlmUsageBucket,
  GlmUsageRecord,
  GlmUsageStatistics,
} from "@/lib/types";

type WindowKey = "today" | "sevenDays" | "all";

const WINDOW_LABELS: { key: WindowKey; label: string }[] = [
  { key: "today", label: "今日" },
  { key: "sevenDays", label: "近 7 天" },
  { key: "all", label: "全部" },
];

// 展示格式化统一到 lib/stats-format（别名保持调用点不变）。
import {
  formatCompactNumber as formatTokens,
  formatGroupedNumber as formatNumber,
  formatLatency,
  formatClock as formatTime,
  formatRatioPercent as formatPercent,
} from "@/lib/stats-format";

function StatCell({ label, value, accent }: { label: string; value: string; accent?: boolean }) {
  return (
    <div className="min-w-0">
      <p className="text-[11px] text-muted-foreground">{label}</p>
      <p
        className={cn(
          "truncate font-semibold tabular-nums leading-6",
          accent ? "text-primary" : "text-foreground",
        )}
        title={value}
      >
        {value}
      </p>
    </div>
  );
}

function BucketCard({ title, bucket, highlight }: { title: string; bucket: GlmUsageBucket; highlight?: boolean }) {
  const cachedTokens = bucket.cachedTokens ?? 0;
  const uncachedPromptTokens = bucket.uncachedPromptTokens ?? Math.max(0, bucket.promptTokens - cachedTokens);
  const cacheHitRate = bucket.cacheHitRate ?? (bucket.promptTokens > 0 ? cachedTokens / bucket.promptTokens : 0);
  const savedTokens = bucket.savedTokens ?? cachedTokens;
  const savedCredits = bucket.savedCredits ?? 0;

  return (
    <Card
      className={cn(
        "min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none",
        highlight && "border-primary/40",
      )}
    >
      <CardContent className="space-y-3 p-4">
        <div className="flex items-center justify-between gap-2">
          <h3 className="truncate text-sm font-semibold" title={title}>
            {title}
          </h3>
          <span className="shrink-0 text-xs text-muted-foreground">{bucket.calls} 次调用</span>
        </div>
        <div className="grid grid-cols-2 gap-x-3 gap-y-2 sm:grid-cols-4">
          <StatCell label="输入 Token" value={formatTokens(bucket.promptTokens)} />
          <StatCell label="输出 Token" value={formatTokens(bucket.completionTokens)} />
          <StatCell label="总 Token" value={formatTokens(bucket.totalTokens)} />
          <StatCell label="消耗积分" value={formatNumber(bucket.credits)} accent />
        </div>

        {/* 缓存命中与成本节省指标专区 */}
        <div className="rounded-lg bg-muted/40 p-2.5 sm:p-3">
          <div className="flex flex-wrap items-center justify-between gap-1 text-xs">
            <span className="flex items-center gap-1.5 font-medium text-foreground/85">
              <Zap className="size-3.5 text-emerald-500" />
              Prompt 缓存命中统计
            </span>
            <span className="tabular-nums text-muted-foreground">
              命中率 <strong className="font-semibold text-emerald-600 dark:text-emerald-400">{(cacheHitRate * 100).toFixed(1)}%</strong>
              {" · "}
              {bucket.cacheHitCalls ?? 0} / {bucket.calls} 次命中
            </span>
          </div>
          <div className="mt-2.5 grid grid-cols-2 gap-2 text-xs sm:grid-cols-3">
            <div>
              <span className="text-[11px] text-muted-foreground">命中 / 未命中 Token</span>
              <p className="truncate font-mono font-medium tabular-nums text-foreground">
                <span className="text-emerald-600 dark:text-emerald-400">{formatTokens(cachedTokens)}</span>
                <span className="mx-1 text-[10px] font-normal text-muted-foreground">/</span>
                <span className="text-muted-foreground">{formatTokens(uncachedPromptTokens)}</span>
              </p>
            </div>
            <div>
              <span className="text-[11px] text-muted-foreground">已节省输入传输</span>
              <p className="truncate font-mono font-medium tabular-nums text-emerald-600 dark:text-emerald-400">
                {formatTokens(savedTokens)}
              </p>
            </div>
            <div>
              <span className="text-[11px] text-muted-foreground">已节省积分（估算）</span>
              <p className="truncate font-mono font-medium tabular-nums text-emerald-600 dark:text-emerald-400">
                {savedCredits > 0 ? `约 ${formatNumber(savedCredits)} 积分` : "—"}
              </p>
            </div>
          </div>
        </div>

        {(bucket.failedCalls ?? 0) > 0 ? (
          <p className="text-[11px] text-destructive">{bucket.failedCalls} 次调用失败（不产生计费）</p>
        ) : null}
        {bucket.creditsPendingCount > 0 ? (
          <p className="text-[11px] text-amber-600">
            {bucket.creditsPendingCount} 条积分待对账（将在下次刷新时自动补齐）
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}

export default function ModelUsagePage() {
  const [stats, setStats] = useState<GlmUsageStatistics | null>(null);
  const [records, setRecords] = useState<GlmUsageRecord[]>([]);
  const [accountsOverview, setAccountsOverview] = useState<AccountUsageOverview[]>([]);
  const [selectedProvider, setSelectedProvider] = useState<string>("all");
  const [loading, setLoading] = useState(false);
  const [window_, setWindow] = useState<WindowKey>("all");
  const [loadError, setLoadError] = useState<string | null>(null);

  async function load() {
    setLoading(true);
    try {
      const [summary, rec, overviewRes] = await Promise.all([
        api.getGlmUsageSummary(),
        api.getGlmUsageRecords(100),
        api.getAccountsUsageOverview().catch(() => ({ ok: false, generatedAt: 0, accounts: [] })),
      ]);
      setStats(summary);
      setRecords(rec.records ?? []);
      if (overviewRes?.accounts) {
        setAccountsOverview(overviewRes.accounts);
      }
      setLoadError(null);
    } catch (e) {
      setLoadError(api.asError(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    if (!api.isWebui() || api.isDemoMode()) return;
    void load();
    const timer = window.setInterval(() => void load(), 30_000);
    return () => window.clearInterval(timer);
  }, []);

  if (!api.isWebui() || api.isDemoMode()) {
    return (
      <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
        <Card>
          <CardContent className="p-5 text-sm text-muted-foreground">
            模型用量统计仅在 webui 后端（Sylvia Switch 启动器）下可用。
          </CardContent>
        </Card>
      </div>
    );
  }

  const windowData = stats?.windows[window_];
  const models = Object.entries(windowData?.models ?? {}).sort((a, b) => b[1].totalTokens - a[1].totalTokens);
  const total: GlmUsageBucket = windowData?.total ?? { calls: 0, promptTokens: 0, completionTokens: 0, totalTokens: 0, credits: 0, creditsPendingCount: 0 };

  return (
    <div className="mx-auto min-w-0 w-full max-w-4xl px-4 py-6 sm:px-6 sm:py-8">
      <header className="mb-8 flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="flex items-center gap-2 text-2xl font-semibold tracking-tight">
            <BarChart3 className="size-6 text-primary" />
            模型用量
          </h1>
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            统一监控 AutoClaw、Loomy 与 TRAE Work CN 账号的 WorkBuddy 模型使用情况。
            实时捕获各账号的 Token 与积分/额度消耗、延迟指标与 Prompt 缓存命中成效。
            {stats ? ` 数据更新于 ${formatTime(stats.generatedAt)}${stats.reconcileMatched > 0 ? `（本次对账补齐 ${stats.reconcileMatched} 条）` : ""}` : ""}
          </p>
        </div>
        <Button size="sm" variant="outline" onClick={() => void load()} disabled={loading} className="shrink-0">
          {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}刷新
        </Button>
      </header>

      {loadError ? (
        <Card className="mb-6">
          <CardContent className="p-4 text-sm text-destructive">加载失败：{loadError}</CardContent>
        </Card>
      ) : null}

      {/* 多账号模型使用监控窗口（AutoClaw · Loomy · TRAE Work CN 列表行形式） */}
      <Card className="mb-8 min-w-0 gap-0 overflow-hidden rounded-xl border py-0 shadow-none">
        <CardContent className="p-0">
          <div className="flex flex-wrap items-center justify-between gap-2 border-b border-border/60 px-4 py-3 sm:px-5">
            <div className="flex items-center gap-2">
              <Users className="size-4 text-primary" />
              <h2 className="text-[13px] font-semibold leading-5">账号模型用量监控</h2>
              <Badge variant="secondary" className="h-5 px-1.5 text-[10px] font-medium">
                AutoClaw · Loomy · TRAE Work CN
              </Badge>
            </div>
            <div className="flex items-center gap-2">
              {selectedProvider !== "all" ? (
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-6 px-2 text-xs text-primary"
                  onClick={() => setSelectedProvider("all")}
                >
                  已按账号筛选 · 点击查看全部
                </Button>
              ) : (
                <span className="text-[11px] text-muted-foreground">
                  点击账号行可在下方联动筛选专属模型与趋势
                </span>
              )}
            </div>
          </div>

          <div className="min-w-0 overflow-x-auto">
            <table className="w-full min-w-[920px] text-left text-xs">
              <thead>
                <tr className="border-b border-border/60 text-muted-foreground">
                  <th className="px-4 py-2.5 font-medium sm:px-5">账号 / 平台</th>
                  <th className="px-3 py-2.5 font-medium">当前余额 / 额度</th>
                  <th className="px-3 py-2.5 text-right font-medium">调用次数</th>
                  <th className="px-3 py-2.5 text-right font-medium">成功率</th>
                  <th className="px-3 py-2.5 text-right font-medium">平均耗时</th>
                  <th className="px-3 py-2.5 text-right font-medium">Token (总 / 输入 / 输出)</th>
                  <th className="px-3 py-2.5 text-right font-medium">缓存命中</th>
                  <th className="px-3 py-2.5 text-right font-medium">消耗积分/额度</th>
                  <th className="px-4 py-2.5 text-right font-medium sm:px-5">最近调用</th>
                </tr>
              </thead>
              <tbody>
                {accountsOverview.map((acc) => {
                  const isSelected = selectedProvider === acc.provider;
                  const platformColor =
                    acc.provider === "autoclaw"
                      ? "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 border-emerald-500/20"
                      : acc.provider === "loomy"
                        ? "bg-amber-500/10 text-amber-600 dark:text-amber-400 border-amber-500/20"
                        : acc.provider === "minimax"
                          ? "bg-violet-500/10 text-violet-600 dark:text-violet-400 border-violet-500/20"
                          : "bg-sky-500/10 text-sky-600 dark:text-sky-400 border-sky-500/20";

                  return (
                    <tr
                      key={acc.provider}
                      onClick={() => setSelectedProvider(isSelected ? "all" : acc.provider)}
                      className={cn(
                        "cursor-pointer border-b border-border/40 transition-colors last:border-0 hover:bg-muted/40",
                        isSelected && "bg-primary/[0.06] hover:bg-primary/[0.09]",
                      )}
                      title={`点击${isSelected ? "取消" : "切换"}针对 ${acc.name} 的模型监控联动`}
                    >
                      <td className="px-4 py-3 sm:px-5">
                        <div className="flex items-center gap-2">
                          <span
                            className={cn(
                              "size-2 shrink-0 rounded-[2px]",
                              acc.provider === "autoclaw"
                                ? "bg-emerald-500"
                                : acc.provider === "loomy"
                                  ? "bg-amber-500"
                                  : acc.provider === "minimax"
                                    ? "bg-violet-500"
                                    : "bg-sky-500",
                            )}
                            aria-hidden="true"
                          />
                          <span className="font-semibold text-foreground">{acc.name}</span>
                          <Badge variant="outline" className={cn("h-4 border px-1 text-[9px]", platformColor)}>
                            {acc.provider === "autoclaw"
                              ? "AutoClaw"
                              : acc.provider === "loomy"
                                ? "Loomy"
                                : acc.provider === "minimax"
                                  ? "MiniMax"
                                  : acc.provider === "trae_work_cn"
                                    ? "TRAE"
                                    : acc.provider}
                          </Badge>
                          {isSelected ? (
                            <CheckCircle2 className="size-3.5 text-primary ml-auto shrink-0" />
                          ) : null}
                        </div>
                      </td>
                      <td className="px-3 py-3">
                        {acc.balance != null ? (
                          <div className="font-semibold tabular-nums text-foreground">
                            {formatNumber(acc.balance)}{" "}
                            <span className="text-[10px] font-normal text-muted-foreground">{acc.balanceUnit}</span>
                          </div>
                        ) : (
                          <span className="text-muted-foreground">未登录 / 不可用</span>
                        )}
                      </td>
                      <td className="px-3 py-3 text-right tabular-nums">
                        <div className="font-medium text-foreground">{formatNumber(acc.calls)} 次</div>
                        <div className="text-[10px] text-muted-foreground">
                          {acc.okCalls} 成功 · {acc.failedCalls} 失败
                        </div>
                      </td>
                      <td className="px-3 py-3 text-right tabular-nums">
                        {acc.calls > 0 ? (
                          <Badge
                            variant={acc.successRate >= 0.9 ? "secondary" : "destructive"}
                            className="h-5 px-1.5 text-[10px]"
                          >
                            {formatPercent(acc.successRate)}
                          </Badge>
                        ) : (
                          <span className="text-muted-foreground">—</span>
                        )}
                      </td>
                      <td
                        className="px-3 py-3 text-right tabular-nums text-muted-foreground"
                        title={`首字延迟: ${formatLatency(acc.avgTtfbMs)}`}
                      >
                        <div className="font-medium text-foreground">{formatLatency(acc.avgLatencyMs)}</div>
                        <div className="text-[10px] text-muted-foreground">首字 {formatLatency(acc.avgTtfbMs)}</div>
                      </td>
                      <td className="px-3 py-3 text-right tabular-nums">
                        <div className="font-medium text-foreground">{formatTokens(acc.totalTokens)}</div>
                        <div className="text-[10px] text-muted-foreground">
                          {formatTokens(acc.promptTokens)} / {formatTokens(acc.completionTokens)}
                        </div>
                      </td>
                      <td className="px-3 py-3 text-right tabular-nums">
                        {acc.cachedTokens > 0 ? (
                          <span className="inline-flex items-center gap-1 font-medium text-emerald-600 dark:text-emerald-400">
                            <Zap className="size-3" />
                            {formatTokens(acc.cachedTokens)}
                            <span className="text-[10px] font-normal text-muted-foreground">
                              ({formatPercent(acc.cacheHitRate)})
                            </span>
                          </span>
                        ) : (
                          <span className="text-muted-foreground">—</span>
                        )}
                      </td>
                      <td className="px-3 py-3 text-right tabular-nums font-semibold text-primary">
                        {acc.credits > 0 ? formatNumber(acc.credits) : "0"}
                      </td>
                      <td className="whitespace-nowrap px-4 py-3 text-right tabular-nums text-muted-foreground sm:px-5">
                        {acc.lastTs ? formatTime(acc.lastTs) : "暂无"}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </CardContent>
      </Card>

      <div className="mb-10">
        <ModelMonitorPanel provider={selectedProvider} onProviderChange={setSelectedProvider} />
      </div>

      <div className="mb-4 flex items-center gap-1.5" role="tablist" aria-label="统计范围">
        {WINDOW_LABELS.map((w) => (
          <Button
            key={w.key}
            size="sm"
            variant={window_ === w.key ? "default" : "outline"}
            onClick={() => setWindow(w.key)}
          >
            {w.label}
          </Button>
        ))}
        <span className="ml-auto text-xs text-muted-foreground">每 30 秒自动刷新</span>
      </div>

      {!stats || !windowData ? (
        <Card>
          <CardContent className="flex items-center gap-2 p-6 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" /> 加载统计中…
          </CardContent>
        </Card>
      ) : (
        <div className="min-w-0 space-y-10">
          <section className="min-w-0 space-y-2.5" aria-labelledby="usage-total">
            <div className="px-1">
              <h2 id="usage-total" className="text-[13px] font-medium leading-5">
                总计
              </h2>
            </div>
            <BucketCard title="全部模型" bucket={total} highlight />
          </section>

          <section className="min-w-0 space-y-2.5" aria-labelledby="usage-models">
            <div className="px-1">
              <h2 id="usage-models" className="text-[13px] font-medium leading-5">
                按模型
              </h2>
            </div>
            {models.length === 0 ? (
              <Card>
                <CardContent className="p-5 text-sm text-muted-foreground">
                  该范围内暂无调用记录。通过对应渠道发起对话后，用量会实时出现在这里。
                </CardContent>
              </Card>
            ) : (
              <div className="grid min-w-0 items-start gap-3">
                {models.map(([model, bucket]) => (
                  <BucketCard key={model} title={model} bucket={bucket} />
                ))}
              </div>
            )}
          </section>

          <section className="min-w-0 space-y-2.5" aria-labelledby="usage-records">
            <div className="px-1">
              <h2 id="usage-records" className="text-[13px] font-medium leading-5">
                调用明细（最近 {records.length} 条）
              </h2>
            </div>
            <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
              <CardContent className="p-0">
                {records.length === 0 ? (
                  <p className="px-4 py-4 text-sm text-muted-foreground sm:px-5">暂无调用记录。</p>
                ) : (
                  <div className="min-w-0 overflow-x-auto">
                    <table className="w-full min-w-[720px] text-left text-xs">
                      <thead>
                        <tr className="border-b border-border/60 text-muted-foreground">
                          <th className="px-4 py-2.5 font-medium sm:px-5">时间</th>
                          <th className="px-3 py-2.5 font-medium">模型</th>
                          <th className="px-3 py-2.5 font-medium">账号</th>
                          <th className="px-3 py-2.5 text-right font-medium">输入</th>
                          <th className="px-3 py-2.5 text-center font-medium">缓存命中</th>
                          <th className="px-3 py-2.5 text-right font-medium">输出</th>
                          <th className="px-3 py-2.5 text-right font-medium">总计</th>
                          <th className="px-3 py-2.5 text-right font-medium">积分</th>
                          <th className="px-4 py-2.5 text-right font-medium sm:px-5">耗时</th>
                        </tr>
                      </thead>
                      <tbody>
                        {records.map((r) => (
                          <tr
                            key={r.id}
                            className={cn(
                              "border-b border-border/40 last:border-0",
                              r.ok === false && "bg-destructive/5",
                            )}
                          >
                            <td className="whitespace-nowrap px-4 py-2 tabular-nums text-muted-foreground sm:px-5">
                              {formatTime(r.ts)}
                            </td>
                            <td
                              className={cn(
                                "whitespace-nowrap px-3 py-2",
                                r.ok === false && "text-destructive",
                              )}
                              title={r.error || undefined}
                            >
                              {r.model}
                              {r.ok === false ? "（失败）" : ""}
                              {r.substituted && r.requestedModel ? (
                                <Badge
                                  variant="secondary"
                                  className="ml-1.5 h-5 px-1.5 text-[10px]"
                                  title={`速度优先：客户端请求 ${r.requestedModel}，实际改投到 ${r.model}`}
                                >
                                  改投 ← {r.requestedModel}
                                </Badge>
                              ) : null}
                              {r.substitutionSkipped ? (
                                <Badge
                                  variant="outline"
                                  className="ml-1.5 h-5 px-1.5 text-[10px]"
                                  title="速度优先：该请求含图片/视频等多模态内容，保持客户端选择的模型"
                                >
                                  跳过多模态
                                </Badge>
                              ) : null}
                            </td>
                            <td className="max-w-[120px] truncate px-3 py-2 text-muted-foreground" title={r.accountLabel || r.accountId || ""}>
                              {r.accountLabel || r.accountId || "—"}
                            </td>
                            <td className="px-3 py-2 text-right tabular-nums">{formatNumber(r.promptTokens)}</td>
                            <td className="whitespace-nowrap px-3 py-2 text-center tabular-nums">
                              {(r.cachedTokens ?? 0) > 0 ? (
                                <Badge
                                  variant="success"
                                  className="h-5 px-1.5 text-[10px]"
                                  title={`本次命中: ${formatNumber(r.cachedTokens)} Token (${r.promptTokens > 0 ? (((r.cachedTokens ?? 0) / r.promptTokens) * 100).toFixed(0) : 0}%) · 未命中: ${formatNumber(r.uncachedPromptTokens ?? Math.max(0, r.promptTokens - (r.cachedTokens ?? 0)))}`}
                                >
                                  命中 {formatTokens(r.cachedTokens ?? 0)}
                                </Badge>
                              ) : (
                                <span className="text-[11px] text-muted-foreground/60">—</span>
                              )}
                            </td>
                            <td className="px-3 py-2 text-right tabular-nums">{formatNumber(r.completionTokens)}</td>
                            <td className="px-3 py-2 text-right tabular-nums">{formatNumber(r.totalTokens)}</td>
                            <td className="px-3 py-2 text-right tabular-nums">
                              {r.creditsPending ? (
                                <Badge variant="warning" className="h-5 px-1.5 text-[10px]">待对账</Badge>
                              ) : r.credits != null ? (
                                <span className="font-medium text-primary">{formatNumber(r.credits)}</span>
                              ) : (
                                <span className="text-muted-foreground">—</span>
                              )}
                            </td>
                            <td className="whitespace-nowrap px-4 py-2 text-right tabular-nums text-muted-foreground sm:px-5">
                              {formatNumber(r.latencyMs)}ms
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </CardContent>
            </Card>
          </section>
        </div>
      )}
    </div>
  );
}
