/**
 * 账号池健康判据（**跨视图共享的唯一实现**）。
 *
 * # 为什么单独成模块
 *
 * 「配额不足 vs 服务故障」这套判据现在有两个消费方：
 * 1. 统计页「WorkBuddy 逆向」顶部的池健康面板（监控视角）；
 * 2. 多账号网关（cloud）区块（管理视角）。
 *
 * 若各写一份，必然漂移 —— 2026-10-13 一天内已因「同一语义两处实现」踩中三次
 * （池级计数漏 `paused`、准入判据两份、region 聚合口径）。
 * ⇒ **判据放这里，视图只负责渲染。**
 *
 * 后端同源判据：`workbuddy_gateway.rs::Entry::healthy`
 * （`!disabled && !paused && until<=now && breaker<=now`）。
 */
import type { WorkbuddyPoolAccount } from "@/lib/types";
import { formatDateTimeShort } from "@/lib/stats-format";

export type VerdictLevel = "ok" | "partial" | "quota" | "failure" | "cooldown" | "empty" | "unknown";

export interface RegionVerdict {
  level: VerdictLevel;
  /** 结论短标签 */
  label: string;
  /** 一句人话说明（为什么是这个结论） */
  detail: string;
}

export const LEVEL_STYLE: Record<VerdictLevel, { dot: string; text: string; box: string }> = {
  ok: { dot: "bg-emerald-500", text: "text-emerald-700", box: "" },
  // 有账号可用但部分模型被限流——必须与「完全可用」区分开，否则会误导
  partial: { dot: "bg-amber-500", text: "text-amber-700", box: "border-amber-300 bg-amber-50/50" },
  quota: { dot: "bg-amber-500", text: "text-amber-700", box: "border-amber-300 bg-amber-50/50" },
  failure: { dot: "bg-red-500", text: "text-red-700", box: "border-red-300 bg-red-50/50" },
  cooldown: { dot: "bg-orange-500", text: "text-orange-700", box: "border-orange-300 bg-orange-50/50" },
  empty: { dot: "bg-slate-400", text: "text-slate-600", box: "border-slate-300 bg-slate-50/50" },
  unknown: { dot: "bg-slate-400", text: "text-slate-600", box: "" },
};

/** 与后端 `Entry::healthy(now)` 同口径：未出池、未暂停、未冷却、未熔断。 */
export function isAccountHealthy(a: WorkbuddyPoolAccount, now: number): boolean {
  return !a.disabled && !a.paused && a.until <= now && a.breaker_until <= now;
}

/** 模型级冷却中且仍生效的项（过滤掉已过期的）。 */
export function activeModelCools(a: WorkbuddyPoolAccount, now: number) {
  return (a.model_cools ?? []).filter((m) => m.until > now);
}

/** 该区当前被限流的模型 → 最早恢复时间（同模型取最早的）。 */
export function cooledModels(accounts: WorkbuddyPoolAccount[], now: number) {
  const map = new Map<string, number>();
  for (const a of accounts) {
    for (const m of activeModelCools(a, now)) {
      const prev = map.get(m.model);
      if (prev === undefined || m.until < prev) map.set(m.model, m.until);
    }
  }
  return [...map.entries()].sort((x, y) => x[1] - y[1]);
}

/**
 * 判定一个 region 的可用性结论。**分支顺序即优先级**，改动前先读这份文档。
 *
 * | region 状态 | 结论 |
 * | --- | --- |
 * | 无账号 | ⚪ 结构性缺号 |
 * | 有健康号 + 无模型限流 | 🟢 可用 |
 * | 有健康号 + 有模型限流 | 🟡 部分模型限流（**「有账号可用」≠「你要的模型能用」**）|
 * | 无健康号 + 有熔断/出池 | 🔴 服务故障 |
 * | 无健康号 + 全部被模型冷却 | 🟡 配额不足 |
 * | 无健康号 + 仅软冷却 | 🟠 冷却中 |
 * | 以上都不命中 | ⚪ 原因未知（**不猜**）|
 */
export function judgeRegion(accounts: WorkbuddyPoolAccount[], now: number): RegionVerdict {
  const total = accounts.length;
  if (total === 0) {
    return {
      level: "empty",
      label: "结构性缺号",
      detail: "该区没有任何账号——不是限流也不是故障，是账号池里就没配。",
    };
  }

  const healthy = accounts.filter((a) => isAccountHealthy(a, now)).length;
  const cooled = cooledModels(accounts, now);
  const cooledDesc = cooled
    .slice(0, 3)
    .map(([m, until]) => `${m}（${formatDateTimeShort(until)} 恢复）`)
    .join("、");

  if (healthy > 0) {
    // 「有账号可用」不等于「你要的模型能用」——这两件事必须分开说，
    // 否则会出现「面板显示可用、实际请求 503」的误导（首版即此，实测打回）。
    if (cooled.length === 0) {
      return {
        level: "ok",
        label: "可用",
        detail: `${healthy}/${total} 个账号可参与选号，且无模型被限流。`,
      };
    }
    return {
      level: "partial",
      label: "部分模型限流",
      detail:
        `${healthy}/${total} 个账号可参与选号，但上游按**模型**限流：${cooledDesc}。` +
        "换号或重启均无效，只能用其他模型或等重置。",
    };
  }

  // —— healthy === 0：池层面已不可用，再分辨「配额」还是「故障」——
  // 用户主动暂停优先说明：它不是故障也不是配额，别让人去查链路。
  const pausedOnly = accounts.filter((a) => a.paused);
  if (pausedOnly.length === total) {
    return {
      level: "empty",
      label: "全部已暂停",
      detail: `${total} 个账号都被**手动暂停**（用户操作）——不是故障也不是限流，在下方明细里点「恢复」即可。`,
    };
  }

  const pooled = accounts.filter((a) => a.disabled || a.breaker_until > now);
  if (pooled.length > 0) {
    const first = pooled[0];
    const until = first.breaker_until > now ? first.breaker_until : 0;
    return {
      level: "failure",
      label: "服务故障",
      detail:
        `${pooled.length}/${total} 个账号熔断或已出池` +
        (until ? `，最近一个熔断至 ${formatDateTimeShort(until)}` : "") +
        (first.reason ? `（${first.reason}）` : "") +
        "。熔断由连续失败喂出，指向链路问题。",
    };
  }
  const modelCooled = accounts.filter((a) => activeModelCools(a, now).length > 0);
  if (modelCooled.length === total) {
    return {
      level: "quota",
      label: "配额不足",
      detail: `全部账号本身健康，但被上游按**模型**限流：${cooledDesc || "见账号明细"}。不能靠换号或重启解决。`,
    };
  }
  const softCooling = accounts.filter((a) => a.until > now);
  if (softCooling.length === total) {
    return {
      level: "cooldown",
      label: "冷却中",
      detail: `${total} 个账号均在限流退避中，最近一个至 ${formatDateTimeShort(
        Math.min(...accounts.map((a) => a.until)),
      )}，会自行恢复。`,
    };
  }
  return {
    level: "unknown",
    label: "原因未知",
    detail: "已排除熔断/出池/模型限流/软冷却/手动暂停，但选号仍不可用——请查网关日志。",
  };
}

/**
 * 按 region 分组并给出裁决（含 cn / intl 之外的未知区兜底）。
 *
 * 以 `accounts[].region` 自行分组、不依赖后端 `regions` 聚合：
 * 旧后端无该字段时也能渲染，且分组口径与明细表**天然一致**。
 */
export function groupByRegion(accounts: WorkbuddyPoolAccount[], now: number) {
  const keys = [...new Set(accounts.map((a) => a.region))];
  // cn / intl 固定在前，其余按字母序追加（保持稳定顺序，避免刷新时跳位）
  keys.sort((x, y) => {
    const rank = (k: string) => (k === "cn" ? 0 : k === "intl" ? 1 : 2);
    return rank(x) - rank(y) || x.localeCompare(y);
  });
  return keys.map((key) => {
    const list = accounts.filter((a) => a.region === key);
    return {
      key,
      accounts: list,
      verdict: judgeRegion(list, now),
      cooled: cooledModels(list, now),
      healthyCount: list.filter((a) => isAccountHealthy(a, now)).length,
    };
  });
}
