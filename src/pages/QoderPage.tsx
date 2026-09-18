import { useCallback, useEffect, useState } from "react";
import {
  Check,
  CircleAlert,
  CircleCheck,
  ExternalLink,
  Loader2,
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

  const loadAccounts = useCallback(async () => {
    const res = await api.getQoderAccounts();
    setAccounts(res.accounts ?? []);
  }, []);

  useEffect(() => {
    void loadAccounts().catch(() => undefined);
  }, [loadAccounts]);

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
                  账号池为空。可「导入本机登录态」，或用「设备授权添加账号」登录其他 Qoder 账号。
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
                            {SOURCE_LABELS[account.source ?? ""] ?? account.source}
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
                      {account.source !== "local-live" ? (
                        <>
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
        <div className="px-1">
          <h2 className="text-[13px] font-medium leading-5">试一试</h2>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardHeader className="gap-0 px-4 pt-3 pb-3 sm:px-5">
            <div className="flex flex-wrap items-center gap-2">
              <Input
                className="h-8 w-28 text-xs"
                value={testModel}
                onChange={(e) => setTestModel(e.target.value)}
                placeholder="模型（lite）"
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
              ，密钥与「GLM 代理」页相同，模型名填
              <code className="mx-1 rounded bg-muted/70 px-1 py-0.5">lite</code>
              （Pro/Max 档以 Qoder 实际下发为准）。
            </p>
            <p>
              Qoder 新用户注册有 Pro 试用与赠送 quota，配合账号池轮转可获得可观的免费用量。
            </p>
          </AlertDescription>
        </Alert>
      </section>

      <GatewayAdminSection gateway="qoder" />
    </div>
  );
}
