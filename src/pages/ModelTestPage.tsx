import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import { useSearchParams } from "react-router-dom";
import { FlaskConical, ListChecks, Loader2, Play, Square, Trash2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import * as api from "@/lib/api";
import type { GatewayInfo } from "@/lib/api";
import { MODEL_TEST_CODE_META, type ModelTestResult } from "@/lib/model-test-types";
import { useModelTestChat, type ChatTurn, type TurnItem } from "@/lib/use-model-test-chat";
import { cn } from "@/lib/utils";

/**
 * 模型可用性测试 · 聊天式界面。
 *
 * 为什么改成聊天式：每次「发送」= 对当前网关勾选的全部模型扇出发起真实探测，
 * 各模型的结果按 pending → running → done 渐进落进气泡，比「表单 + 结果列表」
 * 更贴合「发起一批、逐个出结论」的编排心智。注意它不是多轮对话——每轮独立、
 * 不携带上下文（空态卡片里有诚实声明），聊天形态只是编排视图。
 *
 * 本文件只做界面编排：执行循环 / 并发 / 持久化全部在 useModelTestChat
 * （契约 §4 冻结接口），页面不自己发请求。
 */

const DEFAULT_PROMPT = "只回复两个字：可用";

/**
 * 聊天输入框样式：契约规定的 input.tsx 调整变体。
 * 为什么不逐字复用 input.tsx：原串里的 h-9、file:* 等是 input 专属类，对 textarea 无意义；
 * 这里保留同款的边框 / ring / placeholder 观感，把高度与圆角换成聊天气泡语境的形态。
 */
const PROMPT_TEXTAREA_CLASS =
  "flex w-full rounded-xl border border-input bg-transparent px-3 py-2.5 text-sm shadow-xs placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 aria-invalid:border-destructive min-h-[52px] max-h-40 resize-none";

function formatClock(ts: number): string {
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

function formatLatency(ms: number): string {
  return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(2)} s`;
}

/**
 * 侧栏模型条目：在 api.ts GatewayModelInfo（id/name/capabilities）之上宽读网关总览
 * 新直传的可选字段（契约 §2.2/§3.1）。exposed 等类型在 api.ts 补齐之前先在此声明——
 * 运行时 JSON 透传不受 TS 类型影响，后端没带该字段时是 undefined，行为与旧版一致。
 */
type SidebarModel = {
  id: string;
  name: string | null;
  capabilities: string[];
  exposed?: boolean;
  /** 所属区域（WorkBuddy 池：cn=国内版 / intl=国际版）；其余网关缺省。 */
  region?: string | null;
};

/** 批量回合 id 的占位哨兵：chat.send() 同步入列后，下一渲染由捕获 effect 用真实 turn id 覆盖
 * （真实 id 形如 `${毫秒时间戳}-${序号}`，与本哨兵无碰撞）。 */
const BATCH_TURN_PENDING = "batch-pending";

/**
 * wbServedBy 透传展示（契约 §3.3 条件性口径）：model_probe 若透传了该字段就原样展示，
 * 用于区分 workbuddy-native 直连与 opencode 兜底；响应不带则返回 null、不渲染（降级）。
 * ModelTestResult 类型暂无此字段（model-test-types.ts 冻结形状），故宽读并验型。
 */
function readServedBy(result: ModelTestResult): string | null {
  const value: unknown = (result as { wbServedBy?: unknown }).wbServedBy;
  return typeof value === "string" && value.trim() !== "" ? value : null;
}

/** 状态徽章：复用失败码元数据表，配色按 tone 收敛成三种（与旧页一致）。 */
function ResultBadge({ result }: { result: ModelTestResult }) {
  const meta = MODEL_TEST_CODE_META[result.code] ?? { label: result.code, tone: "danger" as const };
  const toneClass =
    meta.tone === "ok"
      ? "bg-emerald-500/15 text-emerald-700 dark:text-emerald-400"
      : meta.tone === "warn"
        ? "bg-amber-500/15 text-amber-700 dark:text-amber-400"
        : "bg-rose-500/15 text-rose-700 dark:text-rose-400";
  return (
    <Badge variant="secondary" className={cn("shrink-0 border-0 font-medium", toneClass)}>
      {result.ok ? "✅ 成功" : "❌ 失败"} · {meta.label}
    </Badge>
  );
}

/** 侧栏状态点配色：未测=muted、进行中=sky 脉冲、有结论按 code tone 映射三色。 */
function statusDotClass(isRunning: boolean, last: ModelTestResult | null): string {
  if (isRunning) return "bg-sky-500 animate-pulse";
  if (!last) return "bg-muted-foreground/40";
  const tone = MODEL_TEST_CODE_META[last.code]?.tone ?? "danger";
  return tone === "ok" ? "bg-emerald-500" : tone === "warn" ? "bg-amber-500" : "bg-rose-500";
}

/** 顶部统计 chip：muted 仅用于「待测」（它没有对应的 code tone）。 */
function StatChip({ tone, label, count }: { tone: "ok" | "warn" | "danger" | "muted"; label: string; count: number }) {
  const dotClass =
    tone === "ok"
      ? "bg-emerald-500"
      : tone === "warn"
        ? "bg-amber-500"
        : tone === "danger"
          ? "bg-rose-500"
          : "bg-muted-foreground/40";
  return (
    <span className="inline-flex items-center gap-1.5 rounded-full border bg-card px-2.5 py-0.5 text-xs text-muted-foreground">
      <span className={cn("size-2 rounded-full", dotClass)} />
      {label}
      <span className="font-medium tabular-nums text-foreground">{count}</span>
    </span>
  );
}

/** 一轮里的单个模型气泡：pending/running 是骨架态，done 展开完整结论。 */
function ModelBubble({ item, startAt }: { item: TurnItem; startAt?: number }) {
  if (item.state !== "done") {
    const queued = item.state === "pending";
    // 已等待秒数是契约里的可选项：没有起始时刻（理论不可达）时静默退化为纯文案
    const waited =
      !queued && startAt != null ? `（已等待 ${Math.max(1, Math.round((Date.now() - startAt) / 1000))} 秒）` : "";
    return (
      <div className="max-w-[92%] rounded-2xl rounded-bl-md border bg-card px-4 py-3 text-sm">
        <div className="flex items-center gap-2">
          <Loader2 className="size-3.5 shrink-0 animate-spin text-muted-foreground" />
          <span className="font-medium">{item.model}</span>
          <span className="text-xs text-muted-foreground">{queued ? "排队中…" : `请求中…${waited}`}</span>
        </div>
      </div>
    );
  }

  const { result, modelKnown } = item;
  return (
    <div className="max-w-[92%] rounded-2xl rounded-bl-md border bg-card px-4 py-3 text-sm">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium">{result.model}</span>
        <ResultBadge result={result} />
        <span className="ml-auto shrink-0 text-xs tabular-nums text-muted-foreground">
          {formatLatency(result.latencyMs)}
        </span>
      </div>

      <p className={cn("mt-2 break-words", result.ok ? "text-foreground" : "text-rose-600 dark:text-rose-400")}>
        {result.reason}
      </p>

      {modelKnown === false ? (
        <p className="mt-2 rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400">
          ⚠️ 「{result.model}」不在 {result.gatewayName ?? result.gateway} 的模型目录里。
          本机网关对未知模型名会按兜底路由处理（实测请求不存在的模型名同样返回 200），
          所以这条结论**不能代表该模型本身可用**。请从下拉列表选择，或核对模型 ID 拼写。
        </p>
      ) : null}

      {result.reply ? (
        <div className="mt-2 rounded-lg bg-muted/50 px-3 py-2 text-xs">
          <span className="text-muted-foreground">回复</span>
          <span className="ml-2 break-words">{result.reply}</span>
        </div>
      ) : null}

      {result.reasoning ? (
        <details className="mt-2 text-xs text-muted-foreground">
          <summary className="cursor-pointer select-none">思维链</summary>
          <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-lg bg-muted/50 px-3 py-2">
            {result.reasoning}
          </pre>
        </details>
      ) : null}

      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
        {result.status != null ? <span>HTTP {result.status}</span> : null}
        {readServedBy(result) ? (
          // 响应带 wbServedBy 才展示（契约 §3.3）：native 直连 / opencode-fallback 兜底一目了然
          <span title="响应来源：workbuddy-native=内置网关直连，opencode-fallback=OpenCode 兜底">
            wbServedBy {readServedBy(result)}
          </span>
        ) : null}
        {result.tokens?.total != null ? (
          <span>
            tokens {result.tokens.prompt ?? "?"}+{result.tokens.completion ?? "?"}={result.tokens.total}
          </span>
        ) : null}
        {result.endpoint ? <span className="break-all">{result.endpoint}</span> : null}
      </div>

      {result.raw ? (
        <details className="mt-2 text-xs text-muted-foreground">
          <summary className="cursor-pointer select-none">上游原始响应（截断）</summary>
          <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-lg bg-muted/50 px-3 py-2">
            {result.raw}
          </pre>
        </details>
      ) : null}
    </div>
  );
}

/** 一轮 = 用户气泡（右）+ 该轮各模型的结果气泡（左，单列堆叠）。 */
function TurnBlock({ turn, runningStarts }: { turn: ChatTurn; runningStarts: Map<string, number> }) {
  return (
    <div className="space-y-3">
      <div className="ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-primary/10 px-4 py-2.5">
        <p className="whitespace-pre-wrap break-words text-sm">{turn.prompt}</p>
        <p className="mt-1.5 text-xs text-muted-foreground">
          {turn.gatewayName ?? turn.gateway} · {turn.targets.length} 个模型 · {formatClock(turn.at)}
        </p>
      </div>
      <div className="space-y-2">
        {turn.results.map((item) => (
          <ModelBubble
            key={`${item.model}:${item.state}`}
            item={item}
            startAt={item.state === "running" ? runningStarts.get(`${turn.id}::${item.model}`) : undefined}
          />
        ))}
      </div>
    </div>
  );
}

/** 空态：说清用法，并诚实声明这是单轮批量编排器、不是多轮对话。 */
function EmptyState() {
  return (
    <div className="flex max-w-md flex-col items-center gap-3 text-center">
      <div className="flex size-10 items-center justify-center rounded-full bg-muted">
        <FlaskConical className="size-5 text-muted-foreground" />
      </div>
      <p className="text-sm font-medium">把这里当成一次「扇出体检」</p>
      <p className="text-xs leading-relaxed text-muted-foreground">
        这是批量测试编排器：每次发送对每个勾选模型独立单轮调用，不携带上下文——
        发送内容会作为探测提示词，留空则使用默认提示词。选好网关后直接在下方输入框发送即可开始。
      </p>
    </div>
  );
}

export default function ModelTestPage() {
  const [gateways, setGateways] = useState<GatewayInfo[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);

  // ---- URL 预选（契约 §3.3）：?gateway=<id>&model=<id> 只生效一次 ----
  // useSearchParams 在本工程有先例（StatsPage.tsx），只读不写、不引入新路由行为；
  // gateway 直接做初始网关；model 存进 ref，待目标网关目录首次到位时消费一次
  // （仅勾选该模型，使其成为首个测试目标），消费后回归既有「全选」恢复逻辑。
  const [searchParams] = useSearchParams();
  /** 待消费的 query model；null = 已消费或无预选。放 ref：一次性语义，不参与渲染。 */
  const queryModelRef = useRef<string | null>(searchParams.get("model"));
  /** 待消费的 query region（cn/intl，来自 WorkBuddy 页模型表「测试」按钮）；
   *  与 queryModel 同款一次性语义：目录与区域状态就绪后消费。 */
  const queryRegionRef = useRef<string | null>(searchParams.get("region"));

  const [gatewayId, setGatewayId] = useState(() => searchParams.get("gateway") ?? "");
  /** 当前网关内被勾选的模型 id 集合；切换网关时整体重置为全选。 */
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [prompt, setPrompt] = useState("");

  const enabled = api.isWebui() && !api.isDemoMode();

  // ---- 网关清单加载（与旧页一致）----
  const loadGateways = useCallback(async () => {
    try {
      const data = await api.getGatewaysOverview();
      setGateways(data.gateways ?? []);
      setLoadError(null);
    } catch (e) {
      setLoadError(api.asError(e));
    }
  }, []);

  useEffect(() => {
    if (!enabled) return;
    void loadGateways();
  }, [enabled, loadGateways]);

  // ---- 「模型是否真的在目录里」判定：供 hook 在结果落地时调用 ----
  // 目录快照放 ref 而不是 state：resolveKnown 必须是稳定引用（hook 侧可能只在挂载时捕获一次），
  // 否则每次网关刷新都会换掉函数身份。
  const catalogRef = useRef(new Map<string, Set<string>>());
  useEffect(() => {
    const map = new Map<string, Set<string>>();
    for (const g of gateways) {
      map.set(g.id, new Set((g.models ?? []).map((m) => m.id).filter((id): id is string => !!id)));
    }
    catalogRef.current = map;
  }, [gateways]);

  const resolveKnown = useCallback((gateway: string, model: string): boolean | null => {
    // 目录为空（网关未列模型）时判定为「未知」而不是「不存在」，避免误报警告
    const catalog = catalogRef.current.get(gateway);
    return !catalog || catalog.size === 0 ? null : catalog.has(model);
  }, []);

  // ---- 会话状态机（契约 §4 冻结接口）----
  const chat = useModelTestChat({ resolveKnown });
  const busy = chat.activeTurnId !== null;
  const activeTurn = useMemo(
    () => (chat.activeTurnId ? (chat.turns.find((t) => t.id === chat.activeTurnId) ?? null) : null),
    [chat.activeTurnId, chat.turns],
  );
  /** 正在请求中的模型集合：侧栏状态点据此显示 sky 脉冲。 */
  const runningModels = useMemo(() => {
    const set = new Set<string>();
    if (activeTurn) {
      for (const item of activeTurn.results) {
        if (item.state === "running") set.add(item.model);
      }
    }
    return set;
  }, [activeTurn]);

  // 「请求中…（已等待 N 秒）」：仅在测试进行中每秒走一次表针，空闲时不产生多余渲染
  const [, setClockTick] = useState(0);
  useEffect(() => {
    if (!busy) return;
    const timer = window.setInterval(() => setClockTick((n) => n + 1), 1000);
    return () => window.clearInterval(timer);
  }, [busy]);

  // 记录每个模型进入 running 的时刻（纯内存，回合结束即清），供已等待秒数展示
  const runningStartsRef = useRef(new Map<string, number>());
  useEffect(() => {
    if (!activeTurn) {
      runningStartsRef.current.clear();
      return;
    }
    for (const item of activeTurn.results) {
      if (item.state !== "running") continue;
      const key = `${activeTurn.id}::${item.model}`;
      if (!runningStartsRef.current.has(key)) runningStartsRef.current.set(key, Date.now());
    }
  }, [activeTurn]);

  // ---- 当前网关与勾选 ----
  const current = useMemo(() => gateways.find((g) => g.id === gatewayId) ?? null, [gateways, gatewayId]);
  const rawModels = useMemo(
    () => (current?.models ?? []).filter((m): m is SidebarModel => !!m.id),
    [current],
  );
  // 区域切换（WorkBuddy 池：条目自带 region=cn/intl，跨区同名各留一条）。
  // 「全部」模式按 id 去重（cn 先序保留一份），避免双按钮与勾选键冲突；
  // 具体区域模式下发送带 region 提示，池网关按请求体 region 精确切区选号。
  const hasRegions = useMemo(
    () => rawModels.some((m) => m.region === "cn" || m.region === "intl"),
    [rawModels],
  );
  const [regionFilter, setRegionFilter] = useState<"all" | "cn" | "intl">("all");
  const models = useMemo(() => {
    if (!hasRegions || regionFilter === "all") {
      // 「全部」：按 id 去重（保持原顺序），无区域语义网关原样返回
      const seen = new Set<string>();
      return rawModels.filter((m) => (m.id ? (seen.has(m.id) ? false : (seen.add(m.id), true)) : true));
    }
    return rawModels.filter((m) => m.region === regionFilter);
  }, [rawModels, regionFilter, hasRegions]);
  const targetRegions = useMemo(() => {
    if (!hasRegions || regionFilter === "all") return undefined;
    const map: Record<string, string> = {};
    for (const m of models) {
      if (m.id && m.region) map[m.id] = m.region;
    }
    return map;
  }, [hasRegions, regionFilter, models]);

  // 切换网关（或清单首次到位）时重置勾选为全选；会话历史是跨网关的记录，不随之清空。
  // 例外（契约 §3.3 一次性预选）：URL 带的 model 在目标网关目录首次到位时消费——仅勾选它，
  // 使其成为首个测试目标；消费后（或它不在目录）回归既有全选恢复，之后网关切换不再生效。
  // ref 消费天然幂等：挂载时 gateways 还没到位（g 为空）不消费，StrictMode 双跑也安全。
  useEffect(() => {
    const g = gateways.find((x) => x.id === gatewayId);
    const ids = (g?.models ?? []).map((m) => m.id).filter((id): id is string => !!id);
    // 切网关时区域过滤复位（区域清单是池网关特有语义，别带着旧过滤进新网关）
    setRegionFilter("all");
    // query region 一次性消费：目录到位且该区有条目时预选该区域页签
    // （与 query model 的消费时机一致——目录未就绪时不消费， StrictMode 双跑安全）。
    const qr = queryRegionRef.current;
    if (qr != null && g && (qr === "cn" || qr === "intl")) {
      const regionHasEntries = (g.models ?? []).some((m) => m.region === qr);
      if (regionHasEntries) {
        queryRegionRef.current = null;
        setRegionFilter(qr);
      }
    }
    const qm = queryModelRef.current;
    if (qm != null && g) {
      queryModelRef.current = null; // 目录已到位即视为消费完毕，无论命中与否
      if (ids.includes(qm)) {
        setSelected(new Set([qm]));
        return;
      }
    }
    setSelected(new Set(ids));
  }, [gatewayId, gateways]);

  // 发送目标 = 勾选模型按侧栏顺序展开（顺序即勾选顺序）
  const targets = useMemo(() => models.filter((m) => selected.has(m.id)).map((m) => m.id), [models, selected]);
  const allSelected = models.length > 0 && targets.length === models.length;

  const toggleModel = useCallback((id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }, []);

  const toggleAll = useCallback(() => {
    setSelected(allSelected ? new Set<string>() : new Set(models.map((m) => m.id)));
  }, [allSelected, models]);

  // ---- 顶部统计 chips：只统计当前网关，lastByModel 与当前目录的交集 ----
  const gatewayStats = useMemo(() => {
    const stats = { ok: 0, warn: 0, danger: 0 };
    for (const m of models) {
      const last = chat.lastByModel.get(`${gatewayId}::${m.id}`);
      if (!last) continue;
      const tone = MODEL_TEST_CODE_META[last.code]?.tone ?? "danger";
      stats[tone] += 1;
    }
    return { ...stats, untested: models.length - stats.ok - stats.warn - stats.danger };
  }, [models, chat.lastByModel, gatewayId]);

  // ---- 动作 ----
  const sendBlockReason = !current ? "请先选择网关" : targets.length === 0 ? "请至少勾选一个模型" : null;

  const onSend = useCallback(() => {
    if (!current || targets.length === 0 || busy) return;
    chat.send({
      gateway: current.id,
      gatewayName: current.name,
      prompt: prompt.trim() || DEFAULT_PROMPT, // 契约：空文本由调用方兜底成默认提示词
      targets,
      targetRegions,
    });
    // 聊天惯例：发送后清空输入框，下一条留空即自动用默认提示词
    setPrompt("");
  }, [current, targets, targetRegions, busy, chat, prompt]);

  const onStop = useCallback(() => chat.stop(), [chat]);

  // ---- 批量测试本网关全部模型（契约 §3.3）----
  // 复用 hook 的 send：targets 传当前网关全部模型 id，并发 2 与逐个实时上屏都是 hook 内建
  // （CONCURRENCY=2 的 lane worker + markRunning/markDone），页面不自建执行循环。
  // send 在活跃回合未结束时会静默忽略重入，故按钮用 busy 态禁用、这里再守一层。
  const [batchTurnId, setBatchTurnId] = useState<string | null>(null);
  const [batchConfirmOpen, setBatchConfirmOpen] = useState(false);

  // send 同步入列后 activeTurnId 即批量回合 id；下一渲染捕获（幂等，StrictMode 双跑安全）
  useEffect(() => {
    if (batchTurnId === BATCH_TURN_PENDING && chat.activeTurnId !== null) {
      setBatchTurnId(chat.activeTurnId);
    }
  }, [batchTurnId, chat.activeTurnId]);

  const batchTurn = useMemo(
    () =>
      batchTurnId && batchTurnId !== BATCH_TURN_PENDING
        ? (chat.turns.find((t) => t.id === batchTurnId) ?? null)
        : null,
    [batchTurnId, chat.turns],
  );
  /** 批量进行中 = 批量回合正是当前活跃回合；其余 busy 场景来自手动发送，批量按钮只禁用。 */
  const batchRunning = batchTurnId !== null && chat.activeTurnId !== null && chat.activeTurnId === batchTurnId;

  /**
   * 批量汇总条（契约 §3.3「结束后顶部汇总条」）：批量回合 settle（activeTurnId 清空）后
   * 按回合内 done 项统计三档计数；提前停止时未开始的目标保持 pending、不计入，如实反映。
   */
  const batchSummary = useMemo(() => {
    if (!batchTurn || chat.activeTurnId === batchTurn.id) return null; // 进行中不显示汇总
    const stats = { ok: 0, warn: 0, danger: 0 };
    for (const item of batchTurn.results) {
      if (item.state !== "done") continue;
      const tone = MODEL_TEST_CODE_META[item.result.code]?.tone ?? "danger";
      stats[tone] += 1;
    }
    return stats;
  }, [batchTurn, chat.activeTurnId]);

  const batchBlockReason = !current
    ? "请先选择网关"
    : models.length === 0
      ? "该网关未列出模型"
      : busy
        ? "已有测试在进行，请等待当前轮结束"
        : null;

  const startBatch = useCallback(() => {
    if (!current || busy || models.length === 0) return;
    setBatchConfirmOpen(false);
    // 先置哨兵，chat.send 入列后由捕获 effect 换成真实回合 id
    setBatchTurnId(BATCH_TURN_PENDING);
    chat.send({
      gateway: current.id,
      gatewayName: current.name,
      prompt: prompt.trim() || DEFAULT_PROMPT, // 与手动发送同一兜底口径
      targets: models.map((m) => m.id), // 全部条目，与勾选无关（契约 §3.3）
      targetRegions, // 区域模式下逐模型带 region 提示（池网关精确切区）
    });
    setPrompt(""); // 与手动发送一致：发送后清空输入框
  }, [current, busy, models, targetRegions, chat, prompt]);

  const onClear = useCallback(() => {
    if (busy) return; // clear 仅空闲可用，按钮禁用与这里双保险
    chat.clear();
    setBatchTurnId(null); // 历史清空后同步撤掉批量汇总条
  }, [busy, chat]);

  // Enter 发送 / Shift+Enter 换行；IME 组合中的 Enter 是「选字上屏」不是发送意图
  const onPromptKeyDown = (e: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key !== "Enter" || e.shiftKey) return;
    if (e.nativeEvent.isComposing) return;
    e.preventDefault();
    if (busy) return; // 测试中输入框保持可编辑，但 Enter 不触发新一轮
    onSend();
  };

  // 输入增高：跟随内容撑高（上限 160px 与 max-h-40 一致），清空后自动缩回
  const promptRef = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    const el = promptRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
  }, [prompt]);

  // 自动滚动：turns 任一更新（新轮 / 状态迁移）都换新数组，统一跟随滚到底部。
  // 消息区内部滚动（见下方容器），正常情况下不会牵动整页。
  const scrollRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth", block: "end" });
  }, [chat.turns]);

  if (!enabled) {
    return (
      <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
        <Card>
          <CardContent className="p-5 text-sm text-muted-foreground">
            模型可用性测试仅在 webui 后端（Sylvia Switch 启动器）下可用。
          </CardContent>
        </Card>
      </div>
    );
  }

  return (
    <div className="mx-auto w-full max-w-6xl space-y-4 px-4 py-6 sm:px-6 sm:py-8">
      {/* 顶栏：标题 + 副标题 + 当前网关统计 chips（双栏上方通栏） */}
      <header className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
        <div className="flex items-center gap-2">
          <FlaskConical className="size-5 text-primary" />
          <h1 className="text-lg font-semibold">模型可用性测试</h1>
        </div>
        <p className="text-sm text-muted-foreground">
          发送即对勾选模型各发起一次真实调用，覆盖鉴权→路由→账号→上游全链路。
        </p>
        {current ? (
          <div className="ml-auto flex flex-wrap items-center gap-1.5">
            <StatChip tone="ok" label="可用" count={gatewayStats.ok} />
            <StatChip tone="warn" label="受限" count={gatewayStats.warn} />
            <StatChip tone="danger" label="不可用" count={gatewayStats.danger} />
            <StatChip tone="muted" label="待测" count={gatewayStats.untested} />
          </div>
        ) : null}
      </header>

      {/* 批量测试结束汇总条（契约 §3.3）：复用 StatChip 的视觉语言；下次批量或清空历史时撤掉 */}
      {batchSummary ? (
        <div className="flex flex-wrap items-center gap-2 rounded-xl border bg-card px-4 py-2 text-sm">
          <span className="text-xs font-medium text-muted-foreground">批量测试结束</span>
          <StatChip tone="ok" label="可用" count={batchSummary.ok} />
          <StatChip tone="danger" label="不可用" count={batchSummary.danger} />
          <StatChip tone="warn" label="受限" count={batchSummary.warn} />
        </div>
      ) : null}

      {/* 双栏：左模型清单 + 右聊天区；小屏单列时左栏收缩为一条可滚动的矮栏 */}
      <div className="grid gap-4 lg:grid-cols-[260px_minmax(0,1fr)]">
        <Card className="flex max-h-44 flex-col gap-3 overflow-y-auto px-4 py-4 lg:max-h-[calc(clamp(320px,58vh,720px)_+_2px)] lg:overflow-hidden">
          <Select value={gatewayId} onValueChange={setGatewayId} disabled={busy}>
            <SelectTrigger className="w-full" aria-label="选择网关">
              <SelectValue placeholder="选择网关" />
            </SelectTrigger>
            <SelectContent>
              {gateways.map((g) => (
                <SelectItem key={g.id} value={g.id}>
                  {g.name}
                  {g.models.length ? `（${g.models.length} 个模型）` : "（无模型）"}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>

          {loadError ? (
            <p className="text-xs text-rose-600 dark:text-rose-400">加载网关清单失败：{loadError}</p>
          ) : null}

          {current ? (
            <>
              <div className="flex items-center justify-between">
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 px-2 text-xs"
                  onClick={toggleAll}
                  disabled={!models.length}
                >
                  {allSelected ? "全不选" : "全选"}
                </Button>
                <span className="text-xs tabular-nums text-muted-foreground">
                  已选 {targets.length}/{models.length}
                </span>
              </div>

              {hasRegions ? (
                // 区域切换（WorkBuddy 池）：选国内版只显示国内模型，国际版同理
                <div className="mb-1 flex gap-1" role="tablist" aria-label="模型区域">
                  {(
                    [
                      ["all", "全部"],
                      ["cn", "国内版"],
                      ["intl", "国际版"],
                    ] as const
                  ).map(([value, label]) => (
                    <button
                      key={value}
                      type="button"
                      role="tab"
                      aria-selected={regionFilter === value}
                      onClick={() => setRegionFilter(value)}
                      className={cn(
                        "rounded-full px-2.5 py-1 text-xs transition-colors",
                        regionFilter === value
                          ? "bg-primary/15 font-medium text-foreground"
                          : "text-muted-foreground hover:bg-muted/60",
                      )}
                    >
                      {label}
                    </button>
                  ))}
                </div>
              ) : null}

              {models.length === 0 ? (
                <p className="text-xs text-muted-foreground">
                  {hasRegions && regionFilter !== "all"
                    ? regionFilter === "cn"
                      ? "国内版暂无模型。"
                      : "国际版暂无模型。"
                    : "该网关未列出模型。"}
                </p>
              ) : (
                <div className="min-h-0 flex-1 space-y-0.5 overflow-y-auto">
                  {models.map((m) => {
                    const isRunning = runningModels.has(m.id);
                    const last = chat.lastByModel.get(`${gatewayId}::${m.id}`) ?? null;
                    const isSelected = selected.has(m.id);
                    return (
                      <button
                        key={m.id}
                        type="button"
                        aria-pressed={isSelected}
                        onClick={() => toggleModel(m.id)}
                        className={cn(
                          "flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left text-sm transition-colors",
                          isSelected ? "bg-primary/10" : "hover:bg-muted/60",
                        )}
                      >
                        <span className={cn("size-2 shrink-0 rounded-full", statusDotClass(isRunning, last))} />
                        <span className="min-w-0 flex-1 truncate" title={m.id}>
                          {m.name && m.name !== m.id ? m.name : m.id}
                        </span>
                        {m.exposed === false ? (
                          // 网关总览直传 exposed=false（契约 §2.2/§3.3）：ACP 暂未暴露、来自客户端产品配置
                          <span
                            className="shrink-0 rounded bg-muted px-1 py-0.5 text-[10px] leading-4 text-muted-foreground"
                            title="ACP session/new 暂未暴露，来自客户端产品配置，可调用性以实测为准"
                          >
                            隐藏
                          </span>
                        ) : null}
                        {hasRegions && m.region ? (
                          // 区域徽章（仅「全部」视图显示，区域视图下整列同区无需重复）
                          <span
                            className={cn(
                              "shrink-0 rounded px-1 py-0.5 text-[10px] leading-4",
                              m.region === "intl"
                                ? "bg-sky-500/10 text-sky-600 dark:text-sky-400"
                                : "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400",
                            )}
                          >
                            {m.region === "intl" ? "国际版" : "国内版"}
                          </span>
                        ) : null}
                        {last ? (
                          <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
                            {formatLatency(last.latencyMs)}
                          </span>
                        ) : null}
                      </button>
                    );
                  })}
                </div>
              )}
            </>
          ) : !loadError ? (
            <p className="text-xs text-muted-foreground">
              {gateways.length === 0 ? "正在加载网关清单…" : "选择网关后这里会列出它的全部模型，点击行可勾选 / 取消。"}
            </p>
          ) : null}
        </Card>

        {/* 消息区：Card 内独立滚动，高度用 clamp 让页面主体尽量不出整页滚动条 */}
        <Card className="overflow-hidden py-0">
          <CardContent className="p-0">
            <div ref={scrollRef} className="h-[clamp(320px,58vh,720px)] overflow-y-auto">
              <div className="flex min-h-full flex-col px-4 py-4">
                {chat.turns.length === 0 ? (
                  <div className="flex flex-1 items-center justify-center">
                    <EmptyState />
                  </div>
                ) : (
                  <div className="space-y-6">
                    {chat.turns.map((turn) => (
                      <TurnBlock key={turn.id} turn={turn} runningStarts={runningStartsRef.current} />
                    ))}
                  </div>
                )}
                {/* 底部哨兵：自动滚动目标 */}
                <div ref={bottomRef} aria-hidden="true" />
              </div>
            </div>
          </CardContent>
        </Card>
      </div>

      {/* 输入区（通栏）：textarea 样式为契约规定的 input.tsx 变体，Enter 发送 / Shift+Enter 换行 */}
      <div className="space-y-2">
        <textarea
          ref={promptRef}
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          onKeyDown={onPromptKeyDown}
          placeholder={`默认提示词：${DEFAULT_PROMPT}`}
          rows={2}
          aria-label="测试提示词"
          className={PROMPT_TEXTAREA_CLASS}
        />
        <div className="flex flex-wrap items-center gap-2">
          <Button variant="ghost" size="sm" onClick={onClear} disabled={busy || chat.turns.length === 0}>
            <Trash2 className="size-4" />
            清空
          </Button>
          <span className="hidden text-xs text-muted-foreground sm:inline">Enter 发送 · Shift+Enter 换行</span>
          {batchRunning ? (
            // 运行中变停止（契约 §3.3）：复用 hook 既有 stop 语义——未开始的目标不再发起，
            // 进行中的 1-2 个请求等待自然结束、结果照常落
            <Button variant="outline" className="ml-auto" onClick={onStop}>
              <Square className="size-4" />
              停止批量
            </Button>
          ) : (
            // title 挂外层容器的原因与下方发送按钮相同（disabled:pointer-events-none 穿透）
            <span className="ml-auto inline-flex" title={batchBlockReason ?? undefined}>
              <Button variant="outline" onClick={() => setBatchConfirmOpen(true)} disabled={!!batchBlockReason}>
                <ListChecks className="size-4" />
                批量测试本网关全部模型
              </Button>
            </span>
          )}
          {busy ? (
            <Button variant="outline" onClick={onStop}>
              <Square className="size-4" />
              停止
            </Button>
          ) : (
            // title 必须挂在外层容器而不是 Button：ui/button 基类带 disabled:pointer-events-none
            // （button.tsx:8），禁用态不接收 hover，挂在按钮上的原生 title tooltip 永远不会触发，
            // 契约 §5「title 提示原因」就到不了用户。pointer-events-none 的子元素会把命中测试
            // 穿透给父级，所以禁用时外层 span 能收到 hover 并弹出原因；启用时本来无需提示。
            <span className="inline-flex" title={sendBlockReason ?? undefined}>
              <Button onClick={onSend} disabled={!!sendBlockReason}>
                <Play className="size-4" />
                发送
              </Button>
            </span>
          )}
        </div>
      </div>

      {/* 批量确认（契约 §3.3「confirm 后执行」）：桌面 App 的 Tauri WebView 不支持 window.confirm
          （AccountsPage.tsx:478 先例），故用 Dialog 确认 */}
      <Dialog open={batchConfirmOpen} onOpenChange={setBatchConfirmOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>批量测试全部模型</DialogTitle>
            <DialogDescription>
              将对「{current?.name ?? current?.id}」的全部 {models.length} 个模型逐个发起真实调用
              （并发 2，逐个实时上屏）。模型较多时耗时较长，可随时停止。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setBatchConfirmOpen(false)}>
              取消
            </Button>
            <Button onClick={startBatch} disabled={!current || models.length === 0 || busy}>
              开始批量测试
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
