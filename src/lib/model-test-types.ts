/**
 * 模型可用性自测的类型契约。
 *
 * 与后端 `wb-switch-core::modules::model_probe::run()` 的返回字段**一一对应**，
 * 改动任一侧都要同步另一侧（后端有 `result_shape` 相关单测兜住 `code` 取值）。
 */

/** 失败归因码；`ok` 表示调用成功，其余为具体失败类型。 */
export type ModelTestCode =
  | "ok"
  /** 网关 id 不在网关清单里 */
  | "unknown_gateway"
  /** 没填模型 id */
  | "missing_model"
  /** GLM 主密钥为空，无法自证 */
  | "no_master_key"
  /** 内部 HTTP 客户端创建失败 */
  | "client_build_failed"
  /** 连不上本机网关（服务重启中 / 代理干扰） */
  | "connect_failed"
  /** 超时 */
  | "timeout"
  /** 其它网络层错误 */
  | "network_error"
  /** 401/403：密钥或登录态失效 */
  | "auth_failed"
  /** 404：模型不存在或未对该账号开放 */
  | "model_not_found"
  /** 429：限流/额度受限 */
  | "rate_limited"
  /** 上游返回 4xx/5xx，或 200 里内含 error */
  | "upstream_error"
  /** 200 但没有任何正文与思维链 */
  | "empty_reply"
  /** 200 但响应体不是合法 JSON */
  | "bad_response";

/** 一次探测的完整结果。失败时 `reason` 为可读原因，`raw` 为上游原始片段（截断）。 */
export interface ModelTestResult {
  /** 发起时间（毫秒时间戳） */
  at: number;
  ok: boolean;
  code: ModelTestCode;
  /** 人类可读的结论/原因 */
  reason: string;
  gateway: string;
  gatewayName: string | null;
  model: string;
  /** 实际打到的本机端点（便于排查 base 是否拼错） */
  endpoint: string | null;
  protocol: string;
  prompt: string;
  maxTokens: number;
  /** 上游 HTTP 状态码；网络层失败时为 null */
  status: number | null;
  latencyMs: number;
  reply: string | null;
  reasoning: string | null;
  tokens: {
    prompt: number | null;
    completion: number | null;
    total: number | null;
  } | null;
  raw: string | null;
}

/** 发起探测的入参；除 gateway/model 外均可省略。 */
export interface ModelTestRequest {
  gateway: string;
  model: string;
  /** 自定义提示词，默认「只回复两个字：可用」 */
  prompt?: string;
  /** 输出上限，默认 64，最大 8192 */
  maxTokens?: number;
  /** 超时秒数，默认 90，范围 5–600 */
  timeoutSecs?: number;
}

/** 前端渲染用的失败码文案与配色（表驱动，避免 JSX 里堆三元表达式）。 */
export const MODEL_TEST_CODE_META: Record<
  ModelTestCode,
  { label: string; tone: "ok" | "warn" | "danger" }
> = {
  ok: { label: "调通", tone: "ok" },
  unknown_gateway: { label: "网关不存在", tone: "danger" },
  missing_model: { label: "未选模型", tone: "warn" },
  no_master_key: { label: "缺少主密钥", tone: "danger" },
  client_build_failed: { label: "内部错误", tone: "danger" },
  connect_failed: { label: "连不上网关", tone: "danger" },
  timeout: { label: "超时", tone: "warn" },
  network_error: { label: "网络错误", tone: "danger" },
  auth_failed: { label: "鉴权失败", tone: "danger" },
  model_not_found: { label: "模型不存在", tone: "danger" },
  rate_limited: { label: "限流/额度不足", tone: "warn" },
  upstream_error: { label: "上游拒绝", tone: "danger" },
  empty_reply: { label: "上游空回复", tone: "warn" },
  bad_response: { label: "响应异常", tone: "danger" },
};
