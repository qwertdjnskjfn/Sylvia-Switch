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
 *
 * 🔴 2026-09-25（native 多模型改造，docs/workbuddy-native-multimodel-contract.md §五）：
 *    「接入信息 · WorkBuddy 内置模型」区升级为完整 ACP 探测目录：name 中文名 + id(mono) +
 *    倍率徽标 + kind 徽标（档位/模型/自定义；custom 仅 ?include=custom 下发，本页默认不拉），
 *    currentModelId 标「默认」；目录来源徽标（acp-live=实时探测 / acp-cache=缓存（N 分钟前）/
 *    static-fallback=桌面端未运行·静态兜底，黄色警示）+ 计数文案 + 兜底说明一行。
 *    内置模型占位统一取 currentModelId ?? 目录首条 id ?? deepseek-v4.1-flash。
 *
 * 🔴 2026-09-25（接入信息改造，docs/workbuddy-auth-catalog-test-contract.md §3.2）：
 *    ④ 内置模型区升级为**完整表格**：数据拉 GET /workbuddy-native/v1/models?include=custom
 *    （api.ts 既有封装不带 include 参数 → 本页内用同源相对路径 fetch，回落既有封装），
 *    按 kind 三段分组（档位/内置实模/自定义接入），列 = 名称 / id / 类型徽章 / 倍率
 *    （null → —，tooltip「上游未提供倍率」）/ 来源徽章（exposed=true「ACP 实时」；
 *    false「隐藏」+ tooltip「ACP session/new 暂未暴露，来自客户端产品配置，可调用性以实测为准」）/
 *    操作「测试」→ /model-test?gateway=workbuddy-native&model=<id>。
 *    新增「刷新目录」按钮（api.refreshWorkbuddyCatalog()，绕 300s TTL，成功后重渲染 +
 *    展示刷新时间）；探测失败 / 空目录给空态文案 + 重试。
 *    ⑤ 接入密钥改为**掩码展示**（默认露前后各 8 位，眼睛切换明文）+ 复制（成功 toast）；
 *    文案注明亦接受网关密钥 gw_live_*（仅作用于 /workbuddy/v1；native 端点无鉴权）。
 *
 * 🔴 2026-09-25（池区域隔离，docs/workbuddy-pool-region-contract.md §二-T6 / §3.4 / §3.5）：
 *    「多账号网关（cloud）」区块按账号区域分区展示：
 *    ① 新增池模型目录区（GET /workbuddy-pool/v1/models）：按「国内(cn)/国际(intl)」两组
 *    展示（组标题 + 条数 + 来源徽标 动态/静态/回落，来自响应 regions 元信息）；单区缺失给
 *    空态说明（如「国际区当前为静态表（池内无国际账号）」）；旧后端条目无 region 字段 →
 *    分组退化为单一组（可选字段容错）。
 *    ② 总览卡加「区域概况」一行（status.regions 聚合：cn/intl 的 accounts/healthy/models；
 *    旧后端缺省不渲染该行）。
 *    ③ 账号表 region 徽标统一「国内/国际」文案（poolRegionLabel，数据在 accounts[].region，
 *    types.ts 已有字段）。
 *    ④ 接入信息补 curl 调用示例，注明 chat body 可选 region:"cn"|"intl" 字段（契约 T2）。
 *    ⚠️ status 仍是扁平响应（:6030 注释红线）：setPoolStatus(res) 直接读顶层字段，
 *    regions 同为顶层可选字段，不按嵌套信封读取。
 */

import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { toast } from "sonner";
import {
  BarChart3,
  Boxes,
  CircleAlert,
  Cloud,
  Copy,
  Eye,
  EyeOff,
  FlaskConical,
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
import type { WorkbuddyGatewayConfig } from "@/lib/types";
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

/** ms epoch → 当日时分秒（目录刷新时间展示用）；0/缺省 = 空串。 */
function fmtClock(ms: number | null | undefined): string {
  if (!ms || ms <= 0) return "";
  return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
}

/** 池区域 → 中文标签（契约 T6：cn=国内 / intl=国际；其余值原样透出）。
 *  账号表徽标与模型目录分组共用，保证两处文案统一。 */
function poolRegionLabel(region: string): string {
  if (region === "cn") return "国内";
  if (region === "intl") return "国际";
  return region;
}

/** 池模型目录来源徽标（契约 T6 / §3.4 regions.source）：dynamic=动态 / static=静态 /
 *  fallback=回落（后两者黄色警示——静态表与内置表均为人工维护的兜底目录）；
 *  元信息缺省（旧后端）→ 不渲染。 */
function PoolCatalogSourceBadge({ source }: { source?: api.WorkbuddyPoolRegionCatalogMeta["source"] }) {
  if (source === "dynamic") {
    return (
      <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">
        动态
      </Badge>
    );
  }
  if (source === "static") {
    return (
      <Badge variant="warning" className="shrink-0 px-1.5 py-0 text-[10px]">
        静态
      </Badge>
    );
  }
  if (source === "fallback") {
    return (
      <Badge variant="warning" className="shrink-0 px-1.5 py-0 text-[10px]">
        回落
      </Badge>
    );
  }
  return null;
}

/** 池模型目录分组（契约 T6）：组标题 + 条数 + 来源徽标 + 模型 id 列表
 *  （mono chips，同区响应已去重；note 用于静态/回落的空态与兜底说明）。 */
/** 池模型分组表（2026-09-25 升级为与「内置模型」同规格的六列表格：名称/id/类型/倍率/来源/操作）。
 *  2026-09-26：上游 /v3/config 的 name/credits 已透传——名称列优先显示名，倍率列读
 *  multiplier（未标价/缺失仍显示「—」）；「测试」直达模型测试页并预选该网关+模型+区域。 */
function PoolModelGroup({
  title,
  entries,
  source,
  note,
  region,
}: {
  title: string;
  entries: api.WorkbuddyPoolModelEntry[];
  source?: api.WorkbuddyPoolRegionCatalogMeta["source"];
  note?: string;
  region: "cn" | "intl";
}) {
  const navigate = useNavigate();
  return (
    <div className="py-2.5">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs font-medium">{title}</span>
        <span className="text-[11px] text-muted-foreground">（{entries.length}）</span>
        <PoolCatalogSourceBadge source={source} />
      </div>
      {note ? <p className="mt-1 text-[11px] leading-5 text-muted-foreground">{note}</p> : null}
      <div className="min-w-0 overflow-x-auto">
        <table className="w-full min-w-[620px] text-xs">
          <thead>
            <tr className="text-left text-muted-foreground">
              <th className="py-1.5 pr-3 font-medium">名称</th>
              <th className="py-1.5 pr-3 font-medium">id</th>
              <th className="py-1.5 pr-3 font-medium">类型</th>
              <th className="py-1.5 pr-3 font-medium">倍率</th>
              <th className="py-1.5 pr-3 font-medium">来源</th>
              <th className="py-1.5 font-medium">操作</th>
            </tr>
          </thead>
          <tbody>
            {entries.map((m) => (
              <tr key={`${region}:${m.id}`} className="border-t border-border/40">
                <td
                  className="max-w-[220px] truncate py-1.5 pr-3"
                  title={
                    m.context_length
                      ? `${m.name || m.id} · 上下文 ${m.context_length.toLocaleString()} tokens`
                      : m.name || m.id
                  }
                >
                  {m.name || m.id}
                </td>
                <td className="py-1.5 pr-3">
                  <code className="rounded bg-muted/70 px-1.5 py-0.5 font-mono text-[11px]">{m.id}</code>
                </td>
                <td className="py-1.5 pr-3">
                  <Badge variant="outline" className="h-5 px-1.5 text-[10px]">
                    云端模型
                  </Badge>
                </td>
                <td className="py-1.5 pr-3">
                  {m.multiplier != null ? (
                    <Badge variant="secondary" className="shrink-0 px-1.5 py-0 font-mono text-[10px]">
                      x{m.multiplier}
                    </Badge>
                  ) : (
                    <span className="text-muted-foreground" title="上游未提供倍率">
                      —
                    </span>
                  )}
                </td>
                <td className="py-1.5 pr-3">
                  <PoolCatalogSourceBadge source={source} />
                </td>
                <td className="py-1.5">
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-6 px-2 text-xs"
                    onClick={() =>
                      navigate(
                        `/model-test?gateway=workbuddy-pool&model=${encodeURIComponent(m.id)}&region=${region}`,
                      )
                    }
                  >
                    测试
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

/** 访问密钥掩码展示（契约 §3.2）：默认只露前后各 8 位、中间打码，眼睛切换明文；
 *  过短密钥整体打码（掩码不回显长度信息）。 */
function maskApiKey(key: string): string {
  if (key.length <= 16) return "•".repeat(Math.max(8, key.length));
  return `${key.slice(0, 8)}••••••••${key.slice(-8)}`;
}

/** 内置模型表格行（契约 §3.2）：以 refreshWorkbuddyCatalog 的 WbCatalogEntry 为准；
 *  旧后端 / 静态兜底数据缺 exposed（来源徽章不渲染，如实留空）、name/kind 可空 →
 *  归一为本页视图类型，两种数据源（GET models / POST refresh）统一走 toCatalogRow。 */
type WbCatalogRow = {
  id: string;
  name: string | null;
  multiplier: number | null;
  kind: "tier" | "model" | "custom" | null;
  exposed?: boolean;
  owned_by: string;
};

/** 后端目录条目 → 表格行（GET /workbuddy-native/v1/models 与 refresh 响应统一归一）。 */
function toCatalogRow(m: {
  id: string;
  name?: string | null;
  multiplier?: number | null;
  kind?: "tier" | "model" | "custom" | null;
  exposed?: boolean;
  owned_by?: string;
}): WbCatalogRow {
  return {
    id: m.id,
    name: m.name ?? null,
    multiplier: m.multiplier ?? null,
    kind: m.kind ?? null,
    exposed: m.exposed,
    owned_by: m.owned_by ?? "workbuddy",
  };
}

/** 目录全量拉取（契约 §3.2）：GET /workbuddy-native/v1/models?include=custom。
 *  api.ts 既有封装 getWorkbuddyNativeModels 不带 include 参数 → 按任务口径用
 *  同源相对路径 fetch（与 httpCall 的 webui 取数方式一致，API_BASE=""）；
 *  调用方（loadNativeCatalog）在失败时回落既有封装。 */
async function fetchNativeCatalogFull(): Promise<{
  source: api.WorkbuddyNativeModelsSource | null;
  probedAt: number | null;
  currentModelId: string | null;
  region: string | null;
  data: WbCatalogRow[];
}> {
  const res = await fetch("/workbuddy-native/v1/models?include=custom");
  const payload = (await res.json().catch(() => null)) as
    | {
        source?: api.WorkbuddyNativeModelsSource | null;
        probedAt?: number | null;
        currentModelId?: string | null;
        region?: string | null;
        data?: Parameters<typeof toCatalogRow>[0][];
        message?: string;
      }
    | null;
  if (!res.ok) {
    throw new Error(payload?.message || `目录请求失败 (${res.status})`);
  }
  return {
    source: payload?.source ?? null,
    probedAt: payload?.probedAt ?? null,
    currentModelId: payload?.currentModelId ?? null,
    region: payload?.region ?? null,
    data: (payload?.data ?? []).map(toCatalogRow),
  };
}

/** 目录分组（契约 §3.2）：按 kind 三段展示（档位 / 内置实模 / 自定义接入）；空组不渲染。 */
const CATALOG_GROUPS: { kind: NonNullable<WbCatalogRow["kind"]>; title: string }[] = [
  { kind: "tier", title: "档位" },
  { kind: "model", title: "内置实模" },
  { kind: "custom", title: "自定义接入" },
];

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
          // 清空失焦不提交（避免误写；后端对 <=0 多数字段按「走默认」处理，
          // 仅「余额巡检间隔」的 0 = 关闭巡检（core 侧 workbuddy_gateway.rs normalized））
          if (e.target.value.trim() === "") return;
          const v = Number(e.target.value);
          if (Number.isFinite(v) && v !== value) onCommit(v);
        }}
        className="mt-1 h-8 text-xs"
      />
    </div>
  );
}

/** 内置模型 kind → 中文徽标文案（契约 §五）；旧后端缺省（undefined）→ 不显示。 */
function nativeKindLabel(kind: api.WorkbuddyNativeModel["kind"]): string | null {
  switch (kind) {
    case "tier":
      return "档位";
    case "model":
      return "模型";
    case "custom":
      return "自定义";
    default:
      return null;
  }
}

/** 内置模型目录表格行（契约 §3.2）：列 = 名称 + id(mono) + 类型徽章 + 倍率
 *  （null → —，title「上游未提供倍率」）+ 来源徽章（exposed=true「ACP 实时」；
 *  false「隐藏」+ title「ACP session/new 暂未暴露，来自客户端产品配置，可调用性以实测为准」；
 *  旧后端缺 exposed → 留空）+ 操作「测试」→ /model-test 预选该模型。
 *  会话默认模型（currentModelId）在名称列标「默认」。 */
function CatalogTableRow({
  model,
  isDefault,
  onTest,
}: {
  model: WbCatalogRow;
  isDefault: boolean;
  onTest: (id: string) => void;
}) {
  const kindLabel = nativeKindLabel(model.kind);
  const mult =
    model.multiplier != null && Number.isFinite(model.multiplier)
      ? `x${model.multiplier}`
      : null;
  return (
    <tr className="hover:bg-muted/30">
      <td className="max-w-[220px] px-4 py-2 sm:px-5">
        <span className="block truncate text-[13px] font-medium leading-5" title={model.name || model.id}>
          {model.name || model.id}
        </span>
      </td>
      <td className="max-w-[210px] px-2 py-2">
        <code
          className="block truncate rounded bg-muted/70 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground"
          title={model.id}
        >
          {model.id}
        </code>
      </td>
      <td className="px-2 py-2">
        <div className="flex flex-wrap items-center gap-1">
          {isDefault ? (
            <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">
              默认
            </Badge>
          ) : null}
          {kindLabel ? (
            <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">
              {kindLabel}
            </Badge>
          ) : (
            <span className="text-muted-foreground">—</span>
          )}
        </div>
      </td>
      <td className="px-2 py-2">
        {mult ? (
          <Badge variant="secondary" className="shrink-0 px-1.5 py-0 font-mono text-[10px]">
            {mult}
          </Badge>
        ) : (
          <span className="text-muted-foreground" title="上游未提供倍率">
            —
          </span>
        )}
      </td>
      <td className="px-2 py-2">
        {model.exposed === true ? (
          <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">
            ACP 实时
          </Badge>
        ) : model.exposed === false ? (
          <Badge
            variant="warning"
            className="cursor-help shrink-0 px-1.5 py-0 text-[10px]"
            title="ACP session/new 暂未暴露，来自客户端产品配置，可调用性以实测为准"
          >
            隐藏
          </Badge>
        ) : (
          <span className="text-muted-foreground">—</span>
        )}
      </td>
      <td className="px-4 py-2 text-right sm:px-5">
        <Button
          size="sm"
          variant="outline"
          className="h-7 gap-1 px-2.5 text-xs"
          onClick={() => onTest(model.id)}
        >
          <FlaskConical className="size-3.5" />
          测试
        </Button>
      </td>
    </tr>
  );
}

/** 目录来源徽标（契约 §五）：acp-live=实时探测、acp-cache=缓存（N 分钟前）、
 *  static-fallback=桌面端未运行·静态兜底（黄色警示）；旧后端缺省 → 不渲染。 */
function NativeSourceBadge({
  source,
  probedAt,
}: {
  source: api.WorkbuddyNativeModelsSource | null;
  probedAt: number | null;
}) {
  if (source === "acp-live") {
    return (
      <Badge variant="success" className="shrink-0 px-1.5 py-0 text-[10px]">
        实时探测
      </Badge>
    );
  }
  if (source === "acp-cache") {
    const min =
      probedAt != null ? Math.max(0, Math.round((Date.now() - probedAt) / 60000)) : null;
    return (
      <Badge variant="secondary" className="shrink-0 px-1.5 py-0 text-[10px]">
        {min != null ? `缓存（${min} 分钟前）` : "缓存"}
      </Badge>
    );
  }
  if (source === "static-fallback") {
    return (
      <Badge variant="warning" className="shrink-0 px-1.5 py-0 text-[10px]">
        桌面端未运行·静态兜底
      </Badge>
    );
  }
  return null;
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

/** 访问密钥复制按钮（契约 §3.2）：navigator.clipboard 写入 + 成功 toast
 *  （与共享 CopyButton 同观感，额外给 toast 反馈——接入密钥卡片专用）。 */
function CopyKeyButton({ text }: { text: string }) {
  return (
    <Button
      size="icon"
      variant="ghost"
      className="size-7"
      aria-label="复制访问密钥"
      onClick={() => {
        void navigator.clipboard.writeText(text).then(
          () => toast.success("访问密钥已复制"),
          () => toast.error("复制失败，请手动选择复制"),
        );
      }}
    >
      <Copy className="size-3.5" />
    </Button>
  );
}

/** WorkBuddy 池区域专属接入密钥卡（2026-09-25）：国内版 / 国际版各一张。
 *  首次进入自动创建该区专用密钥；掩码展示 + 眼睛切换明文 + 复制。
 *  密钥与请求体 region 绑定：跨区调用被池网关拒绝，杜绝「不知道连的哪个版本」。 */
function RegionKeyCard({ region, label }: { region: "cn" | "intl"; label: string }) {
  const [keyText, setKeyText] = useState<string | null>(null);
  const [reveal, setReveal] = useState(false);
  const [busy, setBusy] = useState(true);
  const [loadFailed, setLoadFailed] = useState(false);

  const ensure = useCallback(async (create: boolean) => {
    setBusy(true);
    setLoadFailed(false);
    try {
      const res = await api.getWorkbuddyPoolRegionKey(region, create);
      setKeyText(res.key?.key ?? null);
      if (!res.key) setLoadFailed(true);
    } catch (e) {
      setLoadFailed(true);
      toast.error(`${label}密钥获取失败`, { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }, [region, label]);

  useEffect(() => {
    // 进入页面即确保存在（create=true：没有就建一把，用户无感）
    void ensure(true);
  }, [ensure]);

  const masked = keyText
    ? `${keyText.slice(0, 12)}${"•".repeat(10)}${keyText.slice(-4)}`
    : "—";

  return (
    <InfoRow label={label} hint="该区专用密钥：仅接受归属本区的调用请求（请求体 region 与该区一致）；与另一区密钥互不串用">
      <div className="flex flex-wrap items-center gap-1.5">
        {busy ? (
          <span className="text-xs text-muted-foreground">
            <Loader2 className="mr-1 inline size-3 animate-align-middle animate-spin" />
            读取中…
          </span>
        ) : loadFailed || !keyText ? (
          <span className="text-xs text-muted-foreground">未获取到密钥</span>
        ) : (
          <code className="max-w-[320px] truncate rounded bg-muted/70 px-2 py-1 text-xs">
            {reveal ? keyText : masked}
          </code>
        )}
        {keyText ? (
          <>
            <Button
              size="icon"
              variant="ghost"
              className="size-7"
              aria-label={reveal ? "隐藏密钥" : "显示密钥"}
              onClick={() => setReveal((v) => !v)}
            >
              {reveal ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
            </Button>
            <CopyKeyButton text={keyText} />
          </>
        ) : (
          <Button
            size="sm"
            variant="outline"
            disabled={busy}
            onClick={() => void ensure(true)}
          >
            <RefreshCw className="mr-1.5 size-3" />
            重试获取
          </Button>
        )}
        {keyText ? <Badge variant="outline" className="ml-1">{region === "cn" ? "国内版" : "国际版"}</Badge> : null}
      </div>
    </InfoRow>
  );
}

export default function WorkbuddyPage() {
  const navigate = useNavigate();
  const [wbStatus, setWbStatus] = useState<WorkbuddyProxyStatus | null>(null);
  const [wbCfg, setWbCfg] = useState<api.WorkbuddyProxyConfigInfo | null>(null);
  // 内置模型目录（契约 §3.2）：?include=custom 全量（tier/model/custom），refresh 后整体替换。
  const [nativeModels, setNativeModels] = useState<WbCatalogRow[]>([]);
  // 目录元信息：source/probedAt/currentModelId/region（旧后端缺省 → null，徽标不渲染）。
  // region = 桌面端登录区域（cn=国内版 / intl=国外版），2026-09-25 区域分类。
  const [nativeMeta, setNativeMeta] = useState<{
    source: api.WorkbuddyNativeModelsSource | null;
    probedAt: number | null;
    currentModelId: string | null;
    region: string | null;
  }>({ source: null, probedAt: null, currentModelId: null, region: null });
  // 目录探测失败信息（契约 §3.2 空态）：非空 → 失败文案 + 重试按钮。
  const [nativeError, setNativeError] = useState<string | null>(null);
  const [nativeLoading, setNativeLoading] = useState(false);
  // 「刷新目录」按钮（契约 §3.2）：loading 态 + 成功后的刷新时刻（ms epoch）。
  const [refreshingCatalog, setRefreshingCatalog] = useState(false);
  const [catalogRefreshedAt, setCatalogRefreshedAt] = useState<number | null>(null);
  // 接入密钥明文展示（契约 §3.2）：默认掩码，眼睛切换。
  const [showApiKey, setShowApiKey] = useState(false);
  const [savingKey, setSavingKey] = useState(false);
  const [savingEnabled, setSavingEnabled] = useState(false);
  const [wbMsg, setWbMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [loading, setLoading] = useState(true);

  // ---- 多账号网关（cloud 账号池）----
  // 状态类型用 api.WorkbuddyPoolStatusResponse（= WorkbuddyPoolStatus + config/apiKey/regions?）：
  // 后端扁平直出，setPoolStatus(res) 直接读顶层（:6030 红线，勿按嵌套信封改）。
  const [poolStatus, setPoolStatus] = useState<api.WorkbuddyPoolStatusResponse | null>(null);
  const [poolCfg, setPoolCfg] = useState<WorkbuddyGatewayConfig | null>(null);
  const [poolLoading, setPoolLoading] = useState(false);
  const [poolBusy, setPoolBusy] = useState(false);
  const [poolMsg, setPoolMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  // 池模型目录（契约 T6 / §3.4）：GET /workbuddy-pool/v1/models，按区分组展示；
  // 请求失败 → poolModelsError 非空（目录区给失败文案，不影响账号表）。
  const [poolModels, setPoolModels] = useState<api.WorkbuddyPoolModelsResponse | null>(null);
  const [poolModelsError, setPoolModelsError] = useState<string | null>(null);

  /** 池状态为只读 GET；随挂载加载一次，不触发任何 chat 请求
   *  （chat 只在用户显式操作时发生，本页不提供自动调用入口）。
   *  后端响应扁平直出：顶层即池状态（total/healthy/…/accounts），另带 config/apiKey。
   *  2026-09-24 修复：旧版读 res.pool（嵌套信封残留），与后端扁平输出不匹配 →
   *  setPoolStatus 永不执行 → 账号表恒显「账号池暂无账号」。 */
  const loadPool = useCallback(async (showError = false) => {
    setPoolLoading(true);
    try {
      const res = await api.getWorkbuddyPoolStatus();
      setPoolStatus(res);
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

  /** 池模型目录（契约 T6 / §3.4）：GET /workbuddy-pool/v1/models 只读拉取。
   *  失败仅影响目录区空态（记 poolModelsError），不波及账号表；可点「刷新」重试。 */
  const loadPoolModels = useCallback(async () => {
    try {
      const res = await api.getWorkbuddyPoolModels();
      setPoolModels(res);
      setPoolModelsError(null);
    } catch (e) {
      setPoolModels(null);
      setPoolModelsError(api.asError(e));
    }
  }, []);

  // 🔴 池模型目录挂载即加载（2026-09-25 用户实测：此前只在手动点「刷新」时拉取，
  // 目录区永远停在「加载中…」，按国内/国外的模型分类因此完全不可见）。
  useEffect(() => {
    void loadPoolModels();
  }, [loadPoolModels]);

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

  /** 目录加载（契约 §3.2）：优先同源 fetch ?include=custom 全量（tier/model/custom 三段齐全）；
   *  失败（如 Tauri 桌面宿主无同源后端）回落既有封装 getWorkbuddyNativeModels（不带 custom），
   *  两次都失败才置 nativeError 呈现空态。成功时整体替换目录与元信息并清空错误。 */
  const loadNativeCatalog = useCallback(async () => {
    setNativeLoading(true);
    try {
      const full = await fetchNativeCatalogFull();
      setNativeModels(full.data);
      setNativeMeta({
        source: full.source,
        probedAt: full.probedAt,
        currentModelId: full.currentModelId,
        region: (full as { region?: string | null }).region ?? null,
      });
      setNativeError(null);
    } catch {
      try {
        const base = await api.getWorkbuddyNativeModels();
        setNativeModels((base.data ?? []).map(toCatalogRow));
        setNativeMeta({
          source: base.source ?? null,
          probedAt: base.probedAt ?? null,
          currentModelId: base.currentModelId ?? null,
          region: (base as { region?: string | null }).region ?? null,
        });
        setNativeError(null);
      } catch (e) {
        setNativeError(api.asError(e));
      }
    } finally {
      setNativeLoading(false);
    }
  }, []);

  /** 「刷新目录」（契约 §3.2）：api.refreshWorkbuddyCatalog() →
   *  POST /api/workbuddy-native/catalog/refresh（绕 300s TTL；失败 502 → nativeError）。
   *  成功后用返回 data 整体重渲染，并记录后端 probedAt 作为刷新时刻。 */
  const refreshCatalog = useCallback(async () => {
    setRefreshingCatalog(true);
    try {
      const res = await api.refreshWorkbuddyCatalog();
      setNativeModels((res.data ?? []).map(toCatalogRow));
      // 后端响应另带 probedAt（wrapper 类型未声明）——读到真实探测时刻就展示，缺省退化为当下。
      const probedAt = (res as { probedAt?: number | null }).probedAt ?? null;
      setCatalogRefreshedAt(probedAt ?? Date.now());
      setNativeError(null);
    } catch (e) {
      setNativeError(api.asError(e));
    } finally {
      setRefreshingCatalog(false);
    }
  }, []);

  /** 「测试」入口（契约 §3.2）：跳模型测试页并预选 workbuddy-native 网关 + 该模型。 */
  const testModel = useCallback(
    (id: string) => {
      void navigate(`/model-test?gateway=workbuddy-native&model=${encodeURIComponent(id)}`);
    },
    [navigate],
  );

  const load = useCallback(async () => {
    setLoading(true);
    // 只取与本页职责相关的三项：通道状态 / 接入配置 / 内置模型（?include=custom 全量，
    // 含回落逻辑，见 loadNativeCatalog；上游 provider 全量清单归 OpenCode 页，本页不再拉取）。
    const [statusRes, cfgRes] = await Promise.allSettled([
      api.getWorkbuddyProxyStatus(),
      api.getWorkbuddyProxyConfig(),
      loadNativeCatalog(),
    ]);
    if (statusRes.status === "fulfilled") setWbStatus(statusRes.value);
    if (cfgRes.status === "fulfilled") setWbCfg(cfgRes.value);
    setLoading(false);
  }, [loadNativeCatalog]);

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

  /** 内置模型 ID（契约 §五）：优先会话默认模型 currentModelId，目录为空时回落同一默认值
   *  （与后端 static 兜底同源）；Hermes 示例 / curl 示例 / 可用模型共用该占位。 */
  const builtinModelId =
    nativeMeta.currentModelId ?? nativeModels[0]?.id ?? "deepseek-v4.1-flash";

  // ---- 内置模型目录派生（契约 §五 / §3.2）----
  // 计数口径：custom 条目不计入总数（本页虽以 ?include=custom 拉全量用于分组展示，
  // 计数仍只看 native）；旧后端无 kind 字段时全部按实模计（与 static 兜底单条目录一致）。
  const nativeCount = nativeModels.filter((m) => m.kind !== "custom").length;
  const hasKindInfo = nativeModels.some((m) => m.kind != null);
  const nativeModelCount = hasKindInfo
    ? nativeModels.filter((m) => m.kind === "model").length
    : nativeCount;
  const nativeTierCount = hasKindInfo
    ? nativeModels.filter((m) => m.kind === "tier").length
    : 0;
  const nativeCountText =
    nativeModels.length === 0
      ? "内置模型"
      : `内置模型 ${nativeCount} 个（${nativeModelCount} 实模 + ${nativeTierCount} 档位）`;
  // 区域徽标（2026-09-25 区域分类）：桌面端登录账号的区域=内置模型的归属区域。
  const nativeRegionLabel =
    nativeMeta.region === "cn"
      ? "国内版"
      : nativeMeta.region === "intl"
        ? "国外版"
        : null;
  // 按 kind 三段分组（契约 §3.2：档位 / 内置实模 / 自定义接入）；空组不渲染。
  const catalogGroups = CATALOG_GROUPS.map((g) => ({
    ...g,
    rows: nativeModels.filter((m) => m.kind === g.kind),
  })).filter((g) => g.rows.length > 0);
  // 刷新时刻：手动刷新成功后的 probedAt 优先，否则用目录元信息里的探测时刻。
  const catalogClock = fmtClock(catalogRefreshedAt ?? nativeMeta.probedAt);

  // 通道态由后端下发（direct = 直连；缺省视为 opencode 兜底）。
  // 后端未定义该字段时做安全读取，避免类型耦合。
  const channel = (wbStatus as { channel?: string } | null)?.channel ?? "opencode-fallback";

  // ---- 池区域派生（契约 T6 / §3.4 / §3.5，全部可选字段容错）----
  // status.regions 区域聚合（旧后端缺省 → null，总览卡「区域概况」行不渲染）。
  const poolRegions = poolStatus?.regions ?? null;
  // 池模型目录条目；旧后端条目无 region 字段 → poolHasRegionInfo=false，分组退化为单一组。
  const poolModelEntries = poolModels?.data ?? [];
  const poolHasRegionInfo = poolModelEntries.some((m) => m.region === "cn" || m.region === "intl");
  // 内置模型区「国外版」小节 + 池目录「国内版」组的原始条目（id/上下文/输出上限）。
  const poolIntlEntries = poolModelEntries.filter((m) => m.region === "intl");
  const poolCnEntries = poolModelEntries.filter((m) => m.region === "cn");

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
          完整清单（15 实模 + 3 档位，实时目录）在下方「接入信息 · WorkBuddy 内置模型」表格列出，可逐个测试；
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

      {/* ④ 接入信息 · WorkBuddy 内置模型（桌面端会话自带，走 WorkBuddy 订阅）
          2026-09-25 native 多模型改造（契约 §五）：完整 ACP 目录 + 来源徽标 + 计数 + 兜底说明
          2026-09-25 接入信息改造（契约 §3.2）：?include=custom 全量表格（kind 三段分组）
          + 刷新目录按钮（refreshWorkbuddyCatalog）+ 空态/失败态重试 */}
      <section className="mb-10 min-w-0 space-y-2.5" aria-label="WorkBuddy 内置模型接入信息">
        <div className="flex min-w-0 flex-wrap items-start justify-between gap-2 px-1">
          <div className="min-w-0">
            <h2 className="text-[13px] font-medium leading-5">接入信息 · WorkBuddy 内置模型</h2>
            <p className="mt-0.5 text-xs text-muted-foreground">
              WorkBuddy 桌面端内嵌会话自带的模型（走 WorkBuddy 订阅），经 ACP 通道转发——与「OpenCode 兜底」是两套模型体系
            </p>
          </div>
          <div className="flex shrink-0 flex-wrap items-center gap-2">
            {catalogClock ? (
              <span className="text-[11px] text-muted-foreground">刷新于 {catalogClock}</span>
            ) : null}
            <Button
              className="shrink-0"
              variant="outline"
              size="sm"
              onClick={() => void refreshCatalog()}
              disabled={refreshingCatalog}
            >
              {refreshingCatalog ? <Loader2 className="animate-spin" /> : <RefreshCw />}
              刷新目录
            </Button>
          </div>
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
            <InfoRow
              label={nativeCountText}
              hint="内置模型不可用或请求模型不在目录时，自动落 OpenCode 兜底并重新拉取模型内容；响应 wbServedBy 字段/响应头标明来源"
            >
              {nativeRegionLabel ? (
                <Badge
                  variant="outline"
                  className="mr-1 shrink-0 border-primary/40 text-primary"
                >
                  {nativeRegionLabel}
                </Badge>
              ) : null}
              <NativeSourceBadge source={nativeMeta.source} probedAt={nativeMeta.probedAt} />
            </InfoRow>
            {/* 模型目录（契约 §3.2）：?include=custom 全量，按 kind 三段分组、保持服务端返回序；
                探测失败 / 空目录 → 空态文案 + 重试 */}
            {nativeError ? (
              <div className="mx-4 border-b border-border/50 py-4 sm:mx-5">
                <p className="text-xs text-destructive">目录探测失败：{nativeError}</p>
                <Button
                  size="sm"
                  variant="outline"
                  className="mt-2 h-7 gap-1 text-xs"
                  onClick={() => void loadNativeCatalog()}
                  disabled={nativeLoading}
                >
                  {nativeLoading ? (
                    <Loader2 className="size-3.5 animate-spin" />
                  ) : (
                    <RefreshCw className="size-3.5" />
                  )}
                  重试
                </Button>
              </div>
            ) : nativeModels.length === 0 ? (
              <div className="mx-4 border-b border-border/50 py-4 sm:mx-5">
                <p className="text-xs text-muted-foreground">
                  未探测到目录（请先在 WorkBuddy 打开一个 AI 会话；目录请求失败时此处为空，对话仍会自动落 OpenCode 兜底）
                </p>
                <Button
                  size="sm"
                  variant="outline"
                  className="mt-2 h-7 gap-1 text-xs"
                  onClick={() => void loadNativeCatalog()}
                  disabled={nativeLoading}
                >
                  {nativeLoading ? (
                    <Loader2 className="size-3.5 animate-spin" />
                  ) : (
                    <RefreshCw className="size-3.5" />
                  )}
                  重试
                </Button>
              </div>
            ) : (
              <div className="border-b border-border/50">
                <table className="w-full text-xs">
                  <thead className="bg-muted/40 text-muted-foreground">
                    <tr>
                      <th className="px-4 py-2 text-left font-medium sm:px-5">名称</th>
                      <th className="px-2 py-2 text-left font-medium">id</th>
                      <th className="px-2 py-2 text-left font-medium">类型</th>
                      <th className="px-2 py-2 text-left font-medium">倍率</th>
                      <th className="px-2 py-2 text-left font-medium">来源</th>
                      <th className="px-4 py-2 text-right font-medium sm:px-5">操作</th>
                    </tr>
                  </thead>
                  {catalogGroups.map((g) => (
                    <tbody key={g.kind} className="divide-y divide-border/40">
                      <tr className="bg-muted/20">
                        <td
                          colSpan={6}
                          className="px-4 py-1.5 text-[11px] font-medium text-muted-foreground sm:px-5"
                        >
                          国内版 · {g.title}
                          <span className="ml-1.5 font-normal">（{g.rows.length}）</span>
                        </td>
                      </tr>
                      {g.rows.map((m) => (
                        <CatalogTableRow
                          key={m.id}
                          model={m}
                          isDefault={m.id === builtinModelId}
                          onTest={testModel}
                        />
                      ))}
                    </tbody>
                  ))}
                </table>
              </div>
            )}

            {/* 国外版 WorkBuddy 模型（2026-09-25：用户要求内置模型区也按国内/国外两版展示，
                且国外版与国内表同规格：名称/id/类型/倍率/来源/操作 六列）。
                通道事实：内置通道（/workbuddy-native/v1）绑定桌面端当前登录的 WorkBuddy 订阅
                （本机为国服账号 → ACP 只会下发国内版模型）；国外版模型来自池网关国际账号
                （/workbuddy-pool/v1，chat 时按模型自动路由 intl 账号，已在区域隔离改造中打通）。
                cloud 上游不提供倍率，倍率列显示「—」。 */}
            {poolIntlEntries.length > 0 ? (
              <div className="border-b border-border/50 px-4 py-4 sm:px-5">
                <div className="mb-2 flex flex-wrap items-center gap-2">
                  <span className="text-xs font-medium">国外版 WorkBuddy 模型</span>
                  <span className="text-[11px] text-muted-foreground">
                    （{poolIntlEntries.length}）· 经池网关国际账号提供（/workbuddy-pool/v1，chat 自动路由 intl 账号）
                  </span>
                  <PoolCatalogSourceBadge source={poolModels?.regions?.intl?.source} />
                </div>
                <div className="min-w-0 overflow-x-auto">
                  <table className="w-full min-w-[640px] text-[13px]">
                    <thead>
                      <tr className="text-left text-[11px] text-muted-foreground">
                        <th className="px-4 pb-1.5 font-medium sm:px-5">名称</th>
                        <th className="px-2 pb-1.5 font-medium">id</th>
                        <th className="px-2 pb-1.5 font-medium">类型</th>
                        <th className="px-2 pb-1.5 font-medium">倍率</th>
                        <th className="px-2 pb-1.5 font-medium">来源</th>
                        <th className="px-4 pb-1.5 text-right font-medium sm:px-5">操作</th>
                      </tr>
                    </thead>
                    <tbody>
                      {poolIntlEntries.map((m) => (
                        <tr key={m.id} className="border-t border-border/40 hover:bg-muted/30">
                          <td className="max-w-[220px] px-4 py-2 sm:px-5">
                            <span
                              className="block truncate font-medium leading-5"
                              title={
                                typeof m.context_length === "number"
                                  ? `${m.name || m.id} · 上下文 ${m.context_length.toLocaleString()} tokens`
                                  : m.name || m.id
                              }
                            >
                              {m.name || m.id}
                            </span>
                          </td>
                          <td className="max-w-[210px] px-2 py-2">
                            <code
                              className="block truncate rounded bg-muted/70 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground"
                              title={m.id}
                            >
                              {m.id}
                            </code>
                          </td>
                          <td className="px-2 py-2">
                            <Badge variant="outline" className="shrink-0 px-1.5 py-0 text-[10px]">
                              云端模型
                            </Badge>
                          </td>
                          <td className="px-2 py-2">
                            {m.multiplier != null ? (
                              <Badge variant="secondary" className="shrink-0 px-1.5 py-0 font-mono text-[10px]">
                                x{m.multiplier}
                              </Badge>
                            ) : (
                              <span className="text-muted-foreground" title="上游未提供倍率">
                                —
                              </span>
                            )}
                          </td>
                          <td className="px-2 py-2">
                            <div className="flex flex-wrap items-center gap-1">
                              <Badge variant="secondary" className="shrink-0 px-1.5 py-0 text-[10px]">
                                池网关国际账号
                              </Badge>
                              <PoolCatalogSourceBadge source={poolModels?.regions?.intl?.source} />
                            </div>
                          </td>
                          <td className="px-4 py-2 text-right sm:px-5">
                            <Button
                              size="sm"
                              variant="outline"
                              className="h-7 gap-1 px-2.5 text-xs"
                              onClick={() =>
                                void navigate(
                                  `/model-test?gateway=workbuddy-pool&model=${encodeURIComponent(m.id)}&region=intl`,
                                )
                              }
                            >
                              <FlaskConical className="size-3.5" />
                              测试
                            </Button>
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </div>
            ) : null}
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
            <InfoRow
              label="访问密钥"
              hint="请求头 Authorization: Bearer <密钥>（或 x-api-key）；亦接受网关密钥 gw_live_*（「网关管理」页创建）。密钥仅作用于 /workbuddy/v1；/workbuddy-native/v1 无需密钥"
            >
              <div className="flex items-center gap-1.5">
                {wbCfg ? (
                  <code
                    className="max-w-[240px] truncate rounded bg-muted/70 px-2 py-1 font-mono text-xs"
                    title={showApiKey ? wbCfg.apiKey : "已掩码显示 · 点击眼睛图标查看明文"}
                  >
                    {showApiKey ? wbCfg.apiKey : maskApiKey(wbCfg.apiKey)}
                  </code>
                ) : (
                  <code className="max-w-[240px] truncate rounded bg-muted/70 px-2 py-1 text-xs">加载中…</code>
                )}
                {wbCfg ? (
                  <Button
                    size="icon"
                    variant="ghost"
                    className="size-7"
                    aria-label={showApiKey ? "隐藏访问密钥" : "显示访问密钥"}
                    onClick={() => setShowApiKey((v) => !v)}
                  >
                    {showApiKey ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
                  </Button>
                ) : null}
                {wbCfg ? <CopyKeyButton text={wbCfg.apiKey} /> : null}
                <Button size="sm" variant="outline" onClick={() => void regenerateWbKey()} disabled={savingKey || !wbCfg}>
                  {savingKey ? <Loader2 className="animate-spin" /> : <RefreshCw />}重新生成
                </Button>
              </div>
            </InfoRow>
            <InfoRow
              label="可用模型"
              hint="完整清单见上方「接入信息 · WorkBuddy 内置模型」表格（实时目录，逐个已验证可调用）；provider/model 形态仍可调用其它模型（走 OpenCode 兜底链路）"
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
              onClick={() => {
                void loadPool(true);
                void loadPoolModels();
              }}
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

            {/* 区域概况（契约 T6 / §3.5 status.regions）：cn/intl 的 accounts/healthy/models；
                models 取自区域目录缓存，缓存未建为 null → 不显示该段。旧后端缺省整行不渲染。 */}
            {poolRegions ? (
              <div className="flex flex-wrap items-center gap-2 border-t px-4 py-2.5 text-xs sm:px-5">
                <span className="text-muted-foreground">区域概况</span>
                {(["cn", "intl"] as const).map((rg) => {
                  const r = poolRegions[rg];
                  if (!r) return null;
                  return (
                    <span
                      key={rg}
                      className="inline-flex items-center gap-1.5 rounded-lg border px-2 py-1"
                    >
                      <Badge variant="outline" className="h-5 px-1.5 py-0 text-[10px]">
                        {poolRegionLabel(rg)}
                      </Badge>
                      <span className="tabular-nums">
                        账号 {r.accounts} · 可用 {r.healthy}
                        {r.models != null ? ` · 目录 ${r.models}` : ""}
                      </span>
                    </span>
                  );
                })}
              </div>
            ) : null}

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
                              {/* 区域徽标（契约 T6）：数据在 accounts[].region（types.ts 既有字段）；
                                  文案走 poolRegionLabel 与模型目录分组统一（cn=国内 / intl=国际） */}
                              <Badge variant="outline" className="h-5 text-[10px]">
                                {poolRegionLabel(a.region)}
                              </Badge>
                            </td>
                            {/* 积分 / 到期由余额巡检回填（workbuddy_gateway.rs balance_patrol_once）；
                                巡检前 credits=0 / credit_expires_at=0，两列显「—」（增补 §2.3，逻辑已有容错） */}
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

            {/* 池模型目录（契约 T6 / §3.4）：按「国内(cn)/国际(intl)」两组展示 ——
                组标题 + 条数 + 来源徽标（动态/静态/回落，来自响应 regions 元信息）；
                单区缺失给空态说明；旧后端条目无 region 字段 → 分组退化为单一组（容错） */}
            <div className="border-t px-4 py-4 sm:px-5">
              <div className="mb-2 flex flex-wrap items-center gap-2">
                <Boxes className="size-3.5 text-muted-foreground" />
                <span className="text-xs font-medium">模型目录（按账号区域隔离）</span>
                <span className="text-[11px] text-muted-foreground">
                  GET /workbuddy-pool/v1/models
                </span>
              </div>
              {poolModelsError ? (
                <p className="py-2.5 text-xs text-destructive">
                  模型目录获取失败：{poolModelsError}（可点右上「刷新」重试）
                </p>
              ) : !poolModels ? (
                <p className="py-2.5 text-xs text-muted-foreground">加载中…</p>
              ) : poolModelEntries.length === 0 ? (
                <p className="py-2.5 text-xs text-muted-foreground">
                  模型目录为空（池内无可用账号且内置目录为空）
                </p>
              ) : !poolHasRegionInfo ? (
                /* 旧后端无 region 字段 → 单一组退化展示 */
                <PoolModelGroup
                  title="全部模型"
                  entries={poolModelEntries}
                  region="cn"
                  note="当前后端未下发模型区域信息（旧版本），无法按国内 / 国际分组展示"
                />
              ) : (
                <div className="divide-y divide-border/40">
                  {/* 国内区（cn）：动态目录 / 回落内置表 */}
                  {poolCnEntries.length > 0 ? (
                    <PoolModelGroup
                      title="国内版 WorkBuddy 模型（cn）"
                      entries={poolCnEntries}
                      source={poolModels.regions?.cn?.source}
                      region="cn"
                      note={
                        poolModels.regions?.cn?.source === "fallback"
                          ? "国内区动态目录获取失败，已回落内置表"
                          : undefined
                      }
                    />
                  ) : (
                    <p className="py-2.5 text-xs text-muted-foreground">
                      国内区暂无模型条目（池内无国内账号或目录获取失败）
                    </p>
                  )}
                  {/* 国际区（intl）：完整清单已在上方「接入信息 · WorkBuddy 内置模型 →
                      国外版 WorkBuddy 模型」表格列出（同源数据），此处不再重复铺开。 */}
                  <div className="py-2.5">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="text-xs font-medium">国外版 WorkBuddy 模型（intl）</span>
                      <span className="text-[11px] text-muted-foreground">（{poolIntlEntries.length}）</span>
                      <PoolCatalogSourceBadge source={poolModels.regions?.intl?.source} />
                    </div>
                    <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
                      {poolIntlEntries.length > 0
                        ? "完整清单见上方「接入信息 · WorkBuddy 内置模型 → 国外版 WorkBuddy 模型」表格（同源：GET /workbuddy-pool/v1/models?region=intl）。"
                        : "国际区暂无模型条目（池内无国际账号，静态表为空）。"}
                    </p>
                  </div>
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
            {/* 区域专属接入密钥（2026-09-25）：国内版 / 国际版各一把，互不串区。
                跨区使用时池 chat 按请求体 region 拒绝并提示，杜绝「不知道连的哪个版本」。 */}
            <RegionKeyCard region="cn" label="国内版接入密钥" />
            <RegionKeyCard region="intl" label="国际版接入密钥" />
            {/* 三协议接入说明（增补契约 §2.3 / §3：responses 与 messages 和 chat 同路由组、同鉴权口径） */}
            <InfoRow label="支持协议" hint="同一 Base URL 与区域专属密钥，三种协议共用选号 / 冷却 / 熔断口径">
              <div className="text-xs leading-5 text-muted-foreground">
                <p>/workbuddy-pool/v1 同时支持 OpenAI Chat / Responses / Anthropic Messages 三协议。</p>
                <p>Codex CLI 配 wire_api=&quot;responses&quot;；Claude Code 用 /v1/messages。</p>
              </div>
            </InfoRow>
            {/* 调用示例（契约 T6）：注明 chat body 可选 region:"cn"|"intl" 字段（契约 T2 精确切换入口；
                缺省按模型区域解析——两区同名模型（glm-5.3）建议显式指定） */}
            <div className="mx-4 flex items-start justify-between gap-3 py-3.5 sm:mx-5">
              <div className="min-w-0">
                <p className="mb-0 text-xs text-muted-foreground">调用示例（curl）：</p>
                <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
                  请求体可选{" "}
                  <code className="rounded bg-muted px-1">region: &quot;cn&quot; | &quot;intl&quot;</code>{" "}
                  字段精确指定账号区域（缺省按模型自动路由；两区同名模型建议显式指定）；密钥需与区域匹配
                </p>
              </div>
              <pre className="min-w-0 overflow-x-auto rounded-lg bg-muted/60 p-3 text-xs leading-5">
                {`curl -X POST ${WB_POOL_BASE_URL}/chat/completions \\
  -H "Authorization: Bearer <该区接入密钥>" \\
  -H "Content-Type: application/json" \\
  -d '{"model":"<模型id>","messages":[{"role":"user","content":"你好"}],"region":"cn"}'`}
              </pre>
              <CopyButton
                text={`curl -X POST ${WB_POOL_BASE_URL}/chat/completions -H "Authorization: Bearer <该区接入密钥>" -H "Content-Type: application/json" -d '{"model":"<模型id>","messages":[{"role":"user","content":"你好"}],"region":"cn"}'`}
                label="调用示例"
              />
            </div>
          </CardContent>
        </Card>
      </section>
    </div>
  );
}
