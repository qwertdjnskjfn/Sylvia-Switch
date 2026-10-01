import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowLeftRight,
  ChevronDown,
  ChevronRight,
  Loader2,
  Play,
  Plus,
  RefreshCw,
} from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import { useRefreshAllListener } from "@/lib/refresh-all";
import { formatDateTimeShort, formatLatency } from "@/lib/stats-format";
import type {
  ModelSyncGatewayResult,
  ModelSyncGatewayStatus,
  ModelSyncRound,
  ModelSyncStatus,
} from "@/lib/api";

// ---- 常量（契约 docs/model-sync-contract.md §五）--------------------------

/** 手动补录网关下拉仅列两个 Manual 源（契约 §一：glmweb/qoder 无上游清单端点）。 */
const MANUAL_GATEWAYS = ["glmweb", "qoder"] as const;

/** 网关标识 → 展示名（口径与 gateway-registry 一致；workbuddy-pool 为同步引擎独有行）。 */
const GATEWAY_LABELS: Record<string, string> = {
  glm: "GLM 代理",
  glmweb: "ChatGLM 网关",
  kimi: "Kimi 网关",
  grok: "Grok 网关",
  qwen: "Qwen 通道",
  qoder: "Qoder 网关",
  loomy: "Loomy",
  miaoshou: "妙手",
  trae: "TRAE Work CN",
  workbuddy: "WorkBuddy",
  "workbuddy-pool": "WorkBuddy 池",
  opencode: "OpenCode",
};

function gatewayLabel(id: string): string {
  return GATEWAY_LABELS[id] ?? id;
}

const MODE_BADGE: Record<"auto" | "manual", { label: string; className: string }> = {
  auto: {
    label: "auto",
    className: "border-transparent bg-sky-500/15 text-sky-700 dark:text-sky-400",
  },
  manual: {
    label: "manual",
    className: "border-transparent bg-violet-500/15 text-violet-700 dark:text-violet-400",
  },
};

const OK_BADGE = "border-transparent bg-emerald-500/15 text-emerald-700 dark:text-emerald-400";
const ERROR_BADGE = "border-transparent bg-rose-500/15 text-rose-700 dark:text-rose-400";
const PENDING_BADGE = "border-transparent bg-muted text-muted-foreground";

const ADDED_BADGE = "bg-emerald-500/15 text-emerald-700 dark:text-emerald-400";

/** 默认同步间隔（分钟）＝契约 §3.5：1440（每天一次），0=关闭。 */
const DEFAULT_INTERVAL_MINUTES = 1440;

// ---- 展示格式化 -----------------------------------------------------------

/** 后端契约统一 ms epoch；对秒级时间戳做容错（< 1e12 视为秒）。 */
function normalizeMs(ts: number | null | undefined): number | null {
  if (typeof ts !== "number" || !Number.isFinite(ts) || ts <= 0) return null;
  return ts < 1e12 ? ts * 1000 : ts;
}

/** 相对时间：刚刚 / N 分钟前 / N 小时前 / N 天前，超过 30 天退回绝对时刻。 */
function formatRelativeTime(ts: number | null | undefined): string {
  const ms = normalizeMs(ts);
  if (!ms) return "—";
  const diff = Date.now() - ms;
  if (diff < 60_000) return "刚刚";
  const min = Math.floor(diff / 60_000);
  if (min < 60) return `${min} 分钟前`;
  const hours = Math.floor(min / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} 天前`;
  return formatDateTimeShort(ms);
}

/** 未来时刻 → 「N 分钟后 / N 小时后」，超过 48 小时退回绝对时刻。 */
function formatFutureTime(ts: number): string {
  const diff = ts - Date.now();
  if (diff <= 0) return "即将运行";
  const min = Math.ceil(diff / 60_000);
  if (min < 60) return `${min} 分钟后`;
  const hours = Math.floor(min / 60);
  if (hours < 48) return `${hours} 小时后`;
  return formatDateTimeShort(ts);
}

/** 下轮提示容错：ms epoch / 纯数字字符串 / 自由文案三态。 */
function formatNextRunHint(
  hint: number | string | null | undefined,
  enabled: boolean,
): string {
  if (!enabled) return "已暂停";
  if (hint == null || hint === "") return "—";
  if (typeof hint === "number") {
    const ms = normalizeMs(hint);
    return ms ? formatFutureTime(ms) : "—";
  }
  if (/^\d+$/.test(hint)) {
    const ms = normalizeMs(Number(hint));
    return ms ? formatFutureTime(ms) : "—";
  }
  return hint;
}

/** 单轮新增总数（基线轮 added 为空 → 0）。 */
function roundAddedTotal(round: ModelSyncRound | undefined): number {
  if (!round?.results) return 0;
  return round.results.reduce((sum, r) => sum + (r.added?.length ?? 0), 0);
}

type RowStatus = {
  key: "ok" | "error" | "pending";
  label: string;
  badge: string;
  error?: string | null;
};

/** 网关状态三态：ok / error / 未同步。首次使用（无同步记录）＝「未同步」正常态。 */
function gatewayRowStatus(row: ModelSyncGatewayStatus): RowStatus {
  const synced = normalizeMs(row.lastSyncTs) != null;
  if (!synced) return { key: "pending", label: "未同步", badge: PENDING_BADGE };
  if (row.ok === false)
    return { key: "error", label: "异常", badge: ERROR_BADGE, error: row.lastError };
  // ok 缺省但已有同步时刻：旧后端容错，视为正常。
  return { key: "ok", label: "正常", badge: OK_BADGE, error: row.lastError };
}

/** run/busy 的 409 → 友好提示。 */
function friendlyRunError(e: unknown): string {
  const msg = api.asError(e);
  const payload = (e as { payload?: { status?: number } }).payload;
  if (payload?.status === 409 || /409|busy/i.test(msg)) {
    return "已有同步任务正在进行，请等它跑完再试（409 busy）";
  }
  return msg;
}

// ---- 本地小组件 -----------------------------------------------------------

function KpiCell({
  label,
  value,
  className,
  title,
}: {
  label: string;
  value: string;
  className?: string;
  title?: string;
}) {
  return (
    <div className="min-w-0" title={title}>
      <p className="text-[11px] text-muted-foreground">{label}</p>
      <p
        className={cn(
          "truncate text-lg font-semibold tabular-nums leading-7",
          className ?? "text-foreground",
        )}
      >
        {value}
      </p>
    </div>
  );
}

function DashedHint({ text }: { text: string }) {
  return (
    <div className="rounded-lg border border-dashed px-4 py-10 text-center text-sm text-muted-foreground">
      {text}
    </div>
  );
}

function SectionTitle({ children }: { children: React.ReactNode }) {
  return (
    <div className="px-1">
      <h2 className="text-[13px] font-medium leading-5">{children}</h2>
    </div>
  );
}

/** 历史轮次单行（可展开看按网关分组的新增明细）。 */
function RoundRow({
  round,
  expanded,
  onToggle,
}: {
  round: ModelSyncRound;
  expanded: boolean;
  onToggle: () => void;
}) {
  const ts = normalizeMs(round.ts);
  const addedTotal = roundAddedTotal(round);
  const hasBaseline = (round.results ?? []).some((r) => r.baseline);
  const hasError = (round.results ?? []).some((r) => r.ok === false);
  const chips = (round.results ?? [])
    .filter((r) => (r.added?.length ?? 0) > 0)
    .map((r) => ({ gateway: r.gateway, count: r.added?.length ?? 0 }));

  const summary = addedTotal > 0
    ? `新增 ${addedTotal} 个模型`
    : hasBaseline
      ? "基线轮（已记录存量，未计新增）"
      : "无新增";

  return (
    <div className="border-b border-border/40 last:border-0">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={expanded}
        className="flex w-full cursor-pointer flex-wrap items-center gap-x-3 gap-y-1.5 px-4 py-3 text-left transition-colors hover:bg-muted/40 sm:px-5"
      >
        {expanded ? (
          <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
        ) : (
          <ChevronRight className="size-4 shrink-0 text-muted-foreground" />
        )}
        <span className="font-medium tabular-nums text-foreground">
          {ts ? formatDateTimeShort(ts) : "—"}
        </span>
        <Badge variant="outline" className="h-5 shrink-0 px-1.5 text-[10px] text-muted-foreground">
          {round.trigger === "manual" ? "手动" : "自动"}
        </Badge>
        <span className="text-xs text-muted-foreground">{summary}</span>
        <span className="flex min-w-0 flex-1 flex-wrap justify-end gap-1">
          {chips.slice(0, 5).map((c) => (
            <Badge
              key={c.gateway}
              variant="outline"
              className={cn("h-5 px-1.5 text-[10px]", ADDED_BADGE)}
            >
              {gatewayLabel(c.gateway)} +{c.count}
            </Badge>
          ))}
          {chips.length > 5 ? (
            <Badge variant="outline" className="h-5 px-1.5 text-[10px] text-muted-foreground">
              …
            </Badge>
          ) : null}
          {hasError && addedTotal === 0 ? (
            <Badge variant="outline" className={cn("h-5 px-1.5 text-[10px]", ERROR_BADGE)}>
              部分网关异常
            </Badge>
          ) : null}
        </span>
        <span className="shrink-0 text-[11px] tabular-nums text-muted-foreground" title="本轮耗时">
          {formatLatency(round.durationMs)}
        </span>
      </button>

      {expanded ? (
        <div className="space-y-3 px-4 pb-4 sm:px-5">
          {(round.results ?? []).length === 0 ? (
            <p className="text-xs text-muted-foreground">本轮无网关明细。</p>
          ) : (
            (round.results ?? []).map((r) => (
              <GatewayResultGroup key={r.gateway} result={r} roundTs={ts} />
            ))
          )}
        </div>
      ) : null}
    </div>
  );
}

/** 按网关分组的新增明细（id + name + 时间 + baseline 标记；失败网关展示错误）。 */
function GatewayResultGroup({
  result,
  roundTs,
}: {
  result: ModelSyncGatewayResult;
  roundTs: number | null;
}) {
  const added = result.added ?? [];
  const mode = result.mode ? MODE_BADGE[result.mode] : undefined;

  return (
    <div className="rounded-lg border border-border/60 bg-muted/30 p-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs font-semibold text-foreground">{gatewayLabel(result.gateway)}</span>
        {mode ? (
          <Badge variant="outline" className={cn("h-4 px-1 text-[9px]", mode.className)}>
            {mode.label}
          </Badge>
        ) : null}
        {result.baseline ? (
          <Badge variant="outline" className={cn("h-4 px-1 text-[9px]", PENDING_BADGE)}>
            基线
          </Badge>
        ) : added.length > 0 ? (
          <Badge variant="outline" className={cn("h-4 px-1 text-[9px]", ADDED_BADGE)}>
            +{added.length}
          </Badge>
        ) : null}
        <span className="ml-auto text-[10px] tabular-nums text-muted-foreground" title="本轮时刻">
          {roundTs ? formatDateTimeShort(roundTs) : "—"}
        </span>
      </div>

      {result.error ? (
        <p className="mt-1.5 text-[11px] leading-5 text-rose-600 dark:text-rose-400" title={result.error}>
          {result.error}
        </p>
      ) : null}

      {result.baseline ? (
        <p className="mt-1.5 text-[11px] text-muted-foreground">
          首轮基线：已记录 {result.total ?? added.length} 个存量模型（不计「新增」）。
        </p>
      ) : null}

      {added.length > 0 ? (
        <ul className="mt-1.5 space-y-1">
          {added.map((m) => (
            <li key={m.id} className="flex min-w-0 items-baseline gap-2 text-[11px]">
              <code className="shrink-0 font-mono text-foreground">{m.id}</code>
              {typeof m.multiplier === "number" ? (
                <span className="shrink-0 rounded bg-primary/10 px-1 font-mono text-[10px] text-primary">
                  x{m.multiplier}
                </span>
              ) : null}
              {m.name ? (
                <span className="min-w-0 truncate text-muted-foreground" title={m.name}>
                  {m.name}
                </span>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

// ---- 页面 ----------------------------------------------------------------

export default function ModelSyncPage() {
  const [status, setStatus] = useState<ModelSyncStatus | null>(null);
  const [config, setConfig] = useState<api.ModelSyncConfig | null>(null);
  const [rounds, setRounds] = useState<ModelSyncRound[]>([]);
  const [logLimit, setLogLimit] = useState(10);
  const [loading, setLoading] = useState(true);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [logError, setLogError] = useState<string | null>(null);

  // 「立即同步」busy 态 + 409 友好提示
  const [running, setRunning] = useState(false);
  // 手动补录表单（网关下拉仅列 glmweb/qoder 两个 Manual 源）
  const [form, setForm] = useState({
    gateway: "glmweb",
    modelId: "",
    displayName: "",
    note: "",
  });
  const [adding, setAdding] = useState(false);
  // 间隔配置（仅首次从后端回填，避免轮询覆盖用户正在编辑的值）
  const [intervalInput, setIntervalInput] = useState(String(DEFAULT_INTERVAL_MINUTES));
  const [enabledInput, setEnabledInput] = useState(true);
  const [savingConfig, setSavingConfig] = useState(false);
  const configInitialized = useRef(false);

  const [expandedRounds, setExpandedRounds] = useState<ReadonlySet<number>>(() => new Set());

  const loadSeq = useRef(0);
  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    setLoading(true);
    const [statusRes, logRes, configRes] = await Promise.allSettled([
      api.getModelSyncStatus(),
      api.getModelSyncLog(logLimit),
      api.getModelSyncConfig(),
    ]);
    if (seq !== loadSeq.current) return;
    if (statusRes.status === "fulfilled") {
      setStatus(statusRes.value);
      setStatusError(null);
    } else {
      setStatusError(api.asError(statusRes.reason));
    }
    if (logRes.status === "fulfilled") {
      setRounds(logRes.value);
      setLogError(null);
    } else {
      setLogError(api.asError(logRes.reason));
    }
    if (configRes.status === "fulfilled") {
      const cfg = configRes.value;
      setConfig(cfg);
      if (!configInitialized.current) {
        configInitialized.current = true;
        setEnabledInput(cfg.enabled !== false);
        const minutes = Number(cfg.intervalMinutes);
        setIntervalInput(
          String(Number.isFinite(minutes) && minutes >= 0 ? minutes : DEFAULT_INTERVAL_MINUTES),
        );
      }
    }
    setLoading(false);
  }, [logLimit]);

  useEffect(() => {
    if (!api.isWebui() || api.isDemoMode()) return;
    void load();
    // status 端点读 state 秒开不触发拉取，60s 轮询只保证相对时间/概览新鲜。
    const timer = window.setInterval(() => void load(), 60_000);
    return () => window.clearInterval(timer);
  }, [load]);

  useEffect(() => useRefreshAllListener(() => void load()), [load]);

  const gateways = status?.gateways ?? [];
  const enabledNow = status?.enabled ?? config?.enabled ?? true;
  const intervalNow = status?.intervalMinutes ?? config?.intervalMinutes ?? DEFAULT_INTERVAL_MINUTES;

  const lastSyncTs = useMemo(() => {
    let max = 0;
    for (const g of gateways) {
      const ms = normalizeMs(g.lastSyncTs);
      if (ms && ms > max) max = ms;
    }
    return max > 0 ? max : null;
  }, [gateways]);

  const latestRound = rounds[0];
  const lastAddedTotal = useMemo(() => {
    if (latestRound) return roundAddedTotal(latestRound);
    // 日志缺失时退回 status 聚合
    return gateways.reduce((sum, g) => sum + (g.addedCount ?? 0), 0);
  }, [latestRound, gateways]);

  const runningNow = running || loading;

  const handleRun = useCallback(async () => {
    if (running) return;
    setRunning(true);
    try {
      const round = await api.runModelSync();
      const total = roundAddedTotal(round);
      if (total > 0) {
        toast.success(`同步完成：本轮新增 ${total} 个模型`);
      } else {
        toast.success("同步完成：本轮无新增");
      }
      await load();
    } catch (e) {
      toast.error(friendlyRunError(e));
    } finally {
      setRunning(false);
    }
  }, [running, load]);

  const handleAddRegistry = useCallback(async () => {
    const modelId = form.modelId.trim();
    if (!modelId) {
      toast.error("请填写模型 ID");
      return;
    }
    setAdding(true);
    try {
      await api.addModelSyncRegistry({
        gateway: form.gateway,
        modelId,
        displayName: form.displayName.trim() || undefined,
        note: form.note.trim() || undefined,
      });
      toast.success(`已补录 ${modelId} 到 ${gatewayLabel(form.gateway)} 目录`);
      setForm((f) => ({ ...f, modelId: "", displayName: "", note: "" }));
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setAdding(false);
    }
  }, [form, load]);

  const handleSaveConfig = useCallback(async () => {
    const parsed = Number.parseInt(intervalInput, 10);
    if (!Number.isFinite(parsed) || parsed < 0) {
      toast.error("同步间隔需为不小于 0 的整数分钟");
      return;
    }
    setSavingConfig(true);
    try {
      await api.saveModelSyncConfig({ enabled: enabledInput, intervalMinutes: parsed });
      toast.success(
        !enabledInput || parsed === 0
          ? "已保存：定时轮已暂停（仍可手动「立即同步」）"
          : `已保存：每 ${parsed} 分钟自动同步一轮`,
      );
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setSavingConfig(false);
    }
  }, [intervalInput, enabledInput, load]);

  const toggleRound = useCallback((index: number) => {
    setExpandedRounds((prev) => {
      const next = new Set(prev);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  }, []);

  if (!api.isWebui() || api.isDemoMode()) {
    return (
      <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
        <Card>
          <CardContent className="p-5 text-sm text-muted-foreground">
            模型目录自动同步仅在 webui 后端（Sylvia Switch 启动器）下可用。
          </CardContent>
        </Card>
      </div>
    );
  }

  const configPaused = !enabledInput || Number.parseInt(intervalInput, 10) === 0;

  return (
    <div className="mx-auto min-w-0 w-full max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
      <header className="mb-5 flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="flex items-center gap-2 text-2xl font-semibold tracking-tight">
            <ArrowLeftRight className="size-6 text-primary" />
            模型同步
          </h1>
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            定期拉取各网关上游模型清单，自动检测新增并落盘报告；grok 新增自动写入注册表，
            glmweb / qoder 支持手动补录。
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <Button size="sm" onClick={() => void handleRun()} disabled={runningNow}>
            {running ? (
              <Loader2 className="animate-spin" />
            ) : (
              <Play />
            )}
            立即同步
          </Button>
          <Button size="sm" variant="outline" onClick={() => void load()} disabled={loading} className="shrink-0">
            {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}刷新
          </Button>
        </div>
      </header>

      {statusError ? (
        <Card className="mb-5">
          <CardContent className="flex items-center justify-between gap-3 p-4 text-sm text-destructive">
            <span className="min-w-0">加载失败：{statusError}</span>
            <Button size="sm" variant="outline" onClick={() => void load()} disabled={loading}>
              重试
            </Button>
          </CardContent>
        </Card>
      ) : null}

      {loading && !status ? (
        <div className="space-y-5">
          <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
            <CardContent className="grid grid-cols-2 gap-3 p-4 sm:grid-cols-3 xl:grid-cols-5">
              {Array.from({ length: 5 }).map((_, i) => (
                <div key={i} className="space-y-2">
                  <Skeleton className="h-3 w-16" />
                  <Skeleton className="h-6 w-20" />
                </div>
              ))}
            </CardContent>
          </Card>
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <CardContent className="space-y-3 p-4">
              {Array.from({ length: 6 }).map((_, i) => (
                <Skeleton key={i} className="h-8 w-full" />
              ))}
            </CardContent>
          </Card>
        </div>
      ) : !status ? null : gateways.length === 0 ? (
        <Card className="border-dashed">
          <CardContent className="flex flex-col items-center gap-2 p-10 text-center text-sm text-muted-foreground">
            <ArrowLeftRight className="size-8 text-muted-foreground/50" />
            后端未返回网关清单，请确认 Sylvia Switch 已升级到含模型同步功能的版本后重试。
          </CardContent>
        </Card>
      ) : (
        <div className="min-w-0">
          {/* 总览卡（契约 §五：启用状态/间隔、上次同步时刻、下轮提示、近一轮新增总数） */}
          <Card className="mb-5 min-w-0 gap-0 rounded-xl py-0 shadow-none">
            <CardContent className="grid grid-cols-2 gap-3 p-4 sm:grid-cols-3 xl:grid-cols-5">
              <KpiCell
                label="定时同步"
                value={enabledNow ? "已启用" : "已暂停"}
                className={enabledNow ? "text-emerald-600 dark:text-emerald-400" : "text-muted-foreground"}
              />
              <KpiCell
                label="同步间隔"
                value={intervalNow > 0 ? `${intervalNow} 分钟` : "已暂停（0）"}
              />
              <KpiCell
                label="下次运行"
                value={formatNextRunHint(status.nextRunHint, enabledNow && intervalNow > 0)}
              />
              <KpiCell
                label="上次同步"
                value={lastSyncTs ? formatRelativeTime(lastSyncTs) : "未同步"}
                title={lastSyncTs ? formatDateTimeShort(lastSyncTs) : undefined}
                className={lastSyncTs ? undefined : "text-muted-foreground"}
              />
              <KpiCell
                label="近一轮新增"
                value={`${lastAddedTotal} 个`}
                className={lastAddedTotal > 0 ? "text-emerald-600 dark:text-emerald-400" : "text-muted-foreground"}
              />
            </CardContent>
          </Card>

          {/* 网关表（契约 §五：12 行，模式/状态徽标 + 相对时间 + 错误悬浮） */}
          <section className="mb-6 min-w-0 space-y-2.5">
            <SectionTitle>网关清单（{gateways.length}）</SectionTitle>
            <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
              <CardContent className="p-0">
                <div className="min-w-0 overflow-x-auto">
                  <table className="w-full min-w-[760px] text-left text-xs">
                    <thead>
                      <tr className="border-b border-border/60 text-muted-foreground">
                        <th className="px-4 py-2.5 font-medium sm:px-5">网关</th>
                        <th className="px-3 py-2.5 font-medium">模式</th>
                        <th className="px-3 py-2.5 font-medium">状态</th>
                        <th className="px-3 py-2.5 font-medium">上次同步</th>
                        <th className="px-3 py-2.5 text-right font-medium">模型数</th>
                        <th className="px-4 py-2.5 text-right font-medium sm:px-5">新增数</th>
                      </tr>
                    </thead>
                    <tbody>
                      {gateways.map((g) => {
                        const st = gatewayRowStatus(g);
                        const mode = g.mode ? MODE_BADGE[g.mode] : undefined;
                        const lastMs = normalizeMs(g.lastSyncTs);
                        return (
                          <tr
                            key={g.gateway}
                            className="border-b border-border/40 transition-colors last:border-0 hover:bg-muted/40"
                          >
                            <td className="px-4 py-2.5 sm:px-5">
                              <div className="flex items-center gap-2">
                                <span className="font-medium text-foreground">
                                  {gatewayLabel(g.gateway)}
                                </span>
                                <span className="text-[10px] tabular-nums text-muted-foreground/70">
                                  {g.gateway}
                                </span>
                              </div>
                              {g.source ? (
                                <div
                                  className="mt-0.5 max-w-[320px] truncate text-[10px] text-muted-foreground"
                                  title={g.source}
                                >
                                  {g.source}
                                </div>
                              ) : null}
                            </td>
                            <td className="px-3 py-2.5">
                              {mode ? (
                                <Badge variant="outline" className={cn("h-5 px-1.5 text-[10px]", mode.className)}>
                                  {mode.label}
                                </Badge>
                              ) : (
                                <span className="text-muted-foreground">—</span>
                              )}
                            </td>
                            <td className="px-3 py-2.5">
                              {st.error ? (
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <Badge
                                      variant="outline"
                                      className={cn("h-5 cursor-help px-1.5 text-[10px]", st.badge)}
                                    >
                                      {st.label}
                                    </Badge>
                                  </TooltipTrigger>
                                  <TooltipContent side="top" className="max-w-72 break-all">
                                    {st.error}
                                  </TooltipContent>
                                </Tooltip>
                              ) : (
                                <Badge variant="outline" className={cn("h-5 px-1.5 text-[10px]", st.badge)}>
                                  {st.label}
                                </Badge>
                              )}
                            </td>
                            <td className="px-3 py-2.5">
                              <span className="tabular-nums" title={lastMs ? formatDateTimeShort(lastMs) : undefined}>
                                {lastMs ? formatRelativeTime(lastMs) : "—"}
                              </span>
                            </td>
                            <td className="px-3 py-2.5 text-right tabular-nums">
                              {typeof g.total === "number" && g.total > 0 ? g.total : "—"}
                            </td>
                            <td className="px-4 py-2.5 text-right sm:px-5">
                              <span
                                className={cn(
                                  "inline-block rounded px-1.5 py-0.5 font-medium tabular-nums",
                                  (g.addedCount ?? 0) > 0 && ADDED_BADGE,
                                )}
                              >
                                {typeof g.addedCount === "number" && g.addedCount > 0
                                  ? `+${g.addedCount}`
                                  : typeof g.addedCount === "number"
                                    ? "0"
                                    : "—"}
                              </span>
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>
              </CardContent>
            </Card>
            <p className="px-1 text-[11px] text-muted-foreground">
              首次使用（尚未跑过同步）时全部显示「未同步」属正常状态；点击「立即同步」完成首轮基线记录。
              状态异常的网关可将鼠标悬浮在徽标上查看错误详情。
            </p>
          </section>

          {/* 操作区：手动补录 + 间隔配置 */}
          <section className="mb-6 grid min-w-0 gap-4 lg:grid-cols-2">
            <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
              <CardContent className="space-y-3 p-4 sm:p-5">
                <div className="flex items-center gap-2">
                  <Plus className="size-4 text-primary" />
                  <h3 className="text-sm font-semibold">手动补录模型</h3>
                  <span className="text-[11px] text-muted-foreground">
                    仅 manual 网关（无上游清单端点）
                  </span>
                </div>
                <div className="grid gap-3 sm:grid-cols-2">
                  <div className="space-y-1.5">
                    <Label htmlFor="model-sync-gateway">网关</Label>
                    <Select
                      value={form.gateway}
                      onValueChange={(v) => setForm((f) => ({ ...f, gateway: v }))}
                    >
                      <SelectTrigger id="model-sync-gateway" className="w-full">
                        <SelectValue placeholder="选择网关" />
                      </SelectTrigger>
                      <SelectContent>
                        {MANUAL_GATEWAYS.map((id) => (
                          <SelectItem key={id} value={id}>
                            {gatewayLabel(id)}（{id}）
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                  <div className="space-y-1.5">
                    <Label htmlFor="model-sync-model-id">模型 ID</Label>
                    <Input
                      id="model-sync-model-id"
                      value={form.modelId}
                      onChange={(e) => setForm((f) => ({ ...f, modelId: e.target.value }))}
                      placeholder="如 glm-5.3-flash"
                      className="font-mono text-xs"
                    />
                  </div>
                  <div className="space-y-1.5">
                    <Label htmlFor="model-sync-display-name">显示名（可选）</Label>
                    <Input
                      id="model-sync-display-name"
                      value={form.displayName}
                      onChange={(e) => setForm((f) => ({ ...f, displayName: e.target.value }))}
                      placeholder="如 GLM-5.3-Flash"
                    />
                  </div>
                  <div className="space-y-1.5">
                    <Label htmlFor="model-sync-note">备注（可选）</Label>
                    <Input
                      id="model-sync-note"
                      value={form.note}
                      onChange={(e) => setForm((f) => ({ ...f, note: e.target.value }))}
                      placeholder="补录原因，便于回溯"
                    />
                  </div>
                </div>
                <div className="flex items-center justify-between gap-3">
                  <p className="min-w-0 text-[11px] text-muted-foreground">
                    补录后立即合入该网关的模型目录（写入注册表）。
                  </p>
                  <Button size="sm" onClick={() => void handleAddRegistry()} disabled={adding} className="shrink-0">
                    {adding ? <Loader2 className="animate-spin" /> : <Plus />}补录
                  </Button>
                </div>
              </CardContent>
            </Card>

            <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
              <CardContent className="space-y-3 p-4 sm:p-5">
                <div className="flex items-center gap-2">
                  <ArrowLeftRight className="size-4 text-primary" />
                  <h3 className="text-sm font-semibold">同步配置</h3>
                  {configPaused ? (
                    <Badge variant="outline" className={cn("h-5 px-1.5 text-[10px]", PENDING_BADGE)}>
                      定时轮暂停中
                    </Badge>
                  ) : null}
                </div>
                <div className="flex items-center justify-between gap-3 rounded-lg border border-border/60 px-3 py-2.5">
                  <div className="min-w-0">
                    <Label htmlFor="model-sync-enabled">启用定时同步</Label>
                    <p className="mt-0.5 text-[11px] text-muted-foreground">
                      关闭开关或间隔为 0 均会暂停定时轮
                    </p>
                  </div>
                  <Switch
                    id="model-sync-enabled"
                    checked={enabledInput}
                    onCheckedChange={setEnabledInput}
                  />
                </div>
                <div className="space-y-1.5">
                  <Label htmlFor="model-sync-interval">同步间隔（分钟）</Label>
                  <Input
                    id="model-sync-interval"
                    type="number"
                    min={0}
                    step={1}
                    inputMode="numeric"
                    value={intervalInput}
                    onChange={(e) => setIntervalInput(e.target.value)}
                    className="max-w-40 tabular-nums"
                  />
                  <p className="text-[11px] text-muted-foreground">
                    0 = 暂停定时轮（仍可手动「立即同步」）；默认 1440（每天一次）。保存后热生效，下一轮按新间隔运行。
                  </p>
                </div>
                <div className="flex justify-end">
                  <Button size="sm" variant="outline" onClick={() => void handleSaveConfig()} disabled={savingConfig}>
                    {savingConfig ? <Loader2 className="animate-spin" /> : null}保存配置
                  </Button>
                </div>
              </CardContent>
            </Card>
          </section>

          {/* 历史轮次（默认近 10 轮，可展开） */}
          <section className="min-w-0 space-y-2.5">
            <SectionTitle>历史轮次（近 {rounds.length} 轮）</SectionTitle>
            <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
              <CardContent className="p-0">
                {logError ? (
                  <p className="p-4 text-sm text-destructive">历史日志加载失败：{logError}</p>
                ) : rounds.length === 0 ? (
                  <div className="p-4">
                    <DashedHint text="暂无同步轮次 —— 点击「立即同步」完成首轮，或等待定时轮触发后生成。" />
                  </div>
                ) : (
                  <div>
                    {rounds.map((round, i) => (
                      <RoundRow
                        key={normalizeMs(round.ts) ?? `round-${i}`}
                        round={round}
                        expanded={expandedRounds.has(i)}
                        onToggle={() => toggleRound(i)}
                      />
                    ))}
                  </div>
                )}
              </CardContent>
            </Card>
            {rounds.length >= logLimit ? (
              <div className="flex justify-center">
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => setLogLimit((n) => n + 10)}
                  disabled={loading}
                  className="text-xs text-muted-foreground"
                >
                  加载更早轮次
                </Button>
              </div>
            ) : null}
          </section>
        </div>
      )}
    </div>
  );
}
