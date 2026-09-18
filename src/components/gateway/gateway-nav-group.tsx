import { useEffect, useMemo, useState } from "react";
import { ChevronDown, Network } from "lucide-react";
import { NavLink, useLocation } from "react-router-dom";

import * as api from "@/lib/api";
import type { GatewayInfo } from "@/lib/api";
import {
  GATEWAY_NAV_ENTRIES,
  isGatewayPath,
  type GatewayNavEntry,
} from "@/lib/gateway-registry";
import { cn } from "@/lib/utils";

const EXPAND_KEY = "sylvia.gateway-group.expanded";

function readStored(): boolean | null {
  try {
    const raw = localStorage.getItem(EXPAND_KEY);
    if (raw === "1") return true;
    if (raw === "0") return false;
  } catch {
    // localStorage 不可用（隐私模式等）时退化为默认行为
  }
  return null;
}

function writeStored(v: boolean) {
  try {
    localStorage.setItem(EXPAND_KEY, v ? "1" : "0");
  } catch {
    // 忽略写入失败
  }
}

/** 组内渲染行：注册表项 + 可选的后端总览状态（静态注册项可能不在总览里）。 */
type GatewayRow = GatewayNavEntry & { info?: GatewayInfo };

function HealthDot({ health }: { health: string }) {
  const cls =
    health === "ok"
      ? "bg-emerald-500"
      : health === "cooling"
        ? "bg-amber-500"
        : health === "error"
          ? "bg-destructive"
          : "bg-muted-foreground/40";
  return <span className={cn("size-1.5 shrink-0 rounded-full", cls)} />;
}

/** 行内的状态摘要：账号数优先，其次模型数；异常时显示错误提示。 */
function HealthNote({ info }: { info?: GatewayInfo }) {
  if (!info) return null;
  if (info.health === "error") {
    return (
      <span
        className="shrink-0 text-[10px] text-destructive"
        title={info.error ?? "网关异常"}
      >
        异常
      </span>
    );
  }
  return (
    <span className="shrink-0 text-[10px] text-muted-foreground">
      {info.accountCount > 0
        ? `${info.healthyAccounts}/${info.accountCount} 账号`
        : `${info.modelCount} 模型`}
    </span>
  );
}

/**
 * 侧栏「模型网关」统一分组容器。
 *
 * 把原先散落的各个网关导航项收拢到一个入口：默认保持在网关页或用户手动展开时展开，
 * 组内按注册表顺序展示全部网关，点击直接跳转到各自管理页 —— 跳转目标、
 * 路由与各网关内部逻辑完全不变。
 *
 * 组内状态摘要（健康 / 账号数 / 模型数）取自现有的 `/api/gateways/overview`，
 * 属纯展示增强：请求失败时静默退化为静态清单，不弹错、不影响导航。
 */
export function GatewayNavGroup() {
  const location = useLocation();
  const [overview, setOverview] = useState<GatewayInfo[]>([]);
  const stored = readStored();
  const [userOpen, setUserOpen] = useState<boolean | null>(stored);

  // 落在任一网关页时默认展开；否则尊重用户上次的选择（未选择则收起）
  const open = userOpen ?? isGatewayPath(location.pathname);

  useEffect(() => {
    let disposed = false;
    (async () => {
      try {
        const data = await api.getGatewaysOverview();
        if (!disposed) setOverview(data.gateways ?? []);
      } catch {
        // 静默降级：分组仍可正常导航，只是不显示状态摘要
      }
    })();
    return () => {
      disposed = true;
    };
  }, []);

  /** 注册表项（含后端状态）+ 后端返回但注册表未登记的新网关（自动归入）。 */
  const entries = useMemo<GatewayRow[]>(() => {
    const byPage = new Map(overview.map((g) => [g.page, g]));
    const list = GATEWAY_NAV_ENTRIES.map<GatewayRow>((entry) => {
      const info = byPage.get(entry.path);
      return {
        ...entry,
        label: info?.name || entry.label,
        info,
      };
    });
    const knownPages = new Set(GATEWAY_NAV_ENTRIES.map((g) => g.path));
    const extra = overview
      .filter((g) => !knownPages.has(g.page))
      .map<GatewayRow>((g) => ({
        id: g.id,
        path: g.page,
        label: g.name,
        icon: Network,
        info: g,
      }));
    return [...list, ...extra];
  }, [overview]);

  const infoByPath = useMemo(() => {
    const m = new Map<string, GatewayInfo>();
    for (const g of overview) m.set(g.page, g);
    return m;
  }, [overview]);

  const activeCount = entries.filter((g) => g.info?.enabled ?? true).length;

  return (
    <div className="mb-0.5">
      <button
        type="button"
        aria-expanded={open}
        aria-label="模型网关分组"
        onClick={() => {
          const next = !open;
          setUserOpen(next);
          writeStored(next);
        }}
        className={cn(
          "flex w-full items-center gap-2.5 rounded-xl px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
          isGatewayPath(location.pathname)
            ? "text-foreground"
            : "text-muted-foreground hover:bg-foreground/[0.045] hover:text-foreground",
        )}
      >
        <Network className="size-4 shrink-0" />
        <span className="min-w-0 flex-1 text-left font-medium">模型网关</span>
        <span className="shrink-0 text-[10px] text-muted-foreground">
          {entries.length}
        </span>
        <ChevronDown
          className={cn(
            "size-3.5 shrink-0 text-muted-foreground transition-transform",
            open && "rotate-180",
          )}
        />
      </button>

      {open ? (
        <div className="mt-0.5 space-y-0.5 pl-3">
          {entries.map((g) => {
            const Icon = g.icon;
            const info = infoByPath.get(g.path);
            return (
              <NavLink
                key={g.id + g.path}
                to={g.path}
                className={({ isActive }) =>
                  cn(
                    "flex items-center gap-2.5 rounded-xl px-3 py-2 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
                    isActive
                      ? "bg-primary/[0.13] font-medium text-foreground"
                      : "text-muted-foreground hover:bg-foreground/[0.045] hover:text-foreground",
                  )
                }
              >
                <Icon className="size-4 shrink-0" />
                <span className="min-w-0 flex-1 truncate">{g.label}</span>
                {info ? <HealthDot health={info.health} /> : null}
                <HealthNote info={info} />
              </NavLink>
            );
          })}
          {activeCount === 0 && overview.length > 0 ? (
            <p className="px-3 py-1 text-[10px] text-muted-foreground">
              全部网关均已停用
            </p>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
