import { useCallback, useEffect, useState } from "react";
import {
  Activity,
  AlarmClock,
  CircleAlert,
  CircleCheck,
  Clock,
  Coins,
  Gauge,
  Globe,
  Info,
  Loader2,
  PauseCircle,
  PlayCircle,
  RefreshCw,
  RotateCw,
  ScanLine,
  Send,
  Trash2,
  Wrench,
} from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { CopyButton } from "@/components/ui/copy-button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import { useRefreshAllListener } from "@/lib/refresh-all";
import type {
  AutoclawIntlAccountRow,
  AutoclawIntlEvent,
  AutoclawIntlPolicy,
  AutoclawIntlStrategy,
  AutoclawIntlSummary,
} from "@/lib/types";

/** 策略中文名（契约 §5 卡 4 策略单选）。 */
const STRATEGY_LABELS: Record<AutoclawIntlStrategy, string> = {
  "round-robin": "轮询（round-robin）",
  random: "随机（random）",
  "least-used": "最少用量（least-used）",
  sticky: "固定优先（sticky）",
};
const STRATEGIES = Object.keys(STRATEGY_LABELS) as AutoclawIntlStrategy[];

/** 状态中文名 + 徽标配色（对齐 AutoClaw 池卡片的状态徽标先例）。 */
const STATE_META: Record<string, { label: string; badge: "success" | "warning" | "destructive" | "secondary" | "outline" }> = {
  active: { label: "健康", badge: "success" },
  "quota-out": { label: "额度用尽", badge: "warning" },
  cooldown: { label: "冷却中", badge: "warning" },
  invalid: { label: "令牌失效", badge: "destructive" },
  banned: { label: "已封禁", badge: "destructive" },
  disabled: { label: "已停用", badge: "outline" },
};

/** 事件 kind 中文（契约 §2.2 events kind）。 */
const EVENT_LABELS: Record<string, string> = {
  join: "入池",
  rotate: "轮换",
  cooldown: "冷却",
  ban: "封禁",
  quota: "额度",
  refresh: "续期",
  replace: "补位",
  disable: "停用",
};

/** 环形健康度（纯 SVG，不引第三方图表库——仓库内无 recharts 环形先例）。 */
function HealthRing({ healthy, total }: { healthy: number; total: number }) {
  const pct = total > 0 ? Math.max(0, Math.min(1, healthy / total)) : 0;
  const R = 34;
  const C = 2 * Math.PI * R;
  const color = total === 0 ? "text-muted-foreground" : pct >= 0.5 ? "text-emerald-500" : pct > 0.2 ? "text-amber-500" : "text-destructive";
  return (
    <div className="relative size-[88px] shrink-0">
      <svg viewBox="0 0 88 88" className="size-full -rotate-90">
        <circle cx="44" cy="44" r={R} fill="none" strokeWidth="8" className="stroke-muted" />
        <circle
          cx="44"
          cy="44"
          r={R}
          fill="none"
          strokeWidth="8"
          strokeLinecap="round"
          className={cn("transition-all duration-500", color)}
          stroke="currentColor"
          strokeDasharray={C}
          strokeDashoffset={C * (1 - pct)}
        />
      </svg>
      <div className="absolute inset-0 flex flex-col items-center justify-center">
        <span className="text-lg font-semibold leading-none tabular-nums">
          {healthy}
          <span className="text-xs font-normal text-muted-foreground">/{total}</span>
        </span>
        <span className="mt-0.5 text-[10px] text-muted-foreground">健康号</span>
      </div>
    </div>
  );
}

/** 相对时间（事件流/最近使用；参照 ZcodePage fmtAgo）。 */
function fmtAgo(v: number | string | null | undefined): string {
  if (v === null || v === undefined || v === "") return "—";
  const ts = typeof v === "number" ? v : Number(v);
  if (!Number.isFinite(ts) || ts <= 0) return "—";
  const diff = Math.max(0, Date.now() - (ts < 1e12 ? ts * 1000 : ts));
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  return `${Math.floor(diff / 86_400_000)} 天前`;
}

/** 秒级倒计时 → 「Xh Ym」/「Ym」/「已过期」。 */
function fmtTtl(secs: number | null | undefined): string {
  if (secs === null || secs === undefined || !Number.isFinite(secs)) return "—";
  const s = Math.floor(secs);
  if (s <= 0) return "已过期";
  if (s < 3600) return `${Math.floor(s / 60)} 分钟`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}h ${String(m).padStart(2, "0")}m`;
}

export default function AutoclawIntlPage() {
  const [summary, setSummary] = useState<AutoclawIntlSummary | null>(null);
  const [accounts, setAccounts] = useState<AutoclawIntlAccountRow[]>([]);
  const [events, setEvents] = useState<AutoclawIntlEvent[]>([]);
  const [policy, setPolicy] = useState<AutoclawIntlPolicy | null>(null);
  const [access, setAccess] = useState<{ baseUrl: string; models: string[] } | null>(null);
  const [backendReady, setBackendReady] = useState<boolean | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  // 策略设置卡的本地编辑态（数字输入以字符串承载，失焦/保存时归一）
  const [policyDraft, setPolicyDraft] = useState({
    perAccountRatePerMin: "",
    perAccountDailyCap: "",
    poolConcurrentCap: "",
    accountConcurrentCap: "",
    quotaResetHour: "",
    minHealthy: "",
  });

  const is404 = (e: unknown): boolean => {
    const msg = api.asError(e);
    return msg.includes("404") || msg.includes("不支持") || msg.includes("未找到");
  };

  const loadAll = useCallback(async () => {
    try {
      const [sum, list, evts] = await Promise.all([
        api.getAutoclawIntlSummary(),
        api.getAutoclawIntlList(),
        api.getAutoclawIntlEvents(50),
      ]);
      setSummary(sum);
      setAccounts(list.accounts);
      setEvents(evts.events);
      setBackendReady(true);
      setPolicy((prev) => prev ?? {
        strategy: (sum.strategy as AutoclawIntlStrategy) ?? "round-robin",
        perAccountDailyCap: sum.perAccountDailyCap,
        poolConcurrentCap: sum.poolConcurrentCap,
        minHealthy: sum.minHealthy,
        autoReplace: sum.autoReplace,
      });
      setPolicyDraft((prev) => ({
        ...prev,
        perAccountDailyCap: prev.perAccountDailyCap || String(sum.perAccountDailyCap ?? ""),
        poolConcurrentCap: prev.poolConcurrentCap || String(sum.poolConcurrentCap ?? ""),
        minHealthy: prev.minHealthy || String(sum.minHealthy ?? ""),
      }));
    } catch (e) {
      if (is404(e)) {
        setBackendReady(false);
      }
      throw e;
    }
  }, []);

  const loadAccess = useCallback(async () => {
    try {
      const a = await api.getAutoclawIntlAccess();
      setAccess({ baseUrl: a.baseUrl, models: (a.models ?? []).map((m) => m.id) });
    } catch {
      // /access 未上线时回落到既有 glm-proxy 配置端点（模型带 region 字段）：
      // endpoint 即网关 base_url，intl 目录 = region==="international" 过滤。
      try {
        const cfg = await api.getGlmProxyConfig();
        setAccess({
          baseUrl: cfg.endpoint,
          models: (cfg.models ?? []).filter((m) => m.region === "international").map((m) => m.id),
        });
      } catch {
        setAccess(null);
      }
    }
  }, []);

  useEffect(() => {
    void loadAll().catch(() => undefined);
    void loadAccess();
    return useRefreshAllListener(() => {
      void loadAll().catch(() => undefined);
      void loadAccess();
    });
  }, [loadAll, loadAccess]);

  // 10s 轮询（契约 §5：ZCodePage 数据刷新惯例；visibilitychange/focus 重拉）
  useEffect(() => {
    const t = window.setInterval(() => {
      if (document.visibilityState !== "visible") return;
      void loadAll().catch(() => undefined);
    }, 10_000);
    return () => window.clearInterval(t);
  }, [loadAll]);

  useEffect(() => {
    const onFocus = () => {
      if (document.visibilityState === "visible") {
        void loadAll().catch(() => undefined);
      }
    };
    document.addEventListener("visibilitychange", onFocus);
    window.addEventListener("focus", onFocus);
    return () => {
      document.removeEventListener("visibilitychange", onFocus);
      window.removeEventListener("focus", onFocus);
    };
  }, [loadAll]);

  async function refresh() {
    setBusy("refresh");
    try {
      await loadAll();
      await loadAccess();
    } catch (e) {
      if (!is404(e)) toast.error("刷新失败", { description: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function savePolicy(patch: AutoclawIntlPolicy, okText: string) {
    setBusy("policy");
    try {
      await api.saveAutoclawIntlPolicy(patch);
      toast.success(okText);
      await loadAll();
    } catch (e) {
      toast.error("策略保存失败", { description: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function runAction(
    key: string,
    label: string,
    fn: () => Promise<unknown>,
    okText?: string,
  ) {
    setBusy(key);
    try {
      await fn();
      if (okText) toast.success(okText);
      await loadAll();
    } catch (e) {
      toast.error(`${label}失败`, { description: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  function runRemove(acc: AutoclawIntlAccountRow) {
    if (!window.confirm(`确定移除账号「${acc.label || acc.id}」？将连轮换池状态一并清理，移除后不可恢复。`)) {
      return;
    }
    void runAction(`remove:${acc.id}`, "移除", () => api.removeAutoclawIntlAccount(acc.id), "已移除");
  }

  const todayPct =
    summary && summary.perAccountDailyCap > 0
      ? Math.min(100, (summary.todayCalls / summary.perAccountDailyCap) * 100)
      : 0;

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-10 flex min-w-0 flex-wrap items-start justify-between gap-4 sm:mb-12">
        <div className="min-w-0">
          <h1 className="flex items-center gap-2 text-[28px] font-semibold tracking-tight">
            AutoClaw 国际池
            <Badge variant="outline" className="text-[10px]">
              🌐 region=international
            </Badge>
          </h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            国际号（OAuth 管线注册）独立轮换账号池：策略选号、限速/预算风控、token 自动续期、
            封禁摘除与水位告警。契约 docs/autoclaw-intl-pool-contract.md。
          </p>
        </div>
        <Button
          size="sm"
          variant="outline"
          onClick={() => void refresh()}
          disabled={busy === "refresh"}
        >
          {busy === "refresh" ? <Loader2 className="animate-spin" /> : <RefreshCw />}刷新
        </Button>
      </header>

      {backendReady === false ? (
        <Card className="mb-4 gap-0 rounded-xl py-0 shadow-none">
          <CardContent className="flex items-start gap-2 px-4 py-3 sm:px-5">
            <CircleAlert className="mt-0.5 size-4 shrink-0 text-amber-600" />
            <p className="text-xs leading-5 text-muted-foreground">
              后端暂未提供 /api/autoclaw/intl/* 管理端点（M3 API 层实现中）。
              页面已就绪，端点上线后数据将自动出现。
            </p>
          </CardContent>
        </Card>
      ) : null}

      {/* 卡 1：池总览 + 卡 3：轮换事件流 */}
      <section className="grid min-w-0 gap-4 lg:grid-cols-2" aria-label="池总览与事件流">
        <Card className="flex min-w-0 flex-col gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <CardTitle className="flex items-center gap-2 text-sm font-medium">
                <Gauge className="size-4 text-muted-foreground" />
                池总览
              </CardTitle>
              <Badge variant="outline" className="text-[10px]">
                lane: {summary?.lane ?? "international"}
              </Badge>
            </div>
            <CardDescription className="mt-1 text-xs">
              健康度 / 策略 / 游标 / 今日调用预算
            </CardDescription>
          </CardHeader>
          <CardContent className="p-0">
            <div className="flex items-center gap-4 px-4 py-3 sm:px-5">
              <HealthRing healthy={summary?.healthy ?? 0} total={summary?.total ?? 0} />
              <div className="grid min-w-0 flex-1 grid-cols-2 gap-x-4 gap-y-1.5 text-xs">
                <span className="text-muted-foreground">策略</span>
                <span className="text-right font-medium">
                  {summary ? STRATEGY_LABELS[summary.strategy as AutoclawIntlStrategy] ?? summary.strategy : "—"}
                </span>
                <span className="text-muted-foreground">游标位置</span>
                <span className="text-right font-medium tabular-nums">
                  {summary ? `#${summary.cursorOffset}` : "—"}
                </span>
                <span className="text-muted-foreground">在途 / 整池并发</span>
                <span className="text-right font-medium tabular-nums">
                  {summary ? `${summary.poolInFlight} / ${summary.poolConcurrentCap}` : "—"}
                </span>
                <span className="text-muted-foreground">累计轮换 / 补位 / 续期</span>
                <span className="text-right font-medium tabular-nums">
                  {summary ? `${summary.rotations} / ${summary.replacements} / ${summary.refreshes}` : "—"}
                </span>
              </div>
            </div>
            {/* 今日调用 / 预算（契约 §5 卡 1「今日调用/预算」） */}
            <div className="border-t border-border/50 px-4 py-3 sm:px-5">
              <div className="flex items-center justify-between text-xs">
                <span className="flex items-center gap-1.5 text-muted-foreground">
                  <Coins className="size-3.5" />
                  今日调用 / 池日预算
                </span>
                <span className="font-medium tabular-nums">
                  {summary ? `${summary.todayCalls.toLocaleString("zh-CN")} / ${summary.perAccountDailyCap.toLocaleString("zh-CN")}` : "—"}
                </span>
              </div>
              <div className="mt-1.5 h-1.5 w-full overflow-hidden rounded-full bg-muted">
                <div
                  className={cn("h-full rounded-full", todayPct > 80 ? "bg-destructive" : todayPct > 50 ? "bg-amber-500" : "bg-emerald-500")}
                  style={{ width: `${todayPct}%` }}
                />
              </div>
            </div>
            {/* 状态分布 + 水位告警 */}
            {summary ? (
              <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1.5 border-t border-border/50 px-4 py-2.5 sm:px-5">
                <Badge variant="secondary" className="text-[10px] tabular-nums">冷却 {summary.cooldown}</Badge>
                <Badge variant="warning" className="text-[10px] tabular-nums">额度用尽 {summary.quotaOut}</Badge>
                <Badge variant="destructive" className="text-[10px] tabular-nums">封禁 {summary.banned}</Badge>
                <Badge variant="destructive" className="text-[10px] tabular-nums">失效 {summary.invalid}</Badge>
                <Badge variant="outline" className="text-[10px] tabular-nums">停用 {summary.disabled}</Badge>
                {summary.lowWater ? (
                  <span className="ml-auto flex items-center gap-1.5 text-[11px] text-amber-600 dark:text-amber-500">
                    <CircleAlert className="size-3.5 shrink-0" />
                    水位不足（&lt; {summary.minHealthy}）：请跑注册管线扩号
                  </span>
                ) : (
                  <span className="ml-auto flex items-center gap-1.5 text-[11px] text-muted-foreground">
                    <CircleCheck className="size-3.5 shrink-0 text-emerald-600" />
                    水位正常（≥ {summary.minHealthy}）
                  </span>
                )}
              </div>
            ) : null}
          </CardContent>
        </Card>

        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
            <div className="flex min-w-0 items-center gap-2">
              <Activity className="size-4 shrink-0 text-muted-foreground" />
              <h2 className="text-sm font-medium">轮换事件流</h2>
              <Badge variant="secondary" className="text-[10px] tabular-nums">
                最近 {events.length} 条
              </Badge>
            </div>
            <Button
              size="sm"
              variant="ghost"
              className="h-7 gap-1 px-2 text-xs"
              onClick={() => void runAction("events", "事件刷新", () => api.getAutoclawIntlEvents(50))}
              disabled={busy === "events"}
            >
              <RotateCw className={cn("size-3.5", busy === "events" && "animate-spin")} />
              刷新
            </Button>
          </div>
          <div className="max-h-[340px] min-h-[120px] divide-y divide-border/50 overflow-y-auto border-t border-border/50">
            {events.length === 0 ? (
              <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                {backendReady === false ? "后端未就绪。" : "暂无轮换事件。入池/轮换/冷却/封禁/续期都会记录在这里。"}
              </p>
            ) : (
              events.map((ev, i) => (
                <div key={`${ev.ts}-${ev.accountId}-${i}`} className="flex min-w-0 items-center gap-2.5 px-4 py-2 sm:px-5">
                  <Badge
                    variant={
                      ev.kind === "ban" || ev.kind === "disable"
                        ? "destructive"
                        : ev.kind === "cooldown" || ev.kind === "quota"
                          ? "warning"
                          : ev.kind === "join" || ev.kind === "replace"
                            ? "success"
                            : "secondary"
                    }
                    className="w-[52px] shrink-0 justify-center text-[10px]"
                  >
                    {EVENT_LABELS[ev.kind] ?? ev.kind}
                  </Badge>
                  <span className="min-w-0 flex-1 truncate text-xs" title={`${ev.accountId} · ${ev.detail}`}>
                    <span className="text-muted-foreground">{ev.accountId}</span>
                    {ev.detail ? <span className="ml-1.5">{ev.detail}</span> : null}
                  </span>
                  <span className="shrink-0 text-[10px] tabular-nums text-muted-foreground">
                    {fmtAgo(ev.ts)}
                  </span>
                </div>
              ))
            )}
          </div>
        </Card>
      </section>

      {/* 卡 2：账号列表（AutoClaw 式行布局） */}
      <section className="mt-10 min-w-0 space-y-2.5" aria-label="账号列表">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">账号列表</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            凭据存于 ~/.wb-switch/autoclaw_accounts.json（intl 条目含 intlMeta）；轮换运行态存于 autoclaw_intl_pool.json
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
            <div className="flex min-w-0 items-center gap-2">
              <Globe className="size-4 shrink-0 text-muted-foreground" />
              <span className="text-sm font-medium">国际号（{accounts.length}）</span>
            </div>
          </div>
          <div className="divide-y divide-border/50 border-t border-border/50">
            {accounts.length === 0 ? (
              <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                {backendReady === false
                  ? "后端未就绪。"
                  : "池内暂无国际号。注册管线产号后将自动入池（region=international）。"}
              </p>
            ) : (
              accounts.map((acc) => {
                const stateMeta = STATE_META[acc.state] ?? { label: acc.state, badge: "outline" as const };
                // token 到期倒计时：<2h 橙色（契约 §5 卡 2）
                const ttl = acc.tokenTtlSecs;
                const ttlUrgent = ttl !== null && ttl !== undefined && Number.isFinite(ttl) && ttl > 0 && ttl < 7200;
                const ttlExpired = ttl !== null && ttl !== undefined && Number.isFinite(ttl) && ttl <= 0;
                return (
                  <div key={acc.id} className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-2 px-4 py-2.5 sm:px-5">
                    {/* 左：label + 徽标堆叠 + 灰 ID + token 倒计时 */}
                    <div className="flex min-w-0 flex-wrap items-center gap-2">
                      <span className="max-w-[180px] truncate text-xs font-medium" title={acc.sourceEmail ?? acc.label ?? acc.id}>
                        {acc.label || acc.sourceEmail || acc.id}
                      </span>
                      <Badge variant="outline" className="text-[10px]">🌐 international</Badge>
                      <Badge variant={stateMeta.badge} className="text-[10px]">{stateMeta.label}</Badge>
                      {acc.enabled ? null : (
                        <Badge variant="outline" className="text-[10px] text-muted-foreground">停用</Badge>
                      )}
                      {acc.inFlight ? (
                        <Badge variant="secondary" className="text-[10px] tabular-nums" title="当前在途请求">
                          在途 {acc.inFlight}
                        </Badge>
                      ) : null}
                      <span className="text-[11px] tabular-nums text-muted-foreground" title="今日调用 / 累计调用">
                        今日 {acc.todayCalls ?? 0} · 累计 {acc.totalCalls ?? 0}
                      </span>
                    </div>
                    <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
                      <span className="max-w-[200px] truncate font-mono text-[10px]" title={acc.id}>
                        {acc.id}
                      </span>
                      <span
                        className={cn(
                          "flex shrink-0 items-center gap-1 tabular-nums",
                          ttlExpired
                            ? "text-destructive"
                            : ttlUrgent
                              ? "text-amber-600 dark:text-amber-500"
                              : "",
                        )}
                        title={acc.tokenExpireAt ? `token 过期于 ${new Date(acc.tokenExpireAt * 1000).toLocaleString()}` : "token 到期时间未知"}
                      >
                        <Clock className="size-3 shrink-0" />
                        token {fmtTtl(ttl)}
                      </span>
                      {acc.state === "cooldown" && (acc.cooldownRemainSecs ?? 0) > 0 ? (
                        <span className="flex shrink-0 items-center gap-1 tabular-nums">
                          <AlarmClock className="size-3 shrink-0" />
                          冷却 {fmtTtl(acc.cooldownRemainSecs)}
                        </span>
                      ) : null}
                      {acc.lastError ? (
                        <span className="max-w-[240px] truncate text-destructive" title={acc.lastError}>
                          {acc.lastError}
                        </span>
                      ) : null}
                      <span className="shrink-0">最近使用 {fmtAgo(acc.lastUsedAt)}</span>
                    </div>
                    {/* 右：操作组（探测/续期/停用/移除，契约 §5 卡 2） */}
                    <div className="ml-auto flex shrink-0 items-center gap-1.5">
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 px-2 text-xs"
                        onClick={() => void runAction(`probe:${acc.id}`, "探测", () => api.probeAutoclawIntlAccount(acc.id), "探测完成")}
                        disabled={busy === `probe:${acc.id}`}
                        title="立即探测该号（复用 watch 探针，最小 chat 调用）"
                      >
                        {busy === `probe:${acc.id}` ? <Loader2 className="animate-spin" /> : <ScanLine />}
                        探测
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 px-2 text-xs"
                        onClick={() => void runAction(`refresh:${acc.id}`, "续期", () => api.refreshAutoclawIntlAccount(acc.id), "续期完成")}
                        disabled={busy === `refresh:${acc.id}`}
                        title="手动刷新 access token（refresh_token 换新）"
                      >
                        {busy === `refresh:${acc.id}` ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                        续期
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 px-2 text-xs"
                        onClick={() =>
                          void runAction(
                            `toggle:${acc.id}`,
                            acc.enabled ? "停用" : "启用",
                            () => api.toggleAutoclawIntlAccount(acc.id, !acc.enabled),
                            acc.enabled ? "已停用（摘出轮换）" : "已启用",
                          )
                        }
                        disabled={busy === `toggle:${acc.id}`}
                        title={acc.enabled ? "停用：摘出轮换（状态保留）" : "启用：重新进入轮换"}
                      >
                        {busy === `toggle:${acc.id}` ? (
                          <Loader2 className="animate-spin" />
                        ) : acc.enabled ? (
                          <PauseCircle />
                        ) : (
                          <PlayCircle />
                        )}
                        {acc.enabled ? "停用" : "启用"}
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 px-2 text-xs text-destructive hover:text-destructive"
                        onClick={() => runRemove(acc)}
                        disabled={busy === `remove:${acc.id}`}
                        title="移除（连轮换池状态一并清理，不可恢复）"
                      >
                        {busy === `remove:${acc.id}` ? <Loader2 className="animate-spin" /> : <Trash2 />}
                        移除
                      </Button>
                    </div>
                  </div>
                );
              })
            )}
          </div>
          <div className="border-t border-border/50 px-4 py-3 sm:px-5">
            <p className="flex items-start gap-1.5 text-[11px] leading-5 text-muted-foreground">
              <Info className="mt-0.5 size-3.5 shrink-0" />
              首号绑真设备（deviceIsolated=false）为已知风险；新注册号每号独立虚拟 device_id 后自动标注。
              探测与轮换流量计入日预算，探测节奏 30 分钟一次。
            </p>
          </div>
        </Card>
      </section>

      {/* 卡 4：策略设置 + 卡 5：接入信息 */}
      <section className="mt-10 grid min-w-0 gap-4 lg:grid-cols-2" aria-label="策略与接入">
        <Card className="flex min-w-0 flex-col gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <CardTitle className="flex items-center gap-2 text-sm font-medium">
              <Wrench className="size-4 text-muted-foreground" />
              策略设置
            </CardTitle>
            <CardDescription className="mt-1 text-xs">
              限速/预算为风控参数（实测封号流水反推），勿随意调大；改动即时生效
            </CardDescription>
          </CardHeader>
          <CardContent className="p-0">
            {/* 策略单选（契约 §5 卡 4） */}
            <div className="border-b border-border/50 px-4 py-3 sm:px-5">
              <p className="text-xs font-medium">选号策略</p>
              <div className="mt-2 grid grid-cols-1 gap-1.5 sm:grid-cols-2">
                {STRATEGIES.map((s) => {
                  const current = policy?.strategy ?? summary?.strategy;
                  const checked = current === s;
                  return (
                    <label
                      key={s}
                      className={cn(
                        "flex cursor-pointer items-center gap-2 rounded-lg border px-2.5 py-2 text-xs transition-colors",
                        checked ? "border-primary/50 bg-primary/[0.06] font-medium" : "border-border/60 hover:bg-accent/40",
                      )}
                    >
                      <input
                        type="radio"
                        name="intl-strategy"
                        checked={checked}
                        onChange={() => void savePolicy({ strategy: s }, `策略已切换：${STRATEGY_LABELS[s]}`)}
                        disabled={busy === "policy"}
                        className="accent-primary"
                      />
                      {STRATEGY_LABELS[s]}
                    </label>
                  );
                })}
              </div>
            </div>
            {/* 限速/预算/并发数字输入 */}
            <div className="grid grid-cols-2 gap-3 border-b border-border/50 px-4 py-3 sm:px-5">
              {([
                ["perAccountRatePerMin", "单号限速（次/分）", "风控参数，实测 12 为安全上限"],
                ["perAccountDailyCap", "单号日预算（次）", "本地计数，跨零点重置"],
                ["poolConcurrentCap", "整池并发上限", "同时在途请求总数"],
                ["accountConcurrentCap", "单号并发上限", "同号同时在途请求"],
                ["quotaResetHour", "额度重置时刻（0-23 时）", "810001 当日额度重置点"],
                ["minHealthy", "健康号水位阈值", "低于该值触发水位告警"],
              ] as [keyof typeof policyDraft, string, string][]).map(([key, label, hint]) => (
                <div key={key}>
                  <label className="text-xs text-muted-foreground" title={hint}>{label}</label>
                  <Input
                    type="number"
                    className="mt-1 h-8 text-xs"
                    min={key === "quotaResetHour" ? 0 : 1}
                    max={key === "quotaResetHour" ? 23 : undefined}
                    value={policyDraft[key]}
                    onChange={(e) => setPolicyDraft((p) => ({ ...p, [key]: e.target.value.replace(/[^0-9]/g, "") }))}
                    placeholder="—"
                  />
                </div>
              ))}
            </div>
            {/* autoReplace 开关 + 保存 */}
            <div className="flex flex-wrap items-center gap-3 px-4 py-3 sm:px-5">
              <Switch
                checked={policy?.autoReplace ?? summary?.autoReplace ?? true}
                onCheckedChange={(checked) => void savePolicy({ autoReplace: checked }, checked ? "已开启自动补位" : "已关闭自动补位")}
                disabled={busy === "policy"}
              />
              <span className="text-xs text-muted-foreground">
                autoReplace（水位不足自动补位；自动注册触发留二期，先通知）
              </span>
              <Button
                size="sm"
                className="ml-auto h-8"
                disabled={busy === "policy"}
                onClick={() => {
                  const num = (v: string): number | undefined => {
                    const n = Number(v);
                    return v !== "" && Number.isFinite(n) ? n : undefined;
                  };
                  void savePolicy(
                    {
                      perAccountRatePerMin: num(policyDraft.perAccountRatePerMin),
                      perAccountDailyCap: num(policyDraft.perAccountDailyCap),
                      poolConcurrentCap: num(policyDraft.poolConcurrentCap),
                      accountConcurrentCap: num(policyDraft.accountConcurrentCap),
                      quotaResetHour: num(policyDraft.quotaResetHour),
                      minHealthy: num(policyDraft.minHealthy),
                    },
                    "策略已保存",
                  );
                }}
              >
                {busy === "policy" ? <Loader2 className="animate-spin" /> : <RotateCw />}
                保存全部参数
              </Button>
            </div>
          </CardContent>
        </Card>

        {/* 卡 5：接入信息（契约 §5：base_url + intl 模型清单 + region=international 调用示例） */}
        <Card className="flex min-w-0 flex-col gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <CardTitle className="flex items-center gap-2 text-sm font-medium">
              <Send className="size-4 text-muted-foreground" />
              接入信息
            </CardTitle>
            <CardDescription className="mt-1 text-xs">
              国际 lane 数据面入口：OpenAI 兼容端点，请求体 region=international 锁定本池
            </CardDescription>
          </CardHeader>
          <CardContent className="p-0">
            <div className="border-b border-border/50 px-4 py-3 sm:px-5">
              <div className="flex items-center justify-between gap-2">
                <p className="text-xs font-medium">Base URL</p>
                {access?.baseUrl ? (
                  <div className="flex items-center gap-1.5">
                    <code className="max-w-[280px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={access.baseUrl}>
                      {access.baseUrl}
                    </code>
                    <CopyButton text={access.baseUrl} label="接口地址" />
                  </div>
                ) : (
                  <code className="rounded bg-muted/70 px-2 py-1 text-xs">http://127.0.0.1:57890/v1</code>
                )}
              </div>
              <p className="mt-1.5 text-[11px] leading-5 text-muted-foreground">
                密钥与国内 lane 通用（「GLM 代理」页管理）；不传 region 时按模型目录归属自动路由。
              </p>
            </div>
            <div className="border-b border-border/50 px-4 py-3 sm:px-5">
              <div className="flex items-center gap-2">
                <p className="text-xs font-medium">国际区模型清单</p>
                <Badge variant="secondary" className="text-[10px] tabular-nums">
                  {access?.models?.length ?? 0} 个
                </Badge>
              </div>
              <div className="mt-2 flex flex-wrap gap-1.5">
                {(access?.models ?? []).length === 0 ? (
                  <span className="text-[11px] text-muted-foreground">—</span>
                ) : (
                  access?.models.map((m) => (
                    <span key={m} className="rounded bg-muted/70 px-2 py-1 text-xs">
                      {m}
                    </span>
                  ))
                )}
              </div>
            </div>
            <div className="px-4 py-3 sm:px-5">
              <div className="flex items-start justify-between gap-3">
                <pre className="min-w-0 flex-1 overflow-x-auto rounded-lg bg-muted/60 p-2.5 text-[11px] leading-5">
{`curl ${access?.baseUrl || "http://127.0.0.1:57890/v1"}/chat/completions \\
  -H "Authorization: Bearer <访问密钥>" \\
  -H "Content-Type: application/json" \\
  -d '{"model":"${access?.models?.[0] ?? "auto"}","region":"international","messages":[{"role":"user","content":"你好"}]}'`}
                </pre>
                <CopyButton
                  text={`curl ${access?.baseUrl || "http://127.0.0.1:57890/v1"}/chat/completions -H "Authorization: Bearer <访问密钥>" -H "Content-Type: application/json" -d '{"model":"${access?.models?.[0] ?? "auto"}","region":"international","messages":[{"role":"user","content":"你好"}]}'`}
                  label="调用示例"
                />
              </div>
            </div>
          </CardContent>
        </Card>
      </section>
    </div>
  );
}
