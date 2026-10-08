import {
  Blocks,
  Bot,
  Cat,
  Cpu,
  Globe,
  MessagesSquare,
  Network,
  PlugZap,
  Sparkles,
  Terminal,
  Layers,
  Zap,
  type LucideIcon,
} from "lucide-react";

/**
 * 模型网关统一分组容器的数据源（纯展示层，不涉及任何请求 / 配置逻辑）。
 *
 * 这里是侧栏「模型网关」分组的唯一数据源：顺序即展示顺序，图标与命名沿用原有风格。
 * 各网关的 API 地址、Key、请求参数、路由逻辑一律不变，仍由各自的路由与管理页负责。
 *
 * 新增网关只需两步：
 *   1. 在 `GATEWAY_NAV_ENTRIES` 追加一条（顺序 = 侧栏展示位置）；
 *   2. 在 `App.tsx` 的 `Routes` 里注册同路径的路由。
 * 若后端 `/api/gateways/overview` 先上线了新网关且 `page` 已注册路由，
 * 分组会自动把它并入（见 gateway-nav-group 的合并逻辑）。
 */
export type GatewayNavEntry = {
  /** 与后端总览 GatewayInfo.id 对齐（glm / kimi / qoder …），用于状态富化 */
  id: string;
  /** 前端管理页路由，必须同时在 App.tsx 的 Routes 中注册 */
  path: string;
  label: string;
  icon: LucideIcon;
};

export const GATEWAY_NAV_ENTRIES: GatewayNavEntry[] = [
  { id: "glm", path: "/glm-proxy", label: "GLM 代理", icon: PlugZap },
  { id: "glmplan", path: "/glm-plan", label: "GLM Plan", icon: Layers },
  { id: "minimax", path: "/minimax", label: "MiniMax", icon: Bot },
  { id: "qwen", path: "/qwen", label: "Qwen 通道", icon: Globe },
  { id: "qoder", path: "/qoder", label: "Qoder 网关", icon: Zap },
  { id: "kimi", path: "/kimi", label: "Kimi 网关", icon: MessagesSquare },
  { id: "glmweb", path: "/glmweb", label: "ChatGLM 网关", icon: Globe },
  { id: "grok", path: "/grok", label: "Grok 网关", icon: Sparkles },
  { id: "loomy", path: "/loomy", label: "Loomy", icon: Sparkles },
  // 妙手（美团智能体）：原为侧栏独立导航项，2026-09-26 并入分组消除重复入口
  // （此前靠后端总览自动合并进分组，用的是兜底图标；显式登记后用回 Cat 图标）。
  { id: "miaoshou", path: "/miaoshou", label: "妙手", icon: Cat },
  // ── 2026-09-22 三拆：原先合并为一条「TRAE/WB 模型」，现按上游来源拆成三个相互独立的网关 ──
  { id: "trae-work-cn", path: "/trae-work-cn", label: "TRAE Work CN", icon: Blocks },
  { id: "workbuddy", path: "/workbuddy", label: "WorkBuddy", icon: Cpu },
  { id: "opencode", path: "/opencode", label: "OpenCode", icon: Terminal },
  // DuMate（百度搭子）：2026-10-01 新增渠道（契约 docs/dumate-channel-contract.md §4.3）
  { id: "dumate", path: "/dumate", label: "DuMate（百度搭子）", icon: Bot },
  // ZCode 账号池：2026-10-02 新增渠道（契约 docs/zcode-channel-contract.md §3）
  { id: "zcode", path: "/zcode", label: "ZCode 账号", icon: Bot },
  // AutoClaw 国际池：2026-10-07 新增渠道（契约 docs/autoclaw-intl-pool-contract.md §5）
  { id: "autoclaw-intl", path: "/autoclaw-intl", label: "AutoClaw 国际池", icon: Globe },
];

/** 分组里未登记、但后端总览已返回且路由可达时的兜底图标。 */
export const FALLBACK_GATEWAY_ICON = Network;

/** 当前路径是否落在某个模型网关管理页上。 */
export function isGatewayPath(pathname: string): boolean {
  return GATEWAY_NAV_ENTRIES.some((g) => pathname.startsWith(g.path));
}
