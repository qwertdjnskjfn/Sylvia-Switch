import { useCallback, useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { CopyButton } from "@/components/ui/copy-button";
import { Download, Loader2, Plus, RefreshCw, Send, Trash2, Zap } from "lucide-react";
import * as api from "@/lib/api";

/** MiniMax 通道页（1.6.0）：Agent 网页端反代 + 账号池 + 每日签到。
 *  参照 GLM Plan 页模式：国内版/国际版分组 + 接入信息两区化。对话为实验性（payload 模板待抓包校准）。 */

const REGION_ICONS = { international: "🌐", domestic: "🇨🇳" } as const;
const REGION_LABELS = { international: "国际版", domestic: "国内版" } as const;

type MmAccount = {
  id: string;
  label: string;
  region: string;
  regionLabel: string;
  tokenMasked: string;
  uuid: string;
  deviceId: string;
  userId: string;
  agentId: string;
  baseUrl: string;
  enabled: boolean;
  lastError: string | null;
  cooldown: boolean;
  signinLastDate: string;
  signinPointsTotal: number;
  // 2026-10-05：多账号识别（由 status 暴露）
  usable?: boolean;
  isLocal?: boolean;
  pinned?: boolean;
  // 固定集合内的顺序（0 起；null/undefined = 未固定）
  pinOrder?: number | null;
  // 2026-10-05：冷却剩余秒数（0 = 未冷却）
  cooldownRemainSecs?: number;
  balance?: number | null;
  creditConsumed?: number | null;
  balanceAt?: number | null;
};

type MmStatus = {
  enabled: boolean;
  apiKey: string;
  defaultModel: string;
  models: string[];
  accounts: MmAccount[];
  // 2026-10-05：选号模式（多账号切换）
  accountMode?: string;
  pinnedAccountId?: string;
  // 固定集合（有序，可同时多个）
  pinnedAccountIds?: string[];
  // 2026-10-05：熔断冷却参数（秒，可配）
  cooldownBaseSecs?: number;
  cooldownMaxSecs?: number;
  // 2026-10-05：单次请求换号上限（0 = 不限制）
  maxFailover?: number;
  // 2026-10-05：curl 闸门/重试
  curlMaxConcurrent?: number;
  curlRetries?: number;
  chatExperimental: boolean;
  endpoint: string;
  payloadTemplate: string;
};

type Msg = { type: "ok" | "err"; text: string } | null;

function Section({ title, description, children }: { title: string; description: string; children: React.ReactNode }) {
  return (
    <section className="min-w-0 space-y-2.5" aria-label={title}>
      <div className="px-1">
        <h2 className="text-[13px] font-medium leading-5">{title}</h2>
        <p className="text-xs text-muted-foreground">{description}</p>
      </div>
      <Card>
        <CardContent className="p-0">{children}</CardContent>
      </Card>
    </section>
  );
}

export default function MiniMaxPage() {
  const [status, setStatus] = useState<MmStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<Msg>(null);
  // 添加账号（网页端 JWT + 指纹三件套，均从浏览器抓取）
  const [fLabel, setFLabel] = useState("");
  const [fRegion, setFRegion] = useState<"domestic" | "international">("domestic");
  const [fToken, setFToken] = useState("");
  const [fUuid, setFUuid] = useState("");
  const [fDeviceId, setFDeviceId] = useState("");
  const [fUserId, setFUserId] = useState("");
  const [fAgentId, setFAgentId] = useState("");
  const [fScreenW, setFScreenW] = useState("1707");
  const [fScreenH, setFScreenH] = useState("1067");
  // 试一试
  // 熔断参数输入（空串=未改，显示服务端值）
  const [mf, setMf] = useState("");
  const [curlMc, setCurlMc] = useState("");
  const [curlRt, setCurlRt] = useState("");
  const [onlyUsable, setOnlyUsable] = useState(false);
  const [, setTick] = useState(0);
  const [cdBase, setCdBase] = useState("");
  const [cdMax, setCdMax] = useState("");
  const [testPrompt, setTestPrompt] = useState("只回复 OK");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ ok: boolean; text: string } | null>(null);

  const refresh = useCallback(async () => {
    try {
      const data = (await api.getMinimaxStatus()) as MmStatus;
      setStatus(data);
      setLoadError(null);
    } catch (e) {
      setLoadError(String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 2026-10-05：冷却倒计时（本地每秒重渲染）+ 每 30s 后台刷新一次真实状态
  const statusAtRef = useRef<number>(Date.now());
  useEffect(() => {
    statusAtRef.current = Date.now();
  }, [status]);
  useEffect(() => {
    const t = window.setInterval(() => setTick((x) => x + 1), 1000);
    const r = window.setInterval(() => void refresh(), 30000);
    return () => {
      window.clearInterval(t);
      window.clearInterval(r);
    };
  }, [refresh]);

  async function addAccount() {
    setBusy("add");
    try {
      await api.addMinimaxAccount({
        label: fLabel || "未命名",
        region: fRegion,
        token: fToken.trim(),
        uuid: fUuid.trim(),
        deviceId: fDeviceId.trim(),
        userId: fUserId.trim(),
        agentId: fAgentId.trim(),
        screenW: fScreenW,
        screenH: fScreenH,
      });
      setMsg({ type: "ok", text: `已加入${REGION_LABELS[fRegion]}账号池` });
      setFLabel("");
      setFToken("");
      setFUuid("");
      setFDeviceId("");
      setFUserId("");
      setFAgentId("");
      await refresh();
    } catch (e) {
      setMsg({ type: "err", text: String(e) });
    } finally {
      setBusy(null);
    }
  }

  async function act(name: string, body: Record<string, unknown>, okText: string): Promise<unknown> {
    setBusy(name);
    try {
      // 动态 id 操作前缀拦截：probe:<id> / remove:<id> / enable:<id> / pin:<id>
      const prefix = (pre: string) => name === pre || name.startsWith(pre + ":");
      if (prefix("clearCooldown")) {
        const id = name.slice("clearCooldown:".length);
        await api.clearMinimaxCooldown({ id });
        setMsg({ type: "ok", text: "已解除冷却，可立即重试" });
        await refresh();
        return null;
      }
      if (prefix("pin")) {
        const res = await api.pinMinimaxAccount(body as { id?: string; pinned: boolean; all?: boolean; onlyUsable?: boolean });
        void res;
        setMsg({ type: "ok", text: okText });
        await refresh();
        return null;
      }
      if (prefix("probe")) {
        const id = name.slice("probe:".length);
        const res = await fetch("/api/minimax/probe", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ id }),
        });
        const d = (await res.json()) as { ok?: boolean; balance?: number | null; detail?: string };
        setMsg({
          type: d.ok ? "ok" : "err",
          text: `探测：${d.ok ? "可用" : "异常"}${d.balance != null ? ` · 积分余额 ${d.balance}` : ""}${d.detail ? ` · ${d.detail}` : ""}`,
        });
        await refresh();
        return null;
      }
      const actions: Record<string, () => Promise<unknown>> = {
        saveMinimaxConfig: () => api.saveMinimaxConfig(body),
        capture: () => api.captureMinimaxDesktop(),
        removeMinimaxAccount: () => api.removeMinimaxAccount(body as { id: string }),
        enableMinimaxAccount: () => api.enableMinimaxAccount(body as { id: string; enabled: boolean }),
        // 2026-10-05：切换账号 / 选号策略（多账号支持）
        switchAccount: () => api.switchMinimaxAccount(body as { mode?: string; id?: string }),
        // 2026-10-05：固定集合增删（幂等；支持同时固定多个）
        pinAccount: () => api.pinMinimaxAccount(body as { id?: string; pinned: boolean; all?: boolean; onlyUsable?: boolean }),
        clearCooldown: () => api.clearMinimaxCooldown(body as { id?: string }),
        "probe": async () => {
          const res = await fetch("/api/minimax/probe", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(body),
          });
          const d = (await res.json()) as { ok?: boolean; balance?: number | null; detail?: string };
          setMsg({ type: "ok", text: `探测完成：${d.ok ? "可用" : "异常"}${d.balance != null ? ` · 余额 ${d.balance}` : ""}${d.detail ? ` · ${d.detail}` : ""}` });
        },
      };
      const fn = actions[name];
      if (!fn) throw new Error(`未知操作: ${name}`);
      const result = await fn();
      if (name !== "probe") setMsg({ type: "ok", text: okText });
      await refresh();
      return result;
    } catch (e) {
      setMsg({ type: "err", text: String(e) });
    } finally {
      setBusy(null);
    }
  }

  async function runSignin() {
    setBusy("signin");
    try {
      const r = (await api.runMinimaxSignin({})) as { results?: { account?: string; action?: string; error?: string; points?: unknown }[] };
      const lines = (r.results || []).map(
        (x) => `${x.account || "?"}: ${x.action ? (x.action === "already-claimed" ? "今日已签" : `已领 ${x.points ?? "?"} 分`) : x.error || "失败"}`,
      );
      setMsg({ type: "ok", text: lines.length ? `签到完成：${lines.join("；")}` : "无待签账号（或未启用）" });
      await refresh();
    } catch (e) {
      setMsg({ type: "err", text: String(e) });
    } finally {
      setBusy(null);
    }
  }

  async function runTest() {
    if (!status?.enabled) return;
    setTesting(true);
    setTestResult(null);
    try {
      const res = await fetch("/api/minimax/test", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ prompt: testPrompt }),
      });
      const data = (await res.json()) as { ok?: boolean; error?: string; content?: string };
      if (!res.ok || !data.ok) throw new Error(data.error || `HTTP ${res.status}`);
      setTestResult({ ok: true, text: data.content || "(空)" });
    } catch (e) {
      setTestResult({ ok: false, text: String(e) });
    } finally {
      setTesting(false);
    }
  }

  if (loadError && !status) {
    return (
      <div className="space-y-3 p-4">
        <h1 className="text-lg font-medium">MiniMax 通道</h1>
        <p className="text-sm text-destructive">加载失败：{loadError}</p>
      </div>
    );
  }
  if (!status) {
    return (
      <div className="flex items-center gap-2 p-6 text-sm text-muted-foreground">
        <Loader2 className="animate-spin" /> 加载中…
      </div>
    );
  }

  const curl = (region: string) =>
    `curl ${status.endpoint}/chat/completions \\\n  -H "Authorization: Bearer <访问密钥>" \\\n  -H "Content-Type: application/json" \\\n  -d '{"model":"MiniMax-M3","region":"${region}","messages":[{"role":"user","content":"你好"}]}'`;

  const groups = [
    { key: "domestic", icon: REGION_ICONS.domestic, label: REGION_LABELS.domestic, models: status.accounts.filter((a) => a.region === "domestic") },
    { key: "international", icon: REGION_ICONS.international, label: REGION_LABELS.international, models: status.accounts.filter((a) => a.region === "international") },
  ];

  return (
    <div className="min-w-0 space-y-4 p-4 sm:p-6">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h1 className="text-lg font-medium">MiniMax 通道</h1>
          <p className="text-xs text-muted-foreground">
            MiniMax Agent 网页端反代 → OpenAI 兼容接口（国内版 agent.minimaxi.com / 国际版 agent.minimax.io 双区，账号池轮询 + 每日自动签到）
          </p>
        </div>
        <div className="flex items-center gap-2">
          <span className="text-xs text-muted-foreground">{status.enabled ? "已启用" : "已停用"}</span>
          <Switch
            checked={status.enabled}
            onCheckedChange={(checked) => void act("saveMinimaxConfig", { enabled: checked }, checked ? "已启用" : "已停用")}
            disabled={busy === "saveMinimaxConfig"}
          />
        </div>
      </div>

      {status.chatExperimental ? (
        <div className="rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-600">
          <p className="font-medium">⚠ 对话功能当前不可用（2026-10-05 排查结论）</p>
          <p className="mt-1.5 leading-relaxed">
            经排查，上游 <code className="mx-0.5">/mavis/api/v1/llm/v1/messages</code> 端点对本渠道的桌面端 OAuth
            请求返回固定输入（<code className="mx-0.5">usage.input_tokens</code> 恒为 21，与 prompt 长度无关），
            导致回复答非所问。已逐一排除：端点 / payload 形状 / max_tokens / header / User-Agent /
            content 格式 / HTTP 版本 / TLS 后端（rustls 与 native-tls）/ 报文细节 / 连接池 /
            HTTP 客户端（curl 子进程同样复现）—— 均非原因。
          </p>
          <p className="mt-1.5">
            同参数用 Python urllib 直连则每次正常（<code className="mx-0.5">in=41</code>），
            故判断为<strong>上游侧对该 OAuth 账号/会话的限制或状态异常</strong>，非本项目代码问题。
            签到与余额为多仓库交叉验证的可靠部分，不受影响。
          </p>
        </div>
      ) : null}

      {msg ? (
        <div className={"rounded-lg px-3 py-2 text-xs " + (msg.type === "ok" ? "bg-emerald-500/10 text-emerald-600" : "bg-destructive/10 text-destructive")}>
          {msg.text}
        </div>
      ) : null}

      <Section title="接入信息" description="Base URL 与访问密钥两区通用；用请求体 region 字段锁定区域版本">
        <CardContent className="p-0">
          <div className="flex flex-wrap items-center gap-2 px-4 py-3.5 sm:px-5">
            <span className="text-xs text-muted-foreground">Base URL</span>
            <code className="rounded bg-muted/70 px-2 py-1 text-xs">{status.endpoint}</code>
            <CopyButton text={status.endpoint} label="Base URL" />
            <span className="ml-2 text-xs text-muted-foreground">API Key</span>
            <code className="rounded bg-muted/70 px-2 py-1 text-xs">{status.apiKey}</code>
            <CopyButton text={status.apiKey} label="API Key" />
          </div>
          <div className="grid gap-2 px-4 pb-3.5 sm:grid-cols-2 sm:px-5">
            {(["domestic", "international"] as const).map((r) => (
              <div key={r} className="rounded-lg border border-border/50 p-2.5">
                <div className="text-[12px] font-medium">
                  {REGION_ICONS[r]} {REGION_LABELS[r]}接入
                </div>
                <pre className="mt-1.5 min-w-0 overflow-x-auto rounded bg-muted/60 p-2 text-[10px] leading-4">{curl(r)}</pre>
                <div className="mt-1.5">
                  <CopyButton text={curl(r)} label={`${REGION_LABELS[r]}示例`} />
                </div>
              </div>
            ))}
          </div>
        </CardContent>
      </Section>

      <Section
        title="模型目录"
        description="来自桌面端 config.yaml 的真实模型表（本机实勘）；对话通道默认走 MiniMax-M3.1-Flash-Preview"
      >
        <CardContent className="p-0">
          <div className="space-y-1.5 px-4 py-3 sm:px-5">
            {(status.models || []).map((m) => (
              <div key={m} className="flex flex-wrap items-center gap-2 rounded bg-muted/60 px-2.5 py-1.5 text-xs">
                <span className="font-medium">{m}</span>
                {m === "MiniMax-M3.1-Flash-Preview" ? (
                  <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">默认 · 对话通道在用</Badge>
                ) : null}
                {m === "MiniMax-M3" ? (
                  <span className="text-[11px] text-muted-foreground">512k 上下文 · 可选 1M（higher_usage）· 支持图片附件</span>
                ) : null}
                {m === "MiniMax-M2.7" ? (
                  <span className="text-[11px] text-muted-foreground">200k 上下文 · 128k 输出 · 强制思考</span>
                ) : null}
                {m === "MiniMax-M2.7-highspeed" ? (
                  <span className="text-[11px] text-muted-foreground">M2.7 高速版</span>
                ) : null}
              </div>
            ))}
            {(status.models || []).length === 0 ? (
              <p className="text-xs text-muted-foreground">未读取到桌面端模型表（检查 ~/.minimax/config.yaml 是否存在）</p>
            ) : null}
          </div>
        </CardContent>
      </Section>

      <Section title="账号池" description="凭据取自网页端：localStorage._token（JWT）+ uuid/device_id/user_id（localStorage/sessionStorage）+ agent_id（URL 路径）；yy 签名覆盖指纹，必须与浏览器一致">
        <CardContent className="p-0">
          <div className="space-y-2 px-4 pt-3 sm:px-5">
            <div className="flex flex-wrap items-end gap-2">
              <div className="space-y-1">
                <p className="text-[10px] text-muted-foreground">区域版本</p>
                <Select value={fRegion} onValueChange={(v) => setFRegion(v as "domestic" | "international")}>
                  <SelectTrigger className="w-36">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="domestic">🇨🇳 国内版</SelectItem>
                    <SelectItem value="international">🌐 国际版</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="min-w-0 flex-1 space-y-1">
                <p className="text-[10px] text-muted-foreground">备注名</p>
                <Input value={fLabel} onChange={(e) => setFLabel(e.target.value)} placeholder="如：minimax 主号" />
              </div>
              <div className="min-w-0 flex-[1.6] space-y-1">
                <p className="text-[10px] text-muted-foreground">JWT（localStorage._token，约 40 天）</p>
                <Input value={fToken} onChange={(e) => setFToken(e.target.value)} placeholder="粘贴 JWT…" />
              </div>
              <Button size="sm" disabled={busy === "add" || !fToken.trim()} onClick={() => void addAccount()}>
                {busy === "add" ? <Loader2 className="animate-spin" /> : <Plus />}加入账号池
              </Button>
            </div>
            <details className="text-xs text-muted-foreground">
              <summary className="cursor-pointer select-none">展开：指纹三件套 + agent_id（yy 签名覆盖，强烈建议填写真实值）</summary>
              <div className="mt-2 grid gap-2 sm:grid-cols-4">
                <Input value={fUuid} onChange={(e) => setFUuid(e.target.value)} placeholder="uuid（UNIQUE_USER_ID）" />
                <Input value={fDeviceId} onChange={(e) => setFDeviceId(e.target.value)} placeholder="device_id（纯数字）" />
                <Input value={fUserId} onChange={(e) => setFUserId(e.target.value)} placeholder="user_id" />
                <Input value={fAgentId} onChange={(e) => setFAgentId(e.target.value)} placeholder="agent_id（URL 路径）" />
                <Input value={fScreenW} onChange={(e) => setFScreenW(e.target.value)} placeholder="屏宽" />
                <Input value={fScreenH} onChange={(e) => setFScreenH(e.target.value)} placeholder="屏高" />
              </div>
            </details>
          </div>
          {groups.map((g) => (
            <div key={g.key} className="min-w-0">
              <div className="flex items-center gap-2 border-b border-border/40 px-4 pb-1.5 pt-3 sm:px-5">
                <span className="text-[12px] font-medium">
                  {g.icon} {g.label}
                </span>
                <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">
                  {g.models.length} 个账号
                </Badge>
                {g.models.length === 0 ? <span className="text-[11px] text-muted-foreground">暂无账号</span> : null}
              </div>
              {g.models.map((a) => (
                <div key={a.id} className="flex flex-wrap items-center gap-2 border-b border-border/40 px-4 py-2 sm:px-5">
                  <span className="text-xs font-medium">{a.label}</span>
                  <code className="text-[11px] text-muted-foreground">{a.tokenMasked}</code>
                  {a.pinned ? <Badge className="h-5 px-1.5 text-[10px]">当前账号</Badge> : null}
                  {a.cooldown ? (
                    <Badge variant="warning" className="h-5 px-1.5 text-[10px]">
                      {(() => {
                        const elapsed = Math.floor((Date.now() - statusAtRef.current) / 1000);
                        const remain = Math.max(0, (a.cooldownRemainSecs ?? 0) - elapsed);
                        return remain > 0 ? `冷却中 还剩 ${remain}s` : "冷却中 即将恢复";
                      })()}
                    </Badge>
                  ) : null}
                  {a.usable === false && !a.cooldown ? <Badge variant="warning" className="h-5 px-1.5 text-[10px]">不可用</Badge> : null}
                  {a.isLocal ? <Badge variant="outline" className="h-5 px-1.5 text-[10px]">本机</Badge> : null}
                  {(a as { source?: string }).source === "desktop" ? (
                    <Badge variant="outline" className="h-5 px-1.5 text-[10px]">桌面端共享</Badge>
                  ) : null}
                  {a.balance != null ? (
                    <Badge variant="secondary" className="h-5 px-1.5 text-[10px]" title={`已消耗 ${a.creditConsumed ?? 0} 分（credit/details 明细）`}>
                      积分 {a.balance} · 已用 {a.creditConsumed ?? 0}
                    </Badge>
                  ) : null}
                  <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">
                    签到 {a.signinLastDate ? a.signinLastDate : "未签"} · 累计签到 {a.signinPointsTotal} 分
                  </Badge>
                  {a.lastError ? <span className="min-w-0 flex-1 truncate text-[11px] text-destructive">{a.lastError}</span> : <span className="flex-1" />}
                  {a.pinned ? (() => {
                    const ids = status.pinnedAccountIds || [];
                    const i = ids.indexOf(a.id);
                    const move = (dir: -1 | 1) => {
                      const next = [...ids];
                      const j = i + dir;
                      if (i < 0 || j < 0 || j >= next.length) return;
                      [next[i], next[j]] = [next[j], next[i]];
                      void act("switchAccount", { ids: next }, `已调整固定顺序：${a.label} ${dir < 0 ? "上移" : "下移"}`);
                    };
                    return (
                      <span className="flex items-center gap-0.5">
                        <Button
                          variant="outline"
                          size="sm"
                          className="h-6 w-6 p-0 text-[10px]"
                          disabled={busy === "switchAccount" || i <= 0}
                          onClick={() => move(-1)}
                          title="在固定集合内上移（= 提高优先顺序）"
                        >
                          ↑
                        </Button>
                        <Button
                          variant="outline"
                          size="sm"
                          className="h-6 w-6 p-0 text-[10px]"
                          disabled={busy === "switchAccount" || i < 0 || i >= ids.length - 1}
                          onClick={() => move(1)}
                          title="在固定集合内下移"
                        >
                          ↓
                        </Button>
                      </span>
                    );
                  })() : null}
                  {a.cooldown ? (
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-6 px-2 text-[10px]"
                      disabled={busy === `clearCooldown:${a.id}`}
                      onClick={() => void act(`clearCooldown:${a.id}`, {}, "已解除冷却，可立即重试")}
                      title="清除冷却计数与错误，立即允许再次使用该账号"
                    >
                      解除冷却
                    </Button>
                  ) : null}
                  <Button
                    variant={a.pinned ? "default" : "outline"}
                    size="sm"
                    className="h-6 px-2 text-[10px]"
                    disabled={busy === `pin:${a.id}`}
                    onClick={() =>
                      void act(
                        `pin:${a.id}`,
                        { id: a.id, pinned: !a.pinned },
                        a.pinned ? `已把「${a.label}」移出固定集合` : `已把「${a.label}」加入固定集合（可继续固定其他账号）`,
                      )
                    }
                    title={a.pinned ? "点击取消固定（从固定集合移出）" : "加入固定集合：可同时固定多个账号，集合内有顺序"}
                  >
                    {a.pinned ? `已固定${typeof a.pinOrder === "number" ? ` #${a.pinOrder + 1}` : ""}` : "固定"}
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    className="h-6 px-2 text-[10px]"
                    disabled={busy === `probe:${a.id}`}
                    onClick={() =>
                      void act(
                        `probe:${a.id}`,
                        { id: a.id },
                        "",
                      )
                    }
                  >
                    <Zap />探测
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    className="h-6 px-2 text-[10px]"
                    disabled={busy === `remove:${a.id}`}
                    onClick={() => void act(`remove:${a.id}`, { id: a.id }, "已移出账号池")}
                  >
                    <Trash2 />移除
                  </Button>
                </div>
              ))}
            </div>
          ))}
          <div className="flex flex-wrap items-center gap-2 px-4 py-3 sm:px-5">
            <Button
              size="sm"
              variant="outline"
              disabled={busy === "capture"}
              onClick={() =>
                void act("capture", {}, "").then((r) => {
                  const res = r as { added?: number; skipped?: string[] } | undefined;
                  if (res?.added) setMsg({ type: "ok", text: `已从桌面端导入 ${res.added} 个账号（只读共享登录态，桌面端续期自动跟上）` });
                })
              }
            >
              {busy === "capture" ? <Loader2 className="animate-spin" /> : <Download />}从桌面端导入
            </Button>
            <div className="flex flex-wrap items-center gap-1.5">
              <span className="text-xs text-muted-foreground">curl 闸门：</span>
              <input
                className="h-7 w-14 rounded-md border border-border bg-transparent px-2 text-xs"
                value={curlMc || String(status.curlMaxConcurrent ?? 3)}
                onChange={(e) => setCurlMc(e.target.value)}
                inputMode="numeric"
                title="最多同时在飞的 curl 子进程数（1~32，默认 3）"
              />
              <span className="text-[11px] text-muted-foreground">并发 · 重试</span>
              <input
                className="h-7 w-14 rounded-md border border-border bg-transparent px-2 text-xs"
                value={curlRt || String(status.curlRetries ?? 3)}
                onChange={(e) => setCurlRt(e.target.value)}
                inputMode="numeric"
                title="curl 最大尝试次数（1~10，默认 3）"
              />
              <span className="text-[11px] text-muted-foreground">次</span>
              <Button
                size="sm"
                variant="outline"
                className="h-7 px-2 text-xs"
                disabled={busy === "saveMinimaxConfig"}
                onClick={() =>
                  void act(
                    "saveMinimaxConfig",
                    {
                      curlMaxConcurrent: Number(curlMc || status.curlMaxConcurrent || 3),
                      curlRetries: Number(curlRt || status.curlRetries || 3),
                    },
                    "curl 闸门参数已保存",
                  ).then(() => {
                    setCurlMc("");
                    setCurlRt("");
                  })
                }
              >
                保存
              </Button>
            </div>
            <div className="flex flex-wrap items-center gap-1.5">
              <span className="text-xs text-muted-foreground">换号上限：</span>
              <input
                className="h-7 w-14 rounded-md border border-border bg-transparent px-2 text-xs"
                value={mf || String(status.maxFailover ?? 0)}
                onChange={(e) => setMf(e.target.value)}
                inputMode="numeric"
                title="单次请求最多尝试几个候选账号；0 = 不限制（默认，按候选池顺序试完为止）"
              />
              <span className="text-[11px] text-muted-foreground">
                {(mf || String(status.maxFailover ?? 0)) === "0" ? "个（0 = 不限制，全部候选都会轮到）" : "个"}
              </span>
              <Button
                size="sm"
                variant="outline"
                className="h-7 px-2 text-xs"
                disabled={busy === "saveMinimaxConfig"}
                onClick={() =>
                  void act(
                    "saveMinimaxConfig",
                    { maxFailover: Number(mf || status.maxFailover || 0) },
                    "换号上限已保存",
                  ).then(() => setMf(""))
                }
              >
                保存
              </Button>
            </div>
            <div className="flex flex-wrap items-center gap-1.5">
              <span className="text-xs text-muted-foreground">熔断冷却：</span>
              <input
                className="h-7 w-16 rounded-md border border-border bg-transparent px-2 text-xs"
                value={cdBase || String(status.cooldownBaseSecs ?? 60)}
                onChange={(e) => setCdBase(e.target.value)}
                inputMode="numeric"
                title="冷却基数（秒）：首次失败后的冷却时长；风控(429/403) 自动 ×2"
              />
              <span className="text-[11px] text-muted-foreground">秒起 · 上限</span>
              <input
                className="h-7 w-20 rounded-md border border-border bg-transparent px-2 text-xs"
                value={cdMax || String(status.cooldownMaxSecs ?? 1800)}
                onChange={(e) => setCdMax(e.target.value)}
                inputMode="numeric"
                title="冷却上限（秒）：指数退避的封顶值"
              />
              <span className="text-[11px] text-muted-foreground">秒</span>
              <Button
                size="sm"
                variant="outline"
                className="h-7 px-2 text-xs"
                disabled={busy === "saveMinimaxConfig"}
                onClick={() =>
                  void act(
                    "saveMinimaxConfig",
                    {
                      cooldownBaseSecs: Number(cdBase || status.cooldownBaseSecs || 60),
                      cooldownMaxSecs: Number(cdMax || status.cooldownMaxSecs || 1800),
                    },
                    "熔断参数已保存",
                  ).then(() => {
                    setCdBase("");
                    setCdMax("");
                  })
                }
              >
                保存
              </Button>
            </div>
            <div className="flex flex-wrap items-center gap-1.5">
              <span className="text-xs text-muted-foreground">选号策略：</span>
              {([
                ["round_robin", "轮询"],
                ["local_first", "本机优先"],
                ["local_only", "仅本机"],
                ["pinned", "固定账号"],
              ] as const).map(([mode, label]) => (
                <Button
                  key={mode}
                  size="sm"
                  variant={status.accountMode === mode ? "default" : "outline"}
                  className="h-7 px-2 text-xs"
                  disabled={busy === "switchAccount"}
                  onClick={() => void act("switchAccount", { mode }, `选号策略已切换为「${label}」`)}
                >
                  {label}
                </Button>
              ))}
              <label className="flex items-center gap-1 text-[11px] text-muted-foreground" title="只把当前可用（启用、未冷却、有凭据）的账号入集合">
                <input type="checkbox" checked={onlyUsable} onChange={(e) => setOnlyUsable(e.target.checked)} />
                仅可用
              </label>
              <Button
                size="sm"
                variant="outline"
                className="h-7 px-2 text-xs"
                disabled={busy === "pinAccount" || !status.accounts.length}
                onClick={() =>
                  void act(
                    "pinAccount",
                    { all: true, pinned: true, onlyUsable },
                    onlyUsable
                      ? `已固定「仅可用」账号（可用 ${status.accounts.filter((x) => x.usable).length} 个）`
                      : `已一键固定全部 ${status.accounts.length} 个账号`,
                  )
                }
                title="把账号池账号一次性加入固定集合（勾选「仅可用」则只收可用账号；顺序=列表顺序）"
              >
                全部固定
              </Button>
              <Button
                size="sm"
                variant="outline"
                className="h-7 px-2 text-xs"
                disabled={busy === "pinAccount" || !(status.pinnedAccountIds || []).length}
                onClick={() => void act("pinAccount", { all: true, pinned: false }, "已清空固定集合")}
                title="一键清空固定集合（不动账号，也不改选号策略）"
              >
                清空固定
              </Button>
              <span className="text-[11px] text-muted-foreground">
                {status.accountMode === "pinned"
                  ? (() => {
                      const ids = status.pinnedAccountIds || [];
                      if (!ids.length) return "固定集合为空：请先点账号卡片上的「固定」";
                      const labels = ids.map(
                        (id) => status.accounts.find((x) => x.id === id)?.label || id,
                      );
                      return `固定集合 ${ids.length} 个：${labels.map((l, i) => `${i + 1}.${l}`).join("  ")}`;
                    })()
                  : `多账号同时可用：按策略依次取号${(status.pinnedAccountIds || []).length ? `（已固定 ${(status.pinnedAccountIds || []).length} 个，切到「固定账号」即只用它们）` : ""}`}
              </span>
            </div>
            <Button size="sm" disabled={busy === "signin" || !status.enabled} onClick={() => void runSignin()}>
              {busy === "signin" ? <Loader2 className="animate-spin" /> : <RefreshCw />}立即全量签到
            </Button>
            <span className="text-xs text-muted-foreground">自动签到：每日 07:00–10:00（UTC+8）窗口内后台自动执行；账号间 2s 防风控节律</span>
          </div>
        </CardContent>
      </Section>

      <Section title="试一试（实验性）" description="经网页端通道发一条测试对话；上游 400 时按提示抓 HAR 更新 payload 模板">
        <CardContent className="p-0">
          <div className="mx-4 space-y-2.5 py-3.5 sm:mx-5">
            <div className="flex flex-wrap items-center gap-2">
              <Input
                className="min-w-0 flex-1"
                value={testPrompt}
                onChange={(e) => setTestPrompt(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void runTest();
                }}
              />
              <Button onClick={() => void runTest()} disabled={testing || !status.enabled}>
                {testing ? <Loader2 className="animate-spin" /> : <Send />}发送
              </Button>
            </div>
            {testResult ? (
              <div className={"min-w-0 whitespace-pre-wrap break-words rounded-lg px-3 py-2.5 text-sm " + (testResult.ok ? "bg-muted/60" : "bg-destructive/10 text-destructive")}>
                {testResult.text}
              </div>
            ) : null}
          </div>
        </CardContent>
      </Section>

      <div className="flex justify-end px-1">
        <Button variant="ghost" size="sm" onClick={() => void refresh()}>
          <RefreshCw />刷新状态
        </Button>
      </div>
    </div>
  );
}
