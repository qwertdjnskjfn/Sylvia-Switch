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
  Loader2,
  RefreshCw,
  Route,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { CopyButton } from "@/components/ui/copy-button";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import type { WorkbuddyProxyStatus } from "@/lib/api";
import { cn } from "@/lib/utils";

const WB_HERMES_SNIPPET = `model:
  default: "<内置模型>"
  provider: "custom"
  base_url: "http://127.0.0.1:57890/workbuddy/v1"
  api_key: "<访问密钥>"`;

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
    </div>
  );
}
