/**
 * 模型分类口径（共享模块）—— WorkBuddy / OpenCode 两个网关页共用，
 * 保证「免费判定」「任务场景」「排序」三件事在两页完全一致。
 *
 * 需求对应：第三项「明确每个模型适用的任务场景（各模型能够承担的工作类型）」。
 */

/** 免费模型判定（元数据自动筛）：id 含 free、或 cost/price 标注为 0 的形态。 */
export function isFreeModelId(id: string): boolean {
  return /free|:0$|\/0$|zero/i.test(id);
}

/* --------------------------------------------------------------------------
 * 任务场景
 * 判定顺序 = 从「专用」到「通用」，避免宽泛特征吃掉专用特征。
 * ------------------------------------------------------------------------ */
export type TaskKind = "fast" | "code" | "vision" | "reasoning" | "general" | "safety";

export const TASK_META: Record<TaskKind, { label: string; hint: string }> = {
  fast: {
    label: "快速",
    hint: "低延迟/低成本档（flash·lightning·mini·nano·small）：日常问答、代码补全、批量小任务",
  },
  code: { label: "代码", hint: "代码专用（code/coder）：生成与修改代码、重构、单测" },
  vision: {
    label: "视觉",
    hint: "支持图像/多模态输入（vl·omni·vision）：截图理解、图文混排任务",
  },
  reasoning: {
    label: "推理",
    hint: "复杂推理/大参数（reasoning·thinking·pro·ultra·super·大 B 数）：难题、方案设计、长链分析",
  },
  general: { label: "通用", hint: "通用对话，未命中特定特征：常规问答与写作" },
  safety: {
    label: "安全",
    hint: "内容安全/审核类（content-safety）：非通用对话用途，勿作主力模型",
  },
};

/** 展示/排序顺序：最常用的档位排前面。 */
export const TASK_ORDER: TaskKind[] = ["fast", "code", "vision", "reasoning", "general", "safety"];

/**
 * 分类单个模型。
 *
 * 🔴 只按「渠道之后」的部分判定：渠道名 `opencode` 里自带 "code"，
 *    若对完整 id 匹配，会把该渠道**整组**模型误判为代码专用（实测踩过）。
 */
export function classifyTask(full: string): TaskKind {
  const s = full.slice(full.indexOf("/") + 1).toLowerCase();
  if (/content-safety|safety|moderation/.test(s)) return "safety";
  if (/coder|coding|[-/]code\b|code[-/]|-code:|-code$/.test(s)) return "code";
  // ⚠️ 不能写成 /thinking/：会误命中厂商名 `thinkingmachines`
  if (/\breasoning\b|-reasoning|[-/]thinking|thinking-/.test(s)) return "reasoning";
  if (/[-/]vl\b|vision|omni|multimodal/.test(s)) return "vision";
  if (/flash|lightning|[-/]mini\b|nano|small|tiny|lite|[-/]xs\b|-fin\b/.test(s)) return "fast";
  // 旗舰/大参数档（无显式能力词时按规模归入推理）；⚠️ 用 \d{3,4}b，否则 gemma-4-31b 会被误判
  if (/super|ultra|[-/]pro\b|\d{3,4}b\b|glm-5\.|gpt-5|qwen3\.8-/.test(s)) return "reasoning";
  return "general";
}

/** 「速度优先」打分：越小越优先。① 免费优先 ② 快速档 ③ 其余按场景序。 */
export function speedScore(id: string): number {
  const free = isFreeModelId(id) ? 0 : 100;
  const kind = classifyTask(id);
  const kindScore = kind === "fast" ? 0 : kind === "code" ? 10 : kind === "vision" ? 12 : 20;
  return free + kindScore;
}

/** 按渠道分组；渠道序：自有 opencode 优先，其余按条目数降序再按名称。 */
export function groupByChannel(ids: string[]): { channel: string; ids: string[] }[] {
  const map = new Map<string, string[]>();
  for (const id of ids) {
    const ch = id.split("/")[0] ?? "unknown";
    const arr = map.get(ch) ?? [];
    arr.push(id);
    map.set(ch, arr);
  }
  return [...map.entries()]
    .map(([channel, list]) => ({
      channel,
      ids: list.slice().sort((a, b) => {
        const d = TASK_ORDER.indexOf(classifyTask(a)) - TASK_ORDER.indexOf(classifyTask(b));
        return d !== 0 ? d : a.localeCompare(b);
      }),
    }))
    .sort((a, b) => {
      if (a.channel === "opencode") return -1;
      if (b.channel === "opencode") return 1;
      return b.ids.length - a.ids.length || a.channel.localeCompare(b.channel);
    });
}

/** 模型名去掉渠道前缀（用于同渠道内展示）。 */
export function stripChannel(id: string): string {
  const i = id.indexOf("/");
  return i >= 0 ? id.slice(i + 1) : id;
}
