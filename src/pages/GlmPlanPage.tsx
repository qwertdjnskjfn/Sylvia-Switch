import { useCallback, useEffect, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { CopyButton } from "@/components/ui/copy-button";
import { Loader2, Plus, RefreshCw, Send, Trash2 } from "lucide-react";
import * as api from "@/lib/api";

/** GLM Plan 网关页（1.4.0 新通道）：GLM Coding Plan 账号池 → OpenAI/Anthropic。
 *  参照 GLM 代理页的模式：国内版/国际版两组分类展示 + 按类选用 + 接入信息两区化。 */

const REGION_ICONS = { international: "🌐", domestic: "🇨🇳" } as const;
const REGION_LABELS = { international: "国际版", domestic: "国内版" } as const;
const PROVIDER_LABELS: Record<string, string> = { bigmodel: "国内版", zai: "国际版" };

type PlanAccount = {
  id: string;
  label: string;
  provider: string;
  providerLabel: string;
  apiKeyMasked: string;
  enabled: boolean;
  lastError: string | null;
  cooling: boolean;
};

type PlanStatus = {
  enabled: boolean;
  apiKey: string;
  apiKeyMasked: string;
  defaultProvider: string;
  defaultModel: string;
  models: string[];
  accounts: PlanAccount[];
  cache: { enabled: boolean; hits: number; misses: number; entries: number };
  endpoint: string;
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

export default function GlmPlanPage() {
  const [status, setStatus] = useState<PlanStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<Msg>(null);
  const [regionFilter, setRegionFilter] = useState<"all" | "domestic" | "international">("all");
  // 添加账号表单
  const [newLabel, setNewLabel] = useState("");
  const [newProvider, setNewProvider] = useState<"bigmodel" | "zai">("bigmodel");
  const [newKey, setNewKey] = useState("");
  // 试一试
  const [loadModel, setLoadModel] = useState("glm-4.6");
  const [testPrompt, setTestPrompt] = useState("只回复 OK");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ ok: boolean; text: string } | null>(null);

  const refresh = useCallback(async () => {
    try {
      const data = (await api.getGlmplanStatus()) as PlanStatus;
      setStatus(data);
      setLoadError(null);
    } catch (e) {
      setLoadError(String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function act(name: string, body: Record<string, unknown>, okText: string, after?: () => void) {
    setBusy(name);
    try {
      const actions: Record<string, () => Promise<unknown>> = {
        save_glmplan_config: () => api.saveGlmplanConfig(body),
        add_glmplan_account: () => api.addGlmplanAccount(body as { label: string; provider: string; apiKey: string }),
        remove_glmplan_account: () => api.removeGlmplanAccount(body as { id: string }),
        enable_glmplan_account: () => api.enableGlmplanAccount(body as { id: string; enabled: boolean }),
      };
      const fn = actions[name];
      if (!fn) throw new Error(`未知操作: ${name}`);
      await fn();
      setMsg({ type: "ok", text: okText });
      await refresh();
      after?.();
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
      const res = await fetch("/api/glmplan/test", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ model: loadModel, prompt: testPrompt }),
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
        <h1 className="text-lg font-medium">GLM Plan 网关</h1>
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

  const domestic = status.accounts.filter((a) => a.provider === "bigmodel");
  const international = status.accounts.filter((a) => a.provider === "zai");
  const groups =
    regionFilter === "all"
      ? [
          { key: "domestic", icon: REGION_ICONS.domestic, label: REGION_LABELS.domestic, models: domestic },
          { key: "international", icon: REGION_ICONS.international, label: REGION_LABELS.international, models: international },
        ]
      : regionFilter === "domestic"
        ? [{ key: "domestic", icon: REGION_ICONS.domestic, label: REGION_LABELS.domestic, models: domestic }]
        : [{ key: "international", icon: REGION_ICONS.international, label: REGION_LABELS.international, models: international }];

  const curl = (model: string, region: string) =>
    `curl ${status.endpoint}/chat/completions \\\n  -H "Authorization: Bearer <访问密钥>" \\\n  -H "Content-Type: application/json" \\\n  -d '{"model":"${model}","region":"${region}","messages":[{"role":"user","content":"你好"}]}'`;

  return (
    <div className="min-w-0 space-y-4 p-4 sm:p-6">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h1 className="text-lg font-medium">GLM Plan 网关</h1>
          <p className="text-xs text-muted-foreground">
            GLM Coding Plan 订阅账号池 → OpenAI / Anthropic 兼容接口（国内版 BigModel / 国际版 Z.ai 双区，账号轮询 + 精确响应缓存 + CLI 身份拟真）
          </p>
        </div>
        <div className="flex items-center gap-2">
          <span className="text-xs text-muted-foreground">{status.enabled ? "已启用" : "已停用"}</span>
          <Switch
            checked={status.enabled}
            onCheckedChange={(checked) => void act("save_glmplan_config", { enabled: checked }, checked ? "已启用" : "已停用")}
            disabled={busy === "save_glmplan_config"}
          />
        </div>
      </div>

      {msg ? (
        <div className={"rounded-lg px-3 py-2 text-xs " + (msg.type === "ok" ? "bg-emerald-500/10 text-emerald-600" : "bg-destructive/10 text-destructive")}>
          {msg.text}
        </div>
      ) : null}

      <Section title="接入信息" description="Base URL 与访问密钥（国内版/国际版通用；用请求体 region 字段锁定区域版本）">
        <CardContent className="p-0">
          <div className="space-y-2 px-4 py-3.5 sm:px-5">
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-xs text-muted-foreground">Base URL</span>
              <code className="rounded bg-muted/70 px-2 py-1 text-xs">{status.endpoint}</code>
              <CopyButton text={status.endpoint} label="Base URL" />
              <span className="ml-2 text-xs text-muted-foreground">API Key</span>
              <code className="rounded bg-muted/70 px-2 py-1 text-xs">{status.apiKey}</code>
              <CopyButton text={status.apiKey} label="API Key" />
            </div>
            <div className="grid gap-2 sm:grid-cols-2">
              {(
                [
                  { region: "domestic", label: REGION_LABELS.domestic, icon: REGION_ICONS.domestic, model: status.defaultProvider === "zai" ? status.models[0] : status.models[0] },
                  { region: "international", label: REGION_LABELS.international, icon: REGION_ICONS.international, model: status.models[0] },
                ] as const
              ).map((b) => (
                <div key={b.region} className="rounded-lg border border-border/50 p-2.5">
                  <div className="flex items-center gap-1.5 text-[12px] font-medium">
                    {b.icon} {b.label}接入
                  </div>
                  <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
                    请求体加 <code className="rounded bg-muted/70 px-1">"region":"{b.region}"</code> 锁定{b.label}账号池
                  </p>
                  <pre className="mt-1.5 min-w-0 overflow-x-auto rounded bg-muted/60 p-2 text-[10px] leading-4">
                    {curl(b.model, b.region)}
                  </pre>
                  <div className="mt-1.5">
                    <CopyButton text={curl(b.model, b.region)} label={`${b.label}示例`} />
                  </div>
                </div>
              ))}
            </div>
          </div>
        </CardContent>
      </Section>

      <Section title="账号池" description="GLM Coding Plan 订阅账号；同区多账号自动轮询，401/403 自动冷却 10 分钟">
        <CardContent className="p-0">
          <div className="flex flex-wrap items-end gap-2 px-4 pt-3 sm:px-5">
            <div className="space-y-1">
              <p className="text-[10px] text-muted-foreground">区域版本</p>
              <Select value={newProvider} onValueChange={(v) => setNewProvider(v as "bigmodel" | "zai")}>
                <SelectTrigger className="w-36">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="bigmodel">🇨🇳 国内版（BigModel）</SelectItem>
                  <SelectItem value="zai">🌐 国际版（Z.ai）</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="min-w-0 flex-1 space-y-1">
              <p className="text-[10px] text-muted-foreground">备注名</p>
              <Input value={newLabel} onChange={(e) => setNewLabel(e.target.value)} placeholder="如：bigmodel 主号" />
            </div>
            <div className="min-w-0 flex-[1.4] space-y-1">
              <p className="text-[10px] text-muted-foreground">API Key（Coding Plan 的 zai-/bigmodel- 开头密钥）</p>
              <Input value={newKey} onChange={(e) => setNewKey(e.target.value)} placeholder="粘贴 API Key…" />
            </div>
            <Button
              size="sm"
              disabled={busy === "add_glmplan_account" || !newKey.trim()}
              onClick={() =>
                void act(
                  "add_glmplan_account",
                  { label: newLabel || "未命名", provider: newProvider, apiKey: newKey.trim() },
                  `已加入${PROVIDER_LABELS[newProvider]}账号池`,
                  () => {
                    setNewLabel("");
                    setNewKey("");
                  },
                )
              }
            >
              {busy === "add_glmplan_account" ? <Loader2 className="animate-spin" /> : <Plus />}加入账号池
            </Button>
          </div>
          <div className="flex flex-wrap items-center gap-1.5 px-4 pt-3 sm:px-5">
            {(["all", "domestic", "international"] as const).map((key) => (
              <button
                key={key}
                onClick={() => setRegionFilter(key)}
                className={
                  "h-7 rounded-full border px-3 text-[11px] transition-colors " +
                  (regionFilter === key
                    ? "border-primary bg-primary text-primary-foreground"
                    : "border-border/60 text-muted-foreground hover:bg-muted/60")
                }
              >
                {key === "all" ? "全部" : `${REGION_ICONS[key]} ${REGION_LABELS[key]}`}
              </button>
            ))}
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
                {g.models.length === 0 ? (
                  <span className="text-[11px] text-muted-foreground">暂无账号：在上方粘贴 API Key 加入</span>
                ) : null}
              </div>
              {g.models.map((a) => (
                <div key={a.id} className="flex flex-wrap items-center gap-2 border-b border-border/40 px-4 py-2 sm:px-5">
                  <span className="text-xs font-medium">{a.label}</span>
                  <Badge variant="outline" className="h-5 px-1.5 text-[10px]">
                    {REGION_ICONS[a.provider as keyof typeof REGION_ICONS]} {a.providerLabel}
                  </Badge>
                  <code className="text-[11px] text-muted-foreground">{a.apiKeyMasked}</code>
                  {a.cooling ? <Badge variant="warning" className="h-5 px-1.5 text-[10px]">冷却中</Badge> : null}
                  {a.enabled ? <Badge variant="secondary" className="h-5 px-1.5 text-[10px]">启用</Badge> : <Badge variant="destructive" className="h-5 px-1.5 text-[10px]">停用</Badge>}
                  {a.lastError ? <span className="min-w-0 flex-1 truncate text-[11px] text-destructive">{a.lastError}</span> : <span className="flex-1" />}
                  <Button
                    variant="outline"
                    size="sm"
                    className="h-6 px-2 text-[10px]"
                    disabled={busy === "enable_glmplan_account"}
                    onClick={() =>
                      void act("enable_glmplan_account", { id: a.id, enabled: !a.enabled }, a.enabled ? "已停用" : "已启用（清除冷却）")
                    }
                  >
                    {a.enabled ? "停用" : "启用"}
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    className="h-6 px-2 text-[10px]"
                    disabled={busy === "remove_glmplan_account"}
                    onClick={() => void act("remove_glmplan_account", { id: a.id }, "已移出账号池")}
                  >
                    <Trash2 />移除
                  </Button>
                </div>
              ))}
            </div>
          ))}
        </CardContent>
      </Section>

      <Section title="精确响应缓存" description="同语义请求命中缓存秒回且零消耗（规范化：model+messages+tools+采样参数）">
        <CardContent className="p-0">
          <div className="flex flex-wrap items-center gap-4 px-4 py-3.5 sm:px-5">
            <Switch
              checked={status.cache.enabled}
              onCheckedChange={(checked) => void act("save_glmplan_config", { cacheEnabled: checked }, checked ? "缓存已开启" : "缓存已关闭")}
            />
            <span className="text-xs text-muted-foreground">
              命中 {status.cache.hits} · 未命中 {status.cache.misses} · 当前条目 {status.cache.entries}
            </span>
          </div>
        </CardContent>
      </Section>

      <Section title="试一试" description="直接经 GLM Plan 网关发一条测试对话">
        <CardContent className="p-0">
          <div className="mx-4 space-y-2.5 py-3.5 sm:mx-5">
            <div className="flex flex-wrap items-center gap-2">
              <Select value={loadModel} onValueChange={setLoadModel}>
                <SelectTrigger className="w-44">
                  <SelectValue placeholder="选择模型" />
                </SelectTrigger>
                <SelectContent>
                  {status.models.map((m) => (
                    <SelectItem key={m} value={m}>
                      {m}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
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
            {!status.enabled ? <p className="text-xs text-destructive">网关已停用，请打开右上角开关。</p> : null}
            {status.accounts.length === 0 ? (
              <p className="text-xs text-amber-600">账号池为空：请先粘贴 Coding Plan 的 API Key 加入账号池。</p>
            ) : null}
          </div>
        </CardContent>
      </Section>

      <div className="flex justify-end px-1">
        <Button variant="ghost" size="sm" onClick={() => void refresh()} disabled={busy === "refresh"}>
          <RefreshCw />刷新状态
        </Button>
      </div>
    </div>
  );
}
