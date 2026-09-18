import { useEffect, useMemo, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import {
  Activity,
  Check,
  ChevronDown,
  Coins,
  Cpu,
  ExternalLink,
  Loader2,
  Network,
  Search,
} from "lucide-react";
import { toast } from "sonner";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import * as api from "@/lib/api";
import type { GatewayInfo, GatewayModelInfo } from "@/lib/api";
import { cn } from "@/lib/utils";

/** 能力标签的中文名与配色。 */
const CAP_META: Record<string, { label: string; className: string }> = {
  text: { label: "文本", className: "bg-sky-500/10 text-sky-600" },
  reasoning: { label: "推理", className: "bg-violet-500/10 text-violet-600" },
  search: { label: "联网", className: "bg-emerald-500/10 text-emerald-600" },
  image: { label: "图像", className: "bg-amber-500/10 text-amber-600" },
  video: { label: "视频", className: "bg-rose-500/10 text-rose-600" },
  voice: { label: "语音", className: "bg-pink-500/10 text-pink-600" },
  tools: { label: "工具", className: "bg-teal-500/10 text-teal-600" },
};

function CapBadge({ cap }: { cap: string }) {
  const meta = CAP_META[cap];
  if (!meta) return null;
  return (
    <span
      className={cn(
        "rounded px-1 py-0.5 text-[10px] leading-none font-medium",
        meta.className,
      )}
    >
      {meta.label}
    </span>
  );
}

function HealthDot({ health }: { health: string }) {
  const cls =
    health === "ok"
      ? "bg-emerald-500"
      : health === "cooling"
        ? "bg-amber-500"
        : health === "error"
          ? "bg-destructive"
          : "bg-muted-foreground/40";
  return <span className={cn("size-2 shrink-0 rounded-full", cls)} />;
}

interface Props {
  className?: string;
}

/**
 * 统一网关下拉菜单：侧栏顶部常驻入口。
 * 浏览全部模型网关（GLM / Kimi / Qoder / Grok / Loomy）的健康状态、
 * 账号数、模型清单与能力标签；点击模型跳转到对应管理页。
 */
export function GatewaySwitcher({ className }: Props) {
  const [gateways, setGateways] = useState<GatewayInfo[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState(false);
  const navigate = useNavigate();
  const location = useLocation();

  async function load() {
    setLoading(true);
    try {
      const data = await api.getGatewaysOverview();
      setGateways(data.gateways ?? []);
    } catch (e) {
      toast.error("网关总览加载失败", { description: api.asError(e) });
      setGateways([]);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    if (open && gateways === null) void load();
  }, [open, gateways]);

  const filtered = useMemo(() => {
    const list = gateways ?? [];
    const q = query.trim().toLowerCase();
    if (!q) return list;
    return list
      .map((gw) => {
        // 搜索同时匹配网关名与模型 id
        const hitModels = gw.models.filter((m) =>
          (m.id ?? "").toLowerCase().includes(q),
        );
        if (gw.name.toLowerCase().includes(q) || gw.id.includes(q)) {
          return gw;
        }
        if (hitModels.length > 0) {
          return { ...gw, models: hitModels, modelCount: hitModels.length };
        }
        return null;
      })
      .filter((x): x is GatewayInfo => x !== null);
  }, [gateways, query]);

  const active = useMemo(
    () => gateways?.find((gw) => location.pathname.startsWith(gw.page)),
    [gateways, location.pathname],
  );

  const totalModels = useMemo(
    () => (gateways ?? []).reduce((sum, gw) => sum + gw.modelCount, 0),
    [gateways],
  );
  const enabledCount = useMemo(
    () => (gateways ?? []).filter((gw) => gw.enabled).length,
    [gateways],
  );

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          className={cn(
            "group flex w-full items-center gap-2.5 rounded-xl border border-sidebar-border/60 bg-sidebar-accent/30 px-3 py-2.5 text-left text-sm transition-colors hover:bg-sidebar-accent/60",
            className,
          )}
          aria-label="统一网关菜单"
        >
          {active ? (
            <>
              <Network className="size-4 shrink-0 text-emerald-600" />
              <span className="min-w-0 flex-1 truncate font-medium">
                {active.name}
              </span>
              <span className="shrink-0 text-[10px] text-muted-foreground">
                {active.modelCount} 模型
              </span>
            </>
          ) : (
            <>
              <Network className="size-4 shrink-0 text-emerald-600" />
              <span className="min-w-0 flex-1 truncate font-medium">
                模型网关
              </span>
              {loading ? (
                <Loader2 className="size-3.5 shrink-0 animate-spin text-muted-foreground" />
              ) : gateways ? (
                <span className="shrink-0 text-[10px] text-muted-foreground">
                  {enabledCount} 可用 / {totalModels} 模型
                </span>
              ) : (
                <span className="shrink-0 text-[10px] text-muted-foreground">
                  点击浏览
                </span>
              )}
            </>
          )}
          <ChevronDown className="size-3.5 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-180" />
        </button>
      </DropdownMenuTrigger>

      <DropdownMenuContent align="start" sideOffset={6} className="w-80 p-0">
        {/* 搜索框 */}
        <div className="flex items-center gap-2 border-b px-3 py-2">
          <Search className="size-3.5 shrink-0 text-muted-foreground" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="搜索网关或模型…"
            className="h-7 border-0 bg-transparent px-0 text-xs shadow-none focus-visible:ring-0"
          />
        </div>

        <div className="max-h-[420px] overflow-y-auto p-1">
          {gateways === null ? (
            <div className="flex items-center justify-center gap-2 py-8 text-xs text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" /> 加载中…
            </div>
          ) : filtered.length === 0 ? (
            <div className="px-3 py-8 text-center text-xs text-muted-foreground">
              {query ? `没有匹配「${query}」的网关或模型` : "尚未接入任何网关"}
            </div>
          ) : (
            filtered.map((gw) => (
              <div key={gw.id} className="mb-1">
                {/* 网关头：跳管理页 */}
                <DropdownMenuItem
                  className="gap-2 rounded-lg px-2.5 py-2"
                  onSelect={() => {
                    setOpen(false);
                    void navigate(gw.page);
                  }}
                >
                  <HealthDot health={gw.health} />
                  <span className="min-w-0 flex-1 truncate font-medium">
                    {gw.name}
                  </span>
                  {gw.health === "error" ? (
                    <span
                      className="shrink-0 text-[10px] text-destructive"
                      title={gw.error}
                    >
                      异常
                    </span>
                  ) : (
                    <span className="shrink-0 text-[10px] text-muted-foreground">
                      {gw.accountCount > 0
                        ? `${gw.healthyAccounts}/${gw.accountCount} 账号`
                        : `${gw.modelCount} 模型`}
                    </span>
                  )}
                  <ExternalLink className="size-3 shrink-0 opacity-40" />
                </DropdownMenuItem>

                {/* 模型清单（网关下展开，最多显示 6 个，其余提示去管理页看） */}
                {gw.models.length > 0 ? (
                  <div className="mt-0.5 mb-1 space-y-0.5">
                    {gw.models.slice(0, 6).map((m: GatewayModelInfo) => (
                      <DropdownMenuItem
                        key={(m.id ?? "") + String(m.name ?? "")}
                        className="gap-2 rounded-md py-1.5 pl-6"
                        onSelect={() => {
                          setOpen(false);
                          void navigate(gw.page);
                        }}
                      >
                        <Cpu className="size-3 shrink-0 text-muted-foreground/70" />
                        <span
                          className="min-w-0 flex-1 truncate text-xs"
                          title={m.id ?? ""}
                        >
                          {m.id ?? m.name}
                        </span>
                        <span className="flex shrink-0 gap-1">
                          {m.capabilities.slice(0, 3).map((cap) => (
                            <CapBadge key={cap} cap={cap} />
                          ))}
                        </span>
                      </DropdownMenuItem>
                    ))}
                    {gw.models.length > 6 ? (
                      <div className="pl-6 text-[10px] text-muted-foreground">
                        还有 {gw.models.length - 6} 个模型，点击网关名查看全部
                      </div>
                    ) : null}
                  </div>
                ) : gw.health === "error" ? (
                  <div
                    className="mb-1 ml-6 truncate rounded-md bg-destructive/5 px-2 py-1 text-[10px] text-destructive"
                    title={gw.error}
                  >
                    {gw.error ?? "网关异常"}
                  </div>
                ) : null}
              </div>
            ))
          )}
        </div>

        <DropdownMenuSeparator className="my-1" />
        <DropdownMenuItem
          className="gap-2 text-xs"
          onSelect={() => {
            setOpen(false);
            void navigate("/stats?tab=model");
          }}
        >
          <Coins className="size-3.5 shrink-0" />
          <span className="min-w-0 flex-1">模型用量与积分统计</span>
          <Activity className="size-3 shrink-0 opacity-40" />
        </DropdownMenuItem>

        {gateways !== null && !loading ? (
          <div className="flex items-center justify-between border-t px-3 py-1.5 text-[10px] text-muted-foreground">
            <span>
              {enabledCount} 个网关可用 · 共 {totalModels} 个模型
            </span>
            <button
              type="button"
              className="flex items-center gap-1 hover:text-foreground"
              onClick={(e) => {
                e.preventDefault();
                void load();
              }}
            >
              <Check className={cn("size-3", loading && "hidden")} />
              刷新
            </button>
          </div>
        ) : null}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
