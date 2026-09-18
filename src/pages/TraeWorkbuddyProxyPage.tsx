import { useCallback, useEffect, useState } from "react";
import { Brain, CircleAlert, Loader2, RefreshCw } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import * as api from "@/lib/api";
import type { TraeProxyModel, WorkbuddyProxyStatus } from "@/lib/api";

function RateTag({ rate }: { rate?: number | null }) {
  if (typeof rate !== "number") return null;
  return (
    <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px] tabular-nums">
      倍率 x{rate}
    </Badge>
  );
}

function TraeModelRow({ model }: { model: TraeProxyModel }) {
  return (
    <div className="flex min-w-0 items-start gap-3 px-4 py-2.5 sm:px-5">
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          <span className="truncate font-mono text-[12px] font-medium">{model.id}</span>
          {model.display_name && model.display_name !== model.id ? (
            <span className="truncate text-[11px] text-muted-foreground">{model.display_name}</span>
          ) : null}
          {model.reasoning ? (
            <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">
              <Brain className="mr-0.5 size-2.5" />
              推理
            </Badge>
          ) : null}
          {model.multimodal ? (
            <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">多模态</Badge>
          ) : null}
        </div>
        <p className="mt-0.5 text-[11px] text-muted-foreground">
          场景 {model.scene || "—"}
          {typeof model.context_max === "number" ? ` · 上下文 ${model.context_max}` : ""}
        </p>
      </div>
      <RateTag rate={model.rate} />
    </div>
  );
}

export default function TraeWorkbuddyProxyPage() {
  const [traeStatus, setTraeStatus] = useState<api.TraeProxyStatus | null>(null);
  const [traeModels, setTraeModels] = useState<TraeProxyModel[]>([]);
  const [traeError, setTraeError] = useState<string | null>(null);
  const [wbStatus, setWbStatus] = useState<WorkbuddyProxyStatus | null>(null);
  const [wbModels, setWbModels] = useState<WorkbuddyProxyStatus["providers"] | null>(null);
  const [wbError, setWbError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    // TRAE
    api.getTraeProxyStatus().then(setTraeStatus).catch(() => setTraeStatus(null));
    api
      .getTraeProxyModels()
      .then((res) => {
        if (res.ok) {
          setTraeModels(res.models ?? []);
          setTraeError(null);
        } else {
          setTraeError(res.error || "未知错误");
        }
      })
      .catch((e) => setTraeError(api.asError(e)));
    // WorkBuddy
    api.getWorkbuddyProxyStatus().then(setWbStatus).catch(() => setWbStatus(null));
    api
      .getWorkbuddyProxyModels()
      .then((res) => {
        if (res.ok) {
          setWbModels(res.providers ?? []);
          setWbError(null);
        } else {
          setWbError(res.error || "未知错误");
        }
      })
      .catch((e) => setWbError(api.asError(e)));
    setLoading(false);
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const traeCount = traeModels.length;
  const wbCount =
    wbModels?.reduce((acc, p) => acc + (p.models?.length ?? 0), 0) ?? 0;

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-8 flex min-w-0 flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">TRAE / WorkBuddy 模型</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            TRAE Work CN 的模型目录（本地客户端缓存 + 积分倍率）与 WorkBuddy 的模型代理
            （借道本机 OpenCode server，复用其已鉴权 provider 转发对话）。
          </p>
        </div>
        <Button className="shrink-0" variant="outline" size="sm" onClick={() => void load()} disabled={loading}>
          {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
          刷新
        </Button>
      </header>

      {/* TRAE */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="TRAE Work CN">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">
            TRAE Work CN · {traeCount} 个模型
          </h2>
          <span className="text-xs text-muted-foreground">
            凭证：
            {traeStatus?.credential.ok ? (
              <span className="text-foreground">已登录（{traeStatus.credential.user}）</span>
            ) : (
              <span className="text-destructive">{traeStatus?.credential.error || "未登录"}</span>
            )}
          </span>
        </div>
        <Alert variant="default" className="mb-1">
          <CircleAlert />
          <AlertTitle>对话转发暂未开放</AlertTitle>
          <AlertDescription>
            TRAE 的对话接口要求先建立客户端会话（CreateChatSession），单发请求会返回
            「all models failed」。当前提供模型目录与倍率查询；会话栈打通后开放转发。
          </AlertDescription>
        </Alert>
        {traeError ? (
          <Alert variant="warning">
            <CircleAlert />
            <AlertTitle>模型目录读取失败</AlertTitle>
            <AlertDescription>{traeError}</AlertDescription>
          </Alert>
        ) : (
          <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
            <div className="divide-y divide-border/50">
              {traeModels.length === 0 ? (
                <p className="px-4 py-6 text-center text-sm text-muted-foreground">
                  暂无模型数据（打开 TRAE 客户端一次后会同步模型目录缓存）。
                </p>
              ) : (
                traeModels.map((model) => <TraeModelRow key={model.id} model={model} />)
              )}
            </div>
          </Card>
        )}
      </section>

      {/* WorkBuddy */}
      <section className="min-w-0 space-y-2.5" aria-label="WorkBuddy">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-2 px-1">
          <h2 className="text-[13px] font-medium leading-5">
            WorkBuddy · {wbCount} 个模型
          </h2>
          <span className="text-xs text-muted-foreground">
            OpenCode server：
            {wbStatus?.server.ok ? (
              <span className="text-foreground">已连接（端口 {wbStatus.server.port}）</span>
            ) : (
              <span className="text-destructive">{wbStatus?.server.error || "未发现"}</span>
            )}
          </span>
        </div>
        <p className="px-1 text-[11px] leading-5 text-muted-foreground">
          对话端点 <code className="rounded bg-muted px-1">POST /workbuddy/v1/chat/completions</code>，
          模型 ID 形如 <code className="rounded bg-muted px-1">deepseek/deepseek-v4-flash</code>（provider/model）；
          借道本机 OpenCode server（WorkBuddy 内嵌），复用其已鉴权 provider，按 WorkBuddy 订阅计费。
        </p>
        {wbError ? (
          <Alert variant="warning">
            <CircleAlert />
            <AlertTitle>模型目录读取失败</AlertTitle>
            <AlertDescription>{wbError}</AlertDescription>
          </Alert>
        ) : (
          <div className="grid min-w-0 gap-3 sm:grid-cols-2">
            {(wbModels ?? []).map((provider) => (
              <Card key={provider.id} className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
                <CardContent className="p-0">
                  <div className="flex items-center justify-between border-b border-border/50 px-4 py-2.5">
                    <span className="truncate text-[13px] font-medium">{provider.id}</span>
                    <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">
                      {provider.models?.length ?? 0} 模型
                    </Badge>
                  </div>
                  <div className="max-h-52 overflow-y-auto">
                    {(provider.models ?? []).map((model) => (
                      <div key={model.id} className="flex min-w-0 items-center gap-2 px-4 py-1.5 text-[12px]">
                        <span className="min-w-0 flex-1 truncate font-mono">{model.id}</span>
                        {model.reasoning ? <Brain className="size-3 shrink-0 text-muted-foreground" /> : null}
                        {typeof model.context === "number" ? (
                          <span className="shrink-0 text-[10px] tabular-nums text-muted-foreground">
                            {model.context}
                          </span>
                        ) : null}
                      </div>
                    ))}
                  </div>
                </CardContent>
              </Card>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}
