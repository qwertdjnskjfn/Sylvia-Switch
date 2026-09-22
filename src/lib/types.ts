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
export interface TokenStatsSource { source: "workbuddy" | "autoclaw"; summary: TokenStatsTotals; models: TokenStatsGroup[]; projects: TokenStatsGroup[]; sessions: TokenStatsGroup[]; daily: TokenStatsGroup[]; /** Optional model-specific daily series for trend filtering. */ dailyByModel?: Record<string, TokenStatsGroup[]>; hours: TokenStatsGroup[]; filesScanned: number; parseErrors: number; coverageStartAt?: number | null; coverageEndAt?: number | null; }
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

export interface GrokPoolOverview {
  total: number;
  ok: number;
  errored: number;
  cooling: number;
  risk: { low: number; medium: number; high: number };
  policy: Partial<GrokPoolPolicy>;
  accounts: GrokPoolAccount[];
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
  error?: string | null;
}

