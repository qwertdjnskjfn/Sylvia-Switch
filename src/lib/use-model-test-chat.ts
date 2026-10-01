import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as api from "@/lib/api";
import type { ModelTestResult } from "@/lib/model-test-types";

// ---------------------------------------------------------------------------
// 冻结常量
// ---------------------------------------------------------------------------

/**
 * 每轮扇出的并发上限。
 *
 * 为什么是 2：整轮纯串行太慢（模型一多，排在后面的要干等几分钟），
 * 而开高并发会把「限流」和「真的不可用」混在一起——旧页纯串行的初衷
 * 就是让每条失败结论都可归因。取 2 折中：比串行快一倍以上，又不污染结论。
 */
const CONCURRENCY = 2;

/** 探测输出上限：契约冻结恒 64，不做 UI 配置项（保持界面简洁）。 */
const MAX_TOKENS = 64;

/** 聊天历史的 localStorage key（契约冻结；升级持久化形状时换版本号）。 */
const STORAGE_KEY = "model_test_chat_v1";

/** turns 变化后的落盘防抖窗口：一轮探测会连续落地多个结果，只写最后一次。 */
const PERSIST_DEBOUNCE_MS = 300;

/** 持久化最多保留的轮数：更早的轮次自然淘汰，防止 localStorage 无限膨胀。 */
const MAX_PERSISTED_TURNS = 30;

/** 模块级单调递增序号：保证同一进程内 turn id（`${at}-${seq}`）不重复。 */
let turnSeqCounter = 0;

// ---------------------------------------------------------------------------
// 冻结类型（契约 §4，界面工程师按此消费）
// ---------------------------------------------------------------------------

/** 单模型结果条目：pending → running → done 三态迁移，由 hook 内部推进 */
export type TurnItem =
  | { model: string; state: "pending" }
  | { model: string; state: "running" }
  | { model: string; state: "done"; result: ModelTestResult; modelKnown: boolean | null };

/** 一轮测试 = 一次「发送」：用户消息 + 扇出到 N 个模型的全部结果 */
export interface ChatTurn {
  /** `${at}-${seq}`，seq 为模块级单调递增数 */
  id: string;
  /** 发起时间（毫秒时间戳） */
  at: number;
  gateway: string;
  gatewayName: string | null;
  prompt: string;
  /** 本轮目标模型 id（发送时点快照，顺序即侧栏勾选顺序） */
  targets: string[];
  /** 模型 id → 区域提示（WorkBuddy 池：cn/intl，透传上游精确切区）；缺省/无区域语义时省略 */
  targetRegions?: Record<string, string>;
  /** 按完成顺序追加；done 项不可变 */
  results: TurnItem[];
}

export interface UseModelTestChatOptions {
  /** (gateway, model) → 是否在该网关模型目录里；目录为空返回 null（未知）。由页面基于 gateways 快照提供稳定引用 */
  resolveKnown: (gateway: string, model: string) => boolean | null;
}

export interface UseModelTestChat {
  turns: ChatTurn[];
  /** null = 空闲；否则 = 进行中回合的 id */
  activeTurnId: string | null;
  /** 每模型最近一次结论（跨轮聚合，侧栏状态点用）。key = `${gateway}::${model}` */
  lastByModel: Map<string, ModelTestResult>;
  /** 发起一轮测试。prompt 为空串时由调用方先兜底成 DEFAULT_PROMPT；targets 为空时调用方应禁用发送 */
  send(input: {
    gateway: string;
    gatewayName: string | null;
    prompt: string;
    targets: string[];
    /** 模型 id → 区域提示（WorkBuddy 池 cn/intl）；可选，缺省不带 region */
    targetRegions?: Record<string, string>;
  }): void;
  /** 置停止位：未开始的目标不再发起；进行中的 1-2 个请求等待自然结束、结果照常落 */
  stop(): void;
  /** 清空 turns + lastByModel + localStorage */
  clear(): void;
}

// ---------------------------------------------------------------------------
// 持久化（localStorage，key = model_test_chat_v1）
//
// 落盘形状就是 ChatTurn[]，但每轮只保留 state==="done" 的结果——
// pending/running 是瞬时态，落盘毫无意义，刷新后恢复成 done-only 历史。
// ---------------------------------------------------------------------------

/** 挂载时从 localStorage 恢复历史，并按持久化 done 项回放重建 lastByModel。 */
function restoreFromStorage(): { turns: ChatTurn[]; lastByModel: Map<string, ModelTestResult> } {
  const emptyTurns: ChatTurn[] = [];
  const emptyLast = new Map<string, ModelTestResult>();
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { turns: emptyTurns, lastByModel: emptyLast };
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return { turns: emptyTurns, lastByModel: emptyLast };

    const turns: ChatTurn[] = [];
    const lastByModel = new Map<string, ModelTestResult>();
    for (const rawTurn of parsed) {
      if (typeof rawTurn !== "object" || rawTurn === null) continue;
      const t = rawTurn as Record<string, unknown>;
      // 逐字段防御：任何一段脏数据只丢弃所在轮次，不让整份历史报废
      if (
        typeof t.id !== "string" ||
        typeof t.at !== "number" ||
        typeof t.gateway !== "string" ||
        typeof t.prompt !== "string" ||
        !Array.isArray(t.targets)
      ) {
        continue;
      }
      const targets = t.targets.filter((x): x is string => typeof x === "string");

      const results: TurnItem[] = [];
      for (const rawItem of Array.isArray(t.results) ? t.results : []) {
        if (typeof rawItem !== "object" || rawItem === null) continue;
        const it = rawItem as Record<string, unknown>;
        const rawResult = it.result;
        // 只认 done 项：pending/running 本就不落盘，旧版本或半成品数据直接丢弃
        if (typeof rawResult !== "object" || rawResult === null) continue;
        const r = rawResult as Record<string, unknown>;
        // result 的关键字段逐个验型（lastByModel 的 key 与侧栏渲染都依赖它们），
        // 其余字段信任落盘数据，不逐个校验
        if (
          typeof r.at !== "number" ||
          typeof r.ok !== "boolean" ||
          typeof r.code !== "string" ||
          typeof r.reason !== "string" ||
          typeof r.gateway !== "string" ||
          typeof r.model !== "string" ||
          typeof r.latencyMs !== "number"
        ) {
          continue;
        }
        const modelKnown: boolean | null =
          it.modelKnown === null || typeof it.modelKnown === "boolean" ? it.modelKnown : null;
        results.push({
          model: r.model,
          state: "done",
          result: r as unknown as ModelTestResult,
          modelKnown,
        });
      }

      turns.push({
        id: t.id,
        at: t.at,
        gateway: t.gateway,
        gatewayName: typeof t.gatewayName === "string" ? t.gatewayName : null,
        prompt: t.prompt,
        targets,
        results,
      });
      // 纯内存回放重建 lastByModel（不发任何请求）：turns 按时间正序累积，
      // 后写覆盖先写，即每个模型取最近一次结论，刷新后侧栏状态点与统计 chips 不回「未测」
      for (const done of results) {
        // 类型上 results 仍是三态联合，需显式窄化到 done 变体才能取 result
        if (done.state !== "done") continue;
        lastByModel.set(`${t.gateway}::${done.model}`, done.result);
      }
    }
    return { turns, lastByModel };
  } catch {
    // JSON 解析失败静默忽略（契约规格 6）：宁可丢历史，也不让页面挂掉
    return { turns: emptyTurns, lastByModel: emptyLast };
  }
}

/** 把当前 turns 防抖落盘：只存最近 30 轮、每轮只存 done 项；空历史直接移除 key。 */
function persistTurns(turns: ChatTurn[]): void {
  try {
    if (turns.length === 0) {
      // 空历史不写占位串，直接移除 key——与 clear() 的「移除该 key」语义保持一致
      localStorage.removeItem(STORAGE_KEY);
      return;
    }
    const payload: ChatTurn[] = turns
      .slice(-MAX_PERSISTED_TURNS)
      .map((turn) => ({ ...turn, results: turn.results.filter((it) => it.state === "done") }));
    localStorage.setItem(STORAGE_KEY, JSON.stringify(payload));
  } catch {
    // localStorage 不可用（隐私模式 / 配额满）时静默放弃：聊天记录仍在内存里，不影响本轮使用
  }
}

/**
 * 旧页 runOne catch 分支的兜底形态（契约 §4 规格 3「字段照抄旧页 catch 分支」）：
 * 失败也是一条有效结论，照旧页填一条 code="network_error" 的完整结果。
 * gatewayName 照抄旧页填 null——旧页此时手上只有网关 id；气泡与 modelKnown 警示条
 * 都按 `gatewayName ?? gateway` 渲染，会回退显示网关 id，两种值均兼容。
 */
function networkErrorResult(
  gateway: string,
  model: string,
  prompt: string,
  maxTokens: number,
  reason: string,
): ModelTestResult {
  return {
    at: Date.now(),
    ok: false,
    code: "network_error",
    reason,
    gateway,
    gatewayName: null,
    model,
    endpoint: null,
    protocol: "openai-chat",
    prompt,
    maxTokens,
    status: null,
    latencyMs: 0,
    reply: null,
    reasoning: null,
    tokens: null,
    raw: null,
  };
}

/**
 * api.asError 的防炸包装：asError 对 string / Error 之外的 rejection 值走
 * JSON.stringify（api.ts），遇到循环引用 / BigInt 会二次 throw。这里层层兜底，
 * 保证「描述错误」这件事本身永不 throw——若 runOne 的 catch 分支自己能 throw，
 * runOne 就会 reject，执行循环「runOne 永不 reject」的不变量就塌了。
 */
function describeUnexpectedError(e: unknown): string {
  try {
    return api.asError(e);
  } catch {
    try {
      return String(e);
    } catch {
      return "未知错误";
    }
  }
}

// ---------------------------------------------------------------------------
// hook 本体
// ---------------------------------------------------------------------------

export function useModelTestChat(options: UseModelTestChatOptions): UseModelTestChat {
  const [turns, setTurns] = useState<ChatTurn[]>([]);
  const [activeTurnId, setActiveTurnId] = useState<string | null>(null);
  const [lastByModel, setLastByModel] = useState<Map<string, ModelTestResult>>(() => new Map());

  /** 组件是否仍挂载：卸载后一律不再发请求、不再 setState（契约规格 7）。 */
  const mountedRef = useRef(false);
  /** 停止位：stop()/卸载时置 true，send() 开新一轮时复位。 */
  const stopRef = useRef(false);
  /**
   * 当前进行中回合的 id；null = 空闲。
   * 双重职责：send 的「活跃期间忽略再发」防御读它；worker 比对「我这一轮
   * 还是不是活跃轮」——即契约规格 1 里的「seq 已过期」判定（id 内含模块级 seq）。
   */
  const activeRunRef = useRef<string | null>(null);
  /** resolveKnown 的最新引用：worker 是长生命周期闭包，经 ref 取用避免捕获过期的 options。 */
  const optionsRef = useRef(options);
  /** turns 的最新值：防抖定时器与卸载兜底都在 effect 闭包之外读它。 */
  const turnsRef = useRef<ChatTurn[]>([]);
  /** 落盘防抖定时器句柄；非 undefined 即「还有未落盘的变化」。 */
  const persistTimerRef = useRef<number | undefined>(undefined);
  /** 是否已完成挂载恢复：恢复完成前不落盘，避免把「尚未恢复历史」的空态误当成用户清空。 */
  const restoredRef = useRef(false);

  // 卸载守卫：setup 里重新置 true 是给 StrictMode 二次挂载用的（同一 fiber，ref 会保留 cleanup 里的 false）。
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      // 卸载等同 stop：不再发起新请求；in-flight 结果落地时由 mountedRef 挡住 setState（规格 7）
      stopRef.current = true;
    };
  }, []);

  // 挂载即恢复历史（done-only）并重建 lastByModel，让刷新后侧栏状态点与顶部统计 chips 与历史结论一致。
  useEffect(() => {
    const { turns: restoredTurns, lastByModel: restoredLast } = restoreFromStorage();
    if (restoredTurns.length > 0) {
      // 同步段先更新 turnsRef 再 setState：unmount 兜底 flush 在恢复提交前触发时也不会把空态写盘
      turnsRef.current = restoredTurns;
      setTurns(restoredTurns);
      setLastByModel(restoredLast);
    }
    restoredRef.current = true;
  }, []);

  // turns 变化 → 防抖 300ms 落盘（契约规格 6）；顺带保持 turnsRef 与最新 state 同步。
  useEffect(() => {
    turnsRef.current = turns;
    const timer = window.setTimeout(() => {
      persistTimerRef.current = undefined;
      persistTurns(turns);
    }, PERSIST_DEBOUNCE_MS);
    persistTimerRef.current = timer;
    return () => {
      // 标准防抖：turns 再变化时由下一次 effect 重排定时器，只有最后一次变化会真正落盘
      window.clearTimeout(timer);
    };
  }, [turns]);

  // 卸载兜底：防抖窗口（300ms）内还有未落盘的变化时立即写盘，避免路由切换丢掉最后一轮。
  useEffect(() => {
    return () => {
      if (persistTimerRef.current !== undefined) {
        window.clearTimeout(persistTimerRef.current);
        persistTimerRef.current = undefined;
        if (restoredRef.current) persistTurns(turnsRef.current);
      }
    };
  }, []);

  // 每次 render 后同步最新的 options：worker 闭包通过 ref 取用，不进 useCallback 依赖。
  useEffect(() => {
    optionsRef.current = options;
  });

  /** 把某个目标的占位从 pending 推进到 running（原地替换，占位保持在 targets 顺序位）。 */
  const markRunning = useCallback((turnId: string, model: string): void => {
    setTurns((prev) =>
      prev.map((turn) => {
        if (turn.id !== turnId) return turn;
        return {
          ...turn,
          results: turn.results.map((it) =>
            it.model === model && it.state === "pending" ? { model, state: "running" as const } : it,
          ),
        };
      }),
    );
  }, []);

  /**
   * 目标落地：移除该模型的 pending/running 占位，done 项按完成顺序追加到尾部
   * （对应 ChatTurn.results 的冻结注释「按完成顺序追加；done 项不可变」）。
   */
  const markDone = useCallback(
    (
      turnId: string,
      gateway: string,
      model: string,
      result: ModelTestResult,
      modelKnown: boolean | null,
    ): void => {
      setTurns((prev) =>
        prev.map((turn) => {
          if (turn.id !== turnId) return turn;
          const doneItem: TurnItem = { model, state: "done", result, modelKnown };
          return {
            ...turn,
            results: [
              ...turn.results.filter((it) => it.model !== model || it.state === "done"),
              doneItem,
            ],
          };
        }),
      );
      // 同步跨轮聚合表（key = `${gateway}::${model}`），供侧栏状态点与顶部统计 chips 使用
      setLastByModel((prev) => {
        const next = new Map(prev);
        next.set(`${gateway}::${model}`, result);
        return next;
      });
    },
    [],
  );

  /** 单模型探测：调 api.testModel；接口本身 throw（服务未启动/路由缺失）时归一成一条完整结论。 */
  const runOne = useCallback(async (turn: ChatTurn, model: string): Promise<ModelTestResult> => {
    // prompt 由调用方兜底（空串时先兜成 DEFAULT_PROMPT），hook 内透传不做二次加工
    const region = turn.targetRegions?.[model];
    const body = {
      gateway: turn.gateway,
      model,
      prompt: turn.prompt,
      maxTokens: MAX_TOKENS,
      ...(region ? { region } : {}),
    };
    try {
      return await api.testModel(body);
    } catch (e) {
      // 失败也是一条有效结论（契约 §4 规格 3），字段照抄旧页 runOne 的 catch 分支兜底形态。
      // 描述错误用防炸包装而不是裸 asError：asError 对循环引用 / BigInt 的 rejection 值
      // 会二次 throw（内部走 JSON.stringify），catch 分支自己 throw 就等于 runOne 会 reject。
      return networkErrorResult(
        turn.gateway,
        model,
        body.prompt,
        body.maxTokens,
        `无法调用自测接口：${describeUnexpectedError(e)}`,
      );
    }
  }, []);

  /** 执行一轮已入列的 turn：最多 CONCURRENCY 个 worker 领号干活，全部出结果后收尾。 */
  const startTurn = useCallback(
    (turn: ChatTurn): void => {
      const total = turn.targets.length;
      let cursor = 0; // 下一个待领取的目标下标
      let inFlight = 0; // 当前在途请求数
      let settled = false;

      /**
       * 收尾：只在所有目标都出结果（落地或因 stop 放弃）后才清 activeTurnId——
       * 用户要能看到最后一两条结果落下来（契约规格 4）。
       */
      const settle = (): void => {
        if (settled) return;
        settled = true;
        if (activeRunRef.current === turn.id) {
          activeRunRef.current = null;
          if (mountedRef.current) setActiveTurnId(null);
        }
      };

      if (total === 0) {
        // 理论不可达（send 已挡空 targets），纯防御
        settle();
        return;
      }

      const worker = async (): Promise<void> => {
        for (;;) {
          // —— 同步段：守卫检查与领号之间没有 await，单线程下不存在两个 worker 领到同一个目标 ——
          // 任一守卫不满足（已 stop / 已卸载 / seq 已过期 / 目标已领完）就退出；
          // 未开始的目标保持 pending 悬置，落盘时会被过滤掉
          if (
            stopRef.current ||
            !mountedRef.current ||
            activeRunRef.current !== turn.id ||
            cursor >= total
          ) {
            if (inFlight === 0) settle();
            return;
          }
          const model = turn.targets[cursor];
          cursor++;
          inFlight++;
          markRunning(turn.id, model);
          // runOne 内部已把 throw 归一成结果，正常路径不会 reject；这里仍兜一层底：
          // 万一 runOne 意外 reject，worker 若死在 await 处，inFlight 永不递减，
          // 且 total===1（单 lane）时再没有其他 worker 能看到 inFlight===0 去触发 settle，
          // activeTurnId 永久非空，页面就卡死在「测试中」。catch 落一条同形兜底结论 +
          // finally 保证 inFlight 必然递减，循环得以继续、settle 不变量始终成立。
          let result: ModelTestResult;
          try {
            result = await runOne(turn, model);
          } catch (unexpected) {
            result = networkErrorResult(
              turn.gateway,
              model,
              turn.prompt,
              MAX_TOKENS,
              `无法调用自测接口：${describeUnexpectedError(unexpected)}`,
            );
          } finally {
            inFlight--;
          }
          // —— 落地段 ——
          // stop 之后 in-flight 的结果照常落地（规格 4）；组件已卸载时不得 setState（规格 7）
          if (mountedRef.current && activeRunRef.current === turn.id) {
            const modelKnown = optionsRef.current.resolveKnown(turn.gateway, model);
            markDone(turn.id, turn.gateway, model, result, modelKnown);
          }
        }
      };

      // 并发 = CONCURRENCY（2）：多余的 worker 不启动，同一时刻本 turn 内至多 2 个 in-flight
      const lanes = Math.min(CONCURRENCY, total);
      for (let lane = 0; lane < lanes; lane++) void worker();
    },
    [markRunning, markDone, runOne],
  );

  const send = useCallback(
    (input: {
      gateway: string;
      gatewayName: string | null;
      prompt: string;
      targets: string[];
      targetRegions?: Record<string, string>;
    }): void => {
      // 活跃期间再次 send 直接忽略（契约规格 8）：调用方负责禁用按钮，hook 内双保险
      if (activeRunRef.current !== null) return;
      // 卸载后不再开新轮
      if (!mountedRef.current) return;
      if (input.targets.length === 0) return;

      const at = Date.now();
      const id = `${at}-${turnSeqCounter++}`;
      const turn: ChatTurn = {
        id,
        at,
        gateway: input.gateway,
        gatewayName: input.gatewayName,
        prompt: input.prompt,
        // 发送时点快照，防外部数组变异
        targets: [...input.targets],
        // 区域提示快照（WorkBuddy 池 cn/intl）；空对象不落字段，保持历史轮形状
        ...(input.targetRegions && Object.keys(input.targetRegions).length > 0
          ? { targetRegions: { ...input.targetRegions } }
          : {}),
        // 立即插入全部 pending 占位（契约规格 1）
        results: input.targets.map((model) => ({ model, state: "pending" as const })),
      };
      stopRef.current = false;
      activeRunRef.current = id;
      setActiveTurnId(id);
      setTurns((prev) => [...prev, turn]);
      startTurn(turn);
    },
    [startTurn],
  );

  const stop = useCallback((): void => {
    // 只置位；activeTurnId 等本轮 in-flight 全部落地后由执行循环 settle 时才清空（契约规格 4）
    stopRef.current = true;
  }, []);

  const clear = useCallback((): void => {
    // 仅空闲时可用（界面在测试中禁用清空按钮；hook 内同样防御）
    if (activeRunRef.current !== null) return;
    setTurns([]);
    setLastByModel(new Map());
    try {
      localStorage.removeItem(STORAGE_KEY);
    } catch {
      // localStorage 不可用（隐私模式等）时静默忽略：内存态已经清了
    }
  }, []);

  return useMemo(
    () => ({ turns, activeTurnId, lastByModel, send, stop, clear }),
    [turns, activeTurnId, lastByModel, send, stop, clear],
  );
}
