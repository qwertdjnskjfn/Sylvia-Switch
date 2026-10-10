// 与 Rust 后端命令返回结构对齐的类型定义（对照 server.py 各 API 响应）

export interface AccountMeta {
  id: string;
  uid: string | null;
  email: string | null;
  nickname: string | null;
  enterpriseName: string | null;
  expiresAt: number | null;
  refreshExpiresAt: number | null;
  refreshedAt: number | null;
  createdAt: number | null;
  needsRelogin: boolean;
  needsReloginReason: string | null;
}

export interface AppStatus {
  running: boolean;
  authFile: string;
  current: {
    uid: string | null;
    /** 客户端 5.5.6+ 可能写入 $wbEncrypted 加密信封对象，后端已规范化为字符串或 null */
    nickname: string | null;
    email: string | null;
  } | null;
  appPath: string;
  /** Sylvia Switch 自身版本（= 服务二进制版本） */
  version: string;
  /** WorkBuddy 桌面客户端版本；读不到为 null */
  workbuddyVersion?: string | null;
}

export interface OAuthStartResult {
  loginId: string;
  verificationUri: string;
  expiresIn: number;
}

export interface OAuthPollResult {
  done: boolean;
  result?: AccountMeta;
  error?: string;
}

/** 导出文件中的完整账号记录（含 token，仅导出命令返回；字段与账号库原始记录一致）。 */
export interface AccountRecord {
  id?: string;
  uid?: string | null;
  nickname?: string | null;
  email?: string | null;
  access_token?: string | null;
  refresh_token?: string | null;
  token_type?: string | null;
  domain?: string | null;
  expiresAt?: number | null;
  refreshExpiresAt?: number | null;
  auth_raw?: unknown;
  profile_raw?: unknown;
  createdAt?: number | null;
  [key: string]: unknown;
}

/** 导入文件账号的脱敏预览（不含 token）。 */
export interface ImportPreviewAccount {
  index: number;
  uid: string | null;
  nickname: string | null;
  email: string | null;
  hasToken: boolean;
}

/** 导入结果计数。 */
export interface ImportResult {
  ok: boolean;
  imported: number;
  skipped: number;
  overwritten: number;
}

export interface Session {
  id: string;
  title: string;
  cwd: string;
  updatedAt: number;
  hasHistory: boolean;
  /** WorkBuddy playground（侧栏「任务」）；缺省视为空间会话。 */
  isPlayground?: boolean;
}

export interface CopyResult {
  id: string;
  newId: string;
  jsonlCopied: boolean;
  mappingWritten: boolean;
  backup: string;
}

export interface SwitchResult {
  ok: boolean;
  account: string;
  backup: string | null;
  sessionCopy?: {
    sourceUid: string;
    targetUid: string;
    copied: CopyResult[];
    errors?: { id: string; error: string }[];
  };
}

export interface CheckinConfig {
  enabled: boolean;
  /** Legacy persisted fields; accepted by the backend but ignored by scheduling. */
  start_hour?: number;
  end_hour?: number;
  keepalive_days: number;
  lazy_refresh_hours: number;
}

/** 「近期到期」窗口配置（GET/POST /api/config/expiring-soon；后端夹取 1–365）。 */
export interface ExpiringSoonConfig {
  /** 距到期还剩多少天内算「近期到期」；默认 30 */
  days: number;
}

export interface CheckinLog {
  ts: number;
  accountId: string | null;
  email: string;
  result: string;
  error?: string;
}

export interface CheckinResult {
  result: string;
  error?: string;
}

export interface TravelConfig {
  enabled: boolean;
}

export type TravelStatusLabel = "untraveled" | "no-buddy" | "traveling" | "finished";

export interface TravelStatus {
  label: TravelStatusLabel;
  rewardCredit: number | null;
  locationName?: string | null;
  arriveAt?: number | null;
}

export interface CreditResource {
  packageCode: string | null;
  packageName: string | null;
  total: number;
  remaining: number;
  used: number;
  status: number | null;
  expireAt: number | null;
  expired: boolean;
  expiringSoon: boolean;
}

export interface CreditExpiry {
  ok: boolean;
  accountId?: string | null;
  accountName?: string;
  updatedAt?: number;
  totalCapacity?: number;
  totalRemaining?: number;
  expiringSoonRemaining?: number;
  expiredRemaining?: number;
  soonestExpireAt?: number | null;
  expiringSoon?: boolean;
  expired?: boolean;
  resources?: CreditResource[];
  /** 即将过期积分（AutoClaw 平台账号，来自官方 expiring 接口） */
  expiring?: { points: number; days: number | null } | null;
  error?: string;
}

export interface CreditStatsSummary {
  currentRemaining: number;
  currentCapacity: number;
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
  todayCheckedInAccounts: number;
  todaySuccess: number;
  todayAlready: number;
  todayFailed: number;
}

export interface CreditStatsDailyPoint {
  date: string;
  usage: number;
  /** 官方用量按模型聚合（全量，不受请求明细条数限制）；本地观察口径下为空 */
  models?: { model: string; requestCount: number; credit: number }[];
}

export interface CreditStatsAccount {
  accountId: string;
  accountName: string;
  /** 所属签到平台："workbuddy" | "autoclaw" | "trae_work_cn"；历史快照账号缺省 */
  platform?: string;
  isCurrent: boolean;
  currentRemaining: number | null;
  totalCapacity: number | null;
  lastSnapshotAt: number | null;
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
  checkedInToday: boolean | null;
  checkinStatusToday: string | null;
  lastCheckinAt: number | null;
  lastCheckinResult: string | null;
  /** 按账号的逐日观察消耗（缺省兼容旧后端）；官方可用时趋势图优先使用官方 daily */
  daily?: CreditStatsDailyPoint[];
}

export interface CreditStatsUsageEvent {
  kind: "usage";
  ts: number;
  date: string;
  accountId: string;
  accountName: string;
  amount: number;
}

export interface CreditStatsCheckinEvent {
  kind: "checkin";
  ts: number;
  date: string;
  accountId: string | null;
  accountName: string;
  result: string;
  error?: string | null;
}

export type CreditStatsEvent = CreditStatsUsageEvent | CreditStatsCheckinEvent;

export type CreditOfficialUsageStatus = "complete" | "partial" | "unavailable";

export interface CreditOfficialUsageSummary {
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
}

export interface CreditOfficialUsageModel {
  model: string;
  requestCount: number;
  credit: number;
}

export interface CreditOfficialUsageAccount {
  accountId: string;
  accountName: string;
  ok: boolean;
  requestCount: number;
  detailTruncated: boolean;
  usageToday: number | null;
  usage7Days: number | null;
  usageThisMonth: number | null;
  error?: string | null;
  reportedTotal?: number | null;
  fetchedCount?: number;
  /** 缺省兼容旧后端响应。 */
  models?: CreditOfficialUsageModel[];
  /** 按账号的逐日官方消耗（全量聚合，不受 requests 明细上限影响；缺省兼容旧后端） */
  daily?: CreditStatsDailyPoint[];
}

export interface CreditOfficialUsageRequest {
  accountId: string;
  accountName: string;
  requestId: string;
  credit: number;
  model: string;
  client: string;
  requestTime: string;
}

export interface CreditOfficialUsageError {
  accountId: string;
  accountName: string;
  error: string;
}

export interface CreditOfficialUsage {
  status: CreditOfficialUsageStatus;
  rangeStart: string;
  rangeEnd: string;
  /** 官方用量最近一次采集时间；缓存命中时保持采集当时的时间。 */
  collectedAt?: number;
  summary: CreditOfficialUsageSummary;
  daily: CreditStatsDailyPoint[];
  accounts: CreditOfficialUsageAccount[];
  requests: CreditOfficialUsageRequest[];
  /** 官方全部有效请求按模型汇总；不受 requests 明细上限影响。 */
  models?: CreditOfficialUsageModel[];
  detailLimitPerAccount: number;
  errors: CreditOfficialUsageError[];
}

export interface CreditStatistics {
  generatedAt: number;
  retentionDays: number;
  coverageStartAt: number | null;
  summary: CreditStatsSummary;
  daily: CreditStatsDailyPoint[];
  accounts: CreditStatsAccount[];
  events: CreditStatsEvent[];
  /** 官方接口不可用时仍使用上述本地观察字段；缺省兼容旧后端。 */
  officialUsage?: CreditOfficialUsage;
}

export interface TokenStatsTotals { total: number; input: number; output: number; cacheRead: number; cacheWrite: number; uncachedInput: number; records: number; cacheHitRate: number | null; }
export interface TokenStatsGroup extends TokenStatsTotals { key: string; title?: string | null; project?: string; sessionId?: string; }
export interface TokenStatsSource { source: "workbuddy" | "autoclaw" | "minimax"; summary: TokenStatsTotals; models: TokenStatsGroup[]; projects: TokenStatsGroup[]; sessions: TokenStatsGroup[]; daily: TokenStatsGroup[]; /** Optional model-specific daily series for trend filtering. */ dailyByModel?: Record<string, TokenStatsGroup[]>; hours: TokenStatsGroup[]; filesScanned: number; parseErrors: number; coverageStartAt?: number | null; coverageEndAt?: number | null; }
export interface TokenStatistics { generatedAt: number; rangeDays?: number | null; sources: TokenStatsSource[]; }

export interface GithubConfig {
  owner?: string;
  repo?: string;
  proxy?: string;
}

export interface UpdateInfo {
  ok: boolean;
  current?: string;
  latest?: string;
  latestTag?: string;
  hasUpdate?: boolean;
  releaseName?: string;
  releaseUrl?: string;
  publishedAt?: string;
  error?: string;
  message?: string;
}

export type PlatformCheckinItem = {
  accountId: string;
  email: string;
  platform: string;
  ok: boolean;
  todayCheckedIn: boolean;
  /** 该条目是否为本机客户端当前登录的账号（一键切换时用于标注「当前」） */
  localLogin?: boolean;
  /** AutoClaw 平台账号的区域标签（international / domestic） */
  region?: 'international' | 'domestic';
  error?: string;
  /** 积分/额度，结构与 WorkBuddy 的 CreditExpiry 一致 */
  credit?: CreditExpiry;
};

export interface GlmProxyModelInfo {
  id: string;
  alias: string;
  routeId: string;
  /** 消耗等级：低 / 中 / 高 */
  consumption?: string | null;
  contextWindow?: number | null;
  maxTokens?: number | null;
  /** 支持的输入模态，如 ["text","image"] */
  inputs?: string[] | null;
  reasoning?: boolean | null;
  tooltip?: string | null;
  /** Auto 系背后的真实模型名 */
  autoModel?: string | null;
  /** 模型归属区域（international / domestic）：请求该模型只路由到对应区域的账号池 */
  region?: 'international' | 'domestic';
}

export interface GlmProxyConfigInfo {
  enabled: boolean;
  apiKey: string;
  endpoint: string;
  autoclawReady: boolean;
  /** 速度优先：把慢模型改投到更快的目标模型 */
  speedFirst: boolean;
  speedFirstTarget: string;
  /** 会被改投的模型列表 */
  speedFirstSources: string[];
  /** 账号策略：round_robin（轮询）/ local_first（本机优先）/ local_only（只用本机）/ pinned（固定账号） */
  accountMode: string;
  /** pinned 策略使用的账号 id */
  pinnedAccountId: string;
  /** 全部合法策略取值 */
  accountModes?: string[];
  models: GlmProxyModelInfo[];
}

export interface GlmProxyStatus {
  tokenReady: boolean;
  gatewayOk: boolean;
  latencyMs: number;
  error?: string | null;
  models: number;
  checkedAt: number;
  proxy?: {
    enabled: boolean;
    endpoint: string;
    models: { id: string }[];
  };
}

export interface GlmUsageBucket {
  calls: number;
  failedCalls?: number;
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
  credits: number;
  creditsPendingCount: number;
  cachedTokens?: number;
  uncachedPromptTokens?: number;
  cacheHitCalls?: number;
  cacheHitRate?: number;
  savedTokens?: number;
  savedCredits?: number;
}

export interface GlmUsageWindow {
  total: GlmUsageBucket;
  models: Record<string, GlmUsageBucket>;
}

/** 模型监控：窗口内单个模型的汇总（successRate / share 均为 0~1 的小数）。 */
export interface ModelMonitorItem {
  model: string;
  calls: number;
  okCalls: number;
  failedCalls: number;
  successRate: number;
  /** 该模型调用次数占窗口内总调用次数的比例（调用率） */
  share: number;
  avgLatencyMs: number;
  maxLatencyMs: number;
  /** 首字延迟（预填充+排队耗时） */
  avgTtfbMs: number;
  maxTtfbMs: number;
  /** 出字阶段耗时（总延迟 - 首字延迟），近似「思考+生成」 */
  avgStreamMs: number;
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
  cachedTokens?: number;
  cacheHitRate?: number;
  savedTokens?: number;
  credits: number;
  lastTs: number | null;
}

/** 趋势序列上的一个时间桶。 */
export interface ModelMonitorPoint {
  ts: number;
  total: number;
  failed: number;
  /** 模型名 -> 该桶内调用次数（无调用的模型不出现） */
  byModel: Record<string, number>;
}

export interface ModelMonitorSummary {
  calls: number;
  okCalls: number;
  failedCalls: number;
  successRate: number;
  avgLatencyMs: number;
  avgTtfbMs: number;
  maxTtfbMs: number;
  avgStreamMs: number;
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
  cachedTokens?: number;
  cacheHitRate?: number;
  savedTokens?: number;
  credits: number;
  modelCount: number;
}

export interface ModelMonitor {
  generatedAt: number;
  from: number;
  to: number;
  windowHours: number;
  bucketMs: number;
  provider?: string;
  summary: ModelMonitorSummary;
  models: ModelMonitorItem[];
  /** 序列中出现过的模型（按调用次数降序） */
  seriesModels: string[];
  series: ModelMonitorPoint[];
}

export interface AccountUsageOverview {
  provider: "autoclaw" | "loomy" | "trae_work_cn" | string;
  name: string;
  label: string;
  balance?: number | null;
  balanceUnit: string;
  calls: number;
  okCalls: number;
  failedCalls: number;
  successRate: number;
  avgLatencyMs: number;
  avgTtfbMs: number;
  promptTokens: number;
  completionTokens: number;
  cachedTokens: number;
  uncachedPromptTokens: number;
  cacheHitCalls: number;
  cacheHitRate: number;
  totalTokens: number;
  credits: number;
  lastTs?: number | null;
  modelsCount: number;
  status: "active" | "idle";
}

export interface AccountsOverviewResponse {
  ok: boolean;
  generatedAt: number;
  accounts: AccountUsageOverview[];
}

export interface GlmUsageStatistics {
  generatedAt: number;
  reconcileMatched: number;
  balance: {
    current: number | null;
    dailyAvg7d: number;
    daysRemaining: number | null;
  };
  windows: {
    today: GlmUsageWindow;
    sevenDays: GlmUsageWindow;
    all: GlmUsageWindow;
  };
}

export interface GlmUsageRecord {
  id: string;
  ts: number;
  model: string;
  routeId: string;
  accountId?: string | null;
  accountLabel?: string | null;
  ledgerId?: string | null;
  promptTokens: number;
  completionTokens: number;
  reasoningTokens?: number | null;
  totalTokens: number;
  cachedTokens?: number;
  uncachedPromptTokens?: number;
  cacheHit?: boolean;
  credits?: number | null;
  creditsPending?: boolean;
  latencyMs: number;
  /** 首字延迟（预填充时间），仅流式调用记录 */
  ttfbMs?: number | null;
  /** 速度优先改投时，客户端原始请求的模型名 */
  requestedModel?: string | null;
  /** 是否被速度优先改投 */
  substituted?: boolean;
  /** 命中慢模型但请求是多模态时的跳过原因（如 "multimodal"） */
  substitutionSkipped?: string | null;
  ok?: boolean;
  error?: string | null;
}

export interface AutoclawAccountInfo {
  id: string;
  label: string;
  source: string;
  region: 'international' | 'domestic';  // v1.0+: 区域标识
  is_manual_specified: boolean;  // v1.0+: 是否手动指定区域
  status: string;
  lastError?: string | null;
  addedAt?: number | null;
  updatedAt?: number | null;
  tokenPreview?: string;
}

export type QwenChannel = 'auto' | 'web' | 'qoder';

export interface QwenChannelInfo {
  channel: QwenChannel;
  autoFallback: boolean;
  channels: QwenChannel[];
  channelLabels: Record<string, string>;
}

export interface QwenWebProbe {
  /** 是否已产生过探活结果 */
  probed: boolean;
  /** 探活是否成功（真实一次短调用）；未探活前为 null */
  probeOk: boolean | null;
  /** 距上次探活的秒数 */
  probeAtAgeSecs?: number;
  probeLatencyMs?: number;
  probeChannel?: string;
  probeError?: string | null;
  /** 探活任务是否在执行中 */
  probeInFlight: boolean;
  probeTtlSecs: number;
  note?: string;
}

export interface QwenWebChannelStatus {
  available: boolean;
  tokenReady: boolean;
  browserRunning: boolean;
  browserPort: number;
  pageReady: boolean;
  pageLatencyMs: number | null;
  modelsCount: number;
  upstream: string;
  version: string;
}

export interface QwenChannelsStatus {
  current: QwenChannel;
  autoFallback: boolean;
  web: QwenWebChannelStatus;
  webProbe: QwenWebProbe;
  qoder: { poolTotal: number; poolOk: number; available: boolean; upstream: string };
  tokenUser: string | null;
}

export interface ToolCallMonitorStats {
  total: number;
  withTools: number;
  called: number;
  direct: number;
  callRate: number;
  okRate: number;
  transitions: number;
  flips: number;
  fluctuationRate: number;
  lastFlipTs: number | null;
  verdict: string;
  verdictKind: "lite-fluctuation" | "insufficient-data" | "upstream-issue" | "protocol-not-triggering";
  verdictDetail: string;
  isConfigProblem: boolean;
  temperature: {
    missing: number;
    distribution: Array<{ value: string; count: number }>;
    note: string;
  };
  trend: Array<{ bucket: number; total: number; called: number; callRate: number | null }>;
  updatedAt: number;
}

export interface ToolCallMonitorEvent {
  ts: number;
  hadTools: boolean;
  called: boolean;
  toolNames: string[];
  latencyMs: number;
  ok: boolean;
  error: string | null;
  temperature: number | null;
  channel: string;
  tier: string;
  requestedModel: string;
}

export interface ToolCallMonitor {
  stats: ToolCallMonitorStats;
  events: ToolCallMonitorEvent[];
  advisories: {
    accept: { title: string; body: string };
    reduceRandomness: { title: string; body: string };
    switchTier: { title: string; body: string };
  };
}

export interface GatewayApiKey {
  id: string;
  name: string;
  keyPreview: string;
  createdAt: number | null;
  lastUsedAt: number | null;
  requestCount: number;
  enabled: boolean;
  /** 仅创建响应里返回完整密钥 */
  key?: string;
  /** 区域归属：cn=WorkBuddy 国内版专用 / intl=国际版专用；null=通用（无区域约束） */
  region?: string | null;
}

export interface GatewayLogEntry {
  ts: number;
  gateway: string;
  keyId: string | null;
  model: string;
  ok: boolean;
  latencyMs: number;
  error?: string | null;
}

export interface GatewayOverview {
  requests24h: number;
  ok24h: number;
  err24h: number;
  byGateway: Record<string, number>;
  keyCount: number;
  accountPools: { qoder: number; kimi: number };
}

export interface KimiAccountInfo {
  id: string;
  name: string;
  tokenType: string;
  tokenPreview: string;
  enabled: boolean;
  maxConcurrency: number;
  minIntervalSeconds: number;
  hasCachedAccess: boolean;
  accessExpiresAt: number | null;
  status: string;
  error?: string | null;
  cooldownSeconds: number;
}

export interface QoderAccountInfo {
  uid: string | null;
  name?: string | null;
  email?: string | null;
  userTag?: string | null;
  quota?: number | null;
  isQuotaExceeded?: boolean | null;
  source?: string | null;
  status?: string | null;
  enabled?: boolean;
  error?: string | null;
  /** 账号池归属：qoder 默认池 / qwen 独立池 / qwen-flash CN 专用池 */
  gateway?: string | null;
  /** 地域：cn / intl（regionSource=manual 时为手动指定，否则为自动推断） */
  region?: string | null;
  regionLabel?: string | null;
  regionSource?: string | null;
  /** 软件来源标签（后端 with_region 填充）：Qoder（国际版）/ QoderCN / QoderWork CN / 设备授权（Qoder CN）/ 手动导入 */
  sourceLabel?: string | null;
}

export interface TraeAccountInfo {
  id: string;
  label: string;
  userId?: string | null;
  status: string;
  error?: string | null;
  expiredAt?: string | null;
  tokenPreview?: string | null;
  updatedAt?: number | null;
  /** 该账号正是客户端当前登录账号（由实时代管，不重复参与签到） */
  localLogin?: boolean;
  /** 快照 access token 已到期（TRAE 池无法跨客户端切换续期，需重新导入） */
  expired?: boolean;
}

export interface TraeCheckinResult {
  id: string;
  label: string;
  result: string;
  error?: string | null;
}

export interface LoomyCreditInfo {
  ok: boolean;
  balance?: number;
  dailyBalance?: number;
  error?: string;
  recentRecords?: { ledgerId: string; description: string; direction: string; createdAt: number; points: number; source: string }[];
}

export interface LoomyAccountInfo {
  id: string;
  label: string;
  source?: string;
  status: string;
  lastError?: string | null;
  addedAt?: number | null;
  updatedAt?: number | null;
  /** session 有效期（天）与剩余天数（本机实时账号才带） */
  sessionDays?: number;
  expiresDays?: number;
  expiresAt?: number | null;
  credit?: LoomyCreditInfo;
}

export interface LoomyCheckinResult {
  id: string;
  label: string;
  result: string;
  error?: string | null;
}

export interface LoomyProxyStatus {
  enabled: boolean;
  upstream: string;
  poolSize: number;
  models: { ok: boolean; count?: number; error?: string };
  lastError?: { ts: number; error: string } | null;
}

export interface LoomyProxyConfigInfo {
  enabled: boolean;
  apiKey: string;
  endpoint: string;
  modelsEndpoint: string;
}

export interface LoomyModelCapabilities {
  reasoning?: boolean;
  vision?: boolean;
  function_calling?: boolean;
  streaming?: boolean;
  system_message?: boolean;
  input_modalities?: string[];
}

export interface LoomyModel {
  id: string;
  name?: string;
  type?: string;
  protocol?: string;
  context_length?: number | null;
  max_output_tokens?: number | null;
  reasoning_efforts?: string[] | null;
  default_reasoning_effort?: string | null;
  capabilities?: LoomyModelCapabilities;
  /** 从名称里解析的积分倍率（如 "x3.0" → 3） */
  multiplier?: number | null;
  free?: boolean;
}

export interface AutoclawCheckinResult {
  id: string;
  label: string;
  result: string;
  error?: string;
}

export interface AccountProtectionConfigInfo {
  enabled: boolean;
  keepaliveEnabled: boolean;
  keepaliveIntervalMinutes: number;
  maxConcurrencyPerAccount: number;
  cooldownMinutes: number;
  jwtRefreshAheadMinutes: number;
}

export interface CoolingAccountInfo {
  accountId: string;
  label: string;
  cooledAt: number;
  cooldownUntil: number;
  remainingSeconds: number;
  reason: string;
}

export interface ProtectionLogEntry {
  ts: number;
  accountId: string;
  label: string;
  action: string;
  detail: string;
}

export interface AccountProtectionStatusInfo {
  config: AccountProtectionConfigInfo;
  status: "active" | "disabled";
  lastKeepaliveAt: number;
  nextKeepaliveAt: number;
  coolingAccounts: CoolingAccountInfo[];
  activeConcurrency: Record<string, number>;
  poolAccountCount: number;
  simulatedUserAgent: string;
  recentLogs: ProtectionLogEntry[];
}

export interface GrokAccountInfo {
  id: string;
  name: string;
  provider: "web" | "build" | "console" | string;
  tokenMasked?: string;
  tier?: string;
  enabled: boolean;
  status: "ok" | "error" | "cooldown" | string;
  error?: string | null;
  cooldownUntil?: number;
  isCooling?: boolean;
  lastUsedAt?: number | null;
  addedAt?: number;
  updatedAt?: number;
}

export interface GrokPoolAccount {
  id: string;
  name: string;
  provider: string;
  status: string;
  healthScore: number;
  riskLevel: "low" | "medium" | "high" | string;
  consecutiveFailures: number;
  rateLimitCount: number;
  lastSuccessAt?: number | null;
  lastCheckAt?: number | null;
  lastErrorKind?: string | null;
  cooldownUntil?: number;
}

export interface GrokPoolPolicy {
  autoCheckEnabled: boolean;
  checkIntervalMinutes: number;
  batchSize: number;
  riskLowThreshold: number;
  riskHighThreshold: number;
  pauseHighRisk: boolean;
  successDelta: number;
  rateLimitPenalty: number;
  timeoutPenalty: number;
  fatalPenalty: number;
  /** 池容量下限：可用账号数低于该值即触发容量守卫（告警 + 可选暂停调度） */
  minPoolSize: number;
  /** 低于容量下限时暂停调度（选号直接失败并给出原因）；关闭时仅告警不暂停 */
  pauseWhenBelowMin: boolean;
  /** 账号驻留时长（分钟）：轮询切号后账号保持使用的最短时长，范围 5-120，缺省 30（旧配置文件/旧后端缺字段时前端显示 30） */
  resident_ttl_min?: number;
}

export interface GrokPoolSnapshot {
  at: number;
  total: number;
  ok: number;
  errored: number;
  cooling: number;
  avgHealth: number;
  risk: { low: number; medium: number; high: number };
}

export interface GrokPoolCapacity {
  /** 容量下限（>=1） */
  minPoolSize: number;
  /** 是否启用「低于下限暂停调度」 */
  pauseWhenBelowMin: boolean;
  /** 当前可用账号数（与选号层同一判定：启用 + 未冷却 + error 退避期已过） */
  available: number;
  /** 是否已低于下限（pauseWhenBelowMin 开启时才为真） */
  belowMin: boolean;
}

export interface GrokPoolOverview {
  total: number;
  ok: number;
  errored: number;
  cooling: number;
  risk: { low: number; medium: number; high: number };
  capacity: GrokPoolCapacity;
  policy: Partial<GrokPoolPolicy>;
  accounts: GrokPoolAccount[];
}

/**
 * 「注册机」状态（GET /api/grok/register/status）。
 * 注册机 = Grok 融合网关驱动的专用 Chrome 实例（独立 profile、CDP 端口 19233）。
 * 注意：该接口是只读探测，不触发注册、不启动浏览器；注册功能默认关闭、零自动触发。
 */
export interface GrokRegisterStatus {
  /** idle = 待启动/已关闭；open = 已手动打开；captured = 已抓取入池 */
  phase: "idle" | "open" | "captured" | string;
  /** 专用 Chrome 实例是否在运行（CDP 19233 实时探测） */
  browserRunning: boolean;
  cdpPort: number;
  profileDir: string;
  registerUrl: string;
  manualOpenCount: number;
  lastOpenedAt: number | null;
  lastCapturedAt: number | null;
  /** unknown = 无 token 文件；not_logged_in = 未登录（可注册）；logged_in = 已登录 */
  loginState: "unknown" | "not_logged_in" | "logged_in" | string;
  /** sso cookie 前 8 位 + "…"；未登录为 null */
  ssoPreview: string | null;
  cookieCount: number | null;
  /** 抓取并入池后的账号 ID（固定 grok_register）；未入池为 null */
  poolAccountId: string | null;
  /** 入池账号当前 status（ok/error/...）；未入池为 null */
  poolAccountStatus: string | null;
  /** 恒 false：注册机无任何启动/后台自动触发逻辑 */
  autoTrigger: boolean;
  autoTriggerNote: string;
  /** 按 phase/loginState 组合给出的中文人话说明 */
  message: string;
}

export interface GrokTestResult {
  ok: boolean;
  reply: string;
  model: string;
  provider: string;
  accountId: string;
  accountName: string;
  durationMs: number;
  tokens?: number;
}

// ---- 妙手（美团智能体桌面应用）账号池与积分（仅 webui 后端支持）----

/** 妙手账号积分信息：余额 + 套餐 + 下次刷新时间（积分按月刷新）。 */
export interface MiaoshouCreditInfo {
  /** 积分余额 */
  balance: number | null;
  /** 套餐名（如「体验版」） */
  planName: string | null;
  /** 下次积分刷新时间（毫秒时间戳；null 表示未知） */
  nextRefreshTime: number | null;
}

/** 妙手账号池条目（GET /api/miaoshou/accounts）。 */
export interface MiaoshouAccountInfo {
  id: string;
  /** 账号库内部 uid（移除账号时使用） */
  uid?: string | null;
  /** 妙手侧用户 id */
  miaoshouUid?: string | null;
  /** 登录标识（手机号 / 邮箱） */
  email?: string | null;
  nickname?: string | null;
  /** 来源平台标识 */
  platform?: string | null;
  /** 是否本机桌面端登录态导入 */
  isDesktop?: boolean;
  /** 账号级积分（后端附带时返回） */
  credit?: MiaoshouCreditInfo | null;
}

/**
 * 妙手账号状态条目（GET /api/miaoshou/status）。
 * 注意：妙手无每日签到功能（积分按月刷新），checkinSupported 恒为 false、
 * todayCheckedIn 恒为 null —— 前端据此展示「无每日签到」而非签到按钮。
 */
export interface MiaoshouStatusItem {
  id: string;
  ok: boolean;
  /** 是否支持每日签到；妙手恒为 false */
  checkinSupported: false;
  /** 积分余额 */
  balance: number | null;
  /** 套餐名（如「体验版」） */
  planName: string | null;
  /** 下次积分刷新时间（毫秒时间戳） */
  nextRefreshTime: number | null;
  /** 后端备注（如登录态异常原因） */
  note?: string | null;
  /** 今日是否已签到；妙手恒为 null（无签到概念） */
  todayCheckedIn: null;
}

/** 妙手签到结果（保留接口；妙手实际无每日签到，后端恒返回不支持）。 */
/**
 * 妙手接入信息（GET /api/miaoshou/status 的顶层字段，2026-10-10 新增）。
 *
 * `basePath` 是**相对路径**，前端拼 `window.location.origin` 得到完整 Base URL
 * —— 端口变化时无需改动（网关端口可被 launcher 调整）。
 */
export interface MiaoshouAccessInfo {
  apiKey: string;
  basePath: string;
}

export interface MiaoshouModelInfo {
  id: string;
  object: string;
  created: number;
  owned_by: string;
  description?: string | null;
  /** 是否支持图像输入 */
  supportsImages?: boolean;
  /** 上下文档位（token 数，取最大档） */
  contextWindow?: number;
  /** 妙手侧 modelTypeId */
  modelTypeId?: number | null;
  /** 积分倍率（0 = 免费） */
  rateMultiplier?: string | null;
  /** 营销标签（如「限时免费」「新」） */
  marketingCopy?: string | null;
}

export interface MiaoshouCheckinResult {
  id: string;
  ok: boolean;
  result: string;
  error?: string;
}

// ---- WorkBuddy 多账号网关（cloud 账号池 · /workbuddy-pool/v1）----
// 字段名与后端实际输出逐字对齐（勿改）：池状态与 accounts[] 由
// workbuddy_gateway.rs:1504-1529 `pool_status()` 以 snake_case 输出；
// GatewayConfig 由 api.rs:4599-4613 `workbuddy_pool_config_json` 以 camelCase 输出。
// token 类字段一律不出现；uid 在状态里已脱敏（仅前 8 位）。

/** 单个账号的模型级冷却明细（后端 `model_cools[]` 元素，snake_case）。 */
export interface WorkbuddyModelCooling {
  /** 被冷却的模型名 */
  model: string;
  /** 冷却截止（ms epoch） */
  until: number;
  /** 剩余冷却秒数 */
  remaining_sec: number;
  /** 冷却原因（含模型名与重置时间的人话文案） */
  reason: string;
  /** true = 到期时间取自上游文案解析；false = 回退固定软冷却时长 */
  reset_at_parsed: boolean;
}

/** 账号池条目（脱敏视图；不含 access_token / refresh_token）。 */
export interface WorkbuddyPoolAccount {
  /** 账号 uid（状态脱敏：仅前 8 位 + "…"） */
  uid: string;
  /** 昵称；账号库缺省为空串 */
  nickname: string;
  /** 账号区域："cn"（国服）| "intl"（国际版） */
  region: string;
  /** 积分余额；0 = 未知（尚未巡检） */
  credits: number;
  /** 积分到期日（ms epoch）；0 = 未知 */
  credit_expires_at: number;
  /** 账号级冷却类型："soft_rate" | "hard_credit"；空串 = 不在冷却期 */
  cooldown_kind: string;
  /** 账号级冷却截止（ms epoch）；0 = 不在冷却期 */
  until: number;
  /** 熔断截止（ms epoch）；0 = 未熔断 */
  breaker_until: number;
  /** 在途请求数 */
  in_flight: number;
  /** 模型级冷却明细（仅这些模型不可用，换模型仍可用） */
  model_cools: WorkbuddyModelCooling[];
  /** 最近被选中时刻（ms epoch）；0 = 从未使用 */
  last_used: number;
  /** session 死亡（12153）→ 已出池，需人工重登 */
  disabled: boolean;
  /** 出池 / 冷却原因（后端备注） */
  reason: string;
  /** 累计成功次数 */
  success_count: number;
  /** 累计错误次数 */
  err_total: number;
}

/** 池状态总览（契约 §13 pool_status()：五个计数 + 账号明细，snake_case）。 */
export interface WorkbuddyPoolStatus {
  /** 账号库中 WorkBuddy 账号总数 */
  total: number;
  /** 可用账号数（未冷却 / 未熔断 / 未出池） */
  healthy: number;
  /** 账号级冷却中的数量 */
  cooling: number;
  /** 有模型级冷却的账号数量（uid+model 维度计数不在此列） */
  model_cooling: number;
  /** 熔断中的账号数量 */
  breaker: number;
  /** 账号明细（脱敏） */
  accounts: WorkbuddyPoolAccount[];
}

/** 网关配置（契约 §13 GatewayConfig；api.rs:4599 输出为 camelCase，
 *  PATCH /workbuddy-pool/v1/config 请求体同键名）。 */
export interface WorkbuddyGatewayConfig {
  /** 选号模式：balance（均衡分层）| rotate（烧号轮询）| pinned（指定账号） */
  mode: "balance" | "rotate" | "pinned";
  /** pinned 模式固定的账号 uid（完整值，状态表展示的是脱敏值） */
  pinnedUid: string;
  /** 软限流冷却基础秒数（429 普通限流，指数退避起点） */
  softRateBaseSecs: number;
  /** 软限流冷却上限秒数（指数退避封顶） */
  softRateMaxSecs: number;
  /** 熔断触发阈值（连续失败次数） */
  breakerThreshold: number;
  /** 熔断基础冷却秒数 */
  breakerBaseSecs: number;
  /** 熔断冷却上限秒数（指数增长封顶） */
  breakerMaxSecs: number;
  /** 单账号最大在途请求数；0 = 不限 */
  maxInFlight: number;
  /** 会话粘性 TTL（毫秒；conversation_id 绑号） */
  stickyTtlMs: number;
  /** 出站脱敏开关（请求体指纹字段清洗，默认开） */
  sanitizeEnabled: boolean;
  /** 余额巡检间隔（分钟；驱动到期分层） */
  balanceRefreshMinutes: number;
}


// ---- ChatGPT 通道（契约 docs/chatgpt-channel-contract.md §3/§4/§6；字段对齐后端
//      chatgpt_channel::status_value() 聚合与工具链 webui 透传键，2026-09-30 grep 实测）----

/** GET /api/chatgpt/channel/status 响应（后端 chatgpt_channel::status_value()）。 */
export interface ChatGptChannelStatus {
  /** 通道开关（~/.wb-switch/chatgpt_channel.json 的 enabled） */
  enabled: boolean;
  /** WebUI 托管进程是否存活（后端 GET /api/health 探活） */
  webuiRunning: boolean;
  /** WebUI 端口（配置 webui_port，默认 8765） */
  webuiPort: number;
  /** gpt-reg-review 项目目录；未找到为空串（页面给出指引） */
  projectDir: string;
  /** 注册机实时快照（= 工具链 GET /api/auto/status 透传）；WebUI 未运行为 null */
  registrar: ChatGptRegistrarStatus | null;
  /** 已注册 ChatGPT 账号数（= /api/registered 的 total） */
  accountCount: number;
  /** 邮箱号池可用数（= /api/accounts 的 available） */
  poolCount: number;
}

/** 注册机快照（工具链 webui/auto_loop.py:215-249 _snapshot()；枚举 :45-48）。 */
export interface ChatGptRegistrarStatus {
  /** stopped | running | paused */
  state: "stopped" | "running" | "paused" | string;
  started_at: number | null;
  /** 本轮已运行秒数 */
  elapsed: number;
  registered_ok: number;
  registered_fail: number;
  /** 目标成功数（0=不限量；通道页启动时显式传 ≥1） */
  target_count: number;
  remaining: number | null;
  concurrency: number;
  last_message: string | null;
}

/** 批量注册启动请求体（透传工具链 AutoLoopStartReq，webui/app.py:2047-2072；
 *  target_count 必填 ≥1——0=不限量会导致无限跑，通道页前端强制拦截）。 */
export interface ChatGptRegistrarStartBody {
  target_count: number;
  concurrency?: number;
  engine?: "protocol" | "browser" | string;
  proxy?: string;
  proxy_pool?: string[];
  otp_timeout?: number;
}

/** 已注册 ChatGPT 账号条目（GET /api/chatgpt/registered items；
 *  工具链 webui/db.py:1097-1145——token 只回长度不回原文）。 */
export interface ChatGptAccount {
  email: string;
  /** access_token 长度（0=无） */
  at_len: number;
  /** session_token 长度（0=无） */
  st_len: number;
  /** refresh_token 长度（0=无） */
  rt_len: number;
  /** 注册完成时刻（unix 秒，SQLite REAL） */
  created_at: number | null;
  /** 邮箱类型（icloud_relay / mail_com / outlook 等） */
  mail_kind?: string | null;
}

/** GET /api/chatgpt/registered 响应（app.py:823-827）。 */
export interface ChatGptRegisteredResponse {
  ok: boolean;
  items: ChatGptAccount[];
  total: number;
}

/** 邮箱号池条目（GET /api/chatgpt/accounts items = outlook_accounts 表 SELECT *，
 *  db.py:332-354；是邮箱号池，不是 ChatGPT 账号——勿混用）。 */
export interface ChatGptPoolAccount {
  email: string;
  status?: string | null;
  kind?: string | null;
  imported_at?: number | null;
}

/** GET /api/chatgpt/accounts 响应（app.py:465-476）。 */
export interface ChatGptAccountsResponse {
  ok: boolean;
  items: ChatGptPoolAccount[];
  total: number;
  /** 按邮箱类型分组统计：{ kind: { status: n, ..., total: n } } */
  by_kind?: Record<string, Record<string, number>> | null;
}

// ---- DuMate（百度搭子）渠道（契约 docs/dumate-channel-contract.md §2.3/§4.1）----
// 说明：上游 console.bce.baidu.com 的积分数值为字符串（如 "55.73"），前端按
// string | number 容错展示；键不省略（后端恒输出全部键，缺省仅兼容旧后端）。

/** 订阅包 / 加油包条目（quota_overview 的 subscription[] / incremental[] 元素）。 */
export interface DumatePointsPackage {
  packageId?: string | null;
  usedPoints?: string | number | null;
  totalPoints?: string | number | null;
  startDate?: string | null;
  expireDate?: string | null;
  packageType?: string | null;
  source?: string | null;
  status?: string | null;
  autoRenewStatus?: string | null;
}

/** 积分余额（GET /api/dumate/credits = dumate::get_credits() 组装，契约 §2.2）。 */
export interface DumateCredits {
  ok: boolean;
  /** 积分总额（上游为字符串，如 "16020.00"） */
  totalPoints?: string | number | null;
  /** 已用积分（上游为字符串，如 "55.73"） */
  usedPoints?: string | number | null;
  /** 可用积分（后端按 total-used 计算） */
  availablePoints?: string | number | null;
  isSubscribed?: boolean;
  subscription?: DumatePointsPackage[];
  incremental?: DumatePointsPackage[];
  /** 模型限流中（modelThrottleInfo.throttled） */
  throttled?: boolean;
  /** 上游限流原因（throttleReason/throttleType 为 additive 容错字段） */
  throttleReason?: string | null;
  throttleType?: string | null;
  error?: string | null;
}

/** DuMate 渠道状态（GET /api/dumate/status，契约 §2.3；camelCase、键不省略）。 */
export interface DumateStatus {
  ok: boolean;
  /** tasklist 有 DuMate.exe / dumate-*.exe */
  desktopRunning: boolean;
  /** 引擎发现成功（dumate-opencode.exe 进程环境读取） */
  engineDiscovered: boolean;
  /** 引擎地址（如 http://127.0.0.1:52002）；未发现为 null */
  engineUrl: string | null;
  /** GET /global/health 探活 */
  engineAlive: boolean;
  /** inapp key 前 6 位 + "…"；未发现为 null */
  inappKeyPreview: string | null;
  /** auth.json + .cookie-key 都存在 */
  credentialsPresent: boolean;
  /** /dumate/v1 网关开关 */
  gatewayEnabled: boolean;
  /** 网关密钥掩码预览 */
  apiKeyPreview: string | null;
  /** 积分快照（查询失败为 null） */
  credits: DumateCredits | null;
  /** 中文人话状态说明 */
  message: string;
}

/**
 * 每日签到状态（GET /api/dumate/checkin-status）。
 * 后端聚合 loginBonusInfo：ok=false + reason="login_expired" 表示登录态失效。
 */
export interface DumateCheckinStatus {
  ok: boolean;
  /** 今日是否已领（result.hasIssued） */
  todayCheckedIn: boolean;
  /** 累计签到次数（result.totalTimes） */
  totalTimes?: number | null;
  /** 累计获得积分（result.totalPoints） */
  totalPoints?: string | number | null;
  /** 已签日期列表（YYYY-MM-DD，result.signInDays） */
  signInDays?: string[];
  /** 失败原因标记（如 "login_expired"） */
  reason?: string | null;
  /** 人话说明 */
  message?: string | null;
  error?: string | null;
  /** loginBonusInfo 原始 result（容错保留；顶层字段缺失时前端从这里兜底） */
  raw?: {
    hasIssued?: boolean;
    totalTimes?: number | null;
    totalPoints?: string | number | null;
    signInDays?: string[] | null;
  } | null;
}

/** 每日签到结果（POST /api/dumate/checkin）：幂等，already 表示今日已领。 */
export interface DumateCheckinResult {
  ok: boolean;
  /** success=本次签到成功(+500) / already=今日已领 / error=失败 */
  result: "success" | "already" | "error" | string;
  /** 人话说明（如「签到成功，+500 积分」） */
  message?: string | null;
  error?: string | null;
}

/** DuMate 网关接入配置（GET/POST /api/dumate/config，形态对齐 opencode 网关 config_info）。 */
export interface DumateConfigInfo {
  enabled: boolean;
  /** 网关密钥（前端测试卡用它鉴权 /dumate/v1；后端脱敏实现时前端回退主网关密钥） */
  apiKey: string;
  /** OpenAI 兼容调用基址，如 http://127.0.0.1:57890/dumate/v1 */
  endpoint: string;
  note?: string | null;
  /** 引擎地址（additive 容错） */
  opencodeUrl?: string | null;
  /** 引擎 inapp key 预览（additive 容错） */
  inappKeyPreview?: string | null;
}

// ---- 统一账号管理 + DuMate 多账号池（契约 docs/unified-accounts-contract.md §1/§2/§5）----
// 说明：camelCase、可空键容错（后端 additive/缺省字段一律 optional）。

/** 统一账号积分摘要（§2 credits 三键 + 积分包明细；上游数值多为字符串，容错 number）。 */
export interface UnifiedAccountCredits {
  available?: string | number | null;
  total?: string | number | null;
  used?: string | number | null;
}

/** 统一账号积分包明细：名称/剩余/到期/临期标志（后端 unified_accounts 提取）。 */
export interface UnifiedCreditPackage {
  /** 包名（上游 packageName/source/packageType 映射的中文名） */
  name: string;
  /** 剩余积分（展示串，如 "9932.57"） */
  remaining: string;
  /** 总量 */
  total: string;
  /** 已用 */
  used: string;
  /** 到期时间（秒级 epoch，null=无到期信息） */
  expireAt: number | null;
  /** 是否 7 天内到期（含已过期） */
  expiringSoon: boolean;
  /** 来源类型：resource（CreditExpiry.resources）/ subscription / incremental */
  kind?: string | null;
}

/** 统一账号 credits 扩展字段（§「近期到期」：包明细 + 最近到期 + 临期余额）。 */
export interface UnifiedAccountCreditsDetail extends UnifiedAccountCredits {
  packages?: UnifiedCreditPackage[] | null;
  /** 7 天内到期（含已过期）的余额合计（展示串），无则 null */
  expiringSoonRemaining?: string | null;
  /** 最近一个包的到期时间（秒级 epoch），无则 null */
  soonestExpireAt?: number | null;
}

/**
 * 统一账号（GET /api/accounts/unified 单行，契约 §2）：聚合全部平台账号。
 * displayName 为**完整全名无遮罩**（备注优先，无备注用账号自身名称字段，契约 §0/§3）。
 */
export interface UnifiedAccount {
  id: string;
  /** autoclaw | trae_work_cn | dumate | grok | chatglm | loomy | miaoshou | workbuddy | ... */
  platform: string;
  /** 完整全名（无星号；备注名优先） */
  displayName: string;
  /** cn | intl；平台无区域概念为 null */
  region?: "cn" | "intl" | null;
  /** 本机客户端登录态存在 */
  localLogin?: boolean | null;
  /** ok | expired | unknown */
  loginState?: string;
  /** 今日已签；平台无签到概念为 null */
  todayCheckedIn?: boolean | null;
  /** 积分摘要 + 积分包明细（含近期到期） */
  credits?: UnifiedAccountCreditsDetail | null;
  /** 仅 dumate 池账号有（一号一 IP 出站代理），其余 "" */
  proxy?: string;
  /** account-extras 备注 */
  note?: string;
  /** 自动签到开关现状 */
  checkinEnabled?: boolean;
  /** 跳转对应渠道页（platform→路由映射），未知为 null */
  gatewayPage?: string | null;
  /**
   * 同源账号：与 `sameSourceOf` 指向的 id 是同一账号的不同入口
   * （实测 DuMate 本机账号 `dumate:local` 与池主号 `dumate:pool_1`
   *   可用积分与积分包逐项相同）。**仅标注不隐藏**，两条都展示。
   */
  sameSourceOf?: string | null;
}

/** GET /api/accounts/unified 的 summary 统计块（契约 §2）。 */
export interface UnifiedAccountsSummary {
  platforms?: number;
  accounts?: number;
  healthy?: number;
  checkedInToday?: number;
}

/** GET /api/accounts/unified 响应（契约 §2）。 */
export interface UnifiedAccountsPayload {
  accounts: UnifiedAccount[];
  summary?: UnifiedAccountsSummary | null;
}

/**
 * DuMate 账号池条目（GET /api/dumate/accounts 行，契约 §1.1/§1.5）。
 * **不含 cookies 明文**——list 只回 cookieCount（契约 §1.5）；本机主账号以
 * isLocal 标记（id=dumate:local），池账号 id 前缀 dumate:pool_。
 */
export interface DumatePoolAccount {
  /** 池账号 id 前缀 dumate:pool_；本机账号为 dumate:local */
  id: string;
  /** 用户起名（显示用，可改） */
  name: string;
  /** 一号一 IP：http:// 或 socks5:// 出站代理，空 = 直连 */
  proxy?: string;
  note?: string;
  /** ok | expired | unknown */
  status?: string;
  /** 添加时刻（ms epoch） */
  addedAt?: number | null;
  /** 最近签到日（YYYY-MM-DD），未签过为 "" */
  lastCheckinDay?: string;
  /** cookie 条数（不回传明文，契约 §1.5） */
  cookieCount?: number | null;
  /** true = 本机主账号（dumate:local，走客户端登录态） */
  isLocal?: boolean;
  /** 最新缓存积分快照（未查询/查询失败为 null；dumate::get_credits 同构） */
  credits?: DumateCredits | null;
}

// ---- ZCode 账号池（契约 docs/zcode-channel-contract.md §3；camelCase、可空容错）----
// 说明：v1 为 webui-first（仅 webui 后端有 /api/zcode/* 路由组，契约 §2）；
// 后端 W1 按 §1/§2 同步实现，前端按契约 API 形状先行——缺省/新增字段一律 optional。

/** ZCode 登录身份（后端 zcode::ZcodeIdentity 的 camelCase 投影；解析失败各字段可为 null）。 */
export interface ZcodeIdentity {
  /** OAuth provider（如 bigmodel）；未知为 null */
  provider: string | null;
  username: string | null;
  /** 展示名优先级：displayName > username > email > userId */
  displayName: string | null;
  email: string | null;
  /** JWT user_id / sub 兜底 */
  userId: string | null;
  /** access_token JWT exp 判定；true = 已过期（缺省兼容旧后端） */
  jwtExpired?: boolean | null;
}

/** 额度通道单条（后端 quota_for() 的 channels[] 元素：{name, available, ...}）。 */
export interface ZcodeQuotaChannel {
  /** 通道名（monitor / zai_billing 等，未知值原样展示） */
  name: string;
  available?: string | number | null;
  total?: string | number | null;
  used?: string | number | null;
  ok?: boolean | null;
  error?: string | null;
  /** 上游附加字段透传（additive 容错） */
  [key: string]: unknown;
}

/** 实时额度（POST /api/zcode/quota 响应；多通道合并，契约 §0「额度三通道」v1 做两条）。 */
export interface ZcodeQuota {
  ok: boolean;
  channels?: ZcodeQuotaChannel[] | null;
  error?: string | null;
}

/** 账号池条目（~/.wb-switch/zcode_accounts.json 行；不回 credentials 明文，契约 §1）。 */
export interface ZcodeAccount {
  /** zcode:<hash8>（canonical JSON 去重指纹前 8 位） */
  id: string;
  name: string;
  /** 完整去重指纹（SHA256 hex，剔除 device* 前缀键后 canonical JSON） */
  hash?: string | null;
  /** 每账号独立设备遥测身份（UUID v4，防关联）；未生成为 null */
  virtualDeviceMid?: string | null;
  /** 账号出站代理（一号一 IP，覆盖 IP 维度限领/风控；空=直连） */
  proxy?: string | null;
  addedAt?: number | string | null;
  updatedAt?: number | string | null;
  /** 与 live 登录态同指纹（= 当前生效账号，显示「使用中」徽标，契约 §3.2） */
  isActive?: boolean;
  identity?: ZcodeIdentity | null;
  /** 额度缓存摘要（status 附带；未查询/查询失败为 null） */
  quota?: ZcodeQuota | null;
  /** 额度缓存落盘时间（ms）；null=未查询过 */
  quotaFetchedAt?: number | string | null;
  error?: string | null;
}

/** GET /api/zcode/status 响应：live 登录状态 + 账号池（含每账号 identity/额度缓存摘要）。 */
export interface ZcodeAutoSwitchState {
  config?: { enabled: boolean; thresholdTokens: number };
  lastTickAt?: number | null;
  lastState?: string | null;
  lastSwitchAt?: number | null;
  lastSwitchTo?: string | null;
  lastMessage?: string | null;
  events?: { ts: number; message: string }[];
}

export interface ZcodeStatus {
  /** credentials.json 文件存在 */
  liveExists: boolean;
  /** 能解析出登录身份（= 已登录） */
  loggedIn: boolean;
  /** 1.5.2 多账号自动管理（额度耗尽自动切换） */
  autoSwitch?: ZcodeAutoSwitchState;
  identity: ZcodeIdentity | null;
  /** ZCode.exe 进程检测（tasklist，契约 §0） */
  desktopRunning: boolean;
  accounts?: ZcodeAccount[];
  error?: string | null;
  message?: string | null;
}

// ---- AutoClaw 国际号轮换池（契约 docs/autoclaw-intl-pool-contract.md §2/§3.6；仅 webui 后端支持）----

/** 轮换策略（契约 §2.2 policy.strategy，后端 Strategy::parse 兼容常见变体）。 */
export type AutoclawIntlStrategy = 'round-robin' | 'random' | 'least-used' | 'sticky';

/** 单号运行态（契约 §2.2 accounts.<id>；GET /list 行结构，脱敏由后端负责）。 */
export interface AutoclawIntlAccountRow {
  id: string;
  /** 展示名（来源凭据 label；缺省后端回退 id） */
  label?: string | null;
  /** 注册邮箱（intlMeta.sourceEmail，幂等去重键） */
  sourceEmail?: string | null;
  enabled: boolean;
  weight?: number;
  /** active | quota-out | cooldown | invalid | banned | disabled */
  state: string;
  inFlight?: number;
  todayCalls?: number;
  totalCalls?: number;
  consecutiveFails?: number;
  /** 秒级 epoch；null = 无冷却 */
  cooldownUntil?: number | null;
  /** 距冷却结束的秒数（后端按 now 计算，页面直接用） */
  cooldownRemainSecs?: number | null;
  banCode?: string | number | null;
  bannedAt?: number | null;
  /** 秒级 epoch；null = 未知（无法解析 JWT exp） */
  tokenExpireAt?: number | null;
  /** 距 token 过期的秒数（后端按 now 计算） */
  tokenTtlSecs?: number | null;
  lastRefreshAt?: number | null;
  refreshFails?: number;
  lastUsedAt?: number | null;
  lastError?: string | null;
}

/** 池总览（契约 §3.6 summary；字段与 autoclaw_intl_pool::PoolState::summary 对齐）。 */
export interface AutoclawIntlSummary {
  lane: string;
  strategy: string;
  cursorOffset: number;
  total: number;
  healthy: number;
  cooldown: number;
  quotaOut: number;
  banned: number;
  invalid: number;
  disabled: number;
  poolInFlight: number;
  poolConcurrentCap: number;
  perAccountDailyCap: number;
  todayCalls: number;
  minHealthy: number;
  lowWater: boolean;
  autoReplace: boolean;
  rotations: number;
  replacements: number;
  refreshes: number;
}

/** 策略补丁（POST /policy body；与后端 update_policy 允许的字段一一对应）。 */
export interface AutoclawIntlPolicy {
  strategy?: AutoclawIntlStrategy;
  perAccountRatePerMin?: number;
  perAccountDailyCap?: number;
  poolConcurrentCap?: number;
  accountConcurrentCap?: number;
  quotaResetHour?: number;
  autoReplace?: boolean;
  minHealthy?: number;
}

/** 轮换事件（契约 §2.2 events；kind: join|rotate|cooldown|ban|quota|refresh|replace|disable）。 */
export interface AutoclawIntlEvent {
  ts: number;
  kind: string;
  accountId: string;
  detail: string;
}

/** 接入信息（GET /access；后端聚合 glm_proxy 国际目录 + 网关端点）。 */
export interface AutoclawIntlAccess {
  baseUrl: string;
  models: { id: string; region?: string; contextWindow?: number | null }[];
}
