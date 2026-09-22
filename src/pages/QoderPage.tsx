import { useCallback, useEffect, useState } from "react";
import {
  Check,
  CircleAlert,
  CircleCheck,
  ExternalLink,
  Globe,
  Loader2,
  Monitor,
  RefreshCw,
  Trash2,
  UserPlus,
  Zap,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import { GatewayAdminSection } from "@/components/gateway/GatewayAdminSection";
import type { QoderAccountInfo } from "@/lib/types";

interface DeviceFlowSession {
  authorizationUrl: string;
  nonce: string;
  verifier: string;
  startedAt: number;
}

const SOURCE_LABELS: Record<string, string> = {
  "local-live": "本机登录",
  "local-import": "本机快照",
  "qodercn-local-live": "QoderCN 本机登录",
  "qodercn-local": "QoderCN 本机快照",
  "device-flow": "设备授权",
  manual: "手动导入",
};

function formatQuota(quota: number | null | undefined): string {
  if (quota === null || quota === undefined) return "—";
  return String(quota);
}

export default function QoderPage() {
  const [accounts, setAccounts] = useState<QoderAccountInfo[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [deviceSession, setDeviceSession] = useState<DeviceFlowSession | null>(null);
  const [deviceWaiting, setDeviceWaiting] = useState(false);
  const [testModel, setTestModel] = useState("lite");
  const [testPrompt, setTestPrompt] = useState("用一句话介绍你自己");
  const [testResult, setTestResult] = useState<{ ok: boolean; text: string } | null>(null);
  const [testing, setTesting] = useState(false);
  const [qwenStatus, setQwenStatus] = useState<api.QwenDesktopStatus | null>(null);

  const loadAccounts = useCallback(async () => {
    const res = await api.getQoderAccounts();
    setAccounts(res.accounts ?? []);
  }, []);

  const loadQwenStatus = useCallback(async () => {
    try {
      const res = await api.getQwenDesktopStatus();
      setQwenStatus(res);
    } catch {
      // ignore
    }
  }, []);

  useEffect(() => {
    void loadAccounts().catch(() => undefined);
    void loadQwenStatus().catch(() => undefined);
  }, [loadAccounts, loadQwenStatus]);

  async function importLocal() {
    setBusy("import");
    setMsg(null);
    try {
      await api.qoderImportLocal();
      await loadAccounts();
      setMsg({ type: "ok", text: "已导入客户端当前登录的 Qoder 账号" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  /** 导入本机 QoderCN（中国版 IDE）登录态 → 🇨🇳 国内专用池（/qwen-flash/v1 凭据来源）。 */
  async function importLocalCn() {
    setBusy("import-cn");
    setMsg(null);
    try {
      const res = await api.qoderImportLocalCn();
      await loadAccounts();
      setMsg({
        type: "ok",
        text: `已导入 QoderCN 账号 ${res.account.name || res.account.email || ""}（🇨🇳 国内专用池，服务 /qwen-flash/v1）`,
      });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  /** 导入本机 QoderWork CN（独立 Electron 应用）登录态 → 独立池（/qwen-flash/v1 凭据来源）。 */
  async function importLocalWorkCn() {
    setBusy("import-work-cn");
    setMsg(null);
    try {
      const res = await api.qoderImportLocalWorkCn();
      await loadAccounts();
      setMsg({
        type: "ok",
        text: `已导入 QoderWork CN 账号 ${res.account.name || res.account.email || ""}（独立算力池，端点 gateway.qoder.com.cn）`,
      });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  /** 手动指定账号归属区域（防误判）；auto = 清除手动指定回到自动推断。 */
  async function changeRegion(uid: string, region: "cn" | "intl" | "auto") {
    setBusy(uid);
    setMsg(null);
    try {
      await api.qoderSetAccountRegion(uid, region);
      await loadAccounts();
      setMsg({ type: "ok", text: "账号归属区域已更新" });
    } catch (e) {
      setMsg({ type: "err", text: `区域设置失败：${api.asError(e)}` });
    } finally {
      setBusy(null);
    }
  }

  async function startDeviceFlow() {
    setMsg(null);
    try {
      const res = await api.qoderDeviceFlowStart();
      setDeviceSession(res.session);
      // 自动打开授权页
      window.open(res.session.authorizationUrl, "_blank", "noopener");
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    }
  }

  async function completeDeviceFlow() {
    if (!deviceSession) return;
    setDeviceWaiting(true);
    setMsg(null);
    try {
      // 服务端轮询至授权完成（最长约 4 分钟）
      const res = await api.qoderDeviceFlowComplete(deviceSession);
      await loadAccounts();
      setDeviceSession(null);
      const account = res.account ?? {};
      setMsg({
        type: "ok",
        text: `账号 ${account.name || account.email || "已"} 授权成功并加入账号池`,
      });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setDeviceWaiting(false);
    }
  }

  async function refreshAccount(uid: string) {
    setBusy(uid);
    try {
      await api.qoderRefreshAccount(uid);
      await loadAccounts();
    } catch (e) {
      setMsg({ type: "err", text: `刷新失败：${api.asError(e)}` });
    } finally {
      setBusy(null);
    }
  }

  async function removeAccount(uid: string) {
    setBusy(uid);
    try {
      await api.qoderRemoveAccount(uid);
      await loadAccounts();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(null);
    }
  }

  async function runTest() {
    if (!testPrompt.trim()) return;
    setTesting(true);
    setTestResult(null);
    try {
      const res = await api.qoderTestCompletion(testModel, testPrompt.trim());
      setTestResult({ ok: true, text: `${res.content}\n\n—— ${res.model} · ${res.latencyMs}ms` });
    } catch (e) {
      setTestResult({ ok: false, text: api.asError(e) });
    } finally {
      setTesting(false);
    }
  }

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-10 flex min-w-0 flex-wrap items-start justify-between gap-4 sm:mb-12">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">Qoder 网关</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            把多个 Qoder 账号汇聚为一个 OpenAI 兼容接口：
            <code className="mx-1 rounded bg-muted/70 px-1.5 py-0.5 text-xs">http://127.0.0.1:57890/qoder/v1</code>
            鉴权密钥与 GLM 代理相同；请求失败时账号池自动轮转，登录态 401 自动用 refresh
            token 续期。
          </p>
        </div>
      </header>

      {msg ? (
        <p
          className={cn(
            "mb-4 text-xs",
            msg.type === "ok" ? "text-primary" : "text-destructive",
          )}
        >
          {msg.text}
        </p>
      ) : null}

      <section className="min-w-0 space-y-2.5" aria-label="Qoder 账号池">
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">账号池</h2>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-0 sm:px-5">
            <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <CardDescription className="min-w-0 text-xs">
                共 {accounts.length} 个账号 · 本机登录账号自动包含并优先使用
              </CardDescription>
              <div className="flex flex-wrap gap-2">
                <Button size="sm" variant="outline" onClick={() => void importLocal()} disabled={busy === "import"}>
                  {busy === "import" ? <Loader2 className="animate-spin" /> : <UserPlus />}导入本机登录态
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void importLocalCn()}
                  disabled={busy === "import-cn"}
                  title="导入 QoderCN（中国版 IDE）登录态，归入 🇨🇳 国内专用池"
                >
                  {busy === "import-cn" ? <Loader2 className="animate-spin" /> : <Globe />}导入 QoderCN 登录态
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void importLocalWorkCn()}
                  disabled={busy === "import-work-cn"}
                  title="导入 QoderWork CN（独立桌面应用）登录态，归入独立算力池"
                >
                  {busy === "import-work-cn" ? <Loader2 className="animate-spin" /> : <Monitor />}导入 QoderWork CN 登录态
                </Button>
                <Button size="sm" variant="outline" onClick={() => void startDeviceFlow()} disabled={deviceWaiting}>
                  <Zap />设备授权添加账号
                </Button>
              </div>
            </div>
            {deviceSession ? (
              <div className="mt-3 space-y-2 rounded-lg border border-border/60 bg-muted/30 px-3 py-3 text-xs">
                <p>
                  1. 授权页已在浏览器打开（
                  <a href={deviceSession.authorizationUrl} target="_blank" rel="noreferrer" className="inline-flex items-center gap-0.5 text-primary hover:underline">
                    打开授权页
                    <ExternalLink className="size-3" />
                  </a>
                  ），登录 Qoder 并选择账号完成授权。
                </p>
                <p>2. 完成后点击下方按钮（服务端会轮询等待，最长约 4 分钟）。</p>
                <Button size="sm" onClick={() => void completeDeviceFlow()} disabled={deviceWaiting}>
                  {deviceWaiting ? <Loader2 className="animate-spin" /> : <Check />}我已完成授权
                </Button>
              </div>
            ) : null}
          </CardHeader>
          <CardContent className="p-0">
            <div className="divide-y divide-border/50 border-t border-border/50">
              {accounts.length === 0 ? (
                <p className="px-4 py-6 text-center text-sm text-muted-foreground sm:px-5">
                  账号池为空。可「导入本机登录态」（国际版）、「导入 QoderCN 登录态」（🇨🇳
                  国内专用池），或用「设备授权添加账号」登录其他 Qoder 账号。
                </p>
              ) : (
                accounts.map((account) => (
                  <div key={String(account.uid)} className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-4 py-3 sm:px-5">
                    <div className="min-w-0">
                      <div className="flex min-w-0 flex-wrap items-center gap-2">
                        <span className="truncate text-sm font-medium">
                          {account.name || account.email || String(account.uid)}
                        </span>
                        {account.source === "local-live" ? (
                          <Badge className="text-[10px]">本机登录</Badge>
                        ) : (
                          <Badge variant="outline" className="text-[10px]">
                            {account.sourceLabel ?? SOURCE_LABELS[account.source ?? ""] ?? account.source}
                          </Badge>
                        )}
                        {account.userTag ? (
                          <Badge variant="secondary" className="text-[10px]">
                            {String(account.userTag)}
                          </Badge>
                        ) : null}
                        {account.isQuotaExceeded ? (
                          <Badge variant="destructive" className="text-[10px]">
                            配额耗尽
                          </Badge>
                        ) : null}
                        {account.regionLabel ? (
                          <Badge
                            variant="outline"
                            className={
                              account.region === "cn"
                                ? "border-red-500/40 bg-red-500/10 text-[10px] text-red-600 dark:text-red-400"
                                : "border-sky-500/40 bg-sky-500/10 text-[10px] text-sky-600 dark:text-sky-400"
                            }
                          >
                            {account.regionLabel}
                          </Badge>
                        ) : null}
                        {account.gateway && account.gateway !== "qoder" ? (
                          <Badge variant="secondary" className="text-[10px]">
                            {account.gateway === "qwen-flash"
                              ? "CN 专用池"
                              : account.gateway === "qwen"
                                ? "Qwen 独立池"
                                : account.gateway}
                          </Badge>
                        ) : null}
                        {account.status && account.status !== "ok" ? (
                          <Badge variant="destructive" className="text-[10px]">
                            异常
                          </Badge>
                        ) : null}
                      </div>
                      <p className="mt-0.5 truncate text-[11px] text-muted-foreground" title={account.error || ""}>
                        {account.email ? `${account.email} · ` : ""}
                        配额 {formatQuota(account.quota)}
                        {account.error ? ` · ${account.error}` : ""}
                      </p>
                    </div>
                    <div className="flex shrink-0 items-center gap-1.5">
                      {account.source !== "local-live" && account.source !== "qodercn-local-live" ? (
                        <>
                          <select
                            className="h-7 rounded-md border border-border bg-background px-1.5 text-[10px] text-foreground"
                            value={account.regionSource === "manual" ? account.region ?? "auto" : "auto"}
                            onChange={(e) => void changeRegion(String(account.uid), e.target.value as "cn" | "intl" | "auto")}
                            aria-label="账号归属区域"
                            title="手动指定账号归属区域（防误判）；选「区域自动」清除手动指定"
                          >
                            <option value="auto">区域自动</option>
                            <option value="cn">🇨🇳 国内</option>
                            <option value="intl">🌐 国际</option>
                          </select>
                          <Button size="sm" variant="ghost" onClick={() => void refreshAccount(String(account.uid))} disabled={busy === String(account.uid)} aria-label="刷新登录态">
                            {busy === String(account.uid) ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                          </Button>
                          <Button size="sm" variant="ghost" className="text-destructive hover:text-destructive" onClick={() => void removeAccount(String(account.uid))} disabled={busy === String(account.uid)} aria-label="移除账号">
                            <Trash2 />
                          </Button>
                        </>
                      ) : null}
                    </div>
                  </div>
                ))
              )}
            </div>
          </CardContent>
        </Card>
      </section>

      <section className="mt-10 min-w-0 space-y-2.5" aria-label="试一试">
        <div className="flex items-center justify-between px-1">
          <h2 className="text-[13px] font-medium leading-5">试一试</h2>
          <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
            <span>快捷模型：</span>
            <button
              type="button"
              className={cn(
                "rounded px-1.5 py-0.5 transition-colors",
                testModel === "max" || testModel === "qwen3.8-max"
                  ? "bg-primary text-primary-foreground font-medium"
                  : "bg-muted hover:bg-muted/80 text-foreground"
              )}
              onClick={() => setTestModel("max")}
            >
              max (Qwen3.8-Max 旗舰)
            </button>
            <button
              type="button"
              className={cn(
                "rounded px-1.5 py-0.5 transition-colors",
                testModel === "pro"
                  ? "bg-primary text-primary-foreground font-medium"
                  : "bg-muted hover:bg-muted/80 text-foreground"
              )}
              onClick={() => setTestModel("pro")}
            >
              pro (标准档)
            </button>
            <button
              type="button"
              className={cn(
                "rounded px-1.5 py-0.5 transition-colors",
                testModel === "lite"
                  ? "bg-primary text-primary-foreground font-medium"
                  : "bg-muted hover:bg-muted/80 text-foreground"
              )}
              onClick={() => setTestModel("lite")}
            >
              lite (Flash 极速)
            </button>
          </div>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-3 sm:px-5">
            <div className="flex flex-wrap items-center gap-2">
              <Input
                className="h-8 w-36 text-xs"
                value={testModel}
                onChange={(e) => setTestModel(e.target.value)}
                placeholder="模型（如 max / qwen3.8-max）"
                aria-label="模型"
              />
              <Input
                className="h-8 min-w-0 flex-1 text-xs"
                value={testPrompt}
                onChange={(e) => setTestPrompt(e.target.value)}
                placeholder="输入测试内容"
                aria-label="测试内容"
              />
              <Button size="sm" onClick={() => void runTest()} disabled={testing}>
                {testing ? <Loader2 className="animate-spin" /> : <CircleCheck />}发送
              </Button>
            </div>
          </CardHeader>
          {testResult ? (
            <CardContent className="px-4 pb-4 sm:px-5">
              <p className={cn("whitespace-pre-wrap rounded-lg border px-3 py-2.5 text-xs", testResult.ok ? "border-border/60" : "border-destructive/40 text-destructive")}>
                {testResult.text}
              </p>
            </CardContent>
          ) : null}
        </Card>
      </section>

      <section className="mt-10" aria-label="接入说明">
        <Alert>
          <CircleAlert />
          <AlertTitle>接入方式（OpenAI 兼容）</AlertTitle>
          <AlertDescription className="space-y-1.5 text-xs">
            <p>
              Base URL：
              <code className="ml-1 rounded bg-muted/70 px-1.5 py-0.5">http://127.0.0.1:57890/qoder/v1</code>
              ，鉴权密钥与「GLM 代理」页相同。
            </p>
            <p>
              网关现已全面兼容阿里通义千问 2026 最新旗舰
              <strong className="mx-1 text-foreground">Qwen3.8-Max</strong>（2.4T 参数 · 1M 上下文）与
              <strong className="mx-1 text-foreground">Qwen3.8-Flash</strong>：
            </p>
            <ul className="list-inside list-disc space-y-0.5 pl-1 text-[11px] text-muted-foreground">
              <li>
                <code className="rounded bg-muted px-1 py-0.5 text-foreground">max</code> 或 <code className="rounded bg-muted px-1 py-0.5 text-foreground">qwen3.8-max</code>：调用通义千问最强旗舰大模型，适合复杂代码与深度推理。
              </li>
              <li>
                <code className="rounded bg-muted px-1 py-0.5 text-foreground">pro</code> 或 <code className="rounded bg-muted px-1 py-0.5 text-foreground">qwen3.6-plus</code>：调用标准主力档。
              </li>
              <li>
                <code className="rounded bg-muted px-1 py-0.5 text-foreground">lite</code> 或 <code className="rounded bg-muted px-1 py-0.5 text-foreground">qwen3.8-flash</code>：调用极速经济档，低延迟、高吞吐。
              </li>
            </ul>
            <p className="pt-1 text-[11px] text-muted-foreground">
              Qoder 新用户注册享受赠送额度与试用配额，配合网关账号池轮转与 401 自动续期，可保障高可用服务。
            </p>
            <p className="pt-1">
              🇨🇳 国内账号另有免费专属路由：
              <code className="mx-1 rounded bg-muted/70 px-1.5 py-0.5">http://127.0.0.1:57890/qwen-flash/v1</code>
              ，单一模型 <code className="rounded bg-muted px-1 py-0.5">qwen3.8-flash</code>（极速档），走
              Qoder CN 专用账号池（本机 QoderCN 登录态 + 国内设备授权账号），与国际池完全隔离、额度互不侵占。
            </p>
          </AlertDescription>
        </Alert>
      </section>

      {/* Qwen 桌面版与官方内置模型状态卡片 */}
      <section className="mt-6" aria-label="Qwen桌面版状态">
        <Card>
          <CardHeader className="p-4 pb-2 sm:p-5 sm:pb-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="flex items-center gap-2">
                <CardTitle className="text-sm font-semibold">Qwen 桌面版内置模型集成</CardTitle>
                {qwenStatus?.installed ? (
                  <Badge variant="outline" className="border-emerald-500/40 bg-emerald-500/10 text-emerald-600 text-[10px]">
                    已检测到本地桌面版
                  </Badge>
                ) : (
                  <Badge variant="outline" className="text-muted-foreground text-[10px]">
                    未检测到安装
                  </Badge>
                )}
                {qwenStatus?.token_found && (
                  <Badge variant="outline" className="border-sky-500/40 bg-sky-500/10 text-sky-600 text-[10px]">
                    已提取登录凭据 (30天长效)
                  </Badge>
                )}
              </div>
              <Button size="sm" variant="outline" className="h-7 text-xs" onClick={() => void loadQwenStatus()}>
                <RefreshCw className="mr-1 h-3 w-3" />刷新状态
              </Button>
            </div>
            <CardDescription className="text-xs">
              网关已自动对接本地 Qwen 桌面版，统一对外提供标准 OpenAI 兼容端点：
              <code className="mx-1 rounded bg-muted px-1.5 py-0.5 text-foreground font-mono">http://127.0.0.1:57890/qwen/v1</code>
            </CardDescription>
          </CardHeader>
          <CardContent className="p-4 pt-0 sm:p-5 sm:pt-0">
            {qwenStatus?.models && qwenStatus.models.length > 0 ? (
              <div className="mt-2 space-y-2">
                <p className="text-xs font-medium text-foreground">
                  已自动获取到桌面版可用的 {qwenStatus.models.length} 款通义千问官方模型：
                </p>
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                  {qwenStatus.models.map((m) => (
                    <div key={m.id} className="rounded-lg border bg-muted/20 p-2.5 text-xs">
                      <div className="flex items-center justify-between">
                        <code className="font-semibold text-primary">{m.id}</code>
                        <span className="text-[10px] text-muted-foreground">{m.name}</span>
                      </div>
                      {m.description && (
                        <p className="mt-1 text-[11px] text-muted-foreground line-clamp-1">{m.description}</p>
                      )}
                    </div>
                  ))}
                </div>
                <div className="mt-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-3 text-[11px] text-muted-foreground">
                  <span className="font-medium text-amber-600 dark:text-amber-400">调用与风控提示：</span>
                  通义千问桌面端在线生成接口由阿里安全霸下风控（RGV587_ERROR）人机滑块保护。外部第三方工具（Cursor、NextChat 等）直接无头调用若遇风控拦截，Sylvia Switch 网关会自动无缝路由至上方 Qoder 阿里官方算力池作为智能兜底，确保您的模型调用永不中断！
                </div>
              </div>
            ) : null}
          </CardContent>
        </Card>
      </section>

      <GatewayAdminSection gateway="qoder" />
    </div>
  );
}
