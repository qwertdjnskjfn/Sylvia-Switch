/**
 * WorkBuddy 网关（独立网关 · 2026-09-22 从原「TRAE / WorkBuddy 模型」合并页拆出）。
 *
 * 职责边界：只管 WorkBuddy —— 桌面端模型代理、通道状态、上游可达性与保活、接入信息。
 * **不涉及 TRAE 账号池**（那是 TRAE Work CN 网关的事）；
 * 借道 OpenCode 的 provider 目录归属见「OpenCode 网关」页。
 *
 * 能力落点：
 *   网关状态     → 通道态（direct / opencode-fallback）+ OpenCode server 健康与端口
 *   账号保护/保活 → 保活对象是 OpenCode server 进程：看门狗拉起 + 端口探测 + 恢复中瞬态
 *   扣分账号     → 负缓存/连续失败计数（is_neg_cache_error / last_discovery_ok_ms）
 *   接入信息     → Base URL / 密钥 / 内置模型 / Hermes 配置样例
 *
 * 🔴 2026-09-23（用户要求「只保留 WorkBuddy 原本内置的模型」）：
 *    本页**不再展示**借道 OpenCode server 展开的上游 provider 全量清单（原 591 条 / 14 渠道）——
 *    按设计边界，provider 清单归「OpenCode 网关」页呈现（见 docs/gateway-split-20260922.md：
 *    WorkBuddy「不拥有 provider 清单」）。WorkBuddy 侧只保留**内置模型**
 *    （桌面端内嵌会话自带，走 WorkBuddy 订阅 · 经 ACP 通道转发）。
 *    对应后端：`/workbuddy/v1/models` 与总览 modelCount 均走 `channel_pick_models_payload()`
 *    （每渠道各 1 个代表）；`/workbuddy-native/v1/models` 走 `builtin_models_payload()`（严格内置）。
 *    ⚠️ `/api/workbuddy-proxy/models` **保持不变**（它是 OpenCode 页的数据源），
 *       两个 `workbuddy_proxy::models_payload()/models_catalog()` 也**不许动**（opencode.rs 在复用）。
 *
 * ⚠️ 通道现状（用户 2026-09-22 第 2 项要求）：**以直连优先 + OpenCode 兜底**。
 *    直连协议尚在探测中，故当前通道态标为 `opencode-fallback`；探测通过后置为 `direct`，
 *    回落链路由后端负责，前端仅如实呈现 `channel` 字段。
 */

import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import {
  BarChart3,
  CircleAlert,
  Cloud,
  Loader2,
  RefreshCw,
  Route,
  Settings,
  Users,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { CopyButton } from "@/components/ui/copy-button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import type { WorkbuddyProxyStatus } from "@/lib/api";
import type { WorkbuddyGatewayConfig, WorkbuddyPoolStatus } from "@/lib/types";
import { cn } from "@/lib/utils";

const WB_HERMES_SNIPPET = `model:
  default: "<内置模型>"
  provider: "custom"
  base_url: "http://127.0.0.1:57890/workbuddy/v1"
  api_key: "<访问密钥>"`;

/** 多账号网关（cloud）Base URL：契约 §12 独立路由组 /workbuddy-pool/v1。 */
const WB_POOL_BASE_URL = "http://127.0.0.1:57890/workbuddy-pool/v1";

/** ms epoch → 简短日期时间；0/缺省 = 未知。 */
function fmtPoolTs(ms: number | null | undefined): string {
  if (!ms || ms <= 0) return "—";
  return new Date(ms).toLocaleString("zh-CN", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** 冷却剩余秒数 → 人话；<=0 返回空串。 */
function fmtPoolRemain(sec: number | null | undefined): string {
  if (sec == null || sec <= 0) return "";
  if (sec >= 3600) return `${Math.floor(sec / 3600)} 小时 ${Math.floor((sec % 3600) / 60)} 分`;
  if (sec >= 60) return `${Math.floor(sec / 60)} 分 ${sec % 60} 秒`;
  return `${sec} 秒`;
}

/** 账号级冷却类型 → 中文标签（后端 CoolKind 仅 soft_rate / hard_credit 两值，
 *  见 workbuddy_gateway.rs:474-475；熔断是独立的 breaker_until，不走 cooldown_kind）。 */
function poolCoolKindLabel(kind: string): string {
  switch (kind) {
    case "soft_rate":
      return "软限流";
    case "hard_credit":
      return "余额耗尽";
    default:
      return kind || "冷却";
  }
}

/** 配置面板数字字段：失焦且值有变化才 PATCH（对齐 GrokPage 策略面板模式）。 */
function PoolNumberField({
  label,
  value,
  onCommit,
  disabled,
}: {
  label: string;
  value: number;
  onCommit: (v: number) => void;
  disabled?: boolean;
}) {
  return (
    <div>
      <label className="text-xs text-muted-foreground">{label}</label>
      <Input
        type="number"
        min={0}
        defaultValue={value}
        disabled={disabled}
        onBlur={(e) => {
          // 清空失焦不提交（避免误写 0；后端对 <=0 按「走默认」处理）
          if (e.target.value.trim() === "") return;
          const v = Number(e.target.value);
          if (Number.isFinite(v) && v !== value) onCommit(v);
        }}
        className="mt-1 h-8 text-xs"
      />
    </div>
  );
}

function InfoRow({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="mx-4 flex min-w-0 flex-wrap items-center justify-between gap-2 border-b border-border/50 py-3 sm:mx-5">
      <div className="min-w-0">
        <p className="text-sm font-medium leading-5">{label}</p>
        {hint ? <p className="mt-0.5 text-xs text-muted-foreground">{hint}</p> : null}
      </div>
      {children}
    </div>
  );
}

export default function WorkbuddyPage() {
  const [wbStatus, setWbStatus] = useState<WorkbuddyProxyStatus | null>(null);
  const [wbCfg, setWbCfg] = useState<api.WorkbuddyProxyConfigInfo | null>(null);
  const [nativeModels, setNativeModels] = useState<{ id: string; owned_by: string }[]>([]);
  const [savingKey, setSavingKey] = useState(false);
  const [savingEnabled, setSavingEnabled] = useState(false);
  const [wbMsg, setWbMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [loading, setLoading] = useState(true);

  // ---- 多账号网关（cloud 账号池）----
  const [poolStatus, setPoolStatus] = useState<WorkbuddyPoolStatus | null>(null);
  const [poolCfg, setPoolCfg] = useState<WorkbuddyGatewayConfig | null>(null);
  const [poolLoading, setPoolLoading] = useState(false);
  const [poolBusy, setPoolBusy] = useState(false);
  const [poolMsg, setPoolMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  /** 池状态为只读 GET；随挂载加载一次，不触发任何 chat 请求
   *  （chat 只在用户显式操作时发生，本页不提供自动调用入口）。
   *  后端响应是 { ok, pool, config } 信封（api.rs:5208-5215），池数据在 pool 下。 */
  const loadPool = useCallback(async (showError = false) => {
    setPoolLoading(true);
    try {
      const res = await api.getWorkbuddyPoolStatus();
      if (res?.pool) setPoolStatus(res.pool);
      if (res.config) setPoolCfg(res.config);
    } catch (e) {
      // 首屏自动加载容错（与 GrokPage 账号池一致）；手动刷新才把错误透出
      if (showError) setPoolMsg({ type: "err", text: api.asError(e) });
    } finally {
      setPoolLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadPool();
  }, [loadPool]);

  /** PATCH 配置（热更新即写即生效），保存后回拉状态刷新面板。 */
  const patchPoolCfg = useCallback(
    async (patch: Partial<WorkbuddyGatewayConfig>) => {
      setPoolBusy(true);
      try {
        await api.patchWorkbuddyPoolConfig(patch);
        await loadPool();
        setPoolMsg({ type: "ok", text: "配置已保存，即时生效" });
      } catch (e) {
        setPoolMsg({ type: "err", text: api.asError(e) });
      } finally {
        setPoolBusy(false);
      }
    },
    [loadPool],
  );

  const load = useCallback(async () => {
    setLoading(true);
    // 只取与本页职责相关的三项：通道状态 / 接入配置 / 内置模型。
    // （上游 provider 全量清单归 OpenCode 页，本页不再拉取。）
    const [statusRes, cfgRes, nativeRes] = await Promise.allSettled([
      api.getWorkbuddyProxyStatus(),
      api.getWorkbuddyProxyConfig(),
      api.getWorkbuddyNativeModels(),
    ]);
    if (statusRes.status === "fulfilled") setWbStatus(statusRes.value);
    if (cfgRes.status === "fulfilled") setWbCfg(cfgRes.value);
    if (nativeRes.status === "fulfilled") setNativeModels(nativeRes.value.data ?? []);
    setLoading(false);
  }, []);

  useEffect(() => {
    void load();
    const timer = window.setTimeout(() => void load(), 20000);
    return () => window.clearTimeout(timer);
  }, [load]);

  const regenerateWbKey = useCallback(async () => {
    setSavingKey(true);
    try {
      const next = await api.saveWorkbuddyProxyConfig({ regenerateKey: true });
      setWbCfg(next);
      setWbMsg({ type: "ok", text: "已重新生成 WorkBuddy 访问密钥" });
    } catch (e) {
      setWbMsg({ type: "err", text: api.asError(e) });
    }
    setSavingKey(false);
  }, []);

  const toggleWbEnabled = useCallback(async (enabled: boolean) => {
    setSavingEnabled(true);
    try {
      const next = await api.saveWorkbuddyProxyConfig({ enabled });
      setWbCfg(next);
      setWbMsg({ type: "ok", text: enabled ? "已启用 WorkBuddy 代理" : "已停用 WorkBuddy 代理" });
    } catch (e) {
      setWbMsg({ type: "err", text: api.asError(e) });
    }
    setSavingEnabled(false);
  }, []);

  /** 内置模型 ID：后端目录与 ACP 通道同源；桌面端未运行时回落同一默认值。 */
  const builtinModelId = nativeModels[0]?.id ?? "deepseek-v4.1-flash";

  // 通道态由后端下发（direct = 直连；缺省视为 opencode 兜底）。
  // 后端未定义该字段时做安全读取，避免类型耦合。
  const channel = (wbStatus as { channel?: string } | null)?.channel ?? "opencode-fallback";

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-8 flex min-w-0 flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">WorkBuddy 网关</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            独立网关：WorkBuddy 桌面端模型代理。与 OpenCode 网关同级、互不共享账号池与密钥。
            通道策略为 <strong>直连优先 + OpenCode 兜底</strong>（直连协议探测中，当前实际走兜底）。
          </p>
        </div>
        <div className="flex shrink-0 flex-wrap items-center gap-2">
          <Button className="shrink-0" variant="outline" size="sm" asChild>
            <Link to="/stats?tab=model">
              <BarChart3 />
              模型用量
            </Link>
          </Button>
          <Button className="shrink-0" variant="outline" size="sm" onClick={() => void load()} disabled={loading}>
            {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
            刷新
          </Button>
        </div>
      </header>

      {/* ① 通道状态 + 网关状态 */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="WorkBuddy 通道状态">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">
            <Route className="mr-1 inline size-3.5" />
            通道状态
          </h2>
          <div className="flex shrink-0 flex-wrap items-center gap-3">
            {channel === "direct" ? (
              <Badge variant="success" className="shrink-0">直连</Badge>
            ) : (
              <Badge variant="outline" className="shrink-0">OpenCode 兜底（直连待接）</Badge>
            )}
            <span className="text-xs text-muted-foreground">
              OpenCode server：
              {wbStatus?.server.ok ? (
                <span className="text-foreground">已连接（端口 {wbStatus.server.port}）</span>
              ) : (
                <span className="text-destructive">{wbStatus?.server.error || "未发现"}</span>
              )}
            </span>
            {wbCfg ? (
              <div className="flex items-center gap-2">
                <span className="text-xs text-muted-foreground">{wbCfg.enabled ? "已启用" : "已停用"}</span>
                <Switch
                  checked={wbCfg.enabled}
                  disabled={savingEnabled}
                  onCheckedChange={(v) => void toggleWbEnabled(v)}
                  aria-label="启用 WorkBuddy 模型代理"
                />
              </div>
            ) : null}
          </div>
        </div>
        {wbMsg ? (
          <p className={cn("px-1 text-xs", wbMsg.type === "ok" ? "text-primary" : "text-destructive")}>
            {wbMsg.text}
          </p>
        ) : null}
        {wbCfg && !wbCfg.enabled ? (
          <Alert variant="warning">
            <CircleAlert />
            <AlertTitle>WorkBuddy 代理已停用</AlertTitle>
            <AlertDescription>
              开关已关：<code className="mx-1 rounded bg-muted px-1">/workbuddy/v1/*</code>
              全部请求（含对话与模型列表）会直接返回 503，不再转发上游。重新打开即可恢复。
            </AlertDescription>
          </Alert>
        ) : null}
        <p className="px-1 text-[11px] leading-5 text-muted-foreground">
          WorkBuddy 侧目录为<strong>每个渠道各保留 1 个代表模型</strong>
          （免费 · 快速档优先，渠道数与上游一致；全量清单按设计归「
          <Link to="/opencode" className="underline underline-offset-2 hover:text-foreground">
            OpenCode
          </Link>
          」页）。真正走订阅的<strong>内置模型</strong>
          <code className="mx-1 rounded bg-muted px-1">{builtinModelId}</code>
          在下方「接入信息 · WorkBuddy 内置模型」单独列出。
          兜底链路仍为本机 OpenCode server（独立实例，端口自动发现，未运行时由看门狗经
          start-serve-4097.bat 拉起）——<strong>保活对象即该 server 进程</strong>；
          直连链路上线后优先走直连，失败自动回落。
        </p>
      </section>

      {/* ② 速度优先 / ③ 模型目录（provider × model）已于 2026-09-23 移除：
          用户嫌「591 条 / 14 渠道」整表太多，要求精简为每渠道 1 个代表（约 14 条）。
          那两块的数据源是借道 OpenCode server
          展开的上游 provider 全量清单（原 591 条 / 14 渠道），按设计边界归 OpenCode 网关页呈现。
          对应后端：/workbuddy/v1/models 与总览 modelCount 走 channel_pick_models_payload()
          （每渠道各 1 个代表，约 14 条），不再整表列出。
          ⚠️ /api/workbuddy-proxy/models 与 workbuddy_proxy::{models_payload,models_catalog}
             保持不变 —— 它们是 OpenCode 页/网关的数据源，改会一起打断。 */}

      {/* ④ 接入信息 · WorkBuddy 内置模型（桌面端会话自带，走 WorkBuddy 订阅） */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="WorkBuddy 内置模型接入信息">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">接入信息 · WorkBuddy 内置模型</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">
            WorkBuddy 桌面端内嵌会话自带的模型（走 WorkBuddy 订阅），经 ACP 通道转发——与「OpenCode 兜底」是两套模型体系
          </p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            <InfoRow label="接口地址（Base URL）" hint="OpenAI 兼容端点；ACP 真流式（逐块增量下发）">
              <div className="flex items-center gap-1.5">
                <code className="max-w-[260px] truncate rounded bg-muted/70 px-2 py-1 text-xs">
                  http://127.0.0.1:57890/workbuddy-native/v1
                </code>
                <CopyButton text="http://127.0.0.1:57890/workbuddy-native/v1" label="接口地址" />
              </div>
            </InfoRow>
            <InfoRow label="内置模型" hint="实时探测桌面端内嵌会话的 --model；桌面端未运行时为空">
              <span className="font-mono text-xs">
                {nativeModels.length > 0
                  ? nativeModels.map((m) => m.id).join("、")
                  : "未探测到（请先在 WorkBuddy 打开一个 AI 会话）"}
              </span>
            </InfoRow>
            <InfoRow label="访问密钥" hint="仅回环监听，无需密钥；fullAccess 门禁默认开启（本机白名单目录内的会话）">
              <span className="text-xs text-muted-foreground">不需要</span>
            </InfoRow>
            <div className="mx-4 flex items-start justify-between gap-3 py-3.5 sm:mx-5">
              <p className="mb-0 text-xs text-muted-foreground">调用示例（curl）：</p>
              <pre className="min-w-0 overflow-x-auto rounded-lg bg-muted/60 p-3 text-xs leading-5">
                {`curl -X POST http://127.0.0.1:57890/workbuddy-native/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -d '{"model":"${builtinModelId}","messages":[{"role":"user","content":"你好"}]}'`}
              </pre>
              <CopyButton
                text={`curl -X POST http://127.0.0.1:57890/workbuddy-native/v1/chat/completions -H "Content-Type: application/json" -d '{"model":"${builtinModelId}","messages":[{"role":"user","content":"你好"}]}'`}
                label="调用示例"
              />
            </div>
          </CardContent>
        </Card>
      </section>

      {/* ⑤ 接入信息 · WorkBuddy 代理 */}
      <section className="min-w-0 space-y-2.5" aria-label="WorkBuddy 接入信息">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">接入信息 · WorkBuddy</h2>
          <p className="mt-0.5 text-xs text-muted-foreground">在任意 OpenAI 兼容客户端中填入以下信息</p>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            <InfoRow label="接口地址（Base URL）" hint="OpenAI 兼容端点；流式为合成 SSE（整段完成后一次下发）">
              <div className="flex items-center gap-1.5">
                <code className="max-w-[240px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={wbCfg?.endpoint}>
                  {wbCfg?.endpoint ?? "加载中…"}
                </code>
                {wbCfg ? <CopyButton text={wbCfg.endpoint} label="接口地址" /> : null}
              </div>
            </InfoRow>
            <InfoRow label="访问密钥" hint="请求头 Authorization: Bearer <密钥>（或 x-api-key）">
              <div className="flex items-center gap-1.5">
                <code className="max-w-[200px] truncate rounded bg-muted/70 px-2 py-1 text-xs" title={wbCfg?.apiKey}>
                  {wbCfg?.apiKey ?? "加载中…"}
                </code>
                {wbCfg ? <CopyButton text={wbCfg.apiKey} label="访问密钥" /> : null}
                <Button size="sm" variant="outline" onClick={() => void regenerateWbKey()} disabled={savingKey || !wbCfg}>
                  {savingKey ? <Loader2 className="animate-spin" /> : <RefreshCw />}重新生成
                </Button>
              </div>
            </InfoRow>
            <InfoRow
              label="可用模型"
              hint="每渠道各 1 个代表（免费·快速优先）；provider/model 形态仍可调用其它模型（走 OpenCode 兜底链路）"
            >
              <span className="font-mono text-xs">{builtinModelId}</span>
            </InfoRow>
            <div className="mx-4 flex items-start justify-between gap-3 py-3.5 sm:mx-5">
              <p className="mb-0 text-xs text-muted-foreground">Hermes（cli-config.yaml 模型段）：</p>
              <pre className="min-w-0 overflow-x-auto rounded-lg bg-muted/60 p-3 text-xs leading-5">
                {WB_HERMES_SNIPPET.replace("<内置模型>", builtinModelId).replace(
                  "<访问密钥>",
                  wbCfg?.apiKey ?? "<访问密钥>",
                )}
              </pre>
              {wbCfg ? (
                <CopyButton
                  text={WB_HERMES_SNIPPET.replace("<内置模型>", builtinModelId).replace(
                    "<访问密钥>",
                    wbCfg.apiKey,
                  )}
                  label="配置示例"
                />
              ) : null}
            </div>
          </CardContent>
        </Card>
      </section>

      {/* ⑥ 多账号网关（cloud）：账号池总览 / 账号表 / 配置面板 / 接入信息。
          存量 /workbuddy/v1 与 /workbuddy-native/v1 区块保持原样，本区块走独立路由组
          /workbuddy-pool/v1（契约 §12）；样式对齐 GrokPage 账号池卡片。
          数据形状以后端为准：status 为 { ok, pool, config } 信封（api.rs:5208-5215），
          pool/accounts 字段 snake_case（workbuddy_gateway.rs:1504-1529），config 为
          camelCase（api.rs:4599-4613）；改字段前先核对这两处。 */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="WorkBuddy 多账号网关">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">
            <Cloud className="mr-1 inline size-3.5" />
            多账号网关（cloud）
          </h2>
          <div className="flex shrink-0 flex-wrap items-center gap-3">
            {poolMsg ? (
              <span className={cn("text-xs", poolMsg.type === "ok" ? "text-primary" : "text-destructive")}>
                {poolMsg.text}
              </span>
            ) : null}
            <Button
              className="shrink-0"
              variant="outline"
              size="sm"
              onClick={() => void loadPool(true)}
              disabled={poolLoading}
            >
              {poolLoading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
              刷新
            </Button>
          </div>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-0">
            {/* 池总览：五个计数（total/healthy/cooling/model_cooling/breaker） */}
            <div className="grid grid-cols-2 gap-2 p-4 sm:grid-cols-5 sm:p-5">
              <div className="rounded-lg border p-2.5">
                <div className="text-xs text-muted-foreground">账号总数</div>
                <div className="mt-0.5 text-lg font-semibold tabular-nums">{poolStatus?.total ?? "—"}</div>
              </div>
              <div className="rounded-lg border p-2.5">
                <div className="text-xs text-muted-foreground">可用</div>
                <div className="mt-0.5 text-lg font-semibold tabular-nums text-emerald-600">
                  {poolStatus?.healthy ?? "—"}
                </div>
              </div>
              <div className="rounded-lg border p-2.5">
                <div className="text-xs text-muted-foreground">账号冷却</div>
                <div className="mt-0.5 text-lg font-semibold tabular-nums text-amber-600">
                  {poolStatus?.cooling ?? "—"}
                </div>
              </div>
              <div className="rounded-lg border p-2.5">
                <div className="text-xs text-muted-foreground">模型冷却</div>
                <div className="mt-0.5 text-lg font-semibold tabular-nums text-amber-600">
                  {poolStatus?.model_cooling ?? "—"}
                </div>
              </div>
              <div className="rounded-lg border p-2.5">
                <div className="text-xs text-muted-foreground">熔断</div>
                <div className="mt-0.5 text-lg font-semibold tabular-nums text-rose-500">
                  {poolStatus?.breaker ?? "—"}
                </div>
              </div>
            </div>

            {/* 容量告警：有账号不可用（低于容量）或全冷却时出 amber 条 */}
            {poolStatus && poolStatus.total > 0 && poolStatus.healthy < poolStatus.total ? (
              <div className="px-4 pb-3 sm:px-5">
                <Alert variant="warning" className="py-2.5">
                  <CircleAlert className="size-4" />
                  <AlertTitle className="text-xs font-medium">
                    {poolStatus.healthy === 0 ? "账号池全部不可用（全冷却）" : "账号池低于容量"}
                  </AlertTitle>
                  <AlertDescription className="text-xs">
                    {poolStatus.healthy === 0
                      ? `${poolStatus.total} 个账号全部处于冷却 / 熔断 / 出池状态，/workbuddy-pool/v1 的 chat 请求会直接失败；可等待冷却恢复，或在账号库补充 WorkBuddy 账号。`
                      : `${poolStatus.total} 个账号中 ${poolStatus.total - poolStatus.healthy} 个暂不可用（冷却 / 熔断 / 出池），当前可用 ${poolStatus.healthy} 个；高峰期可能换号失败，建议补充账号。`}
                  </AlertDescription>
                </Alert>
              </div>
            ) : null}

            {/* 账号表：昵称 / 区域 / 积分 / 到期 / 冷却状态 / 模型冷却明细 */}
            <div className="px-4 pb-4 sm:px-5">
              {poolLoading && !poolStatus ? (
                <p className="py-6 text-center text-xs text-muted-foreground">加载中…</p>
              ) : (poolStatus?.accounts.length ?? 0) === 0 ? (
                <div className="flex flex-col items-center justify-center rounded-xl border border-dashed py-8 text-center">
                  <Users className="size-8 text-muted-foreground/50" />
                  <p className="mt-2 text-sm font-medium">账号池暂无账号</p>
                  <p className="mt-1 text-xs text-muted-foreground">
                    账号库（~/.wb-switch/accounts.json）里的 WorkBuddy OAuth 账号会自动进入池子，请先到「账号」页添加
                  </p>
                </div>
              ) : (
                <div className="overflow-hidden rounded-xl border">
                  <table className="w-full text-xs">
                    <thead className="bg-muted/40 text-muted-foreground">
                      <tr>
                        <th className="px-3 py-2 text-left font-medium">账号</th>
                        <th className="px-3 py-2 text-left font-medium">区域</th>
                        <th className="px-3 py-2 text-left font-medium">积分</th>
                        <th className="px-3 py-2 text-left font-medium">到期</th>
                        <th className="px-3 py-2 text-left font-medium">冷却状态</th>
                        <th className="px-3 py-2 text-left font-medium">模型冷却明细</th>
                      </tr>
                    </thead>
                    <tbody className="divide-y">
                      {poolStatus?.accounts.map((a) => {
                        // 后端只给绝对截止时刻（workbuddy_gateway.rs:1504-1520）：
                        // 冷却/熔断状态由 until / breaker_until / disabled 现场推导，
                        // 剩余秒数按渲染时刻现算（快照数据，误差在刷新周期内可接受）。
                        const nowMs = Date.now();
                        const cools = a.model_cools ?? [];
                        const coolingActive = a.until > nowMs;
                        const breakerActive = a.breaker_until > nowMs;
                        const coolRemainSec = coolingActive
                          ? Math.max(0, Math.round((a.until - nowMs) / 1000))
                          : 0;
                        return (
                          <tr key={a.uid} className="hover:bg-muted/30">
                            <td className="px-3 py-2 font-medium">
                              <div className="flex flex-col">
                                <span>{a.nickname || a.uid}</span>
                                <span className="font-mono text-[10px] text-muted-foreground">{a.uid}</span>
                              </div>
                            </td>
                            <td className="px-3 py-2">
                              <Badge variant="outline" className="h-5 text-[10px]">
                                {a.region === "cn" ? "国服" : a.region === "intl" ? "国际" : a.region}
                              </Badge>
                            </td>
                            <td className="px-3 py-2 tabular-nums">{a.credits > 0 ? a.credits : "—"}</td>
                            <td className="px-3 py-2 text-muted-foreground">{fmtPoolTs(a.credit_expires_at)}</td>
                            <td className="px-3 py-2">
                              {a.disabled ? (
                                <Badge
                                  variant="outline"
                                  className="h-5 border-rose-500/30 bg-rose-500/10 text-[10px] text-rose-600"
                                >
                                  出池 · 需重登
                                </Badge>
                              ) : breakerActive ? (
                                <Badge
                                  variant="outline"
                                  className="h-5 border-rose-500/30 bg-rose-500/10 text-[10px] text-rose-600"
                                >
                                  熔断中
                                </Badge>
                              ) : coolingActive ? (
                                <Badge
                                  variant="outline"
                                  className="h-5 border-amber-500/30 bg-amber-500/10 text-[10px] text-amber-600"
                                >
                                  {poolCoolKindLabel(a.cooldown_kind)}
                                  {coolRemainSec > 0 ? ` · ${fmtPoolRemain(coolRemainSec)}` : ""}
                                </Badge>
                              ) : (
                                <Badge
                                  variant="outline"
                                  className="h-5 border-emerald-500/30 bg-emerald-500/10 text-[10px] text-emerald-600"
                                >
                                  可用
                                </Badge>
                              )}
                            </td>
                            <td className="px-3 py-2">
                              {cools.length === 0 ? (
                                <span className="text-muted-foreground">—</span>
                              ) : (
                                <div className="space-y-0.5">
                                  {cools.slice(0, 3).map((mc) => (
                                    <div key={mc.model} className="font-mono text-[11px]" title={mc.reason}>
                                      {mc.model}
                                      <span className="ml-1 font-sans text-muted-foreground">
                                        剩 {fmtPoolRemain(mc.remaining_sec)}
                                      </span>
                                    </div>
                                  ))}
                                  {cools.length > 3 ? (
                                    <div className="text-[11px] text-muted-foreground">等 {cools.length - 3} 个模型…</div>
                                  ) : null}
                                </div>
                              )}
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>
              )}
            </div>

            {/* 配置面板：onBlur/onChange 走 PATCH 热更新 */}
            {poolCfg ? (
              <div className="border-t px-4 py-4 sm:px-5">
                <div className="mb-3 flex items-center gap-2">
                  <Settings className="size-3.5 text-muted-foreground" />
                  <span className="text-xs font-medium">网关配置</span>
                  {poolBusy ? <Loader2 className="size-3.5 animate-spin text-muted-foreground" /> : null}
                </div>
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
                  <div>
                    <label className="text-xs text-muted-foreground">选号模式</label>
                    <select
                      value={poolCfg.mode}
                      disabled={poolBusy}
                      onChange={(e) =>
                        void patchPoolCfg({ mode: e.target.value as WorkbuddyGatewayConfig["mode"] })
                      }
                      className="mt-1 block w-full rounded-md border border-input bg-background px-3 py-1.5 text-xs shadow-sm focus:border-primary focus:outline-none focus:ring-1 focus:ring-primary"
                    >
                      <option value="balance">均衡（到期分层 + 档内加权）</option>
                      <option value="rotate">烧号（轮询）</option>
                      <option value="pinned">指定账号</option>
                    </select>
                  </div>
                  <div>
                    <label className="text-xs text-muted-foreground">
                      固定账号 uid（pinned 模式生效；需完整 uid——状态表中的 uid 已脱敏，完整值见账号库 accounts.json）
                    </label>
                    <Input
                      defaultValue={poolCfg.pinnedUid}
                      disabled={poolBusy}
                      placeholder="账号 uid"
                      onBlur={(e) => {
                        const v = e.target.value.trim();
                        if (v !== poolCfg.pinnedUid) void patchPoolCfg({ pinnedUid: v });
                      }}
                      className="mt-1 h-8 text-xs"
                    />
                  </div>
                  <PoolNumberField
                    label="单账号在途上限"
                    value={poolCfg.maxInFlight}
                    disabled={poolBusy}
                    onCommit={(v) => void patchPoolCfg({ maxInFlight: v })}
                  />
                  <PoolNumberField
                    label="软限流冷却 · 基础（秒）"
                    value={poolCfg.softRateBaseSecs}
                    disabled={poolBusy}
                    onCommit={(v) => void patchPoolCfg({ softRateBaseSecs: v })}
                  />
                  <PoolNumberField
                    label="软限流冷却 · 封顶（秒）"
                    value={poolCfg.softRateMaxSecs}
                    disabled={poolBusy}
                    onCommit={(v) => void patchPoolCfg({ softRateMaxSecs: v })}
                  />
                  <PoolNumberField
                    label="熔断阈值（连续失败次数）"
                    value={poolCfg.breakerThreshold}
                    disabled={poolBusy}
                    onCommit={(v) => void patchPoolCfg({ breakerThreshold: v })}
                  />
                  <PoolNumberField
                    label="熔断冷却 · 基础（秒）"
                    value={poolCfg.breakerBaseSecs}
                    disabled={poolBusy}
                    onCommit={(v) => void patchPoolCfg({ breakerBaseSecs: v })}
                  />
                  <PoolNumberField
                    label="熔断冷却 · 封顶（秒）"
                    value={poolCfg.breakerMaxSecs}
                    disabled={poolBusy}
                    onCommit={(v) => void patchPoolCfg({ breakerMaxSecs: v })}
                  />
                  <PoolNumberField
                    label="会话粘性 TTL（毫秒）"
                    value={poolCfg.stickyTtlMs}
                    disabled={poolBusy}
                    onCommit={(v) => void patchPoolCfg({ stickyTtlMs: v })}
                  />
                  <PoolNumberField
                    label="余额巡检间隔（分钟）"
                    value={poolCfg.balanceRefreshMinutes}
                    disabled={poolBusy}
                    onCommit={(v) => void patchPoolCfg({ balanceRefreshMinutes: v })}
                  />
                  <label className="flex items-center gap-2 self-end pb-1.5 text-xs">
                    <input
                      type="checkbox"
                      checked={poolCfg.sanitizeEnabled}
                      disabled={poolBusy}
                      onChange={(e) => void patchPoolCfg({ sanitizeEnabled: e.target.checked })}
                      className="rounded border-input text-primary focus:ring-primary"
                    />
                    出站脱敏（请求体指纹字段清洗）
                  </label>
                </div>
                <p className="mt-3 text-[11px] leading-5 text-muted-foreground">
                  配置为 PATCH 热更新（即写即生效）。模型级冷却（上游 6004）只冷对应模型、不冷账号，也不喂熔断器；
                  账号库 accounts.json 保持唯一真源，网关只读，冷却 / 熔断落在网关自有状态文件。
                </p>
              </div>
            ) : null}

            {/* 接入信息：Base URL / 访问密钥（与页面既有接入信息同款 InfoRow） */}
            <InfoRow label="接口地址（Base URL）" hint="OpenAI 兼容端点；chat/completions 与 models 均在此路由组下">
              <div className="flex items-center gap-1.5">
                <code className="max-w-[260px] truncate rounded bg-muted/70 px-2 py-1 text-xs">
                  {WB_POOL_BASE_URL}
                </code>
                <CopyButton text={WB_POOL_BASE_URL} label="接口地址" />
              </div>
            </InfoRow>
            <InfoRow
              label="访问密钥"
              hint="请求头 Authorization: Bearer <密钥>；池网关与 grok / kimi / qoder 池网关共用同一套网关 Key，无独立密钥字段"
            >
              <span className="text-xs text-muted-foreground">
                网关 Key（<code className="mx-1 rounded bg-muted px-1">gw_live_…</code>开头）统一在「网关管理」页创建 /
                复制；本页不持有、不展示完整密钥
              </span>
            </InfoRow>
          </CardContent>
        </Card>
      </section>
    </div>
  );
}
