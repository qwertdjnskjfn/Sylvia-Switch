import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { FlaskConical, Loader2, Play, Square, Trash2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import * as api from "@/lib/api";
import type { GatewayInfo } from "@/lib/api";
import { MODEL_TEST_CODE_META, type ModelTestResult } from "@/lib/model-test-types";
import { cn } from "@/lib/utils";

/** 手动输入模型的哨兵值：与 Radix Select 的「不能为空串」限制兼容。 */
const MANUAL_MODEL = "__manual__";
const DEFAULT_PROMPT = "只回复两个字：可用";

/**
 * UI 侧在探测结果上追加的一层判断，**不进 API 契约**。
 *
 * 为什么需要它：本机网关对**任意**模型名都会兜底路由 —— 实测请求
 * `model: "__definitely_not_a_model__"` 照样返回 200 与正文，
 * 且响应里的 `model` 字段回显请求值。所以「调通」不等于「该模型存在」。
 * 判定放到前端是因为目录本来就已经加载在内存里（下拉列表就是它），
 * 放后端会给每次探测多加一次目录请求/延迟。
 */
type TestRecord = ModelTestResult & { modelKnown: boolean | null };

function formatClock(ts: number): string {
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

function formatLatency(ms: number): string {
  return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(2)} s`;
}

/** 状态徽章：复用失败码元数据表，配色按 tone 收敛成三种。 */
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

function ResultCard({ result }: { result: TestRecord }) {
  return (
    <div className="rounded-xl border bg-card px-4 py-3 text-sm">
      <div className="flex flex-wrap items-center gap-2">
        <ResultBadge result={result} />
        <span className="font-medium">
          {result.gatewayName ?? result.gateway} · {result.model}
        </span>
        <span className="text-xs text-muted-foreground">{formatClock(result.at)}</span>
        <span className="ml-auto text-xs tabular-nums text-muted-foreground">
          {formatLatency(result.latencyMs)}
        </span>
      </div>

      <p className={cn("mt-2 break-words", result.ok ? "text-foreground" : "text-rose-600 dark:text-rose-400")}>
        {result.reason}
      </p>

      {result.modelKnown === false ? (
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

      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
        {result.status != null ? <span>HTTP {result.status}</span> : null}
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

export default function ModelTestPage() {
  const [gateways, setGateways] = useState<GatewayInfo[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [gatewayId, setGatewayId] = useState("");
  const [modelChoice, setModelChoice] = useState("");
  const [manualModel, setManualModel] = useState("");
  const [prompt, setPrompt] = useState(DEFAULT_PROMPT);
  const [maxTokens, setMaxTokens] = useState("64");

  const [results, setResults] = useState<TestRecord[]>([]);
  const [testing, setTesting] = useState(false);
  const [pendingModel, setPendingModel] = useState<string | null>(null);
  const stopRef = useRef(false);
  const seqRef = useRef(0);
  /** 各网关的模型目录快照（探测时点取值，避免用户事后切换网关影响历史结论）。 */
  const catalogRef = useRef<Map<string, Set<string>>>(new Map());

  const enabled = api.isWebui() && !api.isDemoMode();

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

  // 目录快照：网关模型列表刷新时同步，供「模型是否真的存在」判定使用
  useEffect(() => {
    const map = new Map<string, Set<string>>();
    for (const g of gateways) {
      map.set(
        g.id,
        new Set((g.models ?? []).map((m) => m.id).filter((id): id is string => !!id)),
      );
    }
    catalogRef.current = map;
  }, [gateways]);

  const current = useMemo(
    () => gateways.find((g) => g.id === gatewayId) ?? null,
    [gateways, gatewayId],
  );

  // 切换网关时重置模型选择，避免把上一个网关的模型带过去
  useEffect(() => {
    setModelChoice("");
    setManualModel("");
  }, [gatewayId]);

  const isManual = modelChoice === MANUAL_MODEL;
  const effectiveModel = isManual ? manualModel.trim() : modelChoice;
  const models = useMemo(
    () => (current?.models ?? []).filter((m): m is { id: string; name: string | null; capabilities: string[] } => !!m.id),
    [current],
  );

  const runOne = useCallback(
    async (gw: string, model: string): Promise<TestRecord> => {
      const body = {
        gateway: gw,
        model,
        prompt: prompt.trim() || DEFAULT_PROMPT,
        maxTokens: Number(maxTokens) || 64,
      };
      // 目录为空（网关未列模型）时判定为「未知」而不是「不存在」，避免误报警告
      const catalog = catalogRef.current.get(gw);
      const modelKnown = !catalog || catalog.size === 0 ? null : catalog.has(model);
      try {
        return { ...(await api.testModel(body)), modelKnown };
      } catch (e) {
        // 接口本身失败（服务未启动/路由缺失）也归一成一条结果，保证列表里看得到原因
        return {
          at: Date.now(),
          ok: false,
          code: "network_error",
          reason: `无法调用自测接口：${api.asError(e)}`,
          gateway: gw,
          gatewayName: null,
          model,
          endpoint: null,
          protocol: "openai-chat",
          prompt: body.prompt,
          maxTokens: body.maxTokens,
          status: null,
          latencyMs: 0,
          reply: null,
          reasoning: null,
          tokens: null,
          raw: null,
          modelKnown,
        };
      }
    },
    [prompt, maxTokens],
  );

  /** 串行执行一批模型；串行是刻意的 —— 并发会把「限流」和「真的不可用」混在一起。 */
  const runBatch = useCallback(
    async (gw: string, targets: string[]) => {
      const seq = ++seqRef.current;
      stopRef.current = false;
      setTesting(true);
      try {
        for (const model of targets) {
          if (stopRef.current || seq !== seqRef.current) break;
          setPendingModel(model);
          const res = await runOne(gw, model);
          if (seq !== seqRef.current) return;
          setResults((prev) => [res, ...prev]);
        }
      } finally {
        if (seq === seqRef.current) {
          setTesting(false);
          setPendingModel(null);
        }
      }
    },
    [runOne],
  );

  const onTestOne = useCallback(() => {
    if (!gatewayId || !effectiveModel) return;
    void runBatch(gatewayId, [effectiveModel]);
  }, [gatewayId, effectiveModel, runBatch]);

  const onTestAll = useCallback(() => {
    if (!gatewayId) return;
    void runBatch(
      gatewayId,
      models.map((m) => m.id),
    );
  }, [gatewayId, models, runBatch]);

  const onStop = useCallback(() => {
    stopRef.current = true;
  }, []);

  const stats = useMemo(() => {
    const ok = results.filter((r) => r.ok).length;
    const done = results.length;
    const avg = done ? Math.round(results.reduce((s, r) => s + r.latencyMs, 0) / done) : 0;
    return { ok, fail: done - ok, done, avg };
  }, [results]);

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
    <div className="mx-auto w-full max-w-5xl space-y-4 px-4 py-6 sm:px-6 sm:py-8">
      <div className="flex items-center gap-2">
        <FlaskConical className="size-5 text-primary" />
        <h1 className="text-lg font-semibold">模型可用性测试</h1>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">选择目标模型并发起调用</CardTitle>
          <CardDescription>
            请求会打到本机网关自己的对外端点（鉴权 → 路由 → 账号 → 上游 全链路），
            因此这里的失败原因就是客户端会遇到的原因。失败也是一条有效结论。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {loadError ? (
            <p className="text-sm text-rose-600 dark:text-rose-400">加载网关清单失败：{loadError}</p>
          ) : null}

          <div className="grid gap-4 sm:grid-cols-2">
            <div className="space-y-1.5">
              <Label htmlFor="mt-gateway">网关</Label>
              <Select value={gatewayId} onValueChange={setGatewayId}>
                <SelectTrigger id="mt-gateway" className="w-full">
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
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="mt-model">模型</Label>
              <Select value={modelChoice} onValueChange={setModelChoice} disabled={!gatewayId}>
                <SelectTrigger id="mt-model" className="w-full">
                  <SelectValue placeholder={gatewayId ? "选择模型" : "请先选择网关"} />
                </SelectTrigger>
                <SelectContent>
                  {models.map((m) => (
                    <SelectItem key={m.id} value={m.id}>
                      {m.name && m.name !== m.id ? `${m.name}（${m.id}）` : m.id}
                    </SelectItem>
                  ))}
                  <SelectItem value={MANUAL_MODEL}>✏️ 手动输入模型 ID…</SelectItem>
                </SelectContent>
              </Select>
            </div>

            {isManual ? (
              <div className="space-y-1.5 sm:col-span-2">
                <Label htmlFor="mt-manual">模型 ID</Label>
                <Input
                  id="mt-manual"
                  value={manualModel}
                  onChange={(e) => setManualModel(e.target.value)}
                  placeholder="例如 glm-5.3 / step-3.7-flash"
                />
              </div>
            ) : null}

            <div className="space-y-1.5 sm:col-span-2">
              <Label htmlFor="mt-prompt">测试提示词</Label>
              <Input
                id="mt-prompt"
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                placeholder={DEFAULT_PROMPT}
              />
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="mt-max-tokens">输出上限（max_tokens）</Label>
              <Input
                id="mt-max-tokens"
                type="number"
                min={1}
                max={8192}
                value={maxTokens}
                onChange={(e) => setMaxTokens(e.target.value)}
              />
            </div>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <Button onClick={onTestOne} disabled={testing || !gatewayId || !effectiveModel}>
              {testing ? <Loader2 className="size-4 animate-spin" /> : <Play className="size-4" />}
              测试该模型
            </Button>
            <Button variant="outline" onClick={onTestAll} disabled={testing || !models.length}>
              测试该网关全部模型（{models.length}）
            </Button>
            {testing ? (
              <Button variant="ghost" onClick={onStop}>
                <Square className="size-4" />
                停止
              </Button>
            ) : null}
            {results.length ? (
              <Button variant="ghost" onClick={() => setResults([])} className="ml-auto">
                <Trash2 className="size-4" />
                清空结果
              </Button>
            ) : null}
          </div>

          {testing && pendingModel ? (
            <p className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" />
              正在测试 {pendingModel}…
            </p>
          ) : null}
        </CardContent>
      </Card>

      {results.length ? (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">
              测试结果
              <span className="ml-3 text-xs font-normal text-muted-foreground">
                共 {stats.done} 次 · 成功 {stats.ok} · 失败 {stats.fail} · 平均 {formatLatency(stats.avg)}
              </span>
            </CardTitle>
          </CardHeader>
          <CardContent className="space-y-2">
            {results.map((r) => (
              <ResultCard key={`${r.at}-${r.gateway}-${r.model}`} result={r} />
            ))}
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="p-5 text-sm text-muted-foreground">
            还没有测试记录。选好网关与模型后点「测试该模型」，结果会按时间倒序累积在这里，
            可对同一模型反复测试，也可用「测试该网关全部模型」一次性核对整条链路。
          </CardContent>
        </Card>
      )}
    </div>
  );
}
