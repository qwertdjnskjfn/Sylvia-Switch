import { invoke } from "@tauri-apps/api/core";
import type {
  AccountMeta,
  AccountRecord,
  AppStatus,
  CheckinConfig,
  ExpiringSoonConfig,
  CheckinLog,
  CheckinResult,
  CreditExpiry,
  CreditStatistics,
  TokenStatistics,
  CopyResult,
  GithubConfig,
  ImportPreviewAccount,
  ImportResult,
  OAuthPollResult,
  OAuthStartResult,
  Session,
  SwitchResult,
  TravelConfig,
  TravelStatus,
  UpdateInfo,
  PlatformCheckinItem,
  GlmProxyConfigInfo,
  GlmProxyStatus,
  GlmUsageStatistics,
  GlmUsageRecord,
  ModelMonitor,
  AutoclawAccountInfo,
  AutoclawCheckinResult,
  QwenChannelInfo,
  QwenChannelsStatus,
  ToolCallMonitor,
  GatewayApiKey,
  GatewayLogEntry,
  GatewayOverview,
  KimiAccountInfo,
  QoderAccountInfo,
  TraeAccountInfo,
  TraeCheckinResult,
  LoomyAccountInfo,
  LoomyCheckinResult,
  LoomyCreditInfo,
  LoomyProxyConfigInfo,
  LoomyProxyStatus,
  LoomyModel,
  MiaoshouAccountInfo,
  MiaoshouCheckinResult,
  MiaoshouModelInfo,
  MiaoshouStatusItem,
  AccountProtectionConfigInfo,
  AccountProtectionStatusInfo,
  AccountsOverviewResponse,
  GrokAccountInfo,
  GrokTestResult,
  GrokPoolOverview,
  GrokPoolPolicy,
  GrokPoolSnapshot,
  GrokRegisterStatus,
  WorkbuddyPoolStatus,
  WorkbuddyGatewayConfig,
  ChatGptChannelStatus,
  ChatGptRegistrarStatus,
  ChatGptRegistrarStartBody,
  ChatGptRegisteredResponse,
  ChatGptAccountsResponse,
  DumateStatus,
  DumateCredits,
  DumateCheckinStatus,
  DumateCheckinResult,
  DumateConfigInfo,
  DumatePoolAccount,
  UnifiedAccountsPayload,
  ZcodeStatus,
  ZcodeAccount,
  ZcodeQuota,
  AutoclawIntlAccountRow,
  AutoclawIntlSummary,
  AutoclawIntlPolicy,
  AutoclawIntlEvent,
  AutoclawIntlAccess,
} from "./types";
import type { ChannelDetail, ChannelHealthOverview } from "@/lib/channel-health-types";
import type { ModelTestRequest, ModelTestResult } from "@/lib/model-test-types";
import { DEMO_UNAVAILABLE_MESSAGE, demoModeEnabled } from "./demo-mode";
import { screenshotDemoResponse } from "./screenshot-demo";

/**
 * 双通道适配层：
 * - 桌面 App（Tauri）：`invoke` 调用 Rust commands
 * - webui（浏览器）：HTTP fetch 调用本地服务（127.0.0.1）
 *
 * 注：改为同源相对路径。改版 webui 由本地启动器托管，启动器把 /api 反向代理到
 * wb-switch 后端（默认 127.0.0.1:57890），从而避免跨域（后端未开启 CORS）。
 */
const API_BASE = "";

const DEMO_READ_COMMANDS = new Set([
  "get_status", "get_accounts", "get_checkin_status",
  "get_credit_expiry", "get_credit_statistics", "get_auto_checkin_config",
  "get_token_statistics",
  "get_checkin_logs",
  "get_github_config", "check_update", "get_launch_at_login_enabled", "switch_progress",
  "get_travel_status", "get_auto_travel_config",
]);

export function isDemoMode(): boolean {
  return demoModeEnabled;
}

export function isWebui(): boolean {
  return typeof window !== "undefined" && !("__TAURI_INTERNALS__" in window);
}

/** Tauri mobile 也注入内部 API；用现有平台 UA 约定把桌面宿主与移动宿主区分开。 */
function isMobilePlatform(): boolean {
  if (typeof navigator === "undefined") return false;
  const ua = navigator.userAgent;
  return (
    /Android|iPhone|iPad|iPod/i.test(ua) ||
    (ua.includes("Macintosh") && navigator.maxTouchPoints > 1)
  );
}

/** 是否为提供桌面专属能力的 Tauri 宿主。 */
export function isDesktop(): boolean {
  return !isWebui() && !isMobilePlatform();
}

type Route = { method: "GET" | "POST"; path: string };

/** Tauri command → HTTP 路由映射（webui 模式）。 */
const ROUTES: Record<string, Route> = {
  get_status: { method: "GET", path: "/api/status" },
  get_accounts: { method: "GET", path: "/api/accounts" },
  get_gateways_overview: { method: "GET", path: "/api/gateways/overview" },
  delete_account: { method: "POST", path: "/api/delete" },
  oauth_start: { method: "POST", path: "/api/oauth/start" },
  oauth_status: { method: "POST", path: "/api/oauth/status" },
  import_local: { method: "POST", path: "/api/import-local" },
  export_accounts: { method: "POST", path: "/api/export-accounts" },
  export_accounts_to_path: { method: "POST", path: "/api/export-accounts-to-path" },
  preview_import_accounts: { method: "POST", path: "/api/import/preview" },
  import_accounts: { method: "POST", path: "/api/import" },
  switch_account: { method: "POST", path: "/api/switch" },
  /** 平台账号一键切换（AutoClaw / TRAE Work CN）：关客户端 → 写登录态 → 重启 */
  platform_account_switch: { method: "POST", path: "/api/platform/switch" },
  list_sessions: { method: "GET", path: "/api/sessions" },
  copy_sessions: { method: "POST", path: "/api/sessions/copy" },
  get_checkin_status: { method: "GET", path: "/api/checkin/status" },
  platform_accounts: { method: "GET", path: "/api/platform/accounts" },
  get_glm_proxy_config: { method: "GET", path: "/api/glm-proxy/config" },
  get_glm_proxy_status: { method: "GET", path: "/api/glm-proxy/status" },
  get_glmplan_status: { method: "GET", path: "/api/glmplan/status" },
  save_zcode_autoswitch_config: { method: "POST", path: "/api/zcode/auto-switch/config" },
  tick_zcode_autoswitch: { method: "POST", path: "/api/zcode/auto-switch/tick" },
  get_minimax_status: { method: "GET", path: "/api/minimax/status" },
  save_minimax_config: { method: "POST", path: "/api/minimax/config" },
  add_minimax_account: { method: "POST", path: "/api/minimax/accounts/add" },
  remove_minimax_account: { method: "POST", path: "/api/minimax/accounts/remove" },
  enable_minimax_account: { method: "POST", path: "/api/minimax/accounts/enable" },
  switch_minimax_account: { method: "POST", path: "/api/minimax/accounts/switch" },
  pin_minimax_account: { method: "POST", path: "/api/minimax/accounts/pin" },
  clear_minimax_cooldown: { method: "POST", path: "/api/minimax/accounts/clear-cooldown" },
  run_minimax_signin: { method: "POST", path: "/api/minimax/signin/run" },
  capture_minimax_desktop: { method: "POST", path: "/api/minimax/desktop/capture" },
  save_glmplan_config: { method: "POST", path: "/api/glmplan/config" },
  add_glmplan_account: { method: "POST", path: "/api/glmplan/accounts/add" },
  remove_glmplan_account: { method: "POST", path: "/api/glmplan/accounts/remove" },
  enable_glmplan_account: { method: "POST", path: "/api/glmplan/accounts/enable" },
  get_glm_usage_summary: { method: "GET", path: "/api/glm-usage/summary" },
  get_glm_usage_records: { method: "GET", path: "/api/glm-usage/records" },
  get_glm_usage_monitor: { method: "GET", path: "/api/glm-usage/monitor" },
  get_accounts_usage_overview: { method: "GET", path: "/api/glm-usage/accounts-overview" },
  get_autoclaw_accounts: { method: "GET", path: "/api/autoclaw/accounts" },
  add_autoclaw_account: { method: "POST", path: "/api/autoclaw/accounts" },
  set_autoclaw_account_region: { method: "POST", path: "/api/autoclaw/accounts/set-region" },
  import_autoclaw_local: { method: "POST", path: "/api/autoclaw/accounts/import-local" },
  remove_autoclaw_account: { method: "POST", path: "/api/autoclaw/accounts/remove" },
  refresh_autoclaw_account: { method: "POST", path: "/api/autoclaw/accounts/refresh" },
  autoclaw_checkin_all: { method: "POST", path: "/api/autoclaw/checkin-all" },
  autoclaw_checkin_one: { method: "POST", path: "/api/autoclaw/checkin-one" },
  get_trae_accounts: { method: "GET", path: "/api/trae/accounts" },
  add_trae_account: { method: "POST", path: "/api/trae/accounts" },
  import_trae_local: { method: "POST", path: "/api/trae/accounts/import-local" },
  remove_trae_account: { method: "POST", path: "/api/trae/accounts/remove" },
  refresh_trae_account: { method: "POST", path: "/api/trae/accounts/refresh" },
  trae_checkin_all: { method: "POST", path: "/api/trae/checkin-all" },
  get_qoder_accounts: { method: "GET", path: "/api/qoder/accounts" },
  qoder_import_local: { method: "POST", path: "/api/qoder/accounts/import-local" },
  qoder_remove_account: { method: "POST", path: "/api/qoder/accounts/remove" },
  qoder_refresh_account: { method: "POST", path: "/api/qoder/accounts/refresh" },
  qoder_device_flow_start: { method: "POST", path: "/api/qoder/device-flow/start" },
  qoder_device_flow_complete: { method: "POST", path: "/api/qoder/device-flow/complete" },
  qoder_test_completion: { method: "POST", path: "/api/qoder/test" },
  get_qwen_channel: { method: "GET", path: "/api/qwen/channel" },
  set_qwen_channel: { method: "POST", path: "/api/qwen/channel" },
  get_qwen_channels: { method: "GET", path: "/api/qwen/channels" },
  get_qwen_web_log: { method: "GET", path: "/api/qwen/web/log" },
  get_qwen_toolcall_monitor: { method: "GET", path: "/api/qwen/toolcall-monitor" },
  clear_qwen_toolcall_monitor: { method: "POST", path: "/api/qwen/toolcall-monitor/clear" },
  get_kimi_accounts: { method: "GET", path: "/api/kimi/accounts" },
  kimi_add_account: { method: "POST", path: "/api/kimi/accounts/add" },
  kimi_remove_account: { method: "POST", path: "/api/kimi/accounts/remove" },
  kimi_enable_account: { method: "POST", path: "/api/kimi/accounts/enable" },
  kimi_verify_account: { method: "POST", path: "/api/kimi/accounts/verify" },
  kimi_test_completion: { method: "POST", path: "/api/kimi/test" },
  get_grok_accounts: { method: "GET", path: "/api/grok/accounts" },
  grok_add_account: { method: "POST", path: "/api/grok/accounts/add" },
  grok_remove_account: { method: "POST", path: "/api/grok/accounts/remove" },
  grok_enable_account: { method: "POST", path: "/api/grok/accounts/enable" },
  grok_test: { method: "POST", path: "/api/grok/test" },
  get_grok_pool_overview: { method: "GET", path: "/api/grok/pool/overview" },
  grok_pool_check: { method: "POST", path: "/api/grok/pool/check" },
  get_grok_pool_policy: { method: "GET", path: "/api/grok/pool/policy" },
  set_grok_pool_policy: { method: "POST", path: "/api/grok/pool/policy" },
  get_grok_pool_history: { method: "GET", path: "/api/grok/pool/history" },
  get_grok_sso_status: { method: "GET", path: "/api/grok-sso/status" },
  sync_grok_sso: { method: "POST", path: "/api/grok-sso/sync" },
  open_grok_sso_login: { method: "POST", path: "/api/grok-sso/open-login" },
  // 注册机（专用 Chrome 实例，仅手动触发；status 为只读探测，不启动浏览器）
  get_grok_register_status: { method: "GET", path: "/api/grok/register/status" },
  open_grok_register: { method: "POST", path: "/api/grok/register/open" },
  capture_grok_register: { method: "POST", path: "/api/grok/register/capture" },
  close_grok_register: { method: "POST", path: "/api/grok/register/close" },
  get_gateway_keys: { method: "GET", path: "/api/gateway/keys" },
  create_gateway_key: { method: "POST", path: "/api/gateway/keys" },
  get_workbuddy_pool_region_key: {
    method: "GET",
    path: "/api/workbuddy-pool/region-key",
  },
  delete_gateway_key: { method: "POST", path: "/api/gateway/keys/delete" },
  get_gateway_logs: { method: "GET", path: "/api/gateway/logs" },
  get_gateway_overview: { method: "GET", path: "/api/gateway/overview" },
  qoder_import_manual: { method: "POST", path: "/api/qoder/accounts/import-manual" },
  qoder_import_local_cn: { method: "POST", path: "/api/qoder/accounts/import-local-cn" },
  qoder_import_work_cn: { method: "POST", path: "/api/qoder/accounts/import-work-cn" },
  qoder_set_region: { method: "POST", path: "/api/qoder/accounts/region" },
  get_glmweb_tokens: { method: "GET", path: "/api/glmweb/tokens" },
  glmweb_add_token: { method: "POST", path: "/api/glmweb/tokens" },
  glmweb_remove_token: { method: "POST", path: "/api/glmweb/tokens/remove" },
  // v0.1.46：浏览器同步工具的幂等写回（校验 + upsert + 池健康快照）
  glmweb_sync_token: { method: "POST", path: "/api/glmweb/tokens/sync" },
  glmweb_test_completion: { method: "POST", path: "/api/glmweb/test" },
  get_loomy_status: { method: "GET", path: "/api/loomy/status" },
  get_loomy_accounts: { method: "GET", path: "/api/loomy/accounts" },
  add_loomy_account: { method: "POST", path: "/api/loomy/accounts" },
  import_loomy_local: { method: "POST", path: "/api/loomy/accounts/import-local" },
  remove_loomy_account: { method: "POST", path: "/api/loomy/accounts/remove" },
  get_loomy_credits: { method: "GET", path: "/api/loomy/credits" },
  loomy_checkin_all: { method: "POST", path: "/api/loomy/checkin-all" },
  loomy_checkin_one: { method: "POST", path: "/api/loomy/checkin-one" },
  get_loomy_proxy_config: { method: "GET", path: "/api/loomy-proxy/config" },
  save_loomy_proxy_config: { method: "POST", path: "/api/loomy-proxy/config" },
  get_loomy_proxy_status: { method: "GET", path: "/api/loomy-proxy/status" },
  get_loomy_models_catalog: { method: "GET", path: "/api/loomy/models" },
  // 妙手（美团智能体桌面应用）：账号池 + 积分；平台无每日签到，checkin 端点保留占位
  get_miaoshou_accounts: { method: "GET", path: "/api/miaoshou/accounts" },
  miaoshou_import_local: { method: "POST", path: "/api/miaoshou/accounts/import-local" },
  miaoshou_remove_account: { method: "POST", path: "/api/miaoshou/accounts/remove" },
  get_miaoshou_status: { method: "GET", path: "/api/miaoshou/status" },
  miaoshou_checkin_all: { method: "POST", path: "/api/miaoshou/checkin-all" },
  miaoshou_checkin_one: { method: "POST", path: "/api/miaoshou/checkin-one" },
  get_miaoshou_credits: { method: "GET", path: "/api/miaoshou/credits" },
  get_miaoshou_models: { method: "GET", path: "/api/miaoshou/models" },
  miaoshou_chat: { method: "POST", path: "/api/miaoshou/chat" },
  // DuMate（百度搭子）渠道（契约 docs/dumate-channel-contract.md §3.2/§4.1）；
  // /dumate/v1 网关调用不走 ROUTES（前端 fetch 直连 + Bearer 鉴权，见 dumateTestChat）
  get_dumate_status: { method: "GET", path: "/api/dumate/status" },
  get_dumate_credits: { method: "GET", path: "/api/dumate/credits" },
  get_dumate_config: { method: "GET", path: "/api/dumate/config" },
  save_dumate_config: { method: "POST", path: "/api/dumate/config" },
  dumate_checkin: { method: "POST", path: "/api/dumate/checkin" },
  get_dumate_checkin_status: { method: "GET", path: "/api/dumate/checkin-status" },
  // 统一账号管理 + DuMate 多账号池（契约 docs/unified-accounts-contract.md §1.5/§2/§5；仅 webui 后端支持）；
  // GET /api/dumate/accounts（列表）为契约 §1.5 路由组的一部分，多账号卡的数据源
  get_unified_accounts: { method: "GET", path: "/api/accounts/unified" },
  get_dumate_accounts: { method: "GET", path: "/api/dumate/accounts" },
  add_dumate_account: { method: "POST", path: "/api/dumate/accounts" },
  update_dumate_account: { method: "POST", path: "/api/dumate/accounts/update" },
  remove_dumate_account: { method: "POST", path: "/api/dumate/accounts/remove" },
  import_dumate_client: { method: "POST", path: "/api/dumate/accounts/import-client" },
  dumate_account_checkin: { method: "POST", path: "/api/dumate/account/checkin" },
  // ZCode 账号池（契约 docs/zcode-channel-contract.md §2/§3；仅 webui 后端支持）
  get_zcode_status: { method: "GET", path: "/api/zcode/status" },
get_zcode_claim_state: { method: "GET", path: "/api/zcode/claim/state" },
save_zcode_claim_state: { method: "POST", path: "/api/zcode/claim/state" },
update_zcode_account: { method: "POST", path: "/api/zcode/accounts/update" },
      zcode_claim_scan: { method: "POST", path: "/api/zcode/claim/scan" },
  zcode_claim_auto: { method: "POST", path: "/api/zcode/claim/auto" },
  zcode_claim_preview: { method: "POST", path: "/api/zcode/claim/preview" },
  capture_zcode: { method: "POST", path: "/api/zcode/capture" },
  switch_zcode: { method: "POST", path: "/api/zcode/switch" },
  remove_zcode: { method: "POST", path: "/api/zcode/accounts/remove" },
  zcode_quota: { method: "POST", path: "/api/zcode/quota" },
  get_trae_proxy_status: { method: "GET", path: "/api/trae-proxy/status" },
  get_trae_proxy_models: { method: "GET", path: "/api/trae-proxy/models" },
  get_trae_proxy_config: { method: "GET", path: "/api/trae-proxy/config" },
  get_workbuddy_proxy_status: { method: "GET", path: "/api/workbuddy-proxy/status" },
  get_workbuddy_proxy_models: { method: "GET", path: "/api/workbuddy-proxy/models" },
  get_workbuddy_proxy_config: { method: "GET", path: "/api/workbuddy-proxy/config" },
  save_workbuddy_proxy_config: { method: "POST", path: "/api/workbuddy-proxy/config" },
  get_workbuddy_native_models: { method: "GET", path: "/workbuddy-native/v1/models" },
  refresh_workbuddy_catalog: { method: "POST", path: "/api/workbuddy-native/catalog/refresh" },
  // WorkBuddy 多账号网关（cloud 账号池 · 2026-09-24 重构新增，独立路由组；
  // 存量 /workbuddy/v1 与 /workbuddy-native/v1 原样不动）
  get_workbuddy_pool_status: { method: "GET", path: "/workbuddy-pool/v1/status" },
  patch_workbuddy_pool_config: { method: "POST", path: "/workbuddy-pool/v1/config" },
  get_workbuddy_pool_models: { method: "GET", path: "/workbuddy-pool/v1/models" },
  workbuddy_pool_chat: { method: "POST", path: "/workbuddy-pool/v1/chat/completions" },
  // OpenCode 网关（2026-09-22 三拆新增，独立密钥）
  get_opencode_proxy_status: { method: "GET", path: "/api/opencode-proxy/status" },
  get_opencode_proxy_config: { method: "GET", path: "/api/opencode-proxy/config" },
  save_opencode_proxy_config: { method: "POST", path: "/api/opencode-proxy/config" },
  trae_checkin_one: { method: "POST", path: "/api/trae/checkin-one" },
  test_glm_proxy: { method: "POST", path: "/api/glm-proxy/test" },
  save_glm_proxy_config: { method: "POST", path: "/api/glm-proxy/config" },
  get_credit_expiry: { method: "POST", path: "/api/credits" },
  get_credit_statistics: { method: "GET", path: "/api/credits/stats" },
  get_token_statistics: { method: "GET", path: "/api/token-stats" },
  checkin: { method: "POST", path: "/api/checkin" },
  checkin_all: { method: "POST", path: "/api/checkin/all" },
  get_auto_checkin_config: { method: "GET", path: "/api/checkin/config" },
  save_auto_checkin_config: { method: "POST", path: "/api/checkin/config" },
  get_expiring_soon_config: { method: "GET", path: "/api/config/expiring-soon" },
  save_expiring_soon_config: { method: "POST", path: "/api/config/expiring-soon" },
  get_checkin_logs: { method: "GET", path: "/api/checkin/logs" },
  get_travel_status: { method: "GET", path: "/api/travel/status" },
  get_auto_travel_config: { method: "GET", path: "/api/travel/config" },
  save_auto_travel_config: { method: "POST", path: "/api/travel/config" },
  refresh_account_token: { method: "POST", path: "/api/refresh-token" },
  get_github_config: { method: "GET", path: "/api/update/config" },
  save_github_config: { method: "POST", path: "/api/update/config" },
  check_update: { method: "GET", path: "/api/update/check" },
  switch_progress: { method: "GET", path: "/api/switch/progress" },
  get_protection_status: { method: "GET", path: "/api/protection/status" },
  get_protection_config: { method: "GET", path: "/api/protection/config" },
  save_protection_config: { method: "POST", path: "/api/protection/config" },
  protection_keepalive_now: { method: "POST", path: "/api/protection/keepalive-now" },
  protection_clear_cooldown: { method: "POST", path: "/api/protection/cooldown/clear" },
  get_loomy_protection_status: { method: "GET", path: "/api/loomy/protection/status" },
  loomy_protection_keepalive_now: { method: "POST", path: "/api/loomy/protection/keepalive-now" },
  loomy_protection_clear_cooldown: { method: "POST", path: "/api/loomy/protection/cooldown/clear" },
  get_channel_health: { method: "GET", path: "/api/channel-health" },
  get_channel_health_detail: { method: "GET", path: "/api/channel-health/detail" },
  model_test: { method: "POST", path: "/api/model-test" },
  // 模型目录自动同步（model-sync · 仅 webui 后端支持；契约 docs/model-sync-contract.md §四）
  get_model_sync_status: { method: "GET", path: "/api/model-sync/status" },
  run_model_sync: { method: "POST", path: "/api/model-sync/run" },
  get_model_sync_log: { method: "GET", path: "/api/model-sync/log" },
  get_model_sync_config: { method: "GET", path: "/api/model-sync/config" },
  save_model_sync_config: { method: "POST", path: "/api/model-sync/config" },
  add_model_sync_registry: { method: "POST", path: "/api/model-sync/registry" },
  // ChatGPT 通道（gpt-reg-review 托管+注册机控制+账号表；契约 docs/chatgpt-channel-contract.md §4）
  get_chatgpt_channel_status: { method: "GET", path: "/api/chatgpt/channel/status" },
  chatgpt_webui_start: { method: "POST", path: "/api/chatgpt/webui/start" },
  chatgpt_webui_stop: { method: "POST", path: "/api/chatgpt/webui/stop" },
  chatgpt_registrar_start: { method: "POST", path: "/api/chatgpt/registrar/start" },
  chatgpt_registrar_pause: { method: "POST", path: "/api/chatgpt/registrar/pause" },
  chatgpt_registrar_resume: { method: "POST", path: "/api/chatgpt/registrar/resume" },
  chatgpt_registrar_stop: { method: "POST", path: "/api/chatgpt/registrar/stop" },
  chatgpt_registrar_status: { method: "GET", path: "/api/chatgpt/registrar/status" },
  chatgpt_registered: { method: "GET", path: "/api/chatgpt/registered" },
  chatgpt_accounts: { method: "GET", path: "/api/chatgpt/accounts" },
  chatgpt_accounts_sync: { method: "POST", path: "/api/chatgpt/accounts/sync" },
  // AutoClaw 国际号轮换池（契约 docs/autoclaw-intl-pool-contract.md §3.6；仅 webui 后端支持）
  get_autoclaw_intl_list: { method: "GET", path: "/api/autoclaw/intl/list" },
  get_autoclaw_intl_summary: { method: "GET", path: "/api/autoclaw/intl/summary" },
  save_autoclaw_intl_policy: { method: "POST", path: "/api/autoclaw/intl/policy" },
  toggle_autoclaw_intl_account: { method: "POST", path: "/api/autoclaw/intl/toggle" },
  remove_autoclaw_intl_account: { method: "POST", path: "/api/autoclaw/intl/remove" },
  probe_autoclaw_intl_account: { method: "POST", path: "/api/autoclaw/intl/probe" },
  refresh_autoclaw_intl_account: { method: "POST", path: "/api/autoclaw/intl/refresh" },
  rotate_autoclaw_intl_cursor: { method: "POST", path: "/api/autoclaw/intl/rotate" },
  get_autoclaw_intl_events: { method: "GET", path: "/api/autoclaw/intl/events" },
  get_autoclaw_intl_access: { method: "GET", path: "/api/autoclaw/intl/access" },
};

function queryString(args?: Record<string, unknown>): string {
  if (!args) return "";
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(args)) {
    if (value === undefined || value === null) continue;
    params.set(key, String(value));
  }
  const text = params.toString();
  return text ? `?${text}` : "";
}

async function httpCall<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const route = ROUTES[cmd];
  if (!route) throw new Error(`webui 模式暂不支持该操作: ${cmd}`);
  let res: Response;
  try {
    const url =
      route.method === "GET"
        ? `${API_BASE}${route.path}${queryString(args)}`
        : `${API_BASE}${route.path}`;
    res = await fetch(url, {
      method: route.method,
      headers: { "Content-Type": "application/json" },
      body: route.method === "POST" ? JSON.stringify(args ?? {}) : undefined,
    });
  } catch {
    throw new Error("无法连接 Sylvia Switch 服务，请先启动 Sylvia Switch");
  }
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    const err = new Error(data.message || data.error || `请求失败 (${res.status})`);
    // 挂上原始错误体，便于调用方读机器可读标记（如 recovering）。
    Object.assign(err, { payload: data });
    throw err;
  }
  return data as T;
}

/** G2：上游恢复中的瞬态错误（看门狗拉起窗口，通常 ≤60s）。
 *  优先读后端 recovering 标记，兜底用负缓存文案关键词。 */
export function isRecoveringError(e: unknown): boolean {
  const payload = (e as { payload?: { recovering?: boolean } }).payload;
  if (payload && payload.recovering === true) return true;
  const msg = e instanceof Error ? e.message : String(e);
  return msg.includes("负缓存");
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (demoModeEnabled) {
    if (cmd === "get_credit_statistics" && args?.refresh === true) {
      throw new Error(DEMO_UNAVAILABLE_MESSAGE);
    }
    if (!DEMO_READ_COMMANDS.has(cmd)) throw new Error(DEMO_UNAVAILABLE_MESSAGE);
    return screenshotDemoResponse(cmd, args) as T;
  }
  if (!isWebui()) return invoke<T>(cmd, args);
  return httpCall<T>(cmd, args);
}

// ---------------------------------------------------------------------------
// 状态 / 账号
// ---------------------------------------------------------------------------

export function getStatus(): Promise<AppStatus> {
  return call("get_status");
}

export function getAccounts(): Promise<{ accounts: AccountMeta[] }> {
  return call("get_accounts");
}

/** 所有模型网关的聚合总览（统一网关下拉菜单用）。 */
export function getGatewaysOverview(): Promise<GatewaysOverview> {
  return call("get_gateways_overview");
}

export function deleteAccount(accountId: string): Promise<{ ok: boolean }> {
  return call("delete_account", { accountId });
}

export function oauthStart(): Promise<OAuthStartResult> {
  return call("oauth_start");
}

export function oauthStatus(loginId: string): Promise<OAuthPollResult> {
  return call("oauth_status", { loginId });
}

export function importLocal(): Promise<{ ok: boolean; account: AccountMeta }> {
  return call("import_local");
}

export function exportAccounts(accountIds: string[]): Promise<{ ok: boolean; accounts: AccountRecord[] }> {
  return call("export_accounts", { accountIds });
}

/** 桌面端：把完整记录写入用户选择的路径（系统保存对话框产物）。 */
export function exportAccountsToPath(
  accountIds: string[],
  path: string,
): Promise<{ ok: boolean; path: string }> {
  return call("export_accounts_to_path", { accountIds, path });
}

export function previewImportAccounts(
  fileText: string,
): Promise<{ accounts: ImportPreviewAccount[]; total: number }> {
  return call("preview_import_accounts", { fileText });
}

export function importAccounts(fileText: string, indexes: number[]): Promise<ImportResult> {
  return call("import_accounts", { fileText, indexes });
}

export function switchAccount(args: {
  accountId: string;
  restart?: boolean;
  shareSessions?: boolean;
  copySessionIds?: string[];
}): Promise<SwitchResult> {
  return call("switch_account", args as unknown as Record<string, unknown>);
}

/** 切换进度（webui 轮询用；桌面端走事件，此函数无副作用）。 */
export function switchProgress(): Promise<{ running: boolean; progress: string | null }> {
  return call("switch_progress");
}

/** 平台账号一键切换结果摘要。 */
export interface PlatformSwitchResult {
  ok: boolean;
  platform: string;
  id: string;
  label?: string | null;
  /** 关闭的客户端进程数（0 = 客户端本来没在跑） */
  closedProcesses: number;
  /** 登录态文件备份路径 */
  backup?: string | null;
  /** 是否已自动拉起客户端 */
  launched: boolean;
  /** 未能自动拉起时的说明 */
  note?: string;
}

/** 平台账号一键切换：AutoClaw / TRAE Work CN。 */
export function switchPlatformAccount(
  platform: "autoclaw" | "trae_work_cn",
  id: string,
  restart = true,
): Promise<PlatformSwitchResult> {
  return call("platform_account_switch", { platform, id, restart });
}

export function listSessions(): Promise<{
  sessions: Session[];
  current: string | null;
}> {
  return call("list_sessions");
}

export function copySessions(
  targetAccountId: string,
  sessionIds: string[],
): Promise<{ sourceUid: string; targetUid: string; copied: CopyResult[] }> {
  return call("copy_sessions", { targetAccountId, sessionIds });
}

/** 打开系统设置授权面板（桌面端专用；webui 模式由服务进程权限决定，无操作）。 */
export function openPermissionSettings(
  target?: "app_management" | "all_files",
): Promise<void> {
  if (demoModeEnabled) return Promise.reject(new Error(DEMO_UNAVAILABLE_MESSAGE));
  if (isWebui()) return Promise.resolve();
  return call("open_permission_settings", { target: target ?? "app_management" });
}

/** 权限自检：桌面端写探针；webui 模式由服务进程权限决定。 */
export function checkAuthPermission(): Promise<{
  ok: boolean;
  message?: string;
  error?: string;
  dir?: string;
  hint?: string;
}> {
  if (demoModeEnabled) return Promise.reject(new Error(DEMO_UNAVAILABLE_MESSAGE));
  if (isWebui()) {
    return Promise.resolve({
      ok: true,
      message: "webui 模式由服务进程（终端启动）的权限决定，无需额外授权",
      hint: "",
    });
  }
  return call("check_auth_permission");
}

/** 在 Finder 中显示当前 App（桌面端专用；webui 无操作）。 */
export function revealAppInFinder(): Promise<void> {
  if (demoModeEnabled) return Promise.reject(new Error(DEMO_UNAVAILABLE_MESSAGE));
  if (isWebui()) return Promise.resolve();
  return call("reveal_app_in_finder");
}

// ---------------------------------------------------------------------------
// 阶段 3：签到 + token 刷新
// ---------------------------------------------------------------------------

export async function getCheckinStatus(accountId: string): Promise<{
  ok: boolean;
  todayCheckedIn: boolean;
  error?: string;
  raw?: unknown;
}> {
  if (demoModeEnabled) {
    return screenshotDemoResponse("get_checkin_status", { accountId }) as {
      ok: boolean;
      todayCheckedIn: boolean;
      error?: string;
      raw?: unknown;
    };
  }
  if (isWebui()) {
    // webui 端为批量接口，按 accountId 过滤
    const all = await httpCall<{
      accounts: {
        accountId: string;
        email: string;
        ok: boolean;
        todayCheckedIn: boolean;
        error?: string;
        raw?: unknown;
      }[];
    }>("get_checkin_status");
    const one = all.accounts.find((a) => a.accountId === accountId);
    return one
      ? { ok: one.ok, todayCheckedIn: one.todayCheckedIn, error: one.error, raw: one.raw }
      : { ok: false, todayCheckedIn: false, error: "未找到账号" };
  }
  return call("get_checkin_status", { accountId });
}

/** 平台账号（AutoClaw / TRAE Work CN）签到状态；仅 webui 后端支持，桌面端返回空列表。 */
export async function getPlatformCheckinItems(): Promise<PlatformCheckinItem[]> {
  if (demoModeEnabled || !isWebui()) return [];
  const all = await httpCall<{
    accounts: (PlatformCheckinItem & { raw?: unknown })[];
  }>("platform_accounts");
  return all.accounts.filter((item) => Boolean(item.platform) && item.platform !== "workbuddy");
}

/** GLM 代理配置（仅 webui 后端支持）。 */
export async function getGlmProxyConfig(): Promise<GlmProxyConfigInfo> {
  return call("get_glm_proxy_config");
}
/** GLM Plan 网关（1.4.0 新通道）状态 */
/** 保存 ZCode 自动切换配置（额度耗尽自动热切换） */
export async function saveZcodeAutoSwitchConfig(body: { enabled: boolean; thresholdTokens: number }) {
  return call("save_zcode_autoswitch_config", body);
}
/** 立即跑一轮 ZCode 自动切换检查 */
/** MiniMax 通道（1.6.0）状态 */
export async function getMinimaxStatus() {
  return call("get_minimax_status");
}
/** 保存 MiniMax 通道配置 */
export async function saveMinimaxConfig(body: Record<string, unknown>) {
  return call("save_minimax_config", body);
}
/** 加入 MiniMax 账号池 */
export async function addMinimaxAccount(body: Record<string, string>) {
  return call("add_minimax_account", body);
}
/** 移出 MiniMax 账号池 */
export async function removeMinimaxAccount(body: { id: string }) {
  return call("remove_minimax_account", body);
}
/** 启用/停用 MiniMax 账号 */
export async function enableMinimaxAccount(body: { id: string; enabled: boolean }) {
  return call("enable_minimax_account", body);
}

export async function switchMinimaxAccount(body: { mode?: string; id?: string; region?: string }) {
  return call("switch_minimax_account", body);
}

export async function pinMinimaxAccount(body: { id?: string; pinned: boolean; all?: boolean; onlyUsable?: boolean }) {
  return call("pin_minimax_account", body);
}

export async function clearMinimaxCooldown(body: { id?: string }) {
  return call("clear_minimax_cooldown", body);
}
/** MiniMax 立即全量签到 */
/** 从桌面端登录态导入 MiniMax 账号（只读共享） */
export async function captureMinimaxDesktop() {
  return call("capture_minimax_desktop");
}
export async function runMinimaxSignin(body: { id?: string }) {
  return call("run_minimax_signin", body);
}
export async function tickZcodeAutoSwitch() {
  return call("tick_zcode_autoswitch");
}
export async function getGlmplanStatus() {
  return call("get_glmplan_status");
}
/** 保存 GLM Plan 网关配置 */
export async function saveGlmplanConfig(body: Record<string, unknown>) {
  return call("save_glmplan_config", body);
}
/** 加入 GLM Plan 账号池 */
export async function addGlmplanAccount(body: { label: string; provider: string; apiKey: string }) {
  return call("add_glmplan_account", body);
}
/** 移出 GLM Plan 账号池 */
export async function removeGlmplanAccount(body: { id: string }) {
  return call("remove_glmplan_account", body);
}
/** 启用/停用 GLM Plan 账号（清冷却） */
export async function enableGlmplanAccount(body: { id: string; enabled: boolean }) {
  return call("enable_glmplan_account", body);
}

/** GLM 代理试调：服务端转发一条测试对话。 */
export async function testGlmProxy(model: string, prompt: string): Promise<{
  ok: boolean;
  content: string;
  model: string;
  latencyMs: number;
  usage?: unknown;
}> {
  return call("test_glm_proxy", { model, prompt });
}

/** 模型用量统计汇总（今日/近7天/全部，按模型分列）。 */
export async function getGlmUsageSummary(): Promise<GlmUsageStatistics> {
  return call("get_glm_usage_summary");
}

export async function getGlmUsageRecords(limit = 100): Promise<{ records: GlmUsageRecord[] }> {
  return call("get_glm_usage_records", { limit });
}

/**
 * 模型监控数据：窗口内按模型汇总 + 按时间分桶的调用趋势。
 * @param hours 统计窗口（小时）；0 表示全部历史
 * @param bucketMinutes 可选，时间桶宽度（分钟）；不传则后端按窗口自动选择
 * @param provider 可选，平台/账号过滤（autoclaw, loomy, trae_work_cn, all）
 */
export async function getGlmUsageMonitor(
  hours = 24,
  bucketMinutes?: number,
  provider?: string,
): Promise<ModelMonitor> {
  const params: Record<string, unknown> = { hours };
  if (bucketMinutes) params.bucketMinutes = bucketMinutes;
  if (provider && provider !== "all") params.provider = provider;
  return call("get_glm_usage_monitor", params);
}

/** 多账号（AutoClaw, Loomy, TRAE Work CN）模型用量概览指标 */
export async function getAccountsUsageOverview(): Promise<AccountsOverviewResponse> {
  return call("get_accounts_usage_overview");
}

// ---- AutoClaw 多账号池（仅 webui 后端支持）----

export async function getAutoclawAccounts(): Promise<{ accounts: AutoclawAccountInfo[] }> {
  return call("get_autoclaw_accounts");
}

export async function addAutoclawAccount(input: {
  label?: string;
  accessToken: string;
  refreshToken: string;
  deviceId?: string;
  /** 区域分入口：显式指定 international / domestic；缺省由后端按 token 身份推断 */
  region?: 'international' | 'domestic';
}): Promise<{ ok: boolean; account: AutoclawAccountInfo }> {
  return call("add_autoclaw_account", input as unknown as Record<string, unknown>);
}

/** 手动指定账号归属区域（防误判修正；后端记录 is_manual_specified） */
export async function setAutoclawAccountRegion(
  id: string,
  region: 'international' | 'domestic',
): Promise<{ ok: boolean; account: AutoclawAccountInfo }> {
  return call("set_autoclaw_account_region", { id, region });
}

export async function importAutoclawLocal(): Promise<{ ok: boolean; account: AutoclawAccountInfo }> {
  return call("import_autoclaw_local");
}

export async function removeAutoclawAccount(id: string): Promise<{ ok: boolean }> {
  return call("remove_autoclaw_account", { id });
}

export async function refreshAutoclawAccount(id: string): Promise<{ ok: boolean }> {
  return call("refresh_autoclaw_account", { id });
}

export async function autoclawCheckinAll(): Promise<{ accounts: AutoclawCheckinResult[] }> {
  return call("autoclaw_checkin_all");
}

export async function autoclawCheckinOne(id: string): Promise<AutoclawCheckinResult> {
  return call("autoclaw_checkin_one", { id });
}

// ---- TRAE Work CN 多账号池（仅 webui 后端支持）----

export async function getTraeAccounts(): Promise<{ accounts: TraeAccountInfo[] }> {
  return call("get_trae_accounts");
}

export async function importTraeLocal(): Promise<{ ok: boolean; account: TraeAccountInfo }> {
  return call("import_trae_local");
}

export async function removeTraeAccount(id: string): Promise<{ ok: boolean }> {
  return call("remove_trae_account", { id });
}

export async function refreshTraeAccount(id: string): Promise<{ ok: boolean }> {
  return call("refresh_trae_account", { id });
}

/** 手动添加 TRAE Work CN 账号（粘贴快照凭证，不走本机客户端登录态）。 */
export async function addTraeAccount(input: {
  label?: string;
  accessToken: string;
  refreshToken?: string;
  deviceId?: string;
  machineId?: string;
}): Promise<{ ok: boolean; account: TraeAccountInfo }> {
  return call("add_trae_account", input as unknown as Record<string, unknown>);
}

export async function traeCheckinAll(): Promise<{ accounts: TraeCheckinResult[] }> {
  return call("trae_checkin_all");
}

export async function traeCheckinOne(id: string): Promise<TraeCheckinResult> {
  return call("trae_checkin_one", { id });
}

// ---- Qoder 融合网关（仅 webui 后端支持）----

// ---- Kimi 融合网关（仅 webui 后端支持）----

// ---- Qwen 双通道（网页版 chat.qwen.ai + Qoder 算力池，仅 webui 后端支持）----

export async function getQwenChannel(): Promise<QwenChannelInfo> {
  return call("get_qwen_channel");
}

export async function setQwenChannel(
  channel: 'auto' | 'web' | 'qoder',
  autoFallback = true,
): Promise<{ ok: boolean; channel: string; autoFallback: boolean }> {
  return call("set_qwen_channel", { channel, autoFallback });
}

export async function getQwenChannels(force = false): Promise<QwenChannelsStatus> {
  return call("get_qwen_channels", force ? { force: 1 } : {});
}

export async function getQwenToolcallMonitor(n = 40): Promise<ToolCallMonitor> {
  return call("get_qwen_toolcall_monitor", { n });
}

export async function clearQwenToolcallMonitor(): Promise<{ ok: boolean }> {
  return call("clear_qwen_toolcall_monitor");
}

export async function getQwenWebLog(n = 40): Promise<{ logs: string[] }> {
  return call("get_qwen_web_log", { n });
}

export async function getKimiAccounts(): Promise<{ accounts: KimiAccountInfo[] }> {
  return call("get_kimi_accounts");
}

export async function kimiAddAccount(name: string | undefined, rawToken: string): Promise<{ ok: boolean; account: unknown }> {
  return call("kimi_add_account", { name, raw_token: rawToken });
}

export async function kimiRemoveAccount(id: string): Promise<{ ok: boolean }> {
  return call("kimi_remove_account", { id });
}

export async function kimiEnableAccount(id: string, enabled: boolean): Promise<{ ok: boolean }> {
  return call("kimi_enable_account", { id, enabled });
}

export async function kimiVerifyAccount(id: string): Promise<{ ok: boolean }> {
  return call("kimi_verify_account", { id });
}

// ---- 网关管理（Qoder/Kimi 共用 API Keys 与日志）----

export async function getGatewayKeys(): Promise<{ keys: GatewayApiKey[] }> {
  return call("get_gateway_keys");
}

export async function createGatewayKey(
  name: string,
  region?: string,
): Promise<{ ok: boolean; key: GatewayApiKey }> {
  return call("create_gateway_key", region ? { name, region } : { name });
}

/** 取（或按需创建）WorkBuddy 池区域专用接入密钥；create=false 时找不到仅回 null。 */
export async function getWorkbuddyPoolRegionKey(
  region: "cn" | "intl",
  create: boolean,
): Promise<{ ok: boolean; created: boolean; key: GatewayApiKey | null }> {
  return call("get_workbuddy_pool_region_key", { region, create: create ? "1" : "0" });
}

export async function deleteGatewayKey(id: string): Promise<{ ok: boolean }> {
  return call("delete_gateway_key", { id });
}

export async function getGatewayLogs(gateway?: string, limit = 50): Promise<{ logs: GatewayLogEntry[] }> {
  const args: Record<string, unknown> = { limit };
  if (gateway) args.gateway = gateway;
  return call("get_gateway_logs", args);
}

export async function getGatewayOverview(): Promise<GatewayOverview> {
  return call("get_gateway_overview");
}

// ---- 渠道健康度监控 ----

export async function getChannelHealth(hours: number): Promise<ChannelHealthOverview> {
  return call("get_channel_health", { hours });
}

export async function getChannelHealthDetail(gatewayId: string, hours: number): Promise<ChannelDetail> {
  return call("get_channel_health_detail", { gateway: gatewayId, hours });
}

// ---- 模型可用性自测（webui 后端）----

/** 对指定「网关 + 模型」发起一次真实调用，返回可用性结论。
 *  注意：**失败也是一次有效结论**（后端恒返回 200，用 `ok`/`code`/`reason` 表达），
 *  所以这里不会 throw，调用方直接读结果即可。 */
export async function testModel(input: ModelTestRequest): Promise<ModelTestResult> {
  return call("model_test", input as unknown as Record<string, unknown>);
}

// ---- 模型目录自动同步（model-sync · 仅 webui 后端支持；契约 docs/model-sync-contract.md §四/§五）----

/** 同步模式：auto=有上游清单源（定时拉取 diff）；manual=无清单概念，仅页面手动补录（glmweb/qoder）。 */
export type ModelSyncMode = "auto" | "manual";

/** GET /api/model-sync/status 的单网关行。
 *  可选字段容错：首次使用（state 文件不存在=未同步）时 ok/lastSyncTs/total/addedCount
 *  可为 null 或缺省，调用方按「未同步」正常态展示。 */
export interface ModelSyncGatewayStatus {
  /** 网关标识（glm/glmweb/kimi/grok/qwen/qoder/loomy/miaoshou/trae/workbuddy/workbuddy-pool/opencode）。 */
  gateway: string;
  mode?: ModelSyncMode;
  /** 上游清单来源描述（如「www.kimi.com GetAvailableModels」）。 */
  source?: string | null;
  /** 上一轮该网关是否成功；从未同步为 null。 */
  ok?: boolean | null;
  /** 上次同步时刻（ms epoch）；从未同步为 null/0。 */
  lastSyncTs?: number | null;
  /** 上次同步后的目录总数。 */
  total?: number | null;
  /** 近一轮新增数。 */
  addedCount?: number | null;
  lastError?: string | null;
}

/** GET /api/model-sync/status 响应（读 state，秒开不触发拉取）。 */
export interface ModelSyncStatus {
  enabled: boolean;
  intervalMinutes: number;
  /** 下轮预计运行提示（ms epoch 或后端自定义文案）；暂停/未知为 null。 */
  nextRunHint?: number | string | null;
  gateways: ModelSyncGatewayStatus[];
}

/** 新增模型明细（id + 可选显示名/倍率——倍率目前仅 WorkBuddy ACP 目录提供）。 */
export interface ModelSyncAddedModel {
  id: string;
  name?: string | null;
  multiplier?: number | null;
}

/** 单网关单轮结果（契约 §3.4）。 */
export interface ModelSyncGatewayResult {
  gateway: string;
  mode?: ModelSyncMode;
  ok?: boolean | null;
  total?: number | null;
  /** 本轮相对上次快照新增的模型（保序）；基线轮/失败轮为空。 */
  added?: ModelSyncAddedModel[] | null;
  /** 首轮基线：只记录存量清单，不计「新增」。 */
  baseline?: boolean;
  error?: string | null;
}

/** 一轮同步结果（run 返回值 / log 元素共用形状）。 */
export interface ModelSyncRound {
  ts?: number;
  trigger?: "auto" | "manual";
  durationMs?: number;
  results?: ModelSyncGatewayResult[] | null;
}

/** GET/POST /api/model-sync/config 的配置体。 */
export interface ModelSyncConfig {
  enabled: boolean;
  intervalMinutes: number;
}

/** 同步总览（含每网关状态；未同步是首次使用的正常态）。 */
export async function getModelSyncStatus(): Promise<ModelSyncStatus> {
  return call("get_model_sync_status");
}

/** 立即跑一轮（可选 gateways 过滤）；busy 时后端 409，原样抛错由调用方做友好提示。 */
export async function runModelSync(gateways?: string[]): Promise<ModelSyncRound> {
  const res = await call<ModelSyncRound & { round?: ModelSyncRound }>(
    "run_model_sync",
    gateways && gateways.length > 0 ? { gateways } : {},
  );
  // 兼容后端可能的 { round: {...} } 包裹；裸轮次形状直接透传。
  return res.round ?? res;
}

/** 环形日志倒序（近 N 轮，默认 10）；兼容裸数组与 { rounds | log } 包裹两种返回。 */
export async function getModelSyncLog(limit = 10): Promise<ModelSyncRound[]> {
  const res = await call<
    ModelSyncRound[] | { rounds?: ModelSyncRound[]; log?: ModelSyncRound[] }
  >("get_model_sync_log", { limit });
  if (Array.isArray(res)) return res;
  return res.rounds ?? res.log ?? [];
}

/** 读取同步配置（enabled + intervalMinutes）。 */
export async function getModelSyncConfig(): Promise<ModelSyncConfig> {
  return call("get_model_sync_config");
}

/** 保存同步配置（热更新，即写盘，定时轮下一轮生效）。 */
export async function saveModelSyncConfig(
  patch: Partial<ModelSyncConfig>,
): Promise<ModelSyncConfig> {
  return call("save_model_sync_config", patch as unknown as Record<string, unknown>);
}

/** 手动补录（仅 glmweb/qoder 两个 Manual 网关接收；Auto 网关后端 400 并说明原因）。 */
export async function addModelSyncRegistry(input: {
  gateway: string;
  modelId: string;
  displayName?: string;
  note?: string;
}): Promise<{ ok: boolean; message?: string }> {
  return call("add_model_sync_registry", input as unknown as Record<string, unknown>);
}

// ---- ChatGLM 网页协议网关（仅 webui 后端支持）----

export async function getGlmwebTokens(): Promise<{ tokens: { index: number; type: string; preview: string; hasCachedAccess: boolean; accessExpiresAt: number | null }[]; pool: { accounts: number; guestSlots: number; refreshAccounts: number; withCachedAccess: number } }> {
  return call("get_glmweb_tokens");
}

export async function glmwebAddToken(token: string): Promise<{ ok: boolean; count: number; guestSlots: number }> {
  return call("glmweb_add_token", { token });
}

export async function glmwebRemoveToken(index: number): Promise<{ ok: boolean }> {
  return call("glmweb_remove_token", { index });
}

/** v0.1.46：幂等写回 chatglm refresh token（浏览器同步工具调用）。 */
export async function glmwebSyncToken(input: {
  token: string;
  validate?: boolean;
  source?: string;
}): Promise<{
  ok: boolean;
  added: boolean;
  rotated: boolean;
  validated: boolean;
  account?: { preview?: string; accessExpiresAt?: number | null; needRenew?: boolean };
  pool?: {
    accounts: number;
    guestSlots: number;
    refreshAccounts: number;
    withCachedAccess: number;
    needRenew: number;
    defaultModel: string;
    degraded: boolean;
  };
}> {
  return call("glmweb_sync_token", input as unknown as Record<string, unknown>);
}

export async function glmwebTestCompletion(model: string, prompt: string): Promise<{ ok: boolean; model: string; content: string; latencyMs: number }> {
  return call("glmweb_test_completion", { model, prompt });
}

export async function qoderImportManual(input: { uid?: string; accessToken: string; refreshToken?: string; gateway?: string }): Promise<{ ok: boolean; account: QoderAccountInfo }> {
  return call("qoder_import_manual", input);
}

/** 导入本机 QoderCN（中国版 IDE）登录态 → Qoder CN 专用池（/qwen-flash/v1 凭据来源）。 */
export async function qoderImportLocalCn(): Promise<{ ok: boolean; account: QoderAccountInfo }> {
  return call("qoder_import_local_cn");
}

/** 导入本机 QoderWork CN（独立 Electron 应用）登录态 → 独立池（服务 /qwen-flash/v1）。 */
export async function qoderImportLocalWorkCn(): Promise<{ ok: boolean; account: QoderAccountInfo }> {
  return call("qoder_import_work_cn");
}

/** 手动指定 Qoder 账号地域（防误判）：cn / intl / auto（清除手动指定）。 */
export async function qoderSetAccountRegion(uid: string, region: "cn" | "intl" | "auto"): Promise<{ ok: boolean; account: QoderAccountInfo }> {
  return call("qoder_set_region", { uid, region });
}

export async function kimiTestCompletion(model: string, prompt: string): Promise<{ ok: boolean; model: string; content: string; latencyMs: number }> {
  return call("kimi_test_completion", { model, prompt });
}

export async function getGrokAccounts(): Promise<{ accounts: GrokAccountInfo[] }> {
  return call("get_grok_accounts");
}

// ---- Grok 账号池管理 ----

export async function getGrokPoolOverview(): Promise<GrokPoolOverview> {
  return call("get_grok_pool_overview");
}

export async function grokPoolCheck(): Promise<{ status: string; checked: unknown[]; webAccounts: number }> {
  return call("grok_pool_check");
}

export async function getGrokPoolPolicy(): Promise<GrokPoolPolicy> {
  return call("get_grok_pool_policy");
}

export async function setGrokPoolPolicy(policy: Partial<GrokPoolPolicy>): Promise<{ ok: boolean }> {
  return call("set_grok_pool_policy", policy as Record<string, unknown>);
}

export async function getGrokPoolHistory(): Promise<{ snapshots: GrokPoolSnapshot[] }> {
  return call("get_grok_pool_history");
}

// ---- Grok SSO 自动同步（专用浏览器 + CDP）----

export async function getGrokSsoStatus(): Promise<Record<string, unknown>> {
  return call("get_grok_sso_status");
}

export async function syncGrokSso(): Promise<{
  status: string;
  ssoPresent?: boolean;
  ssoRwPresent?: boolean;
  cookieCount?: number;
  written?: boolean;
  error?: string;
  attempt?: number;
}> {
  return call("sync_grok_sso");
}

export async function openGrokSsoLogin(): Promise<{ ok: boolean; port?: number; message?: string; error?: string }> {
  return call("open_grok_sso_login");
}

// ---- 注册机（专用 Chrome 实例；仅手动触发，零自动）----

/** 只读状态探测：不触发注册、不启动浏览器。 */
export async function getGrokRegisterStatus(): Promise<GrokRegisterStatus> {
  return call("get_grok_register_status");
}

/** 手动打开注册机：启动专用 Chrome 并打开 grok.com 注册页（未登录时即注册页）。 */
export async function openGrokRegister(): Promise<GrokRegisterStatus> {
  return call("open_grok_register");
}

/** 抓取注册成果：读取专用实例的 sso/sso-rw cookie，以固定 ID grok_register 写入账号池。 */
export async function captureGrokRegister(): Promise<{
  ok: boolean;
  status: string;
  accountId?: string;
  accountName?: string;
  cookieCount?: number;
  error?: string;
  /** 后端人话（not_registered 时给出会话失效/重新登录指引） */
  message?: string;
}> {
  return call("capture_grok_register");
}

/** 关闭注册机：只结束带该 profile 目录的 Chrome 进程，不影响日常浏览器。 */
export async function closeGrokRegister(): Promise<GrokRegisterStatus> {
  return call("close_grok_register");
}

export async function grokAddAccount(input: {
  name: string;
  provider: string;
  token: string;
  tier?: string;
}): Promise<{ ok: boolean; account: GrokAccountInfo }> {
  return call("grok_add_account", input);
}

export async function grokRemoveAccount(id: string): Promise<{ ok: boolean }> {
  return call("grok_remove_account", { id });
}

export async function grokEnableAccount(id: string, enabled: boolean): Promise<{ ok: boolean }> {
  return call("grok_enable_account", { id, enabled });
}

export async function grokTest(input: {
  model: string;
  prompt: string;
  enableThinking?: boolean;
  accountId?: string;
}): Promise<GrokTestResult> {
  return call("grok_test", input);
}

export async function getQoderAccounts(): Promise<{ accounts: QoderAccountInfo[] }> {
  return call("get_qoder_accounts");
}

export async function qoderImportLocal(): Promise<{ ok: boolean; account: QoderAccountInfo }> {
  return call("qoder_import_local");
}

export async function qoderRemoveAccount(uid: string): Promise<{ ok: boolean }> {
  return call("qoder_remove_account", { uid });
}

export async function qoderRefreshAccount(uid: string): Promise<{ ok: boolean }> {
  return call("qoder_refresh_account", { uid });
}

export async function qoderDeviceFlowStart(): Promise<{ ok: boolean; session: { authorizationUrl: string; nonce: string; verifier: string; startedAt: number } }> {
  return call("qoder_device_flow_start");
}

export async function qoderDeviceFlowComplete(session: { authorizationUrl: string; nonce: string; verifier: string; startedAt: number }): Promise<{ ok: boolean; account: QoderAccountInfo }> {
  return call("qoder_device_flow_complete", { session });
}

export async function qoderTestCompletion(model: string, prompt: string): Promise<{ ok: boolean; model: string; content: string; latencyMs: number }> {
  return call("qoder_test_completion", { model, prompt });
}

// ---- Loomy（讯飞）账号池与代理（仅 webui 后端支持）----

export async function getLoomyStatus(): Promise<{
  localLogin: boolean;
  dataDir?: string | null;
  apiHost: string;
}> {
  return call("get_loomy_status");
}

export async function getLoomyAccounts(): Promise<{ accounts: LoomyAccountInfo[] }> {
  return call("get_loomy_accounts");
}

export async function addLoomyAccount(input: {
  session: string;
  label?: string;
}): Promise<{ ok: boolean; account: LoomyAccountInfo }> {
  return call("add_loomy_account", input as unknown as Record<string, unknown>);
}

export async function importLoomyLocal(): Promise<{ ok: boolean; account: LoomyAccountInfo }> {
  return call("import_loomy_local");
}

export async function removeLoomyAccount(id: string): Promise<{ ok: boolean }> {
  return call("remove_loomy_account", { id });
}

export async function getLoomyCredits(pool = false): Promise<{
  ok: boolean;
  balance?: number;
  dailyBalance?: number;
  error?: string;
  accounts?: { id: string; label: string; credit: LoomyCreditInfo }[];
}> {
  return httpCall("get_loomy_credits", pool ? { pool: "true" } : undefined);
}

export async function loomyCheckinAll(): Promise<{ accounts: LoomyCheckinResult[] }> {
  return call("loomy_checkin_all");
}

export async function loomyCheckinOne(id: string): Promise<LoomyCheckinResult> {
  return call("loomy_checkin_one", { id });
}

export async function getLoomyProxyConfig(): Promise<LoomyProxyConfigInfo> {
  return call("get_loomy_proxy_config");
}

export async function saveLoomyProxyConfig(patch: {
  enabled?: boolean;
  api_key?: string;
}): Promise<{ ok: boolean }> {
  return call("save_loomy_proxy_config", patch as unknown as Record<string, unknown>);
}

export async function getLoomyProxyStatus(): Promise<LoomyProxyStatus> {
  return call("get_loomy_proxy_status");
}

export async function getLoomyModelsCatalog(): Promise<{ object: string; data: LoomyModel[] }> {
  return httpCall("get_loomy_models_catalog");
}

// ---- 妙手（美团智能体桌面应用）账号池与积分（仅 webui 后端支持）----

export async function getMiaoshouAccounts(): Promise<{ accounts: MiaoshouAccountInfo[] }> {
  return call("get_miaoshou_accounts");
}

/** 导入本机妙手（美团智能体桌面应用）登录态 → 账号池。 */
export async function miaoshouImportLocal(): Promise<{ ok: boolean; account: MiaoshouAccountInfo }> {
  return call("miaoshou_import_local");
}

export async function miaoshouRemoveAccount(uid: string): Promise<{ ok: boolean }> {
  return call("miaoshou_remove_account", { uid });
}

/** 账号状态与积分明细；妙手无每日签到（积分按月刷新），checkinSupported 恒为 false。 */
export async function getMiaoshouStatus(): Promise<{ accounts: MiaoshouStatusItem[] }> {
  return call("get_miaoshou_status");
}

export async function miaoshouCheckinAll(): Promise<{ accounts: MiaoshouCheckinResult[] }> {
  return call("miaoshou_checkin_all");
}

export async function miaoshouCheckinOne(id: string): Promise<MiaoshouCheckinResult> {
  return call("miaoshou_checkin_one", { id });
}

/** 全账号积分明细（余额 / 套餐 / 下次刷新时间）。 */
/** 妙手桌面端模型目录（webui）。force=true 绕过服务端 10 分钟缓存直拉上游。 */
export async function getMiaoshouModels(force = false): Promise<{ object: string; data: MiaoshouModelInfo[] }> {
  return call("get_miaoshou_models", force ? { force: 1 } : {});
}

/** 妙手反代试调（webui 内部用，走 webui 会话而非网关密钥）。 */
export async function miaoshouChat(input: {
  model: string;
  messages: Array<{ role: string; content: string }>;
  stream?: boolean;
}): Promise<{
  id?: string;
  choices?: Array<{ message?: { role?: string; content?: string; reasoning_content?: string } }>;
  usage?: unknown;
  error?: { message?: string };
}> {
  return call("miaoshou_chat", input as unknown as Record<string, unknown>);
}

export async function getMiaoshouCredits(): Promise<{
  ok: boolean;
  accounts: MiaoshouStatusItem[];
  error?: string;
}> {
  return call("get_miaoshou_credits");
}

// ---- DuMate（百度搭子）渠道（契约 docs/dumate-channel-contract.md §4.1；仅 webui 后端支持）----

/** GET /api/dumate/status——渠道聚合状态（桌面端/引擎/凭据/网关 + 积分快照）。 */
export async function getDumateStatus(): Promise<DumateStatus> {
  return call("get_dumate_status");
}

/** GET /api/dumate/credits——积分余额（totalPoints/usedPoints 为上游字符串数值）。 */
export async function getDumateCredits(): Promise<DumateCredits> {
  return call("get_dumate_credits");
}

/** GET /api/dumate/config——网关接入配置（密钥/开关）。 */
export async function getDumateConfig(): Promise<DumateConfigInfo> {
  return call("get_dumate_config");
}

/** POST /api/dumate/config——保存网关开关/密钥；regenerateKey 为 true 时轮换密钥。 */
export async function saveDumateConfig(patch: {
  enabled?: boolean;
  apiKey?: string;
  regenerateKey?: boolean;
}): Promise<DumateConfigInfo> {
  return call("save_dumate_config", patch as unknown as Record<string, unknown>);
}

/** POST /api/dumate/checkin——本机账号一键签到（幂等：服务端 hasIssued 短路，already=今日已领）。 */
export async function dumateCheckin(): Promise<DumateCheckinResult> {
  return call("dumate_checkin");
}

/** GET /api/dumate/checkin-status——今日签到状态（loginBonusInfo 聚合）。 */
export async function getDumateCheckinStatus(): Promise<DumateCheckinStatus> {
  return call("get_dumate_checkin_status");
}

// ---- 统一账号管理（GET /api/accounts/unified；契约 docs/unified-accounts-contract.md §2；仅 webui 后端支持）----

/** 聚合全部平台账号为统一 schema（displayName 无遮罩全名，备注优先；含 summary 统计块）。 */
export function getUnifiedAccounts(): Promise<UnifiedAccountsPayload> {
  return call("get_unified_accounts");
}

// ---- DuMate 多账号池（契约 §1；仅 webui 后端支持）----
// 边界（契约 §0/§8）：多账号只用于「签到/积分」（纯 cookie 接口，一号一独立出站 IP）；
// 模型调用引擎绑定桌面端当前登录用户，仍走本机主账号——UI 文案需如实说明。

/** GET /api/dumate/accounts——账号池列表（含本机 dumate:local，isLocal 标记；不回 cookies 明文，只回 cookieCount）。 */
export async function getDumateAccounts(): Promise<{ accounts: DumatePoolAccount[] }> {
  const res = await call<{ accounts?: DumatePoolAccount[] } | DumatePoolAccount[]>(
    "get_dumate_accounts",
  );
  // 容错：契约形状为 { accounts: [...] }，裸数组也接受
  if (Array.isArray(res)) return { accounts: res };
  return { accounts: res.accounts ?? [] };
}

/** 池账号/本机账号单账号签到结果（POST /api/dumate/account/checkin，契约 §1.2）。 */
export interface DumatePoolCheckinResult {
  /** success=本次签到成功(+500) / already=今日已领 / error=失败 */
  result: "success" | "already" | "error" | string;
  /** 人话说明 */
  message?: string | null;
  /** 本次入账积分（成功时） */
  points?: string | number | null;
  ok?: boolean;
  error?: string | null;
}

/**
 * POST /api/dumate/accounts——添加池账号（契约 §1.2 add_account）。
 * cookies 为已解析 JSON：数组或 {cookies:[...]}，逐条需含非空 name/value；
 * proxy 为一号一 IP 出站代理（http:// 或 socks5://），空 = 直连。
 */
export async function addDumateAccount(input: {
  name: string;
  cookies: unknown;
  proxy?: string;
  note?: string;
}): Promise<{ ok?: boolean; account?: DumatePoolAccount }> {
  return call("add_dumate_account", input as unknown as Record<string, unknown>);
}

/** POST /api/dumate/accounts/update——更新池账号（name/proxy/note 可选，缺省字段不修改；契约 §1.5）。 */
export function updateDumateAccount(patch: {
  id: string;
  name?: string;
  proxy?: string;
  note?: string;
}): Promise<{ ok?: boolean }> {
  return call("update_dumate_account", patch as unknown as Record<string, unknown>);
}

/** POST /api/dumate/accounts/remove——移除池账号（仅 dumate:pool_*；契约 §1.5）。 */
export function removeDumateAccount(id: string): Promise<{ ok?: boolean }> {
  return call("remove_dumate_account", { id });
}

/** POST /api/dumate/accounts/import-client——导入本机客户端登录态为池账号（按 bce-user-info 去重，重复报「已是同一登录态」）。 */
export function importDumateClient(): Promise<{
  ok?: boolean;
  account?: DumatePoolAccount;
  message?: string;
}> {
  return call("import_dumate_client");
}

/** POST /api/dumate/account/checkin——单账号签到（id 可为 dumate:local 或 dumate:pool_*，都走此入口；幂等，契约 §1.5）。 */
export function dumateAccountCheckin(id: string): Promise<DumatePoolCheckinResult> {
  return call("dumate_account_checkin", { id });
}

/** 网关测试对话结果（前端 fetch 直连 /dumate/v1 的聚合返回）。 */
export interface DumateTestChatResult {
  ok: boolean;
  /** assistant 回复文本（空回复时为占位文案） */
  content: string;
  /** 实际服务的模型名（取响应体 model，缺省回请求模型） */
  model: string;
  latencyMs: number;
  usage?: {
    prompt_tokens?: number;
    completion_tokens?: number;
    total_tokens?: number;
  } | null;
  error?: string;
}

/**
 * DuMate 网关测试对话：fetch 直连 POST /dumate/v1/chat/completions（非流式聚合），
 * 鉴权 = DuMate 网关密钥或主网关密钥（后端三态放行），对齐其他网关「测试模型」的做法。
 * 注意：fetch 直连不走 `call`（/dumate/v1 是网关命名空间，不是 /api 命名空间命令）。
 */
export async function dumateTestChat(input: {
  model: string;
  prompt: string;
  /** 网关密钥；空串 = 不带鉴权头（后端对未配置密钥的网关放行） */
  apiKey: string;
}): Promise<DumateTestChatResult> {
  const started = Date.now();
  const base: DumateTestChatResult = {
    ok: false,
    content: "",
    model: input.model,
    latencyMs: 0,
  };
  try {
    const headers: Record<string, string> = { "Content-Type": "application/json" };
    if (input.apiKey) headers.Authorization = `Bearer ${input.apiKey}`;
    const res = await fetch("/dumate/v1/chat/completions", {
      method: "POST",
      headers,
      body: JSON.stringify({
        model: input.model,
        messages: [{ role: "user", content: input.prompt }],
        stream: false,
      }),
    });
    const data = (await res.json().catch(() => ({}))) as {
      model?: string;
      choices?: Array<{ message?: { content?: string } }>;
      usage?: { prompt_tokens?: number; completion_tokens?: number; total_tokens?: number };
      error?: { message?: string } | string;
      message?: string;
    };
    const latencyMs = Date.now() - started;
    if (!res.ok) {
      const errMsg =
        (typeof data.error === "object" && data.error?.message) ||
        (typeof data.error === "string" ? data.error : undefined) ||
        data.message ||
        `请求失败 (HTTP ${res.status})`;
      return { ...base, latencyMs, error: errMsg };
    }
    const content = data.choices?.[0]?.message?.content ?? "";
    return {
      ok: true,
      content: content || "(空回复)",
      model: data.model || input.model,
      latencyMs,
      usage: data.usage ?? null,
    };
  } catch (e) {
    return { ...base, latencyMs: Date.now() - started, error: asError(e) };
  }
}

// ---- TRAE / WorkBuddy 模型代理（仅 webui 后端支持）----

export interface TraeProxyStatus {
  credential: { ok: boolean; user?: string; host?: string; error?: string };
  models: { count: number };
  chatForwarding: boolean;
}

export interface TraeProxyModel {
  id: string;
  display_name?: string | null;
  scene?: string;
  model_type?: string;
  rate?: number | null;
  reasoning?: boolean;
  multimodal?: boolean;
  context_max?: number | null;
  status?: boolean;
}

export interface WorkbuddyProxyStatus {
  enabled: boolean;
  server: { ok: boolean; port?: number; error?: string };
  providers?: { id: string; name?: string | null; models: { id: string; name?: string | null; reasoning?: boolean; context?: number | null }[] }[];
  modelCount?: number;
}

/** WorkBuddy 反代接入配置（GET /api/workbuddy-proxy/config）。 */
export interface WorkbuddyProxyConfigInfo {
  enabled: boolean;
  /** 客户端填入的 Base URL，如 http://127.0.0.1:57890/workbuddy/v1 */
  endpoint: string;
  apiKey: string;
  /** opencode server 端口，0 = 自动发现 */
  serverPort: number;
}

/** TRAE 反代接入配置（GET /api/trae-proxy/config）。
 *
 * v0.1.59+：TRAE 有了自有密钥体系（`traeKey`）；迁移窗口内仍兼容 Loomy 代理的
 * 旧密钥，`keySource` 如实反映当前生效来源（`trae` = TRAE 自有密钥，
 * `loomy` = 仍在迁移窗口、复用 Loomy 代理的密钥）。
 */
export interface TraeProxyConfigInfo {
  endpoint: string;
  apiKey: string;
  /** TRAE 自有密钥（迁移窗口内与 apiKey 可能同值，窗口结束后 apiKey 即该值）。 */
  traeKey?: string;
  keySource: "loomy" | "trae";
  chatForwarding: boolean;
  credential?: { ok: boolean; user?: string; host?: string; error?: string };
  modelCount?: { count: number };
}

export async function getTraeProxyStatus(): Promise<TraeProxyStatus> {
  return httpCall("get_trae_proxy_status");
}

export async function getTraeProxyModels(): Promise<{
  ok: boolean;
  count?: number;
  models?: TraeProxyModel[];
  error?: string;
}> {
  return httpCall("get_trae_proxy_models");
}

export async function getWorkbuddyProxyStatus(): Promise<WorkbuddyProxyStatus> {
  return httpCall("get_workbuddy_proxy_status");
}

export async function getWorkbuddyProxyModels(): Promise<{
  ok: boolean;
  providers?: WorkbuddyProxyStatus["providers"];
  error?: string;
}> {
  return httpCall("get_workbuddy_proxy_models");
}

/** WorkBuddy 反代接入配置（Base URL / 密钥 / 端口）。 */
export async function getWorkbuddyProxyConfig(): Promise<WorkbuddyProxyConfigInfo> {
  return httpCall("get_workbuddy_proxy_config");
}

/** WorkBuddy 内置模型目录来源（契约 §3.1）：acp-live=实时 ACP 探测 /
 *  acp-cache=300s TTL 缓存 / static-fallback=桌面端未运行的静态兜底。 */
export type WorkbuddyNativeModelsSource = "acp-live" | "acp-cache" | "static-fallback";

/** WorkBuddy 内置模型条目（GET /workbuddy-native/v1/models）：
 *  旧字段 {id,object,created,owned_by} 原样保留；name/multiplier/kind 为 2026-09-25
 *  native 多模型改造扩展字段（旧后端缺省 → 可选，向后兼容）。 */
export type WorkbuddyNativeModel = {
  id: string;
  object: string;
  created: number;
  owned_by: string;
  /** 模型名（如「快速」「GLM-5.3-Flash」）；旧后端缺省。 */
  name?: string | null;
  /** 计价倍率（模型选择项描述里的 x0.06 → 0.06）；解析失败为 null。 */
  multiplier?: number | null;
  /** 条目类别：tier=档位别名 / model=实模 / custom=用户自定义接入（?include=custom 才下发）；旧后端缺省。 */
  kind?: "tier" | "model" | "custom" | null;
};

/** WorkBuddy 桌面端自带模型目录响应（契约 §3.1）：
 *  默认只列 native（kind=tier|model）；source/probedAt/currentModelId 为扩展字段（旧后端缺省）。 */
export type WorkbuddyNativeModelsResponse = {
  object: string;
  /** 目录来源；static-fallback 时 probedAt / currentModelId 为 null。 */
  source?: WorkbuddyNativeModelsSource | null;
  /** 探测时刻（ms epoch）；static-fallback 时为 null。 */
  probedAt?: number | null;
  /** 会话默认模型；static-fallback 时为 null。 */
  currentModelId?: string | null;
  /** 桌面端登录区域（cn=国内版 / intl=国外版 / unknown）；旧后端缺省。 */
  region?: string | null;
  data: WorkbuddyNativeModel[];
};

/** WorkBuddy 桌面端自带模型目录（/workbuddy-native/v1/models，实时探测内嵌 ACP 会话模型目录）。 */
export async function getWorkbuddyNativeModels(): Promise<WorkbuddyNativeModelsResponse> {
  return httpCall("get_workbuddy_native_models");
}

/** 目录刷新返回的条目（POST /api/workbuddy-native/catalog/refresh，契约 §3.1）：
 *  后端 catalog_to_payload 以 snake_case 直出；exposed=false 表示该条目
 *  仅来自客户端产品配置补充（ACP availableModels 暂未暴露）。 */
export type WbCatalogEntry = {
  id: string;
  name: string;
  /** 计价倍率；上游未提供为 null（展示为 —）。 */
  multiplier: number | null;
  /** tier=档位别名 / model=内置实模 / custom=用户自定义接入。 */
  kind: "tier" | "model" | "custom";
  /** ACP availableModels 命中=true；仅来自产品配置=false。 */
  exposed: boolean;
  owned_by: string;
};

/** 清缓存重探 WorkBuddy 内置模型目录（绕 300s TTL；契约 §2.4/§3.1）。
 *  后端失败时返回 502 + 错误串，httpCall 会抛 Error（message 取自响应体）。 */
export async function refreshWorkbuddyCatalog(): Promise<{
  object: string;
  data: WbCatalogEntry[];
}> {
  return call("refresh_workbuddy_catalog");
}

/** 保存 WorkBuddy 反代接入配置；regenerateKey 为 true 时轮换密钥。 */
export async function saveWorkbuddyProxyConfig(patch: {
  enabled?: boolean;
  serverPort?: number;
  apiKey?: string;
  regenerateKey?: boolean;
}): Promise<WorkbuddyProxyConfigInfo> {
  return httpCall("save_workbuddy_proxy_config", patch);
}

/* -------------------------------------------------------------------------- */
/* WorkBuddy 多账号网关（cloud 账号池 · /workbuddy-pool/v1 · 2026-09-24 重构）   */
/* -------------------------------------------------------------------------- */

/** 池区域聚合条目（契约 workbuddy-pool-region-contract.md §3.5 status.regions）：
 *  各区域的账号数 / 可用数 / 目录模型数。models 取自区域目录缓存，缓存未建为 null。 */
export interface WorkbuddyPoolRegionStatus {
  accounts: number;
  healthy: number;
  models?: number | null;
}

/** GET /workbuddy-pool/v1/status 响应：池状态**扁平直出**（total/healthy/cooling/
 *  model_cooling/breaker/accounts 在顶层），另带顶层 config（camelCase）与 apiKey。
 *
 *  2026-09-24 修复：旧版误按 { ok, pool, config } 嵌套信封声明并读取（res.pool），
 *  与后端扁平输出不匹配 → res.pool 恒 undefined → setPoolStatus 永不执行 →
 *  账号表恒显「账号池暂无账号」。后端形状以 api.rs status handler 实测为准。
 *
 *  2026-09-25 区域隔离（契约 §3.5）：新增顶层 regions 聚合（cn/intl 的
 *  accounts/healthy/models）。**仍是扁平响应**——前端 setPoolStatus(res) 直接读顶层，
 *  regions 为 additive 可选字段（旧后端缺省 → undefined，区域概况行不渲染）。 */
export type WorkbuddyPoolStatusResponse = WorkbuddyPoolStatus & {
  config?: WorkbuddyGatewayConfig;
  apiKey?: string;
  /** 区域聚合（契约 §3.5）；旧后端缺省 → undefined */
  regions?: {
    cn?: WorkbuddyPoolRegionStatus;
    intl?: WorkbuddyPoolRegionStatus;
  } | null;
};

/** 池状态总览（total/healthy/cooling/model_cooling/breaker + 账号明细）。 */
export async function getWorkbuddyPoolStatus(): Promise<WorkbuddyPoolStatusResponse> {
  return call("get_workbuddy_pool_status");
}

/** PATCH 网关配置（热更新，即写即生效；字段为 camelCase，缺省字段不修改）。 */
export async function patchWorkbuddyPoolConfig(
  patch: Partial<WorkbuddyGatewayConfig>,
): Promise<{ ok: boolean; config?: WorkbuddyGatewayConfig }> {
  return call("patch_workbuddy_pool_config", patch as Record<string, unknown>);
}

/** 池模型目录条目：OpenAI list 扁平形状 + 附加 region 字段（契约 §3.4 additive）。
 *  region: "cn"|"intl"；旧后端缺省 → undefined（前端分组退化为单一组）。
 *  跨区同名模型（glm-5.3）两区各保留一条，靠 region 区分。 */
export interface WorkbuddyPoolModelEntry {
  id: string;
  object?: string;
  created?: number;
  owned_by?: string;
  /** 条目所属区域（契约 §3.4）；旧后端缺省 → undefined */
  region?: string;
  /** cloud 上游元数据（intl 区特有）：上下文长度 / 输出上限 tokens */
  context_length?: number;
  max_output_tokens?: number;
  /** 上游显示名（如 "Auto" / "GLM-5.3-Flash"）；2026-09-26 起下发，旧后端缺省 */
  name?: string;
  /** 计价倍率（credits 原文 "x0.34 credits" 解析）；未标价/解析失败为 null，旧后端缺省 */
  multiplier?: number | null;
}

/** 池模型目录区域来源元信息（契约 §3.4 regions）：cn 区 dynamic=动态目录 /
 *  fallback=回落内置表；intl 区 dynamic=动态 / static=静态表。旧后端缺省 → undefined。 */
export interface WorkbuddyPoolRegionCatalogMeta {
  source?: "dynamic" | "static" | "fallback";
  /** 提供目录的账号 uid；缺省 → 不展示 */
  account?: string;
}

/** GET /workbuddy-pool/v1/models 响应（契约 §3.4）：**保持 OpenAI list 扁平形状**，
 *  regions 与 data[].region 均为 additive 可选字段；不带 region 参数 = 两区全量
 *  （?region=cn|intl 过滤后只含单区，由后端处理）。旧后端无 region → 单一组容错。 */
export interface WorkbuddyPoolModelsResponse {
  object: string;
  /** 各区目录来源（契约 §3.4）；旧后端缺省 → undefined（来源徽标不渲染） */
  regions?: {
    cn?: WorkbuddyPoolRegionCatalogMeta | null;
    intl?: WorkbuddyPoolRegionCatalogMeta | null;
  } | null;
  data: WorkbuddyPoolModelEntry[];
}

/** 池内模型目录（契约 §3.4 按区聚合：cn 取首个成功 cn 账号动态目录，失败回落内置表；
 *  intl 取首个成功 intl 账号动态目录，无 intl 账号 / 失败回落 intl 静态表）。 */
export async function getWorkbuddyPoolModels(): Promise<WorkbuddyPoolModelsResponse> {
  return call("get_workbuddy_pool_models");
}

/** 多账号网关试调（OpenAI 兼容；非流式聚合返回，错误已按分级表换号重试）。
 *  region（契约 T2，可选）：显式 "cn"|"intl" 精确约束选号区域；缺省按模型区域解析
 *  （唯一命中 → 该区；两区同名 / 都无 → 不约束，上游校验兜底）。 */
export async function workbuddyPoolChat(input: {
  model: string;
  messages: Array<{ role: string; content: string }>;
  stream?: boolean;
  region?: "cn" | "intl";
}): Promise<{
  id?: string;
  choices?: Array<{
    message?: { role?: string; content?: string; reasoning_content?: string };
    finish_reason?: string | null;
  }>;
  usage?: unknown;
  error?: { message?: string };
}> {
  return call("workbuddy_pool_chat", input as unknown as Record<string, unknown>);
}

/** TRAE 反代接入配置（密钥复用 Loomy 代理）。 */
export async function getTraeProxyConfig(): Promise<TraeProxyConfigInfo> {
  return httpCall("get_trae_proxy_config");
}

/* -------------------------------------------------------------------------- */
/* OpenCode 网关（独立网关 · 2026-09-22 三拆新增）                                */
/* -------------------------------------------------------------------------- */

/** OpenCode 网关状态：本机 server 健康 + 免费/总模型数 + 渠道数。 */
export async function getOpencodeProxyStatus(): Promise<{
  enabled: boolean;
  server: { ok: boolean; port?: number; error?: string };
  channelCount: number;
  modelCount: number;
  freeModelCount: number;
  exposedModelCount: number;
}> {
  return httpCall("get_opencode_proxy_status");
}

/** OpenCode 网关接入信息（Base URL / 密钥 / 启用态）—— 独立密钥，与 WorkBuddy/TRAE 不共用。 */
export async function getOpencodeProxyConfig(): Promise<{
  enabled: boolean;
  endpoint: string;
  apiKey: string;
  note?: string;
}> {
  return httpCall("get_opencode_proxy_config");
}

/** 保存 OpenCode 网关配置；regenerateKey 为 true 时轮换密钥。 */
export async function saveOpencodeProxyConfig(patch: {
  enabled?: boolean;
  apiKey?: string;
  regenerateKey?: boolean;
}): Promise<{ enabled: boolean; endpoint: string; apiKey: string; note?: string }> {
  return httpCall("save_opencode_proxy_config", patch);
}

/** AutoClaw 网关状态探测（仅 webui 后端支持）。 */
export async function getGlmProxyStatus(): Promise<GlmProxyStatus> {
  return call("get_glm_proxy_status");
}

export async function saveGlmProxyConfig(patch: {
  enabled?: boolean;
  apiKey?: string;
  regenerateKey?: boolean;
  /** 速度优先：慢模型改投到更快的目标模型 */
  speedFirst?: boolean;
  speedFirstTarget?: string;
  /** 账号策略：round_robin / local_first / local_only / pinned */
  accountMode?: string;
  /** pinned 策略使用的账号 id */
  pinnedAccountId?: string;
}): Promise<GlmProxyConfigInfo> {
  return call("save_glm_proxy_config", patch as unknown as Record<string, unknown>);
}

export function getCreditExpiry(accountId: string): Promise<CreditExpiry> {
  return call("get_credit_expiry", { accountId });
}

export function getCreditStatistics(refresh = false): Promise<CreditStatistics> {
  return call("get_credit_statistics", refresh ? { refresh: true } : undefined);
}

export function getTokenStatistics(days?: number): Promise<TokenStatistics> { return call("get_token_statistics", days ? { days } : undefined); }

export function checkin(accountId: string): Promise<CheckinResult> {
  return call("checkin", { accountId });
}

export function checkinAll(): Promise<{
  accounts: { accountId: string; email: string; result: string; error?: string }[];
  status?: string;
  reason?: string;
}> {
  return call("checkin_all");
}

export function getAutoCheckinConfig(): Promise<CheckinConfig> {
  return call("get_auto_checkin_config");
}

export function saveAutoCheckinConfig(config: CheckinConfig): Promise<CheckinConfig> {
  return call("save_auto_checkin_config", {
    config: config as unknown as Record<string, unknown>,
  });
}

export function getExpiringSoonConfig(): Promise<ExpiringSoonConfig> {
  return call("get_expiring_soon_config");
}

export function saveExpiringSoonConfig(
  config: ExpiringSoonConfig,
): Promise<ExpiringSoonConfig> {
  return call("save_expiring_soon_config", {
    config: config as unknown as Record<string, unknown>,
  });
}

export function getCheckinLogs(): Promise<{ logs: CheckinLog[] }> {
  return call("get_checkin_logs");
}

export async function getTravelStatus(accountId: string): Promise<TravelStatus> {
  if (demoModeEnabled) {
    return screenshotDemoResponse("get_travel_status", { accountId }) as TravelStatus;
  }
  if (isWebui()) {
    // webui 端为批量接口，按 accountId 过滤
    const all = await httpCall<{
      accounts: { accountId: string; email: string; label: TravelStatus["label"]; rewardCredit: number | null; locationName?: string | null; arriveAt?: number | null }[];
    }>("get_travel_status");
    const one = all.accounts.find((a) => a.accountId === accountId);
    return one
      ? { label: one.label, rewardCredit: one.rewardCredit, locationName: one.locationName ?? null, arriveAt: one.arriveAt ?? null }
      : { label: "untraveled", rewardCredit: null, locationName: null, arriveAt: null };
  }
  return call("get_travel_status", { accountId });
}

export function getAutoTravelConfig(): Promise<TravelConfig> {
  return call("get_auto_travel_config");
}

export function saveAutoTravelConfig(config: TravelConfig): Promise<TravelConfig> {
  return call("save_auto_travel_config", {
    config: config as unknown as Record<string, unknown>,
  });
}

export function refreshAccountToken(accountId: string): Promise<AccountMeta> {
  return call("refresh_account_token", { accountId });
}

// ---------------------------------------------------------------------------
// 阶段 4：自动更新
// ---------------------------------------------------------------------------

export function getGithubConfig(): Promise<GithubConfig> {
  return call("get_github_config");
}

export function saveGithubConfig(config: GithubConfig): Promise<GithubConfig> {
  return call("save_github_config", {
    config: config as unknown as Record<string, unknown>,
  });
}

export function checkUpdate(proxy?: string, force?: boolean): Promise<UpdateInfo> {
  return call("check_update", { proxy: proxy?.trim() || null, force: force ?? false });
}

export function relaunchApp(): Promise<void> {
  return call("relaunch_app");
}

// ---------------------------------------------------------------------------
// 开机自启（仅桌面端；webui 不提供同名接口，卡片也不在 webui 渲染）
// ---------------------------------------------------------------------------

/** 查询系统当前的开机自启注册状态（桌面端）。 */
export function getLaunchAtLoginEnabled(): Promise<boolean> {
  if (demoModeEnabled) return call("get_launch_at_login_enabled");
  if (!isDesktop()) return Promise.resolve(false);
  return call("get_launch_at_login_enabled");
}

/** 注册 / 移除系统开机自启，返回回读后的权威状态（桌面端）。 */
export function setLaunchAtLoginEnabled(enabled: boolean): Promise<boolean> {
  if (demoModeEnabled) return Promise.reject(new Error(DEMO_UNAVAILABLE_MESSAGE));
  if (!isDesktop()) return Promise.resolve(false);
  return call("set_launch_at_login_enabled", { enabled });
}

// ---------------------------------------------------------------------------
// 账号保护与保活
// ---------------------------------------------------------------------------

export function getProtectionStatus(): Promise<AccountProtectionStatusInfo> {
  return call("get_protection_status");
}

export function getProtectionConfig(): Promise<AccountProtectionConfigInfo> {
  return call("get_protection_config");
}

export function saveProtectionConfig(
  config: Partial<AccountProtectionConfigInfo>,
): Promise<{ ok: boolean; config: AccountProtectionConfigInfo }> {
  return call("save_protection_config", config as Record<string, unknown>);
}

export function triggerProtectionKeepalive(): Promise<{ ok: boolean; report: unknown }> {
  return call("protection_keepalive_now");
}

export function clearProtectionCooldown(
  accountId: string,
): Promise<{ ok: boolean; cleared: boolean }> {
  return call("protection_clear_cooldown", { accountId });
}

export function getLoomyProtectionStatus(): Promise<AccountProtectionStatusInfo> {
  return call("get_loomy_protection_status");
}

export function triggerLoomyProtectionKeepalive(): Promise<{ ok: boolean; report: unknown }> {
  return call("loomy_protection_keepalive_now");
}

export function clearLoomyProtectionCooldown(
  accountId: string,
): Promise<{ ok: boolean; cleared: boolean }> {
  return call("loomy_protection_clear_cooldown", { accountId });
}

/** 把 Tauri command / HTTP 抛出的错误统一为 Error。 */
export function asError(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e ?? "未知错误");
}

/** 统一网关总览（/api/gateways/overview 的返回结构）。 */
export type GatewayModelInfo = {
  id: string | null;
  name: string | null;
  capabilities: string[];
  /** 计价倍率（如 x0.21 → 0.21）；上游未提供为 null；旧后端缺省（契约 §3.1）。 */
  multiplier?: number | null;
  /** 条目类别：tier=档位别名 / model=实模 / custom=自定义接入；旧后端缺省（契约 §3.1）。 */
  kind?: string;
  /** 是否由 ACP availableModels 实时暴露；false=来自客户端产品配置补充（契约 §3.1）。 */
  exposed?: boolean;
  /** 所属区域（WorkBuddy 池：cn=国内版 / intl=国际版，跨区同名各留一条）；其余网关为 null。 */
  region?: string | null;
};

export type GatewayInfo = {
  id: string;
  name: string;
  /** 前端管理页路由 */
  page: string;
  /** OpenAI 兼容调用基址 */
  base: string;
  enabled: boolean;
  /** ok | cooling | disabled | error */
  health: string;
  error?: string;
  accountCount: number;
  healthyAccounts: number;
  modelCount: number;
  capabilities: string[];
  models: GatewayModelInfo[];
};

export type GatewaysOverview = { gateways: GatewayInfo[] };

export interface QwenDesktopStatus {
  installed: boolean;
  install_path?: string | null;
  running: boolean;
  appdata_dir: string;
  token_found: boolean;
  user_id?: string | null;
  token_expires_at?: number | null;
  models_count: number;
  models: Array<{
    id: string;
    name: string;
    description?: string;
    capabilities?: string[];
  }>;
  notice: string;
}

export async function getQwenDesktopStatus(): Promise<QwenDesktopStatus> {
  const res = await fetch("/api/qwen/desktop/status");
  return res.json();
}


// ---- ChatGPT 通道（gpt-reg-review 托管+注册机控制+账号表；契约 docs/chatgpt-channel-contract.md §4）----
// 说明：v1 为 webui-first（桌面壳无对应 Tauri command，桌面模式报「不支持」属预期）。

/** GET /api/chatgpt/channel/status——通道聚合状态（WebUI 存活/端口/注册机快照/账号与号池计数）。 */
export function getChatGptChannelStatus(): Promise<ChatGptChannelStatus> {
  return call("get_chatgpt_channel_status");
}

/** POST /api/chatgpt/webui/start——托管拉起 WebUI（不活时 spawn python start_webui.py --no-browser，等健康 ≤20s）。 */
export function chatGptWebuiStart(): Promise<{ ok: boolean; port?: number }> {
  return call("chatgpt_webui_start");
}

/** POST /api/chatgpt/webui/stop——停止 WebUI（后端先停注册机，再按命令行含 start_webui.py 匹配杀进程）。 */
export function chatGptWebuiStop(): Promise<{ ok: boolean }> {
  return call("chatgpt_webui_stop");
}

/** POST /api/chatgpt/registrar/start——批量注册启动（body 透传工具链 /api/auto/start；
 *  target_count 必填 ≥1，0=不限量会无限跑）。已在外跑时后端透传 HTTP 400。 */
export function chatGptRegistrarStart(
  body: ChatGptRegistrarStartBody,
): Promise<Record<string, unknown>> {
  return call("chatgpt_registrar_start", body as unknown as Record<string, unknown>);
}

/** POST /api/chatgpt/registrar/pause——暂停批量注册循环。 */
export function chatGptRegistrarPause(): Promise<Record<string, unknown>> {
  return call("chatgpt_registrar_pause");
}

/** POST /api/chatgpt/registrar/resume——恢复批量注册循环。 */
export function chatGptRegistrarResume(): Promise<Record<string, unknown>> {
  return call("chatgpt_registrar_resume");
}

/** POST /api/chatgpt/registrar/stop——停止批量注册循环。 */
export function chatGptRegistrarStop(): Promise<Record<string, unknown>> {
  return call("chatgpt_registrar_stop");
}

/** GET /api/chatgpt/registrar/status——注册机快照透传（state/registered_ok/registered_fail/target_count）。 */
export function chatGptRegistrarStatus(): Promise<ChatGptRegistrarStatus> {
  return call("chatgpt_registrar_status");
}

/** GET /api/chatgpt/registered——已注册 ChatGPT 账号表透传（token 只回长度；账号表主源）。 */
export function chatGptRegistered(args?: { limit?: number; offset?: number }): Promise<ChatGptRegisteredResponse> {
  return call("chatgpt_registered", args as Record<string, unknown> | undefined);
}

/** GET /api/chatgpt/accounts——邮箱号池透传（outlook_accounts；用于池计数，非 ChatGPT 账号）。 */
export function chatGptAccounts(args?: { limit?: number; offset?: number }): Promise<ChatGptAccountsResponse> {
  return call("chatgpt_accounts", args as Record<string, unknown> | undefined);
}

/** POST /api/chatgpt/accounts/sync——把已注册账号镜像到本地池 ~/.wb-switch/chatgpt_accounts.json。 */
export function chatGptAccountsSync(): Promise<Record<string, unknown>> {
  return call("chatgpt_accounts_sync");
}

// ---- ZCode 账号池（契约 docs/zcode-channel-contract.md §2/§3；仅 webui 后端支持）----
// 说明：v1 为 webui-first（桌面壳无对应 Tauri command）；切换为冷切换——ZCode 运行中
// 默认拒绝，前端确认框说明会关闭 ZCode 后携 force=true 重试语义（契约 §3.2）。

/** GET /api/zcode/status——live 登录状态 + 账号池（含每账号 identity/额度缓存摘要）。 */
export async function getZcodeStatus(): Promise<ZcodeStatus> {
  const res = await call<ZcodeStatus>("get_zcode_status");
  // 容错：契约形状含 accounts[]；旧后端缺字段时兜底空池
  return { ...res, accounts: Array.isArray(res.accounts) ? res.accounts : [] };
}
export function updateZcodeAccount(input: {
  id: string;
  name?: string;
  proxy?: string;
}): Promise<{ ok?: boolean; error?: string }> {
  return call("update_zcode_account", input as unknown as Record<string, unknown>);
}

export async function saveZcodeClaimState(patch: { autoClaim?: boolean }): Promise<Record<string, unknown>> {
  return call("save_zcode_claim_state", patch);
}

export async function getZcodeClaimState(): Promise<Record<string, unknown>> {
  return call("get_zcode_claim_state");
}

export async function zcodeClaimScan(): Promise<Record<string, unknown>> {
  return call("zcode_claim_scan");
}

export async function zcodeClaimAuto(id: string): Promise<Record<string, unknown>> {
  return call("zcode_claim_auto", { id });
}

export async function zcodeClaimPreview(id?: string): Promise<Record<string, unknown>> {
  return call("zcode_claim_preview", id ? { id } : {});
}


/** POST /api/zcode/capture——把当前 live 登录态捕获入池（hash 去重：同指纹更新名字）。 */
export function captureZcode(name?: string): Promise<{
  ok?: boolean;
  account?: ZcodeAccount | null;
  message?: string;
  error?: string;
}> {
  return call("capture_zcode", { name: name?.trim() || undefined });
}

/**
 * POST /api/zcode/switch——切换到池中账号（冷切换：写 credentials.json → 删套餐缓存 →
 * ensure deviceMid → 写 telemetry-state.json）。force=true 时先 taskkill 关闭 ZCode（等 8s）。
 */
export function switchZcode(input: {
  id: string;
  force?: boolean;
  restart?: boolean;
  hot?: boolean;
}): Promise<{
  ok?: boolean;
  message?: string;
  error?: string;
  hot?: boolean;
  launched?: boolean;
  launchedExe?: string;
  launchError?: string;
}> {
  return call("switch_zcode", input as unknown as Record<string, unknown>);
}

/** POST /api/zcode/accounts/remove——从账号池移除账号（只删池记录，不动 live 文件）。 */
export function removeZcode(id: string): Promise<{ ok?: boolean; message?: string }> {
  return call("remove_zcode", { id });
}

/** POST /api/zcode/quota——实时额度（id 缺省 = live 当前登录态；多通道合并）。 */
export function zcodeQuota(id?: string): Promise<ZcodeQuota> {
  return call("zcode_quota", id ? { id } : {});
}

// ---- AutoClaw 国际号轮换池（契约 docs/autoclaw-intl-pool-contract.md §3.6；仅 webui 后端支持）----
// 说明：v1 为 webui-first（桌面壳无对应 Tauri command）；9 个契约端点 + 1 个接入信息
// 端点。后端由并行会话实现（autoclaw_intl_pool.rs 模块已在 core 注册）；未上线期间
// 页面把 404 归一为「后端未就绪」空态，不打扰（参照 ZcodePage 初次加载静默失败惯例）。

/** GET /api/autoclaw/intl/list——账号行（脱敏 token + 状态 + 用量 + 到期倒计时）。 */
export async function getAutoclawIntlList(): Promise<{ accounts: AutoclawIntlAccountRow[] }> {
  const res = await call<{ accounts?: AutoclawIntlAccountRow[] }>("get_autoclaw_intl_list");
  return { accounts: Array.isArray(res.accounts) ? res.accounts : [] };
}

/** GET /api/autoclaw/intl/summary——池总览（healthy/策略/游标/今日调用/水位）。 */
export function getAutoclawIntlSummary(): Promise<AutoclawIntlSummary> {
  return call("get_autoclaw_intl_summary");
}

/** POST /api/autoclaw/intl/policy——热更新策略（即时生效；返回更新后总策略）。 */
export function saveAutoclawIntlPolicy(
  patch: AutoclawIntlPolicy,
): Promise<{ ok?: boolean; policy?: AutoclawIntlPolicy; error?: string }> {
  return call("save_autoclaw_intl_policy", patch as unknown as Record<string, unknown>);
}

/** POST /api/autoclaw/intl/toggle——单号启用/停用。 */
export function toggleAutoclawIntlAccount(
  id: string,
  enabled: boolean,
): Promise<{ ok?: boolean; error?: string }> {
  return call("toggle_autoclaw_intl_account", { id, enabled });
}

/** POST /api/autoclaw/intl/remove——移除（连 PoolState 清理；本机登录中条目后端沿用现有防护）。 */
export function removeAutoclawIntlAccount(
  id: string,
): Promise<{ ok?: boolean; error?: string }> {
  return call("remove_autoclaw_intl_account", { id });
}

/** POST /api/autoclaw/intl/probe——单号立即探测（复用 watch 探针）。 */
export function probeAutoclawIntlAccount(
  id: string,
): Promise<{ ok?: boolean; state?: string; detail?: string; error?: string }> {
  return call("probe_autoclaw_intl_account", { id });
}

/** POST /api/autoclaw/intl/refresh——单号手动续期。 */
export function refreshAutoclawIntlAccount(
  id: string,
): Promise<{ ok?: boolean; tokenExpireAt?: number | null; error?: string }> {
  return call("refresh_autoclaw_intl_account", { id });
}

/** POST /api/autoclaw/intl/rotate——手动推进/重置游标（reset=true 回 0）。 */
export function rotateAutoclawIntlCursor(
  reset = false,
): Promise<{ ok?: boolean; cursorOffset?: number; error?: string }> {
  return call("rotate_autoclaw_intl_cursor", reset ? { reset: true } : {});
}

/** GET /api/autoclaw/intl/events——轮换事件流（环形 100 条，页面取最近 50）。 */
export async function getAutoclawIntlEvents(limit = 50): Promise<{ events: AutoclawIntlEvent[] }> {
  const res = await call<{ events?: AutoclawIntlEvent[] }>("get_autoclaw_intl_events", { limit });
  return { events: Array.isArray(res.events) ? res.events : [] };
}

/** GET /api/autoclaw/intl/access——接入信息（base_url + intl 模型清单）。 */
export function getAutoclawIntlAccess(): Promise<AutoclawIntlAccess> {
  return call("get_autoclaw_intl_access");
}
