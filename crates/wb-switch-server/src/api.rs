//! HTTP API 层：把 wb-switch-core 暴露为本地 REST 接口，供 webui（浏览器）调用。
//!
//! 路由设计对应 Python 版 server.py 与桌面端 commands.rs。仅绑定 127.0.0.1，
//! token 不出本机。

use std::sync::{Mutex, OnceLock};
#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, RawQuery};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde_json::{json, Value};

use wb_switch_core::modules::{
    account, account_protection, auth_file, channel_health, chatgpt_channel, checkin, config, credit_usage, credits, export_import,
    autoclaw_accounts, glm_proxy, glmplan, loomy, loomy_proxy, model_probe, model_usage, oauth, platform_checkin, platform_switch, process, refresh,
    gateway_admin, gateway_overview, glm_web, grok, grok_pool, grok_register, kimi, qoder, qwen, qwen_mcp, qwen_web, grok2api_manager, grok_sso, miaoshou, model_sync, session, switch, token_stats, toolcall_monitor, travel, update, trae_acp, trae_accounts, trae_proxy, trae_proxy_config, workbuddy_proxy, workbuddy_errors, workbuddy_gateway, workbuddy_protocols, workbuddy_upstream, opencode, dumate, dumate_accounts, unified_accounts,
};

/// 服务实际绑定端口（main.rs serve 启动时写入），供端点展示使用。
static BOUND_PORT: OnceLock<u16> = OnceLock::new();

pub fn set_bound_port(port: u16) {
    let _ = BOUND_PORT.set(port);
}

/// WorkBuddy 运行状态缓存：Windows 上检测要跑 tasklist（慢），缓存几秒避免
/// 前端切 tab 频繁触发命令行导致卡顿/闪窗。
#[cfg(target_os = "windows")]
static RUNNING_CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

fn cached_workbuddy_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        let mut cache = RUNNING_CACHE.lock().unwrap();
        if let Some((t, v)) = cache.as_ref() {
            if t.elapsed() < Duration::from_secs(3) {
                return *v;
            }
        }
        let v = process::is_workbuddy_running();
        *cache = Some((Instant::now(), v));
        v
    }
    #[cfg(not(target_os = "windows"))]
    {
        process::is_workbuddy_running()
    }
}

#[derive(RustEmbed)]
#[folder = "../../dist/"]
struct Assets;

/// 切换进度缓存：webui 通过 GET /api/switch/progress 轮询。
static SWITCH_PROGRESS: Mutex<Option<String>> = Mutex::new(None);
static SWITCH_RUNNING: Mutex<bool> = Mutex::new(false);

/// 请求体上限。
///
/// axum 默认只允许 2 MiB，而代理端点（`/v1/chat/completions`、`/v1/messages`、
/// `/workbuddy/v1/chat/completions` 等）要接收长上下文与 base64 图片，一超就被
/// 读 body 阶段直接拒掉，客户端只会看到 `413 Failed to buffer the request body:
/// length limit exceeded`（且与鉴权无关）。本机服务，放宽到 64 MiB。
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

pub fn router() -> Router {
    Router::new()
        // 二进制自证：版本 / 构建时间 / git sha。部署脚本据此判断新二进制是否真的起来了。
        .route("/api/version", get(api_version))
        .route("/api/status", get(api_status))
        .route("/api/accounts", get(api_accounts))
        .route("/api/delete", post(api_delete))
        .route("/api/oauth/start", post(api_oauth_start))
        .route("/api/oauth/status", post(api_oauth_status))
        .route("/api/import-local", post(api_import_local))
        .route("/api/export-accounts", post(api_export_accounts))
        .route(
            "/api/export-accounts-to-path",
            post(api_export_accounts_to_path),
        )
        .route("/api/import/preview", post(api_preview_import))
        .route("/api/import", post(api_import))
        .route("/api/switch", post(api_switch))
        .route("/api/switch/progress", get(api_switch_progress))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/copy", post(api_copy_sessions))
        .route("/api/checkin/status", get(api_checkin_status))
        .route("/api/credits", post(api_credits))
        .route("/api/credits/stats", get(api_credit_statistics))
        .route("/api/token-stats", get(api_token_statistics))
        .route("/api/checkin", post(api_checkin))
        .route("/api/checkin/all", post(api_checkin_all))
        .route(
            "/api/checkin/config",
            get(api_checkin_config).post(api_save_checkin_config),
        )
        .route("/api/checkin/logs", get(api_checkin_logs))
        // 「近期到期」窗口天数（设置页可配置，默认 30 天）
        .route(
            "/api/config/expiring-soon",
            get(api_expiring_soon_config).post(api_save_expiring_soon_config),
        )
        .route("/api/platform/accounts", get(api_platform_accounts))
        // 统一账号管理（契约 unified-accounts-contract.md §2）：全平台聚合 schema。
        .route("/api/accounts/unified", get(api_unified_accounts))
        .route("/v1/chat/completions", post(api_glm_chat_completions))
        .route("/v1/messages", post(api_glm_anthropic_messages))
        .route("/v1/models", get(api_glm_models))
        .route(
            "/api/glm-proxy/config",
            get(api_glm_proxy_config_get).post(api_glm_proxy_config_save),
        )
        .route("/api/glm-proxy/status", get(api_glm_proxy_status))
        // ── GLM Plan 网关（1.4.0 新通道：GLM Coding Plan 账号池）──
        .route("/api/glmplan/status", get(api_glmplan_status))
        .route("/api/glmplan/test", post(api_glmplan_test))
        .route("/api/glmplan/config", post(api_glmplan_config_save))
        .route("/api/glmplan/accounts/add", post(api_glmplan_accounts_add))
        .route("/api/glmplan/accounts/remove", post(api_glmplan_accounts_remove))
        .route("/api/glmplan/accounts/enable", post(api_glmplan_accounts_enable))
        .route("/glmplan/v1/models", get(api_glmplan_models))
        .route("/glmplan/v1/chat/completions", post(api_glmplan_chat))
        .route("/glmplan/v1/messages", post(api_glmplan_messages))
        .route("/api/glm-proxy/test", post(api_glm_proxy_test))
        // Cuckoo Code 融合模块：本地聊天页 + 自定义 Provider 下载 + 免 Key 中转
        .route("/cuckoo/chat", get(api_cuckoo_chat_page))
        .route("/cuckoo/provider.js", get(api_cuckoo_provider_js))
        .route("/cuckoo/api/chat", post(api_cuckoo_chat_relay))
        .route("/cuckoo/api/models", get(api_cuckoo_models_relay))
        .route("/api/glm-usage/summary", get(api_glm_usage_summary))
        .route("/api/glm-usage/records", get(api_glm_usage_records))
        .route("/api/glm-usage/monitor", get(api_glm_usage_monitor))
        .route("/api/glm-usage/accounts-overview", get(api_glm_usage_accounts_overview))
        .route("/api/protection/status", get(api_protection_status))
        .route(
            "/api/protection/config",
            get(api_protection_config_get).post(api_protection_config_save),
        )
        .route("/api/protection/keepalive-now", post(api_protection_keepalive_now))
        .route("/api/trae/protection/status", get(api_trae_protection_status))
        .route("/api/protection/cooldown/clear", post(api_protection_clear_cooldown))
        .route("/api/autoclaw/accounts", get(api_autoclaw_accounts_list).post(api_autoclaw_accounts_add))
        .route("/api/autoclaw/accounts/import-local", post(api_autoclaw_accounts_import_local))
        .route("/api/autoclaw/accounts/set-region", post(api_autoclaw_accounts_set_region))
        .route("/api/autoclaw/accounts/remove", post(api_autoclaw_accounts_remove))
        .route("/api/autoclaw/accounts/refresh", post(api_autoclaw_accounts_refresh))
        .route("/api/autoclaw/checkin-all", post(api_autoclaw_checkin_all))
        .route("/api/platform/switch", post(api_platform_account_switch))
        .route("/api/trae/accounts", get(api_trae_accounts_list).post(api_trae_accounts_add))
        .route("/api/trae/accounts/import-local", post(api_trae_accounts_import_local))
        .route("/api/trae/accounts/remove", post(api_trae_accounts_remove))
        .route("/api/trae/accounts/refresh", post(api_trae_accounts_refresh))
        .route("/api/trae/checkin-all", post(api_trae_checkin_all))
        .route("/api/trae/checkin-one", post(api_trae_checkin_one))
        .route("/qoder/v1/chat/completions", post(api_qoder_chat_completions))
        .route("/qoder/v1/models", get(api_qoder_models))
        .route("/api/qoder/accounts", get(api_qoder_accounts_list))
        .route("/api/qoder/accounts/import-local", post(api_qoder_import_local))
        .route("/api/qoder/accounts/remove", post(api_qoder_remove_account))
        .route("/api/qoder/accounts/refresh", post(api_qoder_refresh_account))
        .route("/api/qoder/accounts/import-manual", post(api_qoder_import_manual))
        .route("/api/qoder/accounts/import-local-cn", post(api_qoder_import_local_cn))
        // QoderWork CN（独立 Electron 应用）登录态导入 → `qwen3.8-flash` 独立池
        .route(
            "/api/qoder/accounts/import-work-cn",
            post(api_qoder_import_local_work_cn),
        )
        .route("/api/qoder/accounts/region", post(api_qoder_set_region))
        .route("/api/qoder/device-flow/start", post(api_qoder_device_flow_start))
        .route("/api/qoder/device-flow/complete", post(api_qoder_device_flow_complete))
        .route("/api/qoder/test", post(api_qoder_test))
        .route("/qwen/v1/chat/completions", post(api_qwen_chat_completions))
        .route("/qwen/v1/models", get(api_qwen_models))
        .route("/qwen-flash/v1/chat/completions", post(api_qwen_flash_chat_completions))
        .route("/qwen-flash/v1/models", get(api_qwen_flash_models))
        .route("/api/qwen/desktop/status", get(api_qwen_desktop_status))
        .route("/api/qwen/channel", get(api_qwen_channel_get).post(api_qwen_channel_set))
        .route("/api/qwen/channels", get(api_qwen_channels_status))
        .route("/api/qwen/web/log", get(api_qwen_web_log))
        .route("/api/qwen/toolcall-monitor", get(api_qwen_toolcall_monitor))
        .route("/api/qwen/toolcall-monitor/clear", post(api_qwen_toolcall_monitor_clear))
        .route("/mcp", post(api_qwen_mcp).get(api_qwen_mcp_info))
        .route("/qwen/mcp", post(api_qwen_mcp).get(api_qwen_mcp_info))
        .route("/kimi/v1/chat/completions", post(api_kimi_chat_completions))
        .route("/kimi/v1/models", get(api_kimi_models))
        .route("/api/kimi/accounts", get(api_kimi_accounts_list))
        .route("/api/kimi/accounts/add", post(api_kimi_add_account))
        .route("/api/kimi/accounts/remove", post(api_kimi_remove_account))
        .route("/api/kimi/accounts/enable", post(api_kimi_enable_account))
        .route("/api/kimi/accounts/limits", post(api_kimi_update_limits))
        .route("/api/kimi/accounts/verify", post(api_kimi_verify_account))
        .route("/api/kimi/test", post(api_kimi_test))
        .route("/grok/v1/chat/completions", post(api_grok_chat_completions))
        .route("/grok/v1/models", get(api_grok_models))
        // Grok 网关 Anthropic Messages 端点（2026-09-27）：ZCode 等 Anthropic 协议
        // 客户端的入口——桥接透传 grok2api /v1/messages（原生 Messages 协议+工具调用）。
        .route("/grok/v1/messages", post(api_grok_messages))
        .route("/api/grok/accounts", get(api_grok_accounts_list))
        .route("/api/grok/accounts/add", post(api_grok_add_account))
        .route("/api/grok/accounts/remove", post(api_grok_remove_account))
        .route("/api/grok/accounts/enable", post(api_grok_enable_account))
        .route("/api/grok/test", post(api_grok_test))
        .route("/api/grok/pool/overview", get(api_grok_pool_overview))
        .route("/api/grok/pool/check", post(api_grok_pool_check))
        .route("/api/grok/pool/policy", get(api_grok_pool_policy_get).post(api_grok_pool_policy_set))
        .route("/api/grok/pool/history", get(api_grok_pool_history))
        .route("/api/grok-sso/status", get(api_grok_sso_status))
        .route("/api/grok-sso/sync", post(api_grok_sso_sync))
        .route("/api/grok-sso/open-login", post(api_grok_sso_open_login))
        // 注册机（专用 Chrome 实例）：仅手动触发，无任何启动/后台自动逻辑
        .route("/api/grok/register/status", get(api_grok_register_status))
        .route("/api/grok/register/open", post(api_grok_register_open))
        .route("/api/grok/register/capture", post(api_grok_register_capture))
        .route("/api/grok/register/close", post(api_grok_register_close))
        // ── ChatGPT 通道（gpt-reg-review 工具链托管+控制，契约 chatgpt-channel-contract.md §4；
        //    管理面免 key 与现状一致——服务仅绑 127.0.0.1；零自动触发：批量注册仅手动启动）──
        .route("/api/chatgpt/channel/status", get(api_chatgpt_channel_status))
        .route("/api/chatgpt/webui/start", post(api_chatgpt_webui_start))
        .route("/api/chatgpt/webui/stop", post(api_chatgpt_webui_stop))
        .route(
            "/api/chatgpt/registrar/start",
            post(api_chatgpt_registrar_start),
        )
        .route(
            "/api/chatgpt/registrar/pause",
            post(api_chatgpt_registrar_pause),
        )
        .route(
            "/api/chatgpt/registrar/resume",
            post(api_chatgpt_registrar_resume),
        )
        .route(
            "/api/chatgpt/registrar/stop",
            post(api_chatgpt_registrar_stop),
        )
        .route(
            "/api/chatgpt/registrar/status",
            get(api_chatgpt_registrar_status),
        )
        .route("/api/chatgpt/registered", get(api_chatgpt_registered))
        .route("/api/chatgpt/accounts", get(api_chatgpt_accounts))
        .route("/api/chatgpt/accounts/sync", post(api_chatgpt_accounts_sync))
        .route("/api/gateway/keys", get(api_gateway_keys_list).post(api_gateway_key_create))
        .route("/api/gateway/keys/delete", post(api_gateway_key_delete))
        .route("/api/gateway/logs", get(api_gateway_logs))
        .route("/api/gateway/overview", get(api_gateway_overview))
        .route("/api/channel-health", get(api_channel_health))
        .route("/api/channel-health/detail", get(api_channel_health_detail))
        // 模型可用性自测：用户指定「网关 + 模型」在本机直接发起一次真实调用并拿到结论
        .route("/api/model-test", post(api_model_test))
        .route("/glmweb/v1/chat/completions", post(api_glmweb_chat_completions))
        .route("/glmweb/v1/models", get(api_glmweb_models))
        .route("/api/glmweb/tokens", get(api_glmweb_tokens_list).post(api_glmweb_token_add))
        .route("/api/glmweb/tokens/remove", post(api_glmweb_token_remove))
        // v0.1.46：浏览器同步工具的幂等写回端点（校验 + upsert + 健康快照）
        .route("/api/glmweb/tokens/sync", post(api_glmweb_token_sync))
        .route("/api/glmweb/test", post(api_glmweb_test))
        .route("/api/gateways/overview", get(api_gateways_overview))
        .route("/api/autoclaw/checkin-one", post(api_autoclaw_checkin_one))
        .route("/api/loomy/status", get(api_loomy_status))
        .route("/api/loomy/accounts", get(api_loomy_accounts_list).post(api_loomy_accounts_add))
        .route("/api/loomy/accounts/import-local", post(api_loomy_accounts_import_local))
        .route("/api/loomy/accounts/remove", post(api_loomy_accounts_remove))
        .route("/api/loomy/credits", get(api_loomy_credits))
        .route("/api/loomy/checkin-all", post(api_loomy_checkin_all))
        .route("/api/loomy/checkin-one", post(api_loomy_checkin_one))
        // 妙手（美团智能体）：无每日签到，checkin 接口形状与其他平台一致，
        // 统一返回 unsupported 提示（前端不特殊处理）。
        .route("/api/miaoshou/accounts", get(api_miaoshou_accounts_list))
        .route("/api/miaoshou/accounts/import-local", post(api_miaoshou_import_local))
        .route("/api/miaoshou/accounts/remove", post(api_miaoshou_remove_account))
        .route("/api/miaoshou/status", get(api_miaoshou_status))
        .route("/api/miaoshou/checkin-all", post(api_miaoshou_checkin_all))
        .route("/api/miaoshou/checkin-one", post(api_miaoshou_checkin_one))
        .route("/api/miaoshou/credits", get(api_miaoshou_credits))
        .route("/api/miaoshou/models", get(api_miaoshou_models))
        .route("/api/miaoshou/chat", post(api_miaoshou_chat))
        .route("/miaoshou/v1/models", get(api_miaoshou_v1_models))
        .route("/miaoshou/v1/chat/completions", post(api_miaoshou_v1_chat_completions))
        // ── DuMate（百度搭子）渠道（契约 dumate-channel-contract.md §3.2）：
        //    /dumate/v1 OpenAI 兼容网关（独立密钥 + 主网关密钥双轨鉴权）+
        //    本机账号签到/积分/状态/配置管理面。 ──
        .route("/dumate/v1/models", get(api_dumate_v1_models))
        .route("/dumate/v1/chat/completions", post(api_dumate_chat_completions))
        .route("/dumate/v1/messages", post(api_dumate_anthropic_messages))
        .route("/api/dumate/status", get(api_dumate_status))
        .route("/api/dumate/credits", get(api_dumate_credits))
        .route("/api/dumate/config", get(api_dumate_config_get).post(api_dumate_config_post))
        .route("/api/dumate/checkin", post(api_dumate_checkin))
        .route("/api/dumate/checkin-status", get(api_dumate_checkin_status))
        // ── DuMate 多账号池（契约 unified-accounts-contract.md §1.5）：
        //    签到/积分层多账号（模型调用仍走本机主账号）。
        .route(
            "/api/dumate/accounts",
            get(api_dumate_accounts_list).post(api_dumate_accounts_add),
        )
        .route("/api/dumate/accounts/update", post(api_dumate_accounts_update))
        .route("/api/dumate/accounts/remove", post(api_dumate_accounts_remove))
        .route(
            "/api/dumate/accounts/import-client",
            post(api_dumate_accounts_import_client),
        )
        .route("/api/dumate/account/checkin", post(api_dumate_account_checkin))
        .route("/api/loomy-proxy/config", get(api_loomy_proxy_config_get).post(api_loomy_proxy_config_save))
        .route("/api/loomy-proxy/status", get(api_loomy_proxy_status))
        .route("/api/loomy/protection/status", get(api_loomy_protection_status))
        .route("/api/loomy/protection/keepalive-now", post(api_loomy_protection_keepalive_now))
        .route("/api/loomy/protection/cooldown/clear", post(api_loomy_protection_cooldown_clear))
        .route("/api/loomy/models", get(api_loomy_models_catalog))
        .route("/loomy/v1/chat/completions", post(api_loomy_chat_completions))
        .route("/loomy/v1/messages", post(api_loomy_anthropic_messages))
        .route("/loomy/v1/models", get(api_loomy_v1_models))
        .route("/api/trae-proxy/status", get(api_trae_proxy_status))
        .route("/api/trae-proxy/models", get(api_trae_proxy_models))
        .route("/trae/v1/models", get(api_trae_v1_models))
        // TRAE/WorkBuddy-native 对话补全（T6 落地：ACP 真转发；未开启门禁时 503 JSON）。
        // 显式注册后：POST → 503/200 JSON；GET（方法不匹配）→ axum 0.7 MethodRouter 405，
        // 而不是落 SPA 兜底返回 200 HTML 假成功。
        .route("/trae/v1/chat/completions", post(api_trae_v1_chat_completions))
        // WorkBuddy 桌面端自带模型通道（同一 ACP 实现，路由名明确指向原生模型，
        // 与 /workbuddy/v1/* 的 OpenCode 借道模型区分）。
        .route("/workbuddy-native/v1/models", get(api_workbuddy_native_models))
        .route(
            "/workbuddy-native/v1/chat/completions",
            post(api_workbuddy_native_chat_completions),
        )
        // 契约 §2.4：目录强制刷新（绕过 300s TTL 重探 ACP + 合并产品配置补充源）。
        // 管理端点免 key（本机回环惯例，同 /api/workbuddy-proxy/config）。
        .route(
            "/api/workbuddy-native/catalog/refresh",
            post(api_workbuddy_native_catalog_refresh),
        )
        .route("/api/workbuddy-proxy/status", get(api_workbuddy_proxy_status))
        .route("/api/workbuddy-proxy/models", get(api_workbuddy_proxy_models))
        .route(
            "/api/workbuddy-proxy/config",
            get(api_workbuddy_proxy_config_get).post(api_workbuddy_proxy_config_save),
        )
        .route("/api/trae-proxy/config", get(api_trae_proxy_config_get))
        .route("/workbuddy/v1/chat/completions", post(api_workbuddy_chat_completions))
        .route("/workbuddy/v1/models", get(api_workbuddy_v1_models))
        // WorkBuddy 多账号池网关（cloud 上游 + 账号池）：独立路由组，与存量
        // /workbuddy/v1（OpenCode 借道）、/workbuddy-native/v1（ACP）互不影响
        // （契约 docs/workbuddy-gateway-refactor-contract.md §12 切-cutover 策略）。
        //
        // 🔑 鉴权策略（组内统一口径，避免「有的要 key 有的不要」的漂移）：
        //   - chat/completions：**要网关 key**（契约 §13 路由表唯一标注鉴权的端点；
        //     它是真正对外的 OpenAI 兼容入口，会把账号池额度烧出去）。
        //   - models / status / config：**免 key** 的管理三端点。契约 §13 对其余三行
        //     均未标注鉴权；且 webui 面板的 `httpCall`（src/lib/api.ts:287-311）只发
        //     Content-Type、**不带 Authorization 头**，status/config 是面板唯一数据源
        //     （WorkbuddyPage.tsx:163 loadPool / :183 patchWorkbuddyPoolConfig），
        //     要 key 会直接把面板打成全空。
        //   - 与既有惯例对齐：/api/* 管理端点（如 /api/grok/pool/policy 本文件 1730 行、
        //     /api/workbuddy-proxy/config 本文件 4252 行）一律免 key；服务仅绑
        //     127.0.0.1（main.rs:153），无 CORS 层且写端点用 `Json` 提取器
        //     （跨源 JSON POST = 非简单请求 → preflight 必被拦，浏览器页面摸不到）。
        .route(
            "/workbuddy-pool/v1/chat/completions",
            post(api_workbuddy_pool_chat_completions),
        )
        .route("/workbuddy-pool/v1/models", get(api_workbuddy_pool_models))
        // 区域专用接入密钥（国内版/国际版各一把）：GET 取/复用，?create=1 按需新建。
        .route(
            "/api/workbuddy-pool/region-key",
            get(api_workbuddy_pool_region_key),
        )
        .route("/workbuddy-pool/v1/status", get(api_workbuddy_pool_status))
        .route(
            "/workbuddy-pool/v1/config",
            post(api_workbuddy_pool_config_save),
        )
        // P2 协议补全（契约增补 §3）：同一池、同一份选号循环，多两个协议入口。
        // 鉴权口径同 chat/completions（对外烧池内额度的入口都要网关 key），
        // 走 `workbuddy_pool_auth_ok`，失败 401。
        .route(
            "/workbuddy-pool/v1/responses",
            post(api_workbuddy_pool_responses),
        )
        .route(
            "/workbuddy-pool/v1/messages",
            post(api_workbuddy_pool_messages),
        )
        // OpenCode 网关：本机 OpenCode server 的免费模型专用入口（独立密钥）
        .route("/opencode/v1/models", get(api_opencode_v1_models))
        .route(
            "/opencode/v1/chat/completions",
            post(api_opencode_chat_completions),
        )
        .route("/api/opencode-proxy/status", get(api_opencode_proxy_status))
        .route(
            "/api/opencode-proxy/config",
            get(api_opencode_proxy_config_get).post(api_opencode_proxy_config_save),
        )
        .route("/v1/status", get(api_glm_v1_status))
        .route("/api/travel/status", get(api_travel_status))
        .route(
            "/api/travel/config",
            get(api_travel_config).post(api_save_travel_config),
        )
        .route("/api/refresh-token", post(api_refresh_token))
        // 模型目录自动同步（契约 model-sync-contract.md §四）：管理端点一律免 key
        // （本机回环惯例，同 /api/workbuddy-proxy/config）。
        .route("/api/model-sync/status", get(api_model_sync_status))
        .route("/api/model-sync/run", post(api_model_sync_run))
        .route("/api/model-sync/log", get(api_model_sync_log))
        .route(
            "/api/model-sync/config",
            get(api_model_sync_config_get).post(api_model_sync_config_save),
        )
        .route(
            "/api/model-sync/registry",
            post(api_model_sync_registry_add),
        )
        // G6 手动补录的删除配套（补录错了要能撤回恢复原样）。
        .route(
            "/api/model-sync/registry/remove",
            post(api_model_sync_registry_remove),
        )
        .route("/api/update/check", get(api_update_check))
        .route(
            "/api/update/config",
            get(api_update_config).post(api_save_update_config),
        )
        .fallback(static_handler)
        // 代理端点常带长上下文 / base64 图片，必须放宽请求体上限（默认 2 MiB）。
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // 观测性兜底（2026-09-18 ZCode -32603 事故：请求没到 handler 就失败，无任何日志可查）：
        // ① access_log：记录所有非静态资源请求的 method/path/status/耗时 —— body 被前置层
        //    拒绝（413/422）、未知路径落 SPA 兜底等情况从此可见；
        // ② panic_guard：handler panic 不再静默断连（tokio 吞掉 panic = 客户端只看到
        //    connection closed），改为返回 500 JSON。panic_guard 必须在最外层。
        .layer(axum::middleware::from_fn(access_log_middleware))
        .layer(axum::middleware::from_fn(panic_guard_middleware))
}

/// 需要记录访问日志的路径前缀（模型网关与管理 API）。静态资源（dist/assets 等）不记。
///
/// `/workbuddy-pool/` 必须单独列：它不以 `/workbuddy/` 开头（连字符不是斜杠），
/// 漏掉会让池网关「请求没到 handler 就失败」的请求（413/422/鉴权前拒绝）无日志可查。
fn is_logged_path(path: &str) -> bool {
    let prefixes = [
        "/v1/", "/qwen/", "/qwen-flash/", "/qoder/", "/kimi/", "/glmweb/",
        "/grok/", "/loomy/", "/cuckoo/", "/trae/", "/api/", "/workbuddy/",
        "/workbuddy-pool/", "/dumate/",
    ];
    prefixes.iter().any(|p| path.starts_with(p))
}

/// 前置访问日志：所有网关/管理 API 请求的 method、path、状态码、耗时。
/// 处理器内部已有业务日志的请求会出现两条（一条 http 一条业务），属预期 ——
/// 这条的价值在于「请求根本没走到 handler」的场景（413/422/未知路径/连接层失败前的最后状态）。
async fn access_log_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let logged = is_logged_path(&path);
    let start = Instant::now();
    let resp = next.run(req).await;
    if logged {
        let status = resp.status();
        let latency = start.elapsed().as_millis() as u64;
        // 只记失败请求（413 body 超限 / 422 body 解析失败 / 5xx panic 兜底 / 4xx 拒绝）。
        // 成功的网关请求已由各 handler 记业务日志，不重复记。
        if !status.is_success() {
            gateway_admin::record_request(json!({
                "ts": wb_switch_core::modules::config::now_ms(),
                "gateway": "http",
                "keyId": Value::Null,
                "model": format!("{method} {path}"),
                "ok": false,
                "latencyMs": latency,
                "error": json!(format!("HTTP {status}（未进入业务 handler：body 超限/解析失败/鉴权前拒绝等）")),
            }));
        }
    }
    resp
}

/// handler panic 兜底：把 panic 转成 500 JSON（并记入网关日志），
/// 而不是让 tokio 静默吞掉、客户端只看到连接被断开。
async fn panic_guard_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let fut = next.run(req);
    match futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await {
        Ok(resp) => resp,
        Err(_panic) => {
            let msg = format!(
                "网关 handler panic（{method} {path}）：已捕获，请重试；若持续出现请反馈 Trace 上下文"
            );
            gateway_admin::record_request(json!({
                "ts": wb_switch_core::modules::config::now_ms(),
                "gateway": "http",
                "keyId": Value::Null,
                "model": format!("{method} {path}"),
                "ok": false,
                "latencyMs": 0,
                "error": json!(msg),
            }));
            glm_proxy_error(msg, StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

fn json_ok(v: Value) -> Response {
    Json(v).into_response()
}

fn json_err(e: String, code: StatusCode) -> Response {
    (code, Json(json!({ "ok": false, "error": e }))).into_response()
}

// ---------------------------------------------------------------------------
// 状态 / 账号
// ---------------------------------------------------------------------------

/// `GET /api/version` —— 让运行中的二进制自报身份。
///
/// 存在意义有两层：
/// 1. **部署可信**：`deploy.py` 需要「新二进制真的起来了」的判据。此前它探的
///    这个路径并不存在，被 SPA 兜底当成前端路由返回 index.html（200），
///    于是健康检查恒为真 —— 换 exe 后跑的是新是旧无从分辨（2026-09-16 定位）。
/// 2. **排障省事**：线上跑的到底是哪一版、哪次构建，一条 curl 就有答案。
async fn api_version() -> Response {
    json_ok(json!({
        "ok": true,
        "name": "wb-switch",
        "version": env!("CARGO_PKG_VERSION"),
        "buildTime": env!("WB_BUILD_TIME"),
        "buildEpoch": env!("WB_BUILD_EPOCH").parse::<i64>().unwrap_or(0),
        "gitSha": env!("WB_GIT_SHA"),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "pid": std::process::id(),
        "boundPort": BOUND_PORT.get().copied(),
    }))
}

async fn api_status() -> Response {
    let auth = auth_file::read_auth_file();
    let current = auth.as_ref().and_then(|a| {
        let acct = a.get("account").cloned().unwrap_or_else(|| json!({}));
        // WorkBuddy 5.5.6 起，auth 文件里的 nickname/email 可能是 `$wbEncrypted`
        // 加密信封对象（{ $wbEncrypted, envelope }，本地无密钥解不开）。展示
        // 字段必须是字符串：信封对象按 uid 回退账号库取明文昵称，取不到置
        // null——原样透传会让前端把对象当文本渲染（React error #31 白屏）。
        let uid = acct.get("uid").and_then(Value::as_str).unwrap_or("");
        let as_str = |v: Option<&Value>| v.and_then(Value::as_str).map(|s| s.to_string());
        let mut nickname = as_str(acct.get("nickname"));
        let mut email = as_str(acct.get("email"));
        if (nickname.is_none() || email.is_none()) && !uid.is_empty() {
            if let Some(acc) = account::load_accounts()
                .iter()
                .find(|a| a.get("uid").and_then(Value::as_str) == Some(uid))
            {
                if nickname.is_none() {
                    nickname = as_str(acc.get("nickname"));
                }
                if email.is_none() {
                    email = as_str(acc.get("email"));
                }
            }
        }
        Some(json!({
            "uid": acct.get("uid"),
            "nickname": nickname,
            "email": email,
        }))
    });
    json_ok(json!({
        "running": cached_workbuddy_running(),
        "authFile": auth_file::auth_file_path().to_string_lossy(),
        "current": current,
        "appPath": auth_file::workbuddy_app_path().to_string_lossy(),
        "version": update::APP_VERSION,
        // WorkBuddy 桌面客户端版本（注册表卸载信息）；读不到为 null。
        // 注意：与 `version`（Sylvia Switch 自身版本）语义不同，界面需分别标注。
        "workbuddyVersion": process::workbuddy_client_version(),
    }))
}

async fn api_accounts() -> Response {
    json_ok(json!({
        "accounts": account::load_accounts()
            .iter()
            .map(account::account_meta)
            .collect::<Vec<_>>(),
        "current": auth_file::read_auth_file()
            .and_then(|a| a.get("account").and_then(|x| x.get("uid")).and_then(|x| x.as_str()).map(String::from)),
    }))
}

async fn api_delete(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    match account::delete_account(id) {
        Ok(()) => json_ok(json!({ "ok": true })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_import_local() -> Response {
    // import_local 在 auth 文件加密时回退到进程内存扫描（同步、重 CPU），
    // 放到 spawn_blocking 避免卡住 tokio worker。
    match tokio::task::spawn_blocking(account::import_local).await {
        Ok(Ok(acc)) => json_ok(json!({ "ok": true, "account": acc })),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(format!("导入任务失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// ---------------------------------------------------------------------------
// 导出 / 导入账号
// ---------------------------------------------------------------------------

async fn api_export_accounts(Json(body): Json<Value>) -> Response {
    let ids: Vec<String> = body
        .get("accountIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    match export_import::export_accounts(&ids) {
        Ok(records) => json_ok(json!({ "ok": true, "accounts": records })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_export_accounts_to_path(Json(body): Json<Value>) -> Response {
    let ids: Vec<String> = body
        .get("accountIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match export_import::export_accounts_to_path(&ids, &path) {
        Ok(path) => json_ok(json!({ "ok": true, "path": path })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_preview_import(Json(body): Json<Value>) -> Response {
    let text = body
        .get("fileText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match export_import::preview_accounts(&text) {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_import(Json(body): Json<Value>) -> Response {
    let text = body
        .get("fileText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let indexes: Vec<usize> = body
        .get("indexes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|n| n as usize))
                .collect()
        })
        .unwrap_or_default();
    match export_import::import_accounts(&text, &indexes) {
        Ok(result) => json_ok(json!({
            "ok": true,
            "imported": result.imported,
            "skipped": result.skipped,
            "overwritten": result.overwritten,
        })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// OAuth 登录
// ---------------------------------------------------------------------------

async fn api_oauth_start() -> Response {
    match oauth::oauth_start().await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_oauth_status(Json(body): Json<Value>) -> Response {
    let login_id = body
        .get("loginId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    json_ok(oauth::oauth_poll(&login_id).await)
}

// ---------------------------------------------------------------------------
// 切换
// ---------------------------------------------------------------------------

async fn api_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if account_id.trim().is_empty() {
        return json_err("缺少 accountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let share_sessions = body
        .get("shareSessions")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let copy_ids: Vec<String> = body
        .get("copySessionIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    {
        let mut running = SWITCH_RUNNING.lock().unwrap();
        if *running {
            return json_err("已有切换任务进行中".to_string(), StatusCode::CONFLICT);
        }
        *running = true;
        *SWITCH_PROGRESS.lock().unwrap() = Some("开始切换账号…".to_string());
    }

    let progress: switch::ProgressFn = Box::new(|msg| {
        *SWITCH_PROGRESS.lock().unwrap() = Some(msg.to_string());
    });

    let result = tokio::task::spawn_blocking(move || {
        switch::switch_account(
            Some(&progress),
            &account_id,
            restart,
            share_sessions,
            &copy_ids,
        )
    })
    .await;

    *SWITCH_RUNNING.lock().unwrap() = false;

    match result {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_switch_progress() -> Response {
    let p = SWITCH_PROGRESS.lock().unwrap().clone();
    let running = *SWITCH_RUNNING.lock().unwrap();
    json_ok(json!({ "running": running, "progress": p }))
}

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

async fn api_sessions() -> Response {
    match session::current_user_uid() {
        Some(uid) => json_ok(json!({
            "sessions": session::list_sessions_for_user(&uid),
            "current": uid,
        })),
        None => json_ok(json!({ "sessions": [], "current": null })),
    }
}

async fn api_copy_sessions(Json(body): Json<Value>) -> Response {
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let session_ids: Vec<String> = body
        .get("sessionIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let Some(target) = account::find_account(&target_account_id) else {
        return json_err("目标账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let source_uid = session::current_user_uid();
    let result = session::copy_sessions_for_switch(&target, &session_ids);
    json_ok(json!({
        "sourceUid": source_uid,
        "targetUid": target.get("uid"),
        "copied": result,
    }))
}

// ---------------------------------------------------------------------------
// 签到 / 保活
// ---------------------------------------------------------------------------

async fn api_checkin_status() -> Response {
    let list = platform_checkin::all_checkin_accounts();
    let mut items = Vec::new();
    for acc in &list {
        let status = checkin::get_checkin_status_for(acc).await;
        items.push(checkin_status_item(acc, status));
    }
    json_ok(json!({ "accounts": items }))
}

fn checkin_status_item(account: &Value, mut status: Value) -> Value {
    status["accountId"] = account.get("id").cloned().unwrap_or(Value::Null);
    status["email"] = json!(account::account_display_name(account));
    status["platform"] = json!(platform_checkin::account_platform(account));
    // 本机客户端当前登录的账号（一键切换时用于标注「当前」与幂等短路）。
    status["localLogin"] = json!(platform_checkin::is_local_login_account(account));
    // 区域标签（AutoClaw 平台账号：🌐 国际 / 🇨🇳 国内；其他平台无此字段）。
    if let Some(region) = account.get("region").and_then(Value::as_str) {
        status["region"] = json!(region);
    }
    status
}

async fn api_credits(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(credits::get_credit_expiry(&acc).await)
}

fn query_flag_enabled(query: Option<&str>, name: &str) -> bool {
    query.unwrap_or("").split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, "true"));
        key == name && matches!(value, "" | "1" | "true" | "yes")
    })
}

async fn api_credit_statistics(RawQuery(query): RawQuery) -> Response {
    json_ok(credit_usage::get_statistics(query_flag_enabled(query.as_deref(), "refresh")).await)
}

async fn api_token_statistics(RawQuery(query): RawQuery) -> Response {
    let days = query.as_deref().and_then(|value| {
        value.split('&').find_map(|part| {
            part.strip_prefix("days=")?.parse::<i64>().ok()
        })
    });
    match tokio::task::spawn_blocking(move || token_stats::get_statistics(days)).await {
        Ok(statistics) => json_ok(statistics),
        Err(error) => json_err(
            format!("扫描 Token 统计失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn api_checkin(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = platform_checkin::find_checkin_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(checkin::checkin_account(&acc).await)
}

async fn api_checkin_all() -> Response {
    json_ok(checkin::run_checkin_all().await)
}

async fn api_checkin_config() -> Response {
    json_ok(config::load_checkin_config())
}

async fn api_save_checkin_config(Json(body): Json<Value>) -> Response {
    let submitted = body.get("config").unwrap_or(&body);
    match config::save_checkin_config(submitted) {
        Ok(()) => json_ok(config::load_checkin_config()),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_checkin_logs() -> Response {
    json_ok(json!({ "logs": config::load_checkin_logs() }))
}

/// GET /api/config/expiring-soon：读取「近期到期」窗口天数。
async fn api_expiring_soon_config() -> Response {
    json_ok(config::load_expiring_soon_config())
}

/// POST /api/config/expiring-soon：保存「近期到期」窗口天数（1–365，越界自动夹取）。
async fn api_save_expiring_soon_config(Json(body): Json<Value>) -> Response {
    let submitted = body.get("config").unwrap_or(&body);
    match config::save_expiring_soon_config(submitted) {
        Ok(()) => json_ok(config::load_expiring_soon_config()),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

/// 平台账号（AutoClaw / TRAE Work CN）签到状态 + 积分/额度，供账号卡片展示。
// ---------------------------------------------------------------------------
// GLM 代理（AutoClaw → OpenAI 兼容接口）
// ---------------------------------------------------------------------------

fn glm_proxy_auth_ok(headers: &HeaderMap, cfg: &glm_proxy::GlmProxyConfig) -> bool {
    if cfg.api_key.is_empty() {
        return true;
    }
    let bearer_ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|k| k == cfg.api_key)
        .unwrap_or(false);
    let x_api_key_ok = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|k| k == cfg.api_key)
        .unwrap_or(false);
    bearer_ok || x_api_key_ok
}

fn glm_proxy_error(e: String, code: StatusCode) -> Response {
    // OpenAI 风格错误体，Hermes / OpenAI SDK 可直接解析。
    (
        code,
        Json(json!({"error": {"message": e, "type": "glm_proxy_error"}})),
    )
        .into_response()
}

async fn api_glm_chat_completions(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let cfg = glm_proxy::load_config();
    if !cfg.enabled {
        return glm_proxy_error(
            "GLM 代理未启用，请在 Sylvia Switch 设置中开启".to_string(),
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
    if !glm_proxy_auth_ok(&headers, &cfg) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match glm_proxy::chat_completions(body).await {
        Ok(glm_proxy::GlmChatCompletion::Json(value)) => json_ok(value),
        Ok(glm_proxy::GlmChatCompletion::Stream(upstream)) => {
            let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, upstream.content_type)],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(e) => glm_proxy_error(e, StatusCode::BAD_GATEWAY),
    }
}

/// Anthropic Messages API 兼容端点（`POST /v1/messages`）。
/// 鉴权同 OpenAI 端点：`x-api-key` 或 `Authorization: Bearer`。
async fn api_glm_anthropic_messages(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let cfg = glm_proxy::load_config();
    if !cfg.enabled {
        return anthropic_error(
            "GLM 代理未启用，请在 Sylvia Switch 设置中开启".to_string(),
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
    if !glm_proxy_auth_ok(&headers, &cfg) {
        return anthropic_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match glm_proxy::anthropic_messages(body).await {
        Ok(glm_proxy::GlmChatCompletion::Json(value)) => json_ok(value),
        Ok(glm_proxy::GlmChatCompletion::Stream(upstream)) => {
            let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, upstream.content_type)],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(e) => {
            let status = anthropic_error_status(&e);
            anthropic_error(e, status)
        }
    }
}

/// Anthropic 风格错误体：`{"type":"error","error":{type,message}}`。
fn anthropic_error(e: String, code: StatusCode) -> Response {
    let error_type = match code {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::BAD_REQUEST => "invalid_request_error",
        StatusCode::SERVICE_UNAVAILABLE => "overloaded_error",
        _ => "api_error",
    };
    (
        code,
        Json(json!({
            "type": "error",
            "error": {"type": error_type, "message": e},
        })),
    )
        .into_response()
}

fn anthropic_error_status(message: &str) -> StatusCode {
    if message.contains("未知模型") || message.contains("messages 字段") || message.contains("role") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    }
}

async fn api_glm_models(headers: HeaderMap, RawQuery(raw_query): RawQuery) -> Response {
    let cfg = glm_proxy::load_config();
    if !cfg.enabled {
        return glm_proxy_error("GLM 代理未启用".to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    if !glm_proxy_auth_ok(&headers, &cfg) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    // 1.3.0：?region= 按区过滤（domestic/international，兼容 cn/intl 别名，all/缺省=全量）
    let region = raw_query.as_deref().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("region=").map(|v| v.to_string()))
    });
    json_ok(glm_proxy::models_payload_filtered(region.as_deref()))
}

// ---------------------------------------------------------------------------
// Cuckoo Code 融合模块（https://github.com/wangyongpeng90/cuckoo-code）
//
// cuckoo-code 是「零 Token 成本 AI Agent 桌面端」：内嵌浏览器打开 AI 网页，
// 引导 AI 输出工具调用 JSON，确认后本地执行并把结果回传，形成 Agent 循环。
//
// 本模块提供三样东西，让 cuckoo-code 直接以本网关桥接的网页账号为大脑：
//   1. GET  /cuckoo/chat        本地聊天页（OpenAI 兼容 SSE 流式）
//   2. GET  /cuckoo/provider.js cuckoo-code 自定义 Provider 文件（DOM 选择器
//      与本页稳定对应；useIntercept 模式，回复由页面派发事件回收）
//   3. POST /cuckoo/api/chat    免 Key 中转：页面无需持有网关 Key，
//      服务端内部读取 glm_proxy_config 的 api_key 后走同一处理管线。
//      仅回环监听（127.0.0.1:57890），不对外暴露。
// ---------------------------------------------------------------------------

const CUCKOO_CHAT_HTML: &str = include_str!("../assets/cuckoo/chat.html");
const CUCKOO_PROVIDER_JS: &str = include_str!("../assets/cuckoo/provider.js");

async fn api_cuckoo_chat_page() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        CUCKOO_CHAT_HTML,
    )
        .into_response()
}

async fn api_cuckoo_provider_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"sylvia-switch-provider.js\"",
            ),
        ],
        CUCKOO_PROVIDER_JS,
    )
        .into_response()
}

/// 免 Key 的 chat/completions 中转：鉴权留在服务端（回环 + 本地页面），
/// 请求体与响应流与 `/v1/chat/completions` 完全一致。
async fn api_cuckoo_chat_relay(Json(body): Json<Value>) -> Response {
    let cfg = glm_proxy::load_config();
    if !cfg.enabled {
        return glm_proxy_error(
            "GLM 代理未启用，请在 Sylvia Switch 设置中开启".to_string(),
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
    match glm_proxy::chat_completions(body).await {
        Ok(glm_proxy::GlmChatCompletion::Json(value)) => json_ok(value),
        Ok(glm_proxy::GlmChatCompletion::Stream(upstream)) => {
            let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, upstream.content_type)],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(e) => glm_proxy_error(e, StatusCode::BAD_GATEWAY),
    }
}

/// 免 Key 的模型列表中转（供聊天页下拉框）。
async fn api_cuckoo_models_relay() -> Response {
    let cfg = glm_proxy::load_config();
    if !cfg.enabled {
        return glm_proxy_error("GLM 代理未启用".to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    json_ok(glm_proxy::models_payload())
}

/// 从界面直接试调一次对话补全（服务端转发，浏览器无需直连 /v1）。
async fn api_glm_proxy_test(Json(body): Json<Value>) -> Response {
    let cfg = glm_proxy::load_config();
    if !cfg.enabled {
        return json_err("GLM 代理未启用".to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or("glm-5.3").to_string();
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("只回复：OK")
        .trim()
        .to_string();
    if prompt.is_empty() {
        return json_err("请输入测试内容".to_string(), StatusCode::BAD_REQUEST);
    }
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 1500,
        "stream": false,
    });
    let started = std::time::Instant::now();
    match glm_proxy::chat_completions_json(payload).await {
        Ok(value) => {
            let latency = started.elapsed().as_millis() as u64;
            let content = value
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            json_ok(json!({
                "ok": true,
                "model": value.get("model").cloned().unwrap_or(Value::Null),
                "content": content,
                "latencyMs": latency,
                "usage": value.get("usage").cloned().unwrap_or(Value::Null),
            }))
        }
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

// ---------------------------------------------------------------------------
// 模型用量统计
// ---------------------------------------------------------------------------

/// 模型用量汇总：`?channel=<workbuddy|trae|glm|loomy|autoclaw>` 可选过滤；
/// 不带 channel 时与历史行为完全一致（等价 core 的 `get_statistics`）。
async fn api_glm_usage_summary(RawQuery(query): RawQuery) -> Response {
    let channel = query
        .as_deref()
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("channel="))
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty());
    json_ok(model_usage::get_statistics_filtered(channel.as_deref()).await)
}

async fn api_glm_usage_records(RawQuery(query): RawQuery) -> Response {
    let limit = query
        .as_deref()
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("limit=").and_then(|v| v.parse::<usize>().ok()))
        .unwrap_or(100)
        .min(500);
    json_ok(json!({ "records": model_usage::recent_records(limit) }))
}

/// 模型监控：`?hours=<小时数>&bucketMinutes=<分钟>&provider=<平台>`；hours=0 表示全部历史。
async fn api_glm_usage_monitor(RawQuery(query): RawQuery) -> Response {
    fn query_number(query: Option<&str>, name: &str) -> Option<i64> {
        let prefix = format!("{name}=");
        query?.split('&').find_map(|pair| {
            pair.strip_prefix(prefix.as_str())
                .and_then(|value| value.trim().parse::<i64>().ok())
        })
    }
    fn query_string(query: Option<&str>, name: &str) -> Option<String> {
        let prefix = format!("{name}=");
        query?.split('&').find_map(|pair| {
            pair.strip_prefix(prefix.as_str())
                .map(|value| value.trim().to_string())
                .filter(|s| !s.is_empty())
        })
    }
    let raw = query.as_deref();
    let hours = query_number(raw, "hours").unwrap_or(24).clamp(0, 24 * 365);
    let bucket_ms = query_number(raw, "bucketMinutes")
        .filter(|m| *m > 0)
        .map(|m| m * 60_000)
        .unwrap_or_else(|| model_usage::default_bucket_ms(hours));
    let provider = query_string(raw, "provider");
    json_ok(model_usage::get_monitor_filtered(hours, bucket_ms, provider.as_deref()))
}

/// 多账号（AutoClaw, Loomy, TRAE Work CN）模型用量概览指标
async fn api_glm_usage_accounts_overview() -> Response {
    json_ok(model_usage::get_accounts_overview().await)
}

// ---------------------------------------------------------------------------
// 账号保护与保活
// ---------------------------------------------------------------------------

async fn api_protection_status() -> Response {
    json_ok(account_protection::get_protection_status())
}

async fn api_protection_config_get() -> Response {
    json_ok(json!(account_protection::load_protection_config()))
}

async fn api_protection_config_save(Json(body): Json<account_protection::AccountProtectionConfig>) -> Response {
    match account_protection::save_protection_config(&body) {
        Ok(()) => json_ok(json!({ "ok": true, "config": account_protection::load_protection_config() })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_protection_keepalive_now() -> Response {
    let report = account_protection::run_keepalive_cycle().await;
    let trae_report = account_protection::run_trae_keepalive_cycle().await;
    json_ok(json!({ "ok": true, "report": report, "traeReport": trae_report }))
}

/// TRAE Work CN 账号保护状态（冷却熔断 / 并发 / 巡检计数，前缀 trae_work_cn: 隔离）。
async fn api_trae_protection_status() -> Response {
    json_ok(account_protection::get_trae_protection_status())
}

async fn api_protection_clear_cooldown(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("accountId").and_then(Value::as_str) else {
        return json_err("缺少 accountId".to_string(), StatusCode::BAD_REQUEST);
    };
    let cleared = account_protection::clear_account_cooling(id);
    json_ok(json!({ "ok": true, "cleared": cleared }))
}

// ---------------------------------------------------------------------------
// AutoClaw 多账号池
// ---------------------------------------------------------------------------

async fn api_autoclaw_accounts_list() -> Response {
    json_ok(json!({ "accounts": autoclaw_accounts::list_for_display() }))
}

async fn api_autoclaw_accounts_add(Json(body): Json<Value>) -> Response {
    let label = body.get("label").and_then(Value::as_str).map(String::from);
    let Some(access_token) = body.get("accessToken").and_then(Value::as_str) else {
        return json_err("缺少 accessToken".to_string(), StatusCode::BAD_REQUEST);
    };
    let Some(refresh_token) = body.get("refreshToken").and_then(Value::as_str) else {
        return json_err("缺少 refreshToken".to_string(), StatusCode::BAD_REQUEST);
    };
    let device_id = body
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // 区域分入口：前端「导入国际版账号 / 导入国内版账号」显式指定；
    // 未指定时按 token 身份自动归类（邮箱=国际、手机号=国内）。
    let region = body
        .get("region")
        .and_then(Value::as_str)
        .and_then(autoclaw_accounts::AccountRegion::from_str);
    match autoclaw_accounts::add_account_v2(
        label,
        access_token,
        refresh_token,
        Some(device_id),
        region,
        autoclaw_accounts::AccountSource::ManualImport,
    )
    .await
    {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// 手动指定账号归属区域（防误判修正入口）。
async fn api_autoclaw_accounts_set_region(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    let Some(region_str) = body.get("region").and_then(Value::as_str) else {
        return json_err("缺少 region（international / domestic）".to_string(), StatusCode::BAD_REQUEST);
    };
    let Some(region) = autoclaw_accounts::AccountRegion::from_str(region_str) else {
        return json_err(
            format!("无效区域：{region_str}（可选 international / domestic）"),
            StatusCode::BAD_REQUEST,
        );
    };
    match autoclaw_accounts::set_account_region(id, region) {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::NOT_FOUND),
    }
}

async fn api_autoclaw_accounts_import_local() -> Response {
    match autoclaw_accounts::import_local().await {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_autoclaw_accounts_remove(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    if autoclaw_accounts::remove_account(id) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_autoclaw_accounts_refresh(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    match autoclaw_accounts::refresh_stored_account(id).await {
        Ok(_) => json_ok(json!({ "ok": true })),
        Err(e) => json_err(format!("刷新失败：{e}"), StatusCode::BAD_GATEWAY),
    }
}

async fn api_autoclaw_checkin_all() -> Response {
    json_ok(json!({ "accounts": platform_checkin::autoclaw_checkin_pool().await }))
}

/// 平台账号一键切换：关客户端 → 写登录态 → 重启客户端。
/// `platform` 取 `autoclaw` / `trae_work_cn`，`id` 为账号池条目 id。
async fn api_platform_account_switch(Json(body): Json<Value>) -> Response {
    let Some(platform) = body.get("platform").and_then(Value::as_str) else {
        return json_err("缺少 platform".to_string(), StatusCode::BAD_REQUEST);
    };
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    match platform_switch::switch_account(platform, id, body.get("restart").and_then(Value::as_bool).unwrap_or(true)).await {
        Ok(result) => json_ok(result),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_autoclaw_checkin_one(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(platform_checkin::autoclaw_checkin_pool_account_by_id(id).await)
}

// ---------------------------------------------------------------------------
// TRAE Work CN 多账号池
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Kimi 融合网关（移植 kimi2api：账号池调度 + Kimi Web 协议）
// ---------------------------------------------------------------------------

#[allow(dead_code)]
async fn api_kimi_chat_completions(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("kimi", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or("kimi-k2d6").to_string();
    match kimi::chat_completions(body).await {
        Ok(kimi::TabbitLikeOutput::Json(value)) => json_ok(value),
        Ok(kimi::TabbitLikeOutput::Stream(upstream)) => {
            log_gateway_request("kimi", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
            let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(e) => {
            log_gateway_request("kimi", key_id.as_deref(), &model, false, started.elapsed().as_millis() as u64, &e);
            let status = if e.contains("繁忙") || e.contains("冷却") {
                StatusCode::SERVICE_UNAVAILABLE
            } else if e.contains("未知模型") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            glm_proxy_error(e, status)
        }
    }
}

async fn api_kimi_models(headers: HeaderMap) -> Response {
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("kimi", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match kimi::models_payload().await {
        Ok(payload) => {
            log_gateway_request("kimi", key_id.as_deref(), "models", true, 0, "");
            json_ok(payload)
        }
        Err(e) => {
            log_gateway_request("kimi", key_id.as_deref(), "models", false, 0, &e);
            glm_proxy_error(e, StatusCode::BAD_GATEWAY)
        }
    }
}

/// Kimi 试调（服务端转发并聚合，浏览器无需直连 /kimi/v1）。
async fn api_kimi_test(Json(body): Json<Value>) -> Response {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("kimi-k2d6").to_string();
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("只回复：OK")
        .trim()
        .to_string();
    if prompt.is_empty() {
        return json_err("请输入测试内容".to_string(), StatusCode::BAD_REQUEST);
    }
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": true,
    });
    let started = std::time::Instant::now();
    let mut resp = match kimi::chat_completions(payload).await {
        Ok(kimi::TabbitLikeOutput::Stream(resp)) => resp,
        Ok(kimi::TabbitLikeOutput::Json(value)) => {
            let content = value
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return json_ok(json!({
                "ok": true,
                "model": value.get("model").cloned().unwrap_or(json!(model)),
                "content": content,
                "latencyMs": started.elapsed().as_millis() as u64,
            }));
        }
        Err(e) => return json_err(e, StatusCode::BAD_GATEWAY),
    };
    let mut content = String::new();
    let mut model_out = model.clone();
    let mut resp = resp;
    let mut stream_error: Option<String> = None;
    let mut raw_frames: Vec<String> = Vec::new();
    // 跨 chunk 的 UTF-8 半字符尾部：严格 from_utf8 失败会整块丢弃，中文大帧
    // 会被静默吃掉（同 GLM 通路 2026-09-16 事故）。
    let mut utf8_carry: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.rx.recv().await {
        let chunk = match chunk {
            Ok(c) => c,
            // 🔴 流里的 Err 必须往外抛。以前这里写的是 `let Ok(chunk) = chunk else { break }`，
            //    等于把上游业务错误（如「K3 仅对付费会员开放」）整条丢弃，
            //    再返回 ok:true + 空 content —— 用户只看到「成功但空白」，无从判断原因。
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        };
        // raw_frames 保留「上游原始证据」语义（lossy，含被切断的字符也能显示）；
        // 解析则用跨 chunk 解码，保证字节不丢。
        if raw_frames.len() < 8 {
            raw_frames.push(
                String::from_utf8_lossy(&chunk)
                    .chars()
                    .take(300)
                    .collect::<String>(),
            );
        }
        let text =
            wb_switch_core::modules::glm_anthropic::decode_stream_chunk(&mut utf8_carry, &chunk);
        for line in text.lines() {
            let Some(payload) = line.strip_prefix("data: ") else { continue };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" { continue; }
            if let Ok(v) = serde_json::from_str::<Value>(payload) {
                if let Some(m) = v.get("model").and_then(Value::as_str) { model_out = m.to_string(); }
                if let Some(delta) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                    content.push_str(delta);
                }
            }
        }
        if payload_contains_done(&text) { break; }
    }
    if let Some(err) = stream_error {
        return json_err(err, StatusCode::BAD_GATEWAY);
    }
    if content.is_empty() {
        // 零正文 = 上游没真正产出：宁可报错，也不要返回「成功但空白」。
        // 附带原始帧便于定位（帧里不含凭据）。
        let dump = raw_frames.join(" ⏎ ");
        return json_err(
            format!(
                "Kimi 上游未返回任何内容（模型 {model_out}）：账号可能无该模型权限或上游限流。原始帧：{}",
                if dump.is_empty() { "（无）".to_string() } else { dump.chars().take(1000).collect::<String>() }
            ),
            StatusCode::BAD_GATEWAY,
        );
    }
    json_ok(json!({
        "ok": true,
        "model": model_out,
        "content": content,
        "latencyMs": started.elapsed().as_millis() as u64,
    }))
}

fn payload_contains_done(text: &str) -> bool {
    text.contains("data: [DONE]")
}

async fn api_kimi_accounts_list() -> Response {
    json_ok(json!({ "accounts": kimi::list_for_display() }))
}

async fn api_kimi_add_account(Json(body): Json<Value>) -> Response {
    let Some(raw_token) = body.get("raw_token").and_then(Value::as_str) else {
        return json_err("缺少 raw_token".to_string(), StatusCode::BAD_REQUEST);
    };
    let name = body.get("name").and_then(Value::as_str);
    // 扩展可带上浏览器的 device_id（与 kimi.com 同源，降低风控概率）
    let device_id = body.get("device_id").and_then(Value::as_str);
    match kimi::add_account(name, raw_token, device_id) {
        // 🔴 `kimi::add_account` 自己已经返回 `{updated, reason, account}`，这里必须**摊平**再包一层，
        // 否则响应体变成 `account.account.<字段>`（2026-09-16 实测：扩展读 `account.id` 拿到
        // undefined，明明 HTTP 200 却报「写入网关失败（HTTP 200）」，并因此无法回收自己刚建的账号）。
        Ok(added) => {
            let reason = added.get("reason").cloned().unwrap_or_else(|| json!("added"));
            let updated = added.get("updated").cloned().unwrap_or_else(|| json!(false));
            let account = added
                .get("account")
                .cloned()
                .unwrap_or_else(|| added.clone());
            json_ok(json!({
                "ok": true,
                "reason": reason,
                "updated": updated,
                "account": account,
            }))
        }
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_kimi_remove_account(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    if kimi::remove_account(id) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_kimi_enable_account(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    if kimi::set_account_enabled(id, enabled) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_kimi_update_limits(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    let max_concurrency = body.get("max_concurrency").and_then(Value::as_u64).unwrap_or(2) as usize;
    let min_interval_ms = body
        .get("min_interval_seconds")
        .and_then(Value::as_f64)
        .map(|s| (s * 1000.0) as u64)
        .unwrap_or(500);
    if kimi::update_account_limits(id, max_concurrency, min_interval_ms) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_kimi_verify_account(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    match kimi::verify_account(id).await {
        Ok(result) => json_ok(result),
        // 按错误类别回状态码：网络不可达 → 503（可重试）、凭据失效 → 401、
        // 上游异常 → 502。响应体带 code/retryable，供扩展分级展示文案。
        Err(e) => {
            let status = StatusCode::from_u16(e.http_status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            (status, Json(e.to_value())).into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Grok 融合网关（多账号 → OpenAI 兼容接口，移植自 chenyme/grok2api）
// ---------------------------------------------------------------------------

async fn api_grok_chat_completions(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("grok", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or("grok-2").to_string();
    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    match grok::chat_completions_unified(body).await {
        Ok(grok::GrokChatOutput::Stream(resp)) => {
            // 旧直连流路径（build provider 可能仍走这里）。
            log_gateway_request("grok", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
            let stream = futures_util::stream::unfold(resp, |mut resp| async move {
                match resp.chunk().await {
                    Ok(Some(chunk)) => Some((Ok(chunk), resp)),
                    Ok(None) => None,
                    Err(e) => Some((Err(format!("Grok 上游流中断: {e}")), resp)),
                }
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Ok(grok::GrokChatOutput::Json(value)) => {
            log_gateway_request("grok", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
            if !want_stream {
                return json_ok(value);
            }
            // v1.1.0：WebReply 通道无逐 token 流 —— 由完整 JSON 合成 OpenAI chunk 帧。
            let content = value
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let reasoning = value
                .pointer("/choices/0/message/reasoning_content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // v1.6.0：伪工具调用的 tool_calls 与 finish_reason 透出（流式）
            let tool_calls = value
                .pointer("/choices/0/message/tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let finish = value
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_string();
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, String>>(16);
            tokio::spawn(async move {
                let frame = |delta: Value, finish: Option<&str>| {
                    json!({
                        "id": format!("chatcmpl-grok-{}", wb_switch_core::modules::config::now_ms()),
                        "object": "chat.completion.chunk",
                        "created": wb_switch_core::modules::config::now_ms() / 1000,
                        "model": model,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                    })
                    .to_string()
                };
                let _ = tx.send(Ok(axum::body::Bytes::from(format!("data: {}\n\n", frame(json!({"role": "assistant"}), None))))).await;
                if !reasoning.is_empty() {
                    let _ = tx.send(Ok(axum::body::Bytes::from(format!("data: {}\n\n", frame(json!({"reasoning_content": reasoning}), None))))).await;
                }
                if !content.is_empty() {
                    let _ = tx.send(Ok(axum::body::Bytes::from(format!("data: {}\n\n", frame(json!({"content": content}), None))))).await;
                }
                // v1.6.0：工具调用 delta 帧（与 qwen_web.rs:1289 同款形状）
                for (i, tc) in tool_calls.iter().enumerate() {
                    let _ = tx.send(Ok(axum::body::Bytes::from(format!("data: {}\n\n", frame(json!({
                        "tool_calls": [{
                            "index": i,
                            "id": tc.get("id").cloned().unwrap_or(Value::Null),
                            "type": "function",
                            "function": {
                                "name": tc.pointer("/function/name").cloned().unwrap_or(Value::Null),
                                "arguments": tc.pointer("/function/arguments").cloned().unwrap_or(json!("{}")),
                            },
                        }],
                    }), None))))).await;
                }
                let _ = tx.send(Ok(axum::body::Bytes::from(format!("data: {}\n\n", frame(json!({}), Some(finish.as_str())))))).await;
                let _ = tx.send(Ok(axum::body::Bytes::from("data: [DONE]\n\n"))).await;
            });
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(e) => {
            log_gateway_request("grok", key_id.as_deref(), &model, false, started.elapsed().as_millis() as u64, &e);
            let status = if e.contains("尚未配置任何可用账号") || e.contains("账号池为空") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            glm_proxy_error(e, status)
        }
    }
}

async fn api_grok_models(headers: HeaderMap) -> Response {
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("grok", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    log_gateway_request("grok", key_id.as_deref(), "models", true, 0, "");
    json_ok(grok::models_payload())
}

/// Grok 网关 Anthropic Messages 端点（2026-09-27）：ZCode 等 Anthropic 协议客户端
/// 的入口——桥接透传 grok2api `/v1/messages`（cli-chat-proxy 原生支持 Messages
/// 协议与工具调用，Build 池自动续期）。流式（text/event-stream）与非流式（JSON）
/// 均按上游 Content-Type 透传；鉴权与 /grok/v1/chat/completions 同口径。
async fn api_grok_messages(headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("grok", key_id.as_deref(), "messages", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let Some(cfg) = grok::g2a_proxy_config() else {
        return glm_proxy_error(
            "grok2api 桥接未启用：请配置 ~/.wb-switch/grok_g2a_proxy.json".to_string(),
            StatusCode::SERVICE_UNAVAILABLE,
        );
    };
    // 透传客户端的 anthropic-version（上游要求该头；未带时由桥接补默认值）
    let anthropic_version = headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    // 1.0.5：Anthropic 路径默认补 thinking（客户端显式传入优先，见 grok::prepare_messages_body）；
    // 逃生开关 `x-grok-thinking: off` 可关闭该注入。
    let thinking_disabled = grok::thinking_disabled_from_header(
        headers.get("x-grok-thinking").and_then(|v| v.to_str().ok()),
    );
    // 1.1.0：工具调用可见思考链（规划协议+调用链摘要）；
    // 逃生开关 `x-grok-tool-protocol: off` 可按请求关闭。
    let tool_protocol_disabled = grok::tool_protocol_disabled_from_header(
        headers
            .get("x-grok-tool-protocol")
            .and_then(|v| v.to_str().ok()),
    );
    // 1.2.0：工具交换最终回答的收尾复盘尾块；逃生开关 `x-grok-wrapup: off`。
    let wrap_up_disabled = grok::tool_protocol_disabled_from_header(
        headers.get("x-grok-wrapup").and_then(|v| v.to_str().ok()),
    );
    let wrap_up_ctx = if wrap_up_disabled {
        None
    } else {
        grok::wrap_up_context(&cfg, body.as_ref())
    };
    // 托管自愈：桥接失败可能是 grok2api 进程掉线——探活/拉起后重试一次
    let mut forward = grok::g2a_messages_forward(
        &cfg,
        body.as_ref(),
        anthropic_version.as_deref(),
        thinking_disabled,
        tool_protocol_disabled,
    )
    .await;
    if forward.is_err() {
        let _ = grok2api_manager::ensure_alive().await;
        forward = grok::g2a_messages_forward(
            &cfg,
            body.as_ref(),
            anthropic_version.as_deref(),
            thinking_disabled,
            tool_protocol_disabled,
        )
        .await;
    }
    match forward {
        Ok(resp) => {
            let status = resp.status();
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let log_model = "messages";
            if content_type.contains("event-stream") {
                log_gateway_request("grok", key_id.as_deref(), log_model, true, 0, "流式桥接");
                // 1.2.0：收尾复盘尾块（流式）。扣住 message_stop → 判定是否需要
                // 模型复盘（30s 超时 fail-open）→ 注入尾块事件 → 放行 message_stop。
                let injector = grok::WrapUpStreamInjector::new(wrap_up_ctx.is_some());
                let cfg_s = cfg.clone();
                let ctx_s = wrap_up_ctx.clone();
                let mut drained = false;
                let stream = futures_util::stream::unfold(
                    (resp, injector, Some(cfg_s), ctx_s, drained),
                    |(mut resp, mut inj, cfg, ctx, mut drained)| async move {
                        // 扣住 message_stop：先补尾块再放行（同一次 poll 内完成）
                        if inj.is_holding_stop() {
                            let tail = match (&cfg, &ctx) {
                                (Some(c), Some(wctx)) => {
                                    let mut tail = None;
                                    if c.wrap_up_model_review
                                        && grok::wrap_up_review_needed(wctx, inj.draft())
                                    {
                                        tail = grok::g2a_wrap_up_review(c, wctx, inj.draft())
                                            .await
                                            .map(|r| {
                                                format!(
                                                    "【工具链路复盘·网关复核】\n{r}\n（本块由网关在工具链路收尾时自动生成）"
                                                )
                                            });
                                    }
                                    Some(tail.unwrap_or_else(|| grok::wrap_up_tail(wctx)))
                                }
                                _ => None,
                            };
                            let out = inj.complete_with_tail(tail);
                            return Some((
                                Ok::<_, String>(axum::body::Bytes::from(out)),
                                (resp, inj, None, None, drained),
                            ));
                        }
                        if drained {
                            return None;
                        }
                        match resp.chunk().await {
                            Ok(Some(chunk)) => {
                                let mut out = inj.feed(&chunk);
                                if inj.is_holding_stop() {
                                    let tail = match (&cfg, &ctx) {
                                        (Some(c), Some(wctx)) => {
                                            let mut tail = None;
                                            if c.wrap_up_model_review
                                                && grok::wrap_up_review_needed(wctx, inj.draft())
                                            {
                                                tail = grok::g2a_wrap_up_review(c, wctx, inj.draft())
                                                    .await
                                                    .map(|r| {
                                                        format!(
                                                            "【工具链路复盘·网关复核】\n{r}\n（本块由网关在工具链路收尾时自动生成）"
                                                        )
                                                    });
                                            }
                                            Some(tail.unwrap_or_else(|| grok::wrap_up_tail(wctx)))
                                        }
                                        _ => None,
                                    };
                                    out.extend_from_slice(&inj.complete_with_tail(tail));
                                    return Some((
                                        Ok::<_, String>(axum::body::Bytes::from(out)),
                                        (resp, inj, None, None, drained),
                                    ));
                                }
                                Some((Ok(axum::body::Bytes::from(out)), (resp, inj, cfg, ctx, drained)))
                            }
                            Ok(None) => {
                                // 上游结束：冲刷残留缓冲后关闭
                                let out = inj.finish();
                                if out.is_empty() {
                                    None
                                } else {
                                    drained = true;
                                    Some((Ok(axum::body::Bytes::from(out)), (resp, inj, None, None, drained)))
                                }
                            }
                            Err(e) => {
                                Some((
                                    Err(format!("grok2api Messages 流中断: {e}")),
                                    (resp, inj, None, None, true),
                                ))
                            }
                        }
                    },
                );
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, content_type)],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            } else {
                let body_text = resp.text().await.unwrap_or_default();
                // 1.2.0：收尾复盘尾块（非流式）——本轮无 tool_use 且历史有工具调用
                // 时，向 content 数组末尾追加复盘块。
                let body_text = match &wrap_up_ctx {
                    Some(ctx) => match grok::parse_response_content(&body_text) {
                        Some((model_text, false)) => {
                            let mut reviewed = None;
                            if cfg.wrap_up_model_review
                                && grok::wrap_up_review_needed(ctx, &model_text)
                            {
                                reviewed = grok::g2a_wrap_up_review(&cfg, ctx, &model_text)
                                    .await
                                    .map(|r| {
                                        format!(
                                            "【工具链路复盘·网关复核】\n{r}\n（本块由网关在工具链路收尾时自动生成）"
                                        )
                                    });
                            }
                            let tail = reviewed.unwrap_or_else(|| grok::wrap_up_tail(ctx));
                            grok::wrap_up_append_to_json(&body_text, &tail)
                        }
                        _ => body_text,
                    },
                    None => body_text,
                };
                log_gateway_request(
                    "grok",
                    key_id.as_deref(),
                    log_model,
                    status.is_success(),
                    0,
                    &format!("status={status}"),
                );
                (
                    status,
                    [(header::CONTENT_TYPE, content_type)],
                    body_text,
                )
                    .into_response()
            }
        }
        Err(e) => {
            log_gateway_request("grok", key_id.as_deref(), "messages", false, 0, &e);
            glm_proxy_error(format!("grok2api 桥接失败: {e}"), StatusCode::BAD_GATEWAY)
        }
    }
}

async fn api_grok_accounts_list() -> Response {
    json_ok(json!({ "accounts": grok::list_for_display() }))
}

async fn api_grok_add_account(Json(body): Json<Value>) -> Response {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("").trim();
    let provider = body.get("provider").and_then(Value::as_str).unwrap_or("web").trim();
    let token = body.get("token").and_then(Value::as_str).unwrap_or("").trim();
    let tier = body.get("tier").and_then(Value::as_str);

    match grok::import_account(name, provider, token, tier) {
        Ok(acc) => json_ok(json!({ "ok": true, "account": acc })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_grok_remove_account(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("").trim();
    if id.is_empty() {
        return json_err("缺少账号 id".to_string(), StatusCode::BAD_REQUEST);
    }
    if grok::remove_account(id) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在或删除失败".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_grok_enable_account(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("").trim();
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    if id.is_empty() {
        return json_err("缺少账号 id".to_string(), StatusCode::BAD_REQUEST);
    }
    if grok::set_account_enabled(id, enabled) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在或设置失败".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_grok_pool_overview() -> Response {
    json_ok(grok_pool::pool_overview())
}

async fn api_grok_pool_check() -> Response {
    json_ok(grok_pool::run_check_batch().await)
}

async fn api_grok_pool_history() -> Response {
    json_ok(grok_pool::history())
}

async fn api_grok_pool_policy_get() -> Response {
    let p = grok_pool::load_policy();
    json_ok(json!({
        "autoCheckEnabled": p.auto_check_enabled,
        "checkIntervalMinutes": p.check_interval_minutes,
        "batchSize": p.batch_size,
        "riskLowThreshold": p.risk_low_threshold,
        "riskHighThreshold": p.risk_high_threshold,
        "pauseHighRisk": p.pause_high_risk,
        "successDelta": p.success_delta,
        "rateLimitPenalty": p.rate_limit_penalty,
        "timeoutPenalty": p.timeout_penalty,
        "fatalPenalty": p.fatal_penalty,
        "minPoolSize": p.min_pool_size,
        "pauseWhenBelowMin": p.pause_when_below_min,
        "resident_ttl_min": p.resident_ttl_min,
    }))
}

async fn api_grok_pool_policy_set(Json(body): Json<Value>) -> Response {
    let mut p = grok_pool::load_policy();
    if let Some(v) = body.get("autoCheckEnabled").and_then(Value::as_bool) { p.auto_check_enabled = v; }
    if let Some(v) = body.get("checkIntervalMinutes").and_then(Value::as_u64) { p.check_interval_minutes = v.max(15); }
    if let Some(v) = body.get("batchSize").and_then(Value::as_u64) { p.batch_size = (v as usize).clamp(1, 10); }
    if let Some(v) = body.get("riskLowThreshold").and_then(Value::as_i64) { p.risk_low_threshold = v.clamp(0, 100); }
    if let Some(v) = body.get("riskHighThreshold").and_then(Value::as_i64) { p.risk_high_threshold = v.clamp(0, 100); }
    if let Some(v) = body.get("pauseHighRisk").and_then(Value::as_bool) { p.pause_high_risk = v; }
    if let Some(v) = body.get("successDelta").and_then(Value::as_i64) { p.success_delta = v.clamp(0, 50); }
    if let Some(v) = body.get("rateLimitPenalty").and_then(Value::as_i64) { p.rate_limit_penalty = v.clamp(0, 100); }
    if let Some(v) = body.get("timeoutPenalty").and_then(Value::as_i64) { p.timeout_penalty = v.clamp(0, 100); }
    if let Some(v) = body.get("fatalPenalty").and_then(Value::as_i64) { p.fatal_penalty = v.clamp(0, 100); }
    if let Some(v) = body.get("minPoolSize").and_then(Value::as_u64) { p.min_pool_size = (v as usize).clamp(1, 100); }
    if let Some(v) = body.get("pauseWhenBelowMin").and_then(Value::as_bool) { p.pause_when_below_min = v; }
    if let Some(v) = body.get("resident_ttl_min").and_then(Value::as_u64) { p.resident_ttl_min = grok_pool::clamp_resident_ttl_min(v); }
    match grok_pool::save_policy(&p) {
        Ok(_) => json_ok(json!({"ok": true})),
        Err(e) => json_err(e, StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_grok_test(Json(body): Json<Value>) -> Response {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("grok-2");
    let prompt = body.get("prompt").and_then(Value::as_str).unwrap_or("你好");
    let enable_thinking = body.get("enableThinking").and_then(Value::as_bool).unwrap_or(false);
    let account_id = body.get("accountId").and_then(Value::as_str);

    match grok::test_chat(model, prompt, enable_thinking, account_id).await {
        Ok(res) => json_ok(res),
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

// ---------------------------------------------------------------------------
// Qoder 融合网关（多账号 → OpenAI 兼容接口）
// ---------------------------------------------------------------------------

// ---- 网关管理端点（API Keys / 日志 / 概览）----

// ---------------------------------------------------------------------------
// ChatGLM 网页协议网关（glm2api 移植）
// ---------------------------------------------------------------------------

async fn api_glmweb_chat_completions(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("glmweb", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or("glm-4").to_string();
    match glm_web::chat_completions(body).await {
        Ok(glm_web::GlmChatOutput::Stream(upstream)) => {
            log_gateway_request("glmweb", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
            let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Ok(glm_web::GlmChatOutput::Json(value)) => {
            log_gateway_request("glmweb", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
            json_ok(value)
        }
        Err(e) => {
            log_gateway_request("glmweb", key_id.as_deref(), &model, false, started.elapsed().as_millis() as u64, &e);
            glm_proxy_error(e, StatusCode::BAD_GATEWAY)
        }
    }
}

async fn api_glmweb_models(headers: HeaderMap) -> Response {
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    log_gateway_request("glmweb", key_id.as_deref(), "models", true, 0, "");
    json_ok(glm_web::models_payload())
}

/// ChatGLM 试调（服务端转发非流式请求并聚合）。
async fn api_glmweb_test(Json(body): Json<Value>) -> Response {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("glm-4.7-flash").to_string();
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("只回复：OK")
        .trim()
        .to_string();
    if prompt.is_empty() {
        return json_err("请输入测试内容".to_string(), StatusCode::BAD_REQUEST);
    }
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    });
    let started = std::time::Instant::now();
    match glm_web::chat_completions(payload).await {
        Ok(glm_web::GlmChatOutput::Json(value)) => {
            let content = value
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            json_ok(json!({
                "ok": true,
                "model": value.get("model").cloned().unwrap_or(json!(model)),
                "content": content,
                "latencyMs": started.elapsed().as_millis() as u64,
            }))
        }
        Ok(glm_web::GlmChatOutput::Stream(_)) => {
            json_err("非预期：非流式请求返回了流".to_string(), StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

async fn api_glmweb_tokens_list() -> Response {
    json_ok(json!({
        "tokens": glm_web::list_tokens_masked(),
        // v0.1.46：pool 增补 defaultModel / degraded / needRenew，便于前端与同步工具判断
        "pool": glm_web::pool_health(),
    }))
}

/// v0.1.46：浏览器同步工具专用 —— 幂等写回 chatglm refresh token。
///
/// body: `{ "token": "<refresh_token>", "validate": true, "source": "browser-extension" }`
/// - `validate=true`（默认）时先真实调用上游 refresh，失败**不写盘**并返回 401/403 语义；
/// - token 已存在则只刷新缓存、不新增槽位（幂等，可 20 分钟一次反复调用）。
async fn api_glmweb_token_sync(Json(body): Json<Value>) -> Response {
    let Some(token) = body.get("token").and_then(Value::as_str) else {
        return json_err("缺少 token".to_string(), StatusCode::BAD_REQUEST);
    };
    let validate = body.get("validate").and_then(Value::as_bool).unwrap_or(true);
    let source = body.get("source").and_then(Value::as_str);

    match glm_web::sync_token(token, validate, source).await {
        Ok(result) => json_ok(result),
        Err(e) => {
            // 凭据失效（上游 refresh 被拒）→ 401；其余（网络/解析/写盘）→ 502
            let invalid = e.contains("刷新失败")
                || e.contains("401")
                || e.contains("403")
                || e.contains("业务失败")
                || e.contains("缺少 access_token");
            let status = if invalid {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_GATEWAY
            };
            json_err(e, status)
        }
    }
}

async fn api_glmweb_token_add(Json(body): Json<Value>) -> Response {
    let Some(token) = body.get("token").and_then(Value::as_str) else {
        return json_err("缺少 token".to_string(), StatusCode::BAD_REQUEST);
    };
    match glm_web::add_token(token) {
        Ok(result) => json_ok(result),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_glmweb_token_remove(Json(body): Json<Value>) -> Response {
    let Some(index) = body.get("index").and_then(Value::as_u64) else {
        return json_err("缺少 index".to_string(), StatusCode::BAD_REQUEST);
    };
    if glm_web::remove_token(index as usize) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("槽位不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_gateway_keys_list() -> Response {
    json_ok(json!({ "keys": gateway_admin::list_keys_masked() }))
}

async fn api_gateway_key_create(Json(body): Json<Value>) -> Response {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    // region：可选 cn/intl（WorkBuddy 池区域专用密钥）；缺省/其它值 = 通用密钥。
    let region = body.get("region").and_then(Value::as_str).filter(|r| {
        let r = r.trim();
        r == "cn" || r == "intl"
    });
    match gateway_admin::create_key_scoped(name, region) {
        Ok(key) => json_ok(json!({ "ok": true, "key": key })),
        Err(e) => json_err(e, StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_gateway_key_delete(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    if gateway_admin::delete_key(id) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("密钥不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_gateway_logs(RawQuery(query): RawQuery) -> Response {
    let params = query.unwrap_or_default();
    let gateway = params
        .split('&')
        .find_map(|pair| pair.strip_prefix("gateway="))
        .map(String::from);
    let limit = params
        .split('&')
        .find_map(|pair| pair.strip_prefix("limit=").and_then(|v| v.parse::<usize>().ok()))
        .unwrap_or(50)
        .min(500);
    json_ok(json!({ "logs": gateway_admin::list_logs(gateway.as_deref(), limit) }))
}

async fn api_gateway_overview() -> Response {
    json_ok(gateway_admin::overview())
}

/// 所有模型网关的聚合总览（供统一网关下拉菜单浏览与切换）。
async fn api_gateways_overview() -> Response {
    json_ok(gateway_overview::overview().await)
}

/// hours 查询参数解析（照抄 api_gateway_logs 的 split/strip_prefix 写法，不做 URL 解码）：
/// 缺失或非数字回落 24，其余由 channel_health 归一（只接受 1/6/24/168）。
fn parse_hours_param(params: Option<&str>) -> i64 {
    params
        .and_then(|p| p.split('&').find_map(|pair| pair.strip_prefix("hours=")))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(24)
}

/// 渠道健康度总览（?hours=1|6|24|168，默认 24）。
async fn api_channel_health(RawQuery(query): RawQuery) -> Response {
    json_ok(channel_health::overview(parse_hours_param(query.as_deref())).await)
}

/// 渠道健康度详情（?gateway=<id>&hours=…）。
async fn api_channel_health_detail(RawQuery(query): RawQuery) -> Response {
    let params = query.unwrap_or_default();
    let gateway = params
        .split('&')
        .find_map(|pair| pair.strip_prefix("gateway="))
        .map(String::from)
        .unwrap_or_default();
    json_ok(channel_health::detail(&gateway, parse_hours_param(Some(&params))).await)
}

/// 模型可用性自测（POST { gateway, model, prompt?, maxTokens?, timeoutSecs? }）。
///
/// 始终返回 200 + 结果对象：**「测不通」本身就是一个有效结论**，
/// 由结果里的 `ok` / `code` / `reason` 表达，前端据此渲染成功或失败卡片。
/// 若这里改成用 HTTP 状态码表达失败，前端就得把"业务失败"和"接口挂了"混在一起判断。
async fn api_model_test(Json(body): Json<Value>) -> Response {
    // 回环自调用需要本服务实际监听的端口；core 层不持有该值，故由此处注入。
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(model_probe::run(port, &body).await)
}

fn qoder_auth_ok(headers: &HeaderMap) -> bool {
    gateway_key_status(headers).0
}

/// 网关密钥校验：命中"网关专用 Key"（计数+1）或 GLM 代理主密钥。
/// 返回 (有效, key id)。
fn gateway_key_status(headers: &HeaderMap) -> (bool, Option<String>) {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    let cfg = glm_proxy::load_config();
    gateway_admin::verify_request_key(presented, &cfg.api_key)
}

/// 记录网关请求日志（chat 调用完成/失败后调用）。
fn log_gateway_request(gateway: &str, key_id: Option<&str>, model: &str, ok: bool, latency_ms: u64, error: &str) {
    gateway_admin::record_request(json!({
        "ts": wb_switch_core::modules::config::now_ms(),
        "gateway": gateway,
        "keyId": key_id,
        "model": model,
        "ok": ok,
        "latencyMs": latency_ms,
        "error": if error.is_empty() { Value::Null } else { json!(error) },
    }));
}

/// Qoder 系通路的 SSE 修复流。
///
/// 🔴 逐帧修复是**行不通的**：上游把乱码串切在任意位置（GBK 双字节与模型 token
/// 边界不对齐），单帧碎片不满足「GBK 重编码=合法 UTF-8」的修复条件。
/// 正确做法：**按内容累积**——把 content 增量拼起来，对累积串做不动点修复，
/// 只下发「已修复文本」相对上次的新增部分；无法前缀对齐时降级为原样透传。
fn qoder_repaired_sse_body(
    resp: wb_switch_core::modules::qoder::UpstreamResponse,
    err_label: &'static str,
    pre_buffer: Vec<u8>,
) -> axum::body::Body {
    // 预读缓冲（chat_stream_with_failover 已确认含首个内容增量）作为起始 line_buf
    let seeded_line_buf = String::from_utf8_lossy(&pre_buffer).to_string();
    let stream = futures_util::stream::unfold(
        (
            resp,
            Vec::<u8>::new(),
            seeded_line_buf,
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            false,
            None::<String>,
            false,
            err_label,
        ),
        |(mut resp, mut carry, mut line_buf, mut raw, mut emitted, mut raw_r, mut emitted_r, mut broken, mut pending_data, mut content_seen, err_label)| async move {
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        let text = wb_switch_core::modules::glm_anthropic::decode_stream_chunk(
                            &mut carry, &chunk,
                        );
                        line_buf.push_str(&text);
                        let mut out = String::new();
                        while let Some(pos) = line_buf.find('\n') {
                            let line: String = line_buf.drain(..=pos).collect();
                            let trimmed = line.trim_end_matches(['\r', '\n']);
                            // 🔴 上游偶发把 SSE 帧的 JSON 在字符串中间塞进裸换行
                            // （2026-09-18 实测 lite 档 usage 帧：`"prompt_tokens_de`+换行+`tails":`），
                            // 下游严格客户端逐帧 json 解析必报错。修复：解析不完整的 data 帧
                            // 先扣下，与后续行**无缝拼接**（换行在字符串内属上游序列化事故，
                            // 拼回即还原原字节）直至可解析；遇空行仍拼不上则原样放行
                            // （保字节优先，不吞帧），超长兜底 256KB 防止无限扣留。
                            if let Some(pending) = pending_data.as_mut() {
                                if trimmed.is_empty() {
                                    // 事件已结束仍拼不完整 → 原样补发（残帧 + 事件分隔行）
                                    let joined = std::mem::take(pending);
                                    out.push_str(&joined);
                                    out.push_str("\n\n");
                                    continue;
                                }
                                pending.push_str(trimmed);
                                if pending == "[DONE]" || serde_json::from_str::<Value>(pending).is_ok() {
                                    let joined_line = format!("data: {pending}");
                                    pending_data = None;
                                    out.push_str(&repair_qoder_sse_line(
                                        &joined_line, &mut raw, &mut emitted, &mut raw_r, &mut emitted_r, &mut broken, &mut content_seen,
                                    ));
                                } else if pending.len() > 256 * 1024 {
                                    let joined = std::mem::take(pending);
                                    out.push_str(&joined);
                                }
                                out.push('\n');
                                continue;
                            }
                            if let Some(payload) = trimmed.strip_prefix("data: ") {
                                let payload = payload.trim();
                                if !payload.is_empty()
                                    && payload != "[DONE]"
                                    && serde_json::from_str::<Value>(payload).is_err()
                                {
                                    pending_data = Some(payload.to_string());
                                    continue;
                                }
                            }
                            if !content_seen && trimmed == "data: [DONE]" {
                                // 🔴 流结束仍零内容（免费档额度受限/风控时上游返回 200 空流）：
                                // 以显式错误帧替代静默空流，客户端看到真实原因而非 "No response received"。
                                out.push_str(&format!(
                                    "data: {}\n\ndata: [DONE]\n\n",
                                    json!({
                                        "error": {
                                            "message": "上游返回空回复（账号免费额度受限或被风控），请稍后重试或更换档位",
                                            "type": "upstream_empty_response",
                                        },
                                        "object": "chat.completion.chunk",
                                    })
                                ));
                                continue;
                            }
                            out.push_str(&repair_qoder_sse_line(
                                trimmed, &mut raw, &mut emitted, &mut raw_r, &mut emitted_r, &mut broken, &mut content_seen,
                            ));
                            out.push('\n');
                        }
                        if !out.is_empty() {
                            return Some((
                                Ok(axum::body::Bytes::from(out)),
                                (resp, carry, line_buf, raw, emitted, raw_r, emitted_r, broken, pending_data, content_seen, err_label),
                            ));
                        }
                        // 本块没有完整行 → 继续读下一块
                    }
                    Ok(None) => {
                        // 收尾：扣留中的残帧原样放行，再把累积内容强制修复到不动点，剩余部分合成一帧补发
                        let mut out = String::new();
                        if let Some(pending) = pending_data.take() {
                            out.push_str(&pending);
                            out.push('\n');
                        }
                        let repaired = wb_switch_core::modules::qoder::repair_gbk_mojibake(&raw);
                        if repaired.starts_with(&emitted) {
                            let new = &repaired[emitted.len()..];
                            if !new.is_empty() {
                                out.push_str(&format!(
                                    "data: {}\n\n",
                                    json!({"choices":[{"delta":{"content": new},"index":0}],"object":"chat.completion.chunk"})
                                ));
                            }
                        }
                        let repaired_r =
                            wb_switch_core::modules::qoder::repair_gbk_mojibake(&raw_r);
                        if repaired_r.starts_with(&emitted_r) {
                            let new = &repaired_r[emitted_r.len()..];
                            if !new.is_empty() {
                                out.push_str(&format!(
                                    "data: {}\n\n",
                                    json!({"choices":[{"delta":{"reasoning_content": new},"index":0}],"object":"chat.completion.chunk"})
                                ));
                            }
                        }
                        if !content_seen {
                            // 流结束（未发 [DONE] 的异常收尾）仍零内容：补一个显式错误帧
                            out.push_str(&format!(
                                "data: {}\n\n",
                                json!({
                                    "error": {
                                        "message": "上游返回空回复（账号免费额度受限或被风控），请稍后重试或更换档位",
                                        "type": "upstream_empty_response",
                                    },
                                    "object": "chat.completion.chunk",
                                })
                            ));
                        }
                        if !out.is_empty() {
                            return Some((
                                Ok(axum::body::Bytes::from(out)),
                                (resp, carry, line_buf, raw, emitted, raw_r, emitted_r, broken, pending_data, content_seen, err_label),
                            ));
                        }
                        return None;
                    }
                    Err(e) => {
                        return Some((
                            Err(format!("{err_label}: {e}")),
                            (resp, carry, line_buf, raw, emitted, raw_r, emitted_r, broken, pending_data, content_seen, err_label),
                        ))
                    }
                }
            }
        },
    );
    axum::body::Body::from_stream(stream)
}

/// 处理单条 SSE 行：`data:` 帧里的 content/reasoning_content 增量进累积串，
/// 重写为「修复后文本的新增部分」；其余行原样返回。
fn repair_qoder_sse_line(
    line: &str,
    raw: &mut String,
    emitted: &mut String,
    raw_r: &mut String,
    emitted_r: &mut String,
    broken: &mut bool,
    content_seen: &mut bool,
) -> String {
    let Some(data) = line.strip_prefix("data: ") else {
        return line.to_string();
    };
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return line.to_string();
    }
    let Ok(mut value) = serde_json::from_str::<Value>(data) else {
        return line.to_string();
    };
    // 空回复检测：无论修复链是否已放弃（broken），只要出现非空内容增量就标记。
    // 放在 broken 早退之前，保证透传模式下零内容检测依然有效。
    if let Some(delta) = value.pointer("/choices/0/delta") {
        if delta.get("content").and_then(Value::as_str).map_or(false, |s| !s.is_empty())
            || delta
                .get("reasoning_content")
                .and_then(Value::as_str)
                .map_or(false, |s| !s.is_empty())
        {
            *content_seen = true;
        }
    }
    if *broken {
        return line.to_string();
    }
    let mut changed = false;
    if let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            // content：delta（流式）与 message（非流式）都累积进同一串
            for key in ["delta", "message"] {
                let Some(obj) = choice.get_mut(key).and_then(Value::as_object_mut) else {
                    continue;
                };
                let Some(Value::String(s)) = obj.get_mut("content") else {
                    continue;
                };
                raw.push_str(s);
                let repaired = qoder::repair_gbk_mojibake(raw);
                if repaired.starts_with(emitted.as_str()) {
                    let new = repaired[emitted.len()..].to_string();
                    *emitted = repaired;
                    if new != *s {
                        *s = new;
                        changed = true;
                    }
                } else {
                    // 修复结果与已下发前缀冲突 → 该流放弃修复，后续原样透传
                    *broken = true;
                    return line.to_string();
                }
            }
            // reasoning_content：独立累积
            let Some(obj) = choice.get_mut("delta").and_then(Value::as_object_mut) else {
                continue;
            };
            let Some(Value::String(s)) = obj.get_mut("reasoning_content") else {
                continue;
            };
            raw_r.push_str(s);
            let repaired = qoder::repair_gbk_mojibake(raw_r);
            if repaired.starts_with(emitted_r.as_str()) {
                let new = repaired[emitted_r.len()..].to_string();
                *emitted_r = repaired;
                if new != *s {
                    *s = new;
                    changed = true;
                }
            } else {
                *broken = true;
                return line.to_string();
            }
        }
    }
    if changed {
        format!("data: {}", serde_json::to_string(&value).unwrap_or_else(|_| data.to_string()))
    } else {
        line.to_string()
    }
}

async fn api_qoder_chat_completions(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("qoder", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or("lite").to_string();
    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    match qoder::chat_completions_unified(body).await {
        Ok(qoder::QoderChatOutput::Stream(resp, pre)) => {
            // 预读已确认首个内容增量，SSE 到达即视为成功（流内错误对客户端透出，日志记 ok）
            log_gateway_request("qoder", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
            let body = qoder_repaired_sse_body(resp, "Qoder 上游流中断", pre);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                body,
            )
                .into_response()
        }
        Ok(qoder::QoderChatOutput::Json(value)) => {
            log_gateway_request("qoder", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
            json_ok(value)
        }
        Ok(qoder::QoderChatOutput::SynthesizedJson(value)) => {
            // 带 tools 的请求：服务端聚合 + 工具嗅探合成（流式语义以合成帧还原）
            log_gateway_request("qoder", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
            if want_stream {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                    axum::body::Body::from(qoder::synthesized_json_to_sse(&value)),
                )
                    .into_response()
            } else {
                json_ok(value)
            }
        }
        Err(e) => {
            log_gateway_request("qoder", key_id.as_deref(), &model, false, started.elapsed().as_millis() as u64, &e);
            let status = if e.contains("账号池为空") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            glm_proxy_error(e, status)
        }
    }
}

async fn api_qoder_models(headers: HeaderMap) -> Response {
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("qoder", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    log_gateway_request("qoder", key_id.as_deref(), "models", true, 0, "");
    json_ok(qoder::models_payload())
}

async fn api_qwen_models(headers: HeaderMap) -> Response {
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("qwen", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    log_gateway_request("qwen", key_id.as_deref(), "models", true, 0, "");
    json_ok(qwen::models_payload().await)
}

async fn api_qwen_desktop_status() -> Response {
    json_ok(qwen::desktop_status().await)
}

/// 读取 Qwen 双通道配置。
async fn api_qwen_channel_get() -> Response {
    let cfg = qwen_web::load_channel_config();
    json_ok(json!({
        "channel": cfg.channel,
        "autoFallback": cfg.auto_fallback,
        "channels": ["auto", "web", "qoder"],
        "channelLabels": {"auto": "网页版优先（自动回落）", "web": "仅网页版", "qoder": "仅 Qoder"},
    }))
}

/// 切换 Qwen 通道（实时生效：每次请求读取配置，无需重启）。
async fn api_qwen_channel_set(Json(body): Json<Value>) -> Response {
    let Some(channel) = body.get("channel").and_then(Value::as_str) else {
        return json_err("缺少 channel（auto / web / qoder）".to_string(), StatusCode::BAD_REQUEST);
    };
    if !matches!(channel, "auto" | "web" | "qoder") {
        return json_err(format!("无效通道：{channel}（可选 auto / web / qoder）"), StatusCode::BAD_REQUEST);
    }
    let auto_fallback = body.get("autoFallback").and_then(Value::as_bool).unwrap_or(true);
    if let Err(e) = qwen_web::save_channel_config(channel, auto_fallback) {
        return json_err(format!("保存通道配置失败: {e}"), StatusCode::INTERNAL_SERVER_ERROR);
    }
    qwen_web::log_debug(&format!("通道切换 → {channel}（auto_fallback={auto_fallback}）"));
    json_ok(json!({"ok": true, "channel": channel, "autoFallback": auto_fallback}))
}

/// 双通道健康状态（网页版 chat.qwen.ai + Qoder 算力池）。
///
/// 网页版分两层：`web` 是表层检查（token/浏览器/页面就绪，快但**不能证明
/// 能对话**——惩罚期页面打得开而请求被挂起）；`webProbe` 是真实短调用探活
/// （TTL 5 分钟缓存、单飞后台执行，惩罚期失败但不自伤）。`?force=1` 忽略 TTL 立即触发。
async fn api_qwen_channels_status(RawQuery(query): RawQuery) -> Response {
    let force = query
        .as_deref()
        .map(|q| q.split('&').any(|p| p == "force=1" || p == "force" || p == "forcerun=true"))
        .unwrap_or(false);
    let web = qwen_web::web_channel_status().await;
    let web_probe = qwen_web::trigger_web_channel_probe(force);
    let accounts = qoder::list_for_display();
    let total = accounts.len();
    let ok = accounts
        .iter()
        .filter(|a| {
            let status = a.get("status").and_then(Value::as_str).unwrap_or("ok");
            status != "invalid" && status != "error"
                && a.get("quota").and_then(Value::as_bool) != Some(false)
        })
        .count();
    let cfg = qwen_web::load_channel_config();
    json_ok(json!({
        "current": cfg.channel,
        "autoFallback": cfg.auto_fallback,
        "web": web,
        "webProbe": web_probe,
        "qoder": {
            "poolTotal": total,
            "poolOk": ok,
            "available": ok > 0,
            "upstream": "qoder 算力池（QoderGateway 协议）",
        },
        "tokenUser": qwen_web::token_user_hint(),
    }))
}

/// 网页版通道调试日志（最近 n 条，默认 40）。
/// WorkBuddy 工具调用稳定性监控：统计 + 诊断 + 最近事件流。
/// `?n=` 控制返回的事件条数（默认 40，最大 200）。
async fn api_qwen_toolcall_monitor(RawQuery(query): RawQuery) -> Response {
    let n = query
        .as_deref()
        .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("n=")?.parse::<usize>().ok()))
        .unwrap_or(40)
        .min(200);
    let stats = toolcall_monitor::stats();
    let events = toolcall_monitor::recent_events(n);
    json_ok(json!({
        "stats": stats,
        "events": events,
        "advisories": toolcall_monitor::advisories(),
    }))
}

/// 清空监控数据（开始新一轮测试批次时用）。
async fn api_qwen_toolcall_monitor_clear() -> Response {
    toolcall_monitor::clear();
    json_ok(json!({"ok": true}))
}

/// MCP（Model Context Protocol）Streamable HTTP 端点：`POST /mcp`。
///
/// 鉴权与网关一致（Bearer / x-api-key）；支持单条 JSON-RPC 与批量数组。
/// 通知（无 id）返回 202 无响应体；其余返回 JSON-RPC 响应。
async fn api_qwen_mcp(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let started = std::time::Instant::now();
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    // 批量（JSON-RPC 2.0 batch）与单条统一处理。
    let is_batch = body.is_array();
    let messages: Vec<Value> = match body {
        Value::Array(arr) => arr,
        other => vec![other],
    };
    let mut responses: Vec<Value> = Vec::new();
    for msg in &messages {
        if let Some(resp) = qwen_mcp::handle_message(msg).await {
            responses.push(resp);
        }
    }
    let method_summ: String = messages
        .iter()
        .filter_map(|m| m.get("method").and_then(Value::as_str).map(String::from))
        .collect::<Vec<_>>()
        .join(",");
    log_gateway_request(
        "qwen-mcp",
        key_id.as_deref(),
        &method_summ,
        true,
        started.elapsed().as_millis() as u64,
        "",
    );
    if responses.is_empty() {
        // 全部为通知：MCP 规范用 202 Accepted 且无体。
        return (StatusCode::ACCEPTED, "").into_response();
    }
    if responses.len() == 1 && !is_batch {
        return json_ok(responses.remove(0));
    }
    json_ok(Value::Array(responses))
}

/// `GET /mcp`：Streamable HTTP 的能力声明（部分客户端用它探测端点存活）。
async fn api_qwen_mcp_info(headers: HeaderMap) -> Response {
    let (auth_ok, _key_id) = gateway_key_status(&headers);
    if !auth_ok {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    json_ok(json!({
        "name": qwen_mcp::SERVER_NAME,
        "version": qwen_mcp::SERVER_VERSION,
        "protocolVersion": qwen_mcp::DEFAULT_PROTOCOL_VERSION,
        "transport": "streamable-http",
        "endpoint": "/mcp",
        "tools": ["qwen_chat", "qwen_models"],
        "usage": "MCP 客户端以 POST /mcp 发送 JSON-RPC 2.0 消息；鉴权用 Authorization: Bearer <网关密钥>",
    }))
}

async fn api_qwen_web_log(RawQuery(query): RawQuery) -> Response {
    let n = query
        .as_deref()
        .and_then(|q| {
            q.split('&').find_map(|p| p.strip_prefix("n=")?.parse::<usize>().ok())
        })
        .unwrap_or(40)
        .min(80);
    json_ok(json!({ "logs": qwen_web::debug_log_tail(n) }))
}

async fn api_qwen_chat_completions(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("qwen", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let requested_model = body.get("model").and_then(Value::as_str).unwrap_or("qwen3.8-max").to_string();

    // ------------------------------------------------------------------
    // 双通道调度（v2.0）：auto = 网页版优先 + 自动回落 Qoder；
    // web = 仅网页版（失败即报错）；qoder = 仅 Qoder 算力池（现状行为）。
    // ------------------------------------------------------------------
    let channel_cfg = qwen_web::load_channel_config();
    let mut fallback_note: Option<String> = None;
    if channel_cfg.channel != "qoder" {
        match qwen_web::chat_completions(body.clone()).await {
            Ok(qwen_web::QwenWebOutput::Json(value)) => {
                log_gateway_request("qwen", key_id.as_deref(), &requested_model, true, started.elapsed().as_millis() as u64, "");
                let mut v = value;
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("qwen_channel".to_string(), json!("web"));
                    if let Some(note) = &fallback_note {
                        obj.insert("qwen_fallback_note".to_string(), json!(note));
                    }
                }
                return json_ok(v);
            }
            Ok(qwen_web::QwenWebOutput::Stream(rx)) => {
                log_gateway_request("qwen", key_id.as_deref(), &requested_model, true, started.elapsed().as_millis() as u64, "");
                let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                });
                let mut resp = (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response();
                if let Ok(hv) = header::HeaderValue::from_str("web") {
                    resp.headers_mut().insert("X-Qwen-Channel", hv);
                }
                return resp;
            }
            Err(e) => {
                if channel_cfg.channel == "web" {
                    // 仅网页版：不回落，直接报错。
                    log_gateway_request("qwen", key_id.as_deref(), &requested_model, false, started.elapsed().as_millis() as u64, &e);
                    return glm_proxy_error(format!("Qwen 网页版通道失败：{e}"), StatusCode::BAD_GATEWAY);
                }
                // auto：记回落原因，走 Qoder。
                fallback_note = Some(format!("网页版通道失败，已回落 Qoder：{e}"));
            }
        }
    }
    if let Some(note) = &fallback_note {
        log_gateway_request("qwen", key_id.as_deref(), &requested_model, false, 0, note);
    }

    let mapped_qoder_model = if requested_model.contains("max") {
        "max"
    } else if requested_model.contains("flash") || requested_model.contains("turbo") || requested_model.contains("lite") {
        "lite"
    } else {
        "pro"
    };

    let mut qoder_body = body.clone();
    if let Some(obj) = qoder_body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(mapped_qoder_model.to_string()));
    }

    // /qwen/v1 使用**独立账号池**（gateway=qwen），与 /qoder 的默认池额度互不侵占
    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut qoder_result = qoder::chat_completions_unified_for("qwen", qoder_body.clone()).await;

    // 🔴 档位降级（2026-09-20 实测定位）：Qoder Free 档账号实测**仅 lite 有额度**，
    // pro/max 零额度时上游返回 200 空正文（不是错误码），于是"账号 4/4 却全部
    // 空回复"。映射档全灭时自动降一档到 lite 重试——有额度时仍按原档走（不降质），
    // 只有原档确实打不通才降级，并在响应里标注。
    let mut tier_degraded = false;
    if qoder_result.is_err() && mapped_qoder_model != "lite" {
        let empty_pool = qoder_result
            .as_ref()
            .err()
            .map(|e| e.contains("空回复") || e.contains("账号池为空"))
            .unwrap_or(false);
        if empty_pool {
            log_gateway_request(
                "qwen",
                key_id.as_deref(),
                &requested_model,
                false,
                0,
                &format!("原档 {mapped_qoder_model} 无额度/空回复，降级 lite 重试"),
            );
            if let Some(obj) = qoder_body.as_object_mut() {
                obj.insert("model".to_string(), Value::String("lite".to_string()));
            }
            qoder_result = qoder::chat_completions_unified_for("qwen", qoder_body).await;
            tier_degraded = qoder_result.is_ok();
            if tier_degraded {
                // 真实请求确认降级：立即把档位写进 opencode 模型显示名，
                // 让用户在 WorkBuddy 模型列表里直接看到"⚠lite降级"。
                qwen_web::sync_opencode_tier_label(qwen_web::QwenTierLabel::Degraded);
            }
        }
    }
    match qoder_result {
        Ok(qoder::QoderChatOutput::Stream(resp, pre)) => {
            log_gateway_request("qwen", key_id.as_deref(), &requested_model, true, started.elapsed().as_millis() as u64, "");
            let body = qoder_repaired_sse_body(resp, "Qwen 上游流中断", pre);
            let mut out = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                body,
            )
                .into_response();
            if let Ok(hv) = header::HeaderValue::from_str("qoder") {
                out.headers_mut().insert("X-Qwen-Channel", hv);
            }
            if tier_degraded {
                out.headers_mut().insert("X-Qwen-Tier", header::HeaderValue::from_static("tier-lite"));
            }
            if let Some(note) = &fallback_note {
                // 头只允许可见 ASCII，中文回落原因取前缀。
                let ascii: String = note.chars().filter(|c| c.is_ascii()).take(120).collect();
                if let Ok(hv) = header::HeaderValue::from_str(&ascii) {
                    out.headers_mut().insert("X-Qwen-Channel-Note", hv);
                }
            }
            out
        }
        Ok(qoder::QoderChatOutput::Json(value)) => {
            log_gateway_request("qwen", key_id.as_deref(), &requested_model, true, started.elapsed().as_millis() as u64, "");
            let mut v = value;
            if let Some(obj) = v.as_object_mut() {
                obj.insert("qwen_channel".to_string(), json!("qoder"));
                if tier_degraded {
                    obj.insert("qwen_tier_degraded".to_string(), json!(true));
                    obj.insert(
                        "qwen_tier_note".to_string(),
                        json!(format!(
                            "原档 {mapped_qoder_model} 无额度，已降级 lite 档（Free 账号仅 lite 可用）"
                        )),
                    );
                }
                if let Some(note) = &fallback_note {
                    obj.insert("qwen_fallback_note".to_string(), json!(note));
                }
            }
            json_ok(v)
        }
        Ok(qoder::QoderChatOutput::SynthesizedJson(value)) => {
            // 带 tools 的 agent 请求：服务端聚合 + 工具嗅探合成（流式语义以合成帧还原）
            log_gateway_request("qwen", key_id.as_deref(), &requested_model, true, started.elapsed().as_millis() as u64, "");
            if want_stream {
                let mut out = (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                    axum::body::Body::from(qoder::synthesized_json_to_sse(&value)),
                )
                    .into_response();
                if let Ok(hv) = header::HeaderValue::from_str("qoder") {
                    out.headers_mut().insert("X-Qwen-Channel", hv);
                }
                out
            } else {
                let mut v = value;
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("qwen_channel".to_string(), json!("qoder"));
                    if let Some(note) = &fallback_note {
                        obj.insert("qwen_fallback_note".to_string(), json!(note));
                    }
                }
                json_ok(v)
            }
        }
        Err(e) => {
            let err_msg = if e.contains("账号池为空") {
                format!(
                    "Qwen 独立账号池为空（/qwen/v1 已与 /qoder 默认池拆分，额度互不共享）。请导入本机 Qoder 登录态或通过设备授权添加账号到 **Qwen 独立池**（添加时选择 qwen 池），即可解锁免费无风控的 {} 满血算力！",
                    requested_model
                )
            } else {
                format!("Qwen 算力调度异常：{e}")
            };
            let err_msg = match &fallback_note {
                Some(note) => format!("{note}；{err_msg}"),
                None => err_msg,
            };
            log_gateway_request("qwen", key_id.as_deref(), &requested_model, false, started.elapsed().as_millis() as u64, &err_msg);
            glm_proxy_error(err_msg, StatusCode::BAD_GATEWAY)
        }
    }
}

/// `/qwen-flash/v1/models`：只暴露 Qwen3.8-Flash 单一模型。
async fn api_qwen_flash_models(headers: HeaderMap) -> Response {
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("qwen-flash", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    log_gateway_request("qwen-flash", key_id.as_deref(), "models", true, 0, "");
    json_ok(qoder::qwen_flash_models_payload())
}

/// `/qwen-flash/v1/chat/completions`：Qwen3.8-Flash 专用路由。
///
/// 模型由 core 层强制 pin 成 `qwen3.8-flash`（上游 lite），此处只做鉴权、日志与 SSE 修复。
async fn api_qwen_flash_chat_completions(
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("qwen-flash", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    // 上下文守卫：客户端（IDE）对中文 token 估算严重偏低，超限请求会被上游
    // 以 200 空正文回绝（表象同额度耗尽）。这里按保守估算裁剪到预算内。
    let (trimmed, tok_before, tok_after) = guard_context_budget(&mut body);
    if trimmed {
        log_gateway_request(
            "qwen-flash",
            key_id.as_deref(),
            "context-guard",
            false,
            0,
            &format!("上下文超预算已裁剪: {tok_before} → {tok_after} tokens"),
        );
    }
    let requested_model = qoder::QWEN_FLASH_MODEL.to_string();
    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    // 诊断：请求规模（字符数 / 消息数 / 是否带 tools）。上游对超限上下文返回
    // 200 空正文，表象与"额度受限"完全一样——不记录规模就无法区分两者
    // （2026-09-20 用户连续遇到空回复，实测 8 万字符通过、9 万即空）。
    let diag_chars: usize = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter()
                .map(|m| match m.get("content") {
                    Some(Value::String(s)) => s.chars().count(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .map(|s| s.chars().count())
                        .sum(),
                    _ => 0,
                })
                .sum()
        })
        .unwrap_or(0);
    let diag_msgs = body.get("messages").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
    let diag_tools = body.get("tools").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
    // 诊断：客户端自带的 system 提示词与工具名（只记摘要，不记全文）——用于
    // 分析"模型为何不思考/不调工具"：是客户端提示词要求简洁，还是我们的工具
    // 协议缺少规划步骤。无 tools 时不记录，避免刷日志。
    if diag_tools > 0 {
        let tool_names: Vec<String> = body
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        t.pointer("/function/name")
                            .or_else(|| t.get("name"))
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sys_preview: String = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|msgs| {
                msgs.iter()
                    .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
                    .and_then(|m| m.get("content").and_then(Value::as_str))
            })
            .unwrap_or("")
            .chars()
            .take(220)
            .collect();
        let user_preview: String = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|msgs| msgs.iter().rev().find(|m| m.get("role").and_then(Value::as_str) == Some("user")))
            .and_then(|m| m.get("content").and_then(Value::as_str))
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        // 请求结构全貌：顶层字段 + 每条消息角色/长度 + tools 字节数。
        // 复现失败请求时「规模/工具都不是原因」，必须看到真实结构才能定位差异。
        let top_keys: Vec<String> = body
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let msg_shape: Vec<String> = body
            .get("messages")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|m| {
                        let role = m.get("role").and_then(Value::as_str).unwrap_or("?");
                        let n = match m.get("content") {
                            Some(Value::String(s)) => s.chars().count(),
                            Some(Value::Array(parts)) => parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .map(|s| s.chars().count())
                                .sum(),
                            _ => 0,
                        };
                        format!("{role}:{n}")
                    })
                    .collect()
            })
            .unwrap_or_default();
        let tools_bytes = serde_json::to_string(body.get("tools").unwrap_or(&Value::Null))
            .map(|s| s.len())
            .unwrap_or(0);
        let extra_fields: Vec<String> = top_keys
            .iter()
            .filter(|k| !["model", "messages", "tools", "tool_choice", "stream"].contains(&k.as_str()))
            .cloned()
            .collect();
        log_gateway_request(
            "qwen-flash",
            key_id.as_deref(),
            "proto-diag",
            true,
            0,
            &format!(
                "tools[{}]={} | topKeys={:?} extra={:?} toolsBytes={} | msgs=[{}] | sys({})={:?} | user={:?}",
                diag_tools,
                tool_names.join(","),
                top_keys,
                extra_fields,
                tools_bytes,
                msg_shape.join(" "),
                sys_preview.chars().count(),
                sys_preview,
                user_preview
            ),
        );
    }

    // 工具调用稳定性监控：提取本次请求的观测要素（在 body 被 move 之前）。
    let mon_had_tools = diag_tools > 0;
    let mon_temperature = body.get("temperature").and_then(Value::as_f64);

    match qoder::qwen_flash_chat_completions_unified(body).await {
        Ok(qoder::QoderChatOutput::Stream(resp, pre)) => {
            log_gateway_request("qwen-flash", key_id.as_deref(), &requested_model, true, started.elapsed().as_millis() as u64, "");
            toolcall_monitor::record(toolcall_monitor::ToolCallEvent {
                had_tools: mon_had_tools,
                // 流式路径不带 tools（带 tools 必走 SynthesizedJson），故 called=false
                called: false,
                tool_names: Vec::new(),
                latency_ms: started.elapsed().as_millis(),
                ok: true,
                error: String::new(),
                temperature: mon_temperature,
                channel: "qoder".to_string(),
                tier: "lite".to_string(),
                requested_model: requested_model.clone(),
            });
            let body = qoder_repaired_sse_body(resp, "Qwen3.8-Flash 上游流中断", pre);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                body,
            )
                .into_response()
        }
        Ok(qoder::QoderChatOutput::Json(value)) => {
            log_gateway_request("qwen-flash", key_id.as_deref(), &requested_model, true, started.elapsed().as_millis() as u64, "");
            toolcall_monitor::record(toolcall_monitor::ToolCallEvent {
                had_tools: mon_had_tools,
                called: false,
                tool_names: Vec::new(),
                latency_ms: started.elapsed().as_millis(),
                ok: true,
                error: String::new(),
                temperature: mon_temperature,
                channel: "qoder".to_string(),
                tier: "lite".to_string(),
                requested_model: requested_model.clone(),
            });
            json_ok(value)
        }
        Ok(qoder::QoderChatOutput::SynthesizedJson(value)) => {
            // 带 tools 的请求：服务端聚合 + 工具嗅探合成（流式语义以合成帧还原）
            log_gateway_request("qwen-flash", key_id.as_deref(), &requested_model, true, started.elapsed().as_millis() as u64, "");
            let mon_tool_names: Vec<String> = value
                .pointer("/choices/0/message/tool_calls")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            toolcall_monitor::record(toolcall_monitor::ToolCallEvent {
                had_tools: mon_had_tools,
                called: !mon_tool_names.is_empty(),
                tool_names: mon_tool_names,
                latency_ms: started.elapsed().as_millis(),
                ok: true,
                error: String::new(),
                temperature: mon_temperature,
                channel: "qoder".to_string(),
                tier: "lite".to_string(),
                requested_model: requested_model.clone(),
            });
            if want_stream {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                    axum::body::Body::from(qoder::synthesized_json_to_sse(&value)),
                )
                    .into_response()
            } else {
                json_ok(value)
            }
        }
        Err(e) => {
            // 空回复时把请求规模一并报出：超限上下文与额度受限的表象完全相同，
            // 没有规模信息无法判断该降上下文还是换账号。
            let size_note = format!(
                "[请求规模 chars={diag_chars} msgs={diag_msgs} tools={diag_tools}]"
            );
            let err_msg = if e.contains("账号池为空") {
                "Qwen3.8-Flash 账号池为空（Qoder CN 专用池）。请打开并登录 QoderCN 桌面端（本机登录态自动识别导入），或在 Qoder 页面用设备码添加 CN 账号后重试。".to_string()
            } else if e.contains("空回复") {
                format!("Qwen3.8-Flash 算力调度异常：{e} {size_note}")
            } else {
                format!("Qwen3.8-Flash 算力调度异常：{e}")
            };
            log_gateway_request("qwen-flash", key_id.as_deref(), &requested_model, false, started.elapsed().as_millis() as u64, &err_msg);
            toolcall_monitor::record(toolcall_monitor::ToolCallEvent {
                had_tools: mon_had_tools,
                called: false,
                tool_names: Vec::new(),
                latency_ms: started.elapsed().as_millis(),
                ok: false,
                error: e.clone(),
                temperature: mon_temperature,
                channel: "qoder".to_string(),
                tier: "lite".to_string(),
                requested_model: requested_model.clone(),
            });
            glm_proxy_error(err_msg, StatusCode::BAD_GATEWAY)
        }
    }
}

/// 输入 token 预算（Qoder lite 档实测上限约 12.8 万 tokens，见阶梯实测：
/// 8 万中文字符通过、9 万即空回复）。留出工具 schema 与输出余量。
const QWEN_INPUT_TOKEN_BUDGET: usize = 100_000;

/// 保守估算一段文本的 token 数。
///
/// 客户端（WorkBuddy 等 IDE）常用「约 4 字符/token」的英式启发式，对中文
/// 严重低估（中文约 1–2 token/字符），于是它以为没超限、不裁剪，上游收到
/// 超限请求后返回 **HTTP 200 空正文**——表象与额度耗尽一模一样。
/// 这里按 1.6 token/字符的保守值估算，宁可裁早一点也不要撞墙。
fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    // 逐字符判定：CJK 按 1.6 token，其余按 0.3 token（英文/JSON 偏松）。
    let mut tokens = 0usize;
    for c in text.chars() {
        let is_cjk = ('\u{4E00}'..='\u{9FFF}').contains(&c)
            || ('\u{3000}'..='\u{303F}').contains(&c)
            || ('\u{FF00}'..='\u{FFEF}').contains(&c);
        tokens += if is_cjk { 16 } else { 3 };
    }
    let _ = chars;
    tokens / 10
}

/// 消息数组的总估算 token 数。
fn messages_tokens(msgs: &[Value]) -> usize {
    msgs.iter()
        .map(|m| match m.get("content") {
            Some(Value::String(s)) => estimate_tokens(s),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .map(estimate_tokens)
                .sum(),
            _ => 0,
        })
        .sum()
}

/// 上下文守卫：估算输入超预算时，从最旧的非 system 消息开始裁剪，
/// 始终保留 system 消息与最新的用户消息。
///
/// 返回 (是否裁剪, 裁剪前估算 tokens, 裁剪后估算 tokens)。
/// 只在确实超预算时改动 body；tools 数组不计入 messages，但其 schema 也占
/// token，因此预算已扣除一份固定余量。
fn guard_context_budget(body: &mut Value) -> (bool, usize, usize) {
    let Some(msgs) = body.get("messages").and_then(Value::as_array).cloned() else {
        return (false, 0, 0);
    };
    if msgs.is_empty() {
        return (false, 0, 0);
    }
    let tools_tokens = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| estimate_tokens(&serde_json::to_string(t).unwrap_or_default()))
        .unwrap_or(0);
    let before = messages_tokens(&msgs) + tools_tokens;
    if before <= QWEN_INPUT_TOKEN_BUDGET {
        return (false, before, before);
    }

    // 从最旧开始丢（system 与最后一条永远保留）。
    let last_idx = msgs.len() - 1;
    let mut kept: Vec<Value> = Vec::with_capacity(msgs.len());
    // 先放最后一条，再按从新到旧的顺序补，直到预算内。
    kept.push(msgs[last_idx].clone());
    let mut used = estimate_tokens_of_message(&msgs[last_idx]);
    for m in msgs[..last_idx].iter().rev() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let t = estimate_tokens_of_message(m);
        if role != "system" && used + t + tools_tokens > QWEN_INPUT_TOKEN_BUDGET {
            continue; // 丢这条最旧的
        }
        used += t;
        kept.push(m.clone());
    }
    kept.reverse();
    // 若连 system + 最新都超（单条巨大消息），截断最新消息的正文。
    if used + tools_tokens > QWEN_INPUT_TOKEN_BUDGET {
        if let Some(last) = kept.last_mut().and_then(Value::as_object_mut) {
            if let Some(Value::String(s)) = last.get_mut("content") {
                let keep_chars = (QWEN_INPUT_TOKEN_BUDGET.saturating_sub(tools_tokens)) * 10 / 16;
                let truncated: String = s.chars().take(keep_chars).collect();
                *s = truncated;
            }
        }
    }
    if let Some(arr) = body.get_mut("messages").and_then(Value::as_array_mut) {
        *arr = kept.clone();
    }
    let after = messages_tokens(&kept) + tools_tokens;
    (true, before, after)
}

/// 单条消息的估算 token 数（与 messages_tokens 同规则）。
fn estimate_tokens_of_message(m: &Value) -> usize {
    match m.get("content") {
        Some(Value::String(s)) => estimate_tokens(s),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .map(estimate_tokens)
            .sum(),
        _ => 0,
    }
}

/// Qoder 试调（服务端转发非流式请求并聚合，浏览器无需直连 /qoder/v1）。
async fn api_qoder_test(Json(body): Json<Value>) -> Response {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("lite").to_string();
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("只回复：OK")
        .trim()
        .to_string();
    if prompt.is_empty() {
        return json_err("请输入测试内容".to_string(), StatusCode::BAD_REQUEST);
    }
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    });
    let started = std::time::Instant::now();
    let mut resp = match qoder::chat_completions(payload).await {
        Ok(resp) => resp,
        Err(e) => return json_err(e, StatusCode::BAD_GATEWAY),
    };
    // 聚合 SSE 全文，提取最终 content。
    // 严格 from_utf8 失败会整块丢弃（TCP 切点落在中文等多字节字符中间），
    // 用跨 chunk 的 carry 保证字节不丢——同 glm_anthropic::decode_stream_chunk。
    let mut sse_text = String::new();
    let mut carry: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        sse_text.push_str(&wb_switch_core::modules::glm_anthropic::decode_stream_chunk(
            &mut carry,
            &chunk,
        ));
        if sse_text.contains("[DONE]") {
            break;
        }
    }
    if !carry.is_empty() {
        sse_text.push_str(&String::from_utf8_lossy(&carry));
    }
    let mut content = String::new();
    let mut model_out = model.clone();
    for line in sse_text.lines() {
        let Some(payload) = line.strip_prefix("data: ") else { continue };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" { continue; }
        if let Ok(v) = serde_json::from_str::<Value>(payload) {
            if let Some(m) = v.get("model").and_then(Value::as_str) { model_out = m.to_string(); }
            if let Some(delta) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                content.push_str(delta);
            }
            if let Some(message) = v.pointer("/choices/0/message/content").and_then(Value::as_str) {
                content.push_str(message);
            }
        }
    }
    json_ok(json!({
        "ok": true,
        "model": model_out,
        "content": content,
        "latencyMs": started.elapsed().as_millis() as u64,
    }))
}

async fn api_qoder_import_manual(Json(body): Json<Value>) -> Response {
    let token = body.get("accessToken").and_then(Value::as_str).unwrap_or_default().to_string();
    let refresh = body.get("refreshToken").and_then(Value::as_str).unwrap_or_default().to_string();
    let uid = body.get("uid").and_then(Value::as_str).unwrap_or_default().to_string();
    // 池归属：缺省 qoder；传 "qwen" 时账号只服务 /qwen/v1 独立池
    let gateway = body.get("gateway").and_then(Value::as_str).unwrap_or("qoder");
    match qoder::import_manual(&uid, &token, &refresh, gateway) {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// 导入本机 QoderCN（中国版 IDE）登录态 → Qoder CN 专用池（`/qwen-flash/v1` 凭据来源）。
async fn api_qoder_import_local_cn() -> Response {
    match qoder::import_local_cn() {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// 导入本机 **QoderWork CN**（独立 Electron 应用，`%APPDATA%\QoderWork CN\auth-v2.dat`）登录态。
/// 自动打 `gateway = "qwen3.8-flash"` 独立池标 + `region = "cn"` + per-account
/// `endpoint = https://gateway.qoder.com.cn`（其实测只有该主机接受其 dt-）。
async fn api_qoder_import_local_work_cn() -> Response {
    match qoder::import_local_work_cn() {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// 手动指定账号地域（防误判）：`region` ∈ {"cn","intl","auto"}，auto 清除手动指定。
async fn api_qoder_set_region(Json(body): Json<Value>) -> Response {
    let Some(uid) = body.get("uid").and_then(Value::as_str) else {
        return json_err("缺少 uid".to_string(), StatusCode::BAD_REQUEST);
    };
    let Some(region) = body.get("region").and_then(Value::as_str) else {
        return json_err("缺少 region".to_string(), StatusCode::BAD_REQUEST);
    };
    match qoder::set_account_region(uid, region) {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_qoder_accounts_list() -> Response {
    json_ok(json!({ "accounts": qoder::list_for_display() }))
}

async fn api_qoder_import_local() -> Response {
    match qoder::import_local() {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_qoder_remove_account(Json(body): Json<Value>) -> Response {
    let Some(uid) = body.get("uid").and_then(Value::as_str) else {
        return json_err("缺少 uid".to_string(), StatusCode::BAD_REQUEST);
    };
    if qoder::remove_account(uid) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_qoder_refresh_account(Json(body): Json<Value>) -> Response {
    let Some(uid) = body.get("uid").and_then(Value::as_str) else {
        return json_err("缺少 uid".to_string(), StatusCode::BAD_REQUEST);
    };
    match qoder::refresh_stored_account(uid).await {
        Ok(_) => json_ok(json!({ "ok": true })),
        Err(e) => json_err(format!("刷新失败：{e}"), StatusCode::BAD_GATEWAY),
    }
}

async fn api_qoder_device_flow_start(body: axum::body::Bytes) -> Response {
    // body 可为空（旧前端不传）；非空时解析 gateway，决定授权账号入哪个池
    let gateway = if body.is_empty() {
        "qoder".to_string()
    } else {
        serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("gateway")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|g| !g.is_empty())
            .unwrap_or_else(|| "qoder".to_string())
    };
    match qoder::device_flow_start(&gateway) {
        Ok(session) => json_ok(json!({ "ok": true, "session": session })),
        Err(e) => json_err(e, StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_qoder_device_flow_complete(Json(body): Json<Value>) -> Response {
    let Some(session) = body.get("session") else {
        return json_err("缺少 session".to_string(), StatusCode::BAD_REQUEST);
    };
    match qoder::device_flow_complete(session).await {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

async fn api_trae_accounts_list() -> Response {
    json_ok(json!({ "accounts": trae_accounts::list_for_display() }))
}

async fn api_trae_accounts_import_local() -> Response {
    match trae_accounts::import_local_snapshot() {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// T4：手动添加 TRAE 池条目（凭据由用户粘贴；无设备密钥对，续期不可用）。
async fn api_trae_accounts_add(Json(body): Json<Value>) -> Response {
    let input = trae_accounts::ManualAccountInput {
        label: body
            .get("label")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        access_token: body
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        refresh_token: body
            .get("refreshToken")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        device_id: body
            .get("deviceId")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        machine_id: body
            .get("machineId")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
    };
    match trae_accounts::add_manual_account(input) {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_trae_accounts_remove(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    if trae_accounts::remove_account(id) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_trae_accounts_refresh(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    match trae_accounts::refresh_stored_account(id).await {
        Ok(_) => json_ok(json!({ "ok": true })),
        Err(e) => json_err(format!("刷新失败：{e}"), StatusCode::BAD_GATEWAY),
    }
}

async fn api_trae_checkin_all() -> Response {
    json_ok(json!({ "accounts": platform_checkin::trae_checkin_pool().await }))
}

async fn api_trae_checkin_one(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(platform_checkin::trae_checkin_pool_account_by_id(id).await)
}

// ---------------------------------------------------------------------------
// Loomy（讯飞）
// ---------------------------------------------------------------------------

async fn api_loomy_status() -> Response {
    let local = loomy::read_local_session().is_ok();
    json_ok(json!({
        "localLogin": local,
        "dataDir": loomy::auth_session_path().map(|p| p.to_string_lossy().to_string()),
        "apiHost": "https://loomyad.xunfei.cn",
    }))
}

async fn api_loomy_accounts_list() -> Response {
    let mut accounts = loomy::list_for_display();
    // 本机实时账号顺带拉一次积分（其余池条目按需刷新，避免批量打网关）。
    if let Some(first) = accounts.first_mut() {
        if first.get("source").and_then(Value::as_str) == Some("local-live") {
            let credits = loomy::credits_local().await;
            first["credit"] = credits;
        }
    }
    json_ok(json!({ "accounts": accounts }))
}

async fn api_loomy_accounts_add(Json(body): Json<Value>) -> Response {
    let Some(session) = body.get("session").and_then(Value::as_str) else {
        return json_err("缺少 session".to_string(), StatusCode::BAD_REQUEST);
    };
    let label = body.get("label").and_then(Value::as_str).map(String::from);
    match loomy::add_account(label, session).await {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_loomy_accounts_import_local() -> Response {
    match loomy::import_local().await {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_loomy_accounts_remove(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    if loomy::remove_account(id) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

async fn api_loomy_credits(RawQuery(query): RawQuery) -> Response {
    if query_flag_enabled(query.as_deref(), "pool") {
        // 逐池条目查积分（Loomy 页面账号池刷新按钮）。
        let mut out = Vec::new();
        for account in loomy::load_accounts() {
            let Some(id) = account.get("id").and_then(Value::as_str).map(String::from) else {
                continue;
            };
            let credits = loomy::credits_stored(&id).await;
            out.push(json!({
                "id": id,
                "label": account.get("label").cloned().unwrap_or(Value::Null),
                "credit": credits,
            }));
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        return json_ok(json!({ "accounts": out }));
    }
    json_ok(loomy::credits_local().await)
}

async fn api_loomy_checkin_all() -> Response {
    // 本机账号 + 池条目逐个校验登录态（每日积分随登录自动发放）。
    let mut results = Vec::new();
    if let Some(entry) = loomy::discover_loomy_account() {
        let status = platform_checkin::get_loomy_status().await;
        results.push(json!({
            "id": entry.get("id").cloned().unwrap_or(Value::Null),
            "label": "本机登录",
            "result": if status.get("ok").and_then(Value::as_bool) == Some(true) { "already" } else { "error" },
            "error": status.get("error").cloned().unwrap_or(Value::Null),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    for account in loomy::load_accounts() {
        if loomy::hidden_by_local_login(&account) {
            continue;
        }
        if account.get("status").and_then(Value::as_str) == Some("invalid") {
            continue;
        }
        let Some(id) = account.get("id").and_then(Value::as_str).map(String::from) else {
            continue;
        };
        let res = loomy::checkin_stored(&id).await;
        results.push(json!({
            "id": id,
            "label": account.get("label").cloned().unwrap_or(Value::Null),
            "result": res.get("result").cloned().unwrap_or(json!("error")),
            "error": res.get("error").cloned().unwrap_or(Value::Null),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    json_ok(json!({ "accounts": results }))
}

async fn api_loomy_checkin_one(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    // 本机账号（id == loomy:local 或与发现 ID 相同）实时校验。
    let local_id = loomy::local_discovery_id();
    if id == "loomy:local" || local_id.as_deref() == Some(id) {
        let status = platform_checkin::get_loomy_status().await;
        return json_ok(json!({
            "id": id,
            "result": if status.get("ok").and_then(Value::as_bool) == Some(true) { "already" } else { "error" },
            "error": status.get("error").cloned().unwrap_or(Value::Null),
        }));
    }
    json_ok(loomy::checkin_stored(id).await)
}

async fn api_loomy_proxy_config_get() -> Response {
    let cfg = loomy_proxy::load_config();
    json_ok(json!({
        "enabled": cfg.enabled,
        "apiKey": cfg.api_key,
        "endpoint": "/loomy/v1/chat/completions",
        "modelsEndpoint": "/loomy/v1/models",
    }))
}

async fn api_loomy_proxy_config_save(Json(body): Json<Value>) -> Response {
    let mut cfg = loomy_proxy::load_config();
    if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if let Some(api_key) = body.get("api_key").and_then(Value::as_str) {
        let trimmed = api_key.trim();
        if !trimmed.is_empty() {
            cfg.api_key = trimmed.to_string();
        }
    }
    match loomy_proxy::save_config(&cfg) {
        Ok(()) => json_ok(json!({ "ok": true })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_loomy_proxy_status() -> Response {
    json_ok(loomy_proxy::proxy_status().await)
}

/// Loomy 模型目录（页面展示用，无需代理密钥；完整字段原样透传）。
async fn api_loomy_models_catalog() -> Response {
    match loomy_proxy::fetch_models().await {
        Ok(value) => json_ok(value),
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

fn loomy_proxy_auth_ok(headers: &HeaderMap, cfg: &loomy_proxy::LoomyProxyConfig) -> bool {
    if cfg.api_key.is_empty() {
        return true;
    }
    let bearer_ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|k| k == cfg.api_key)
        .unwrap_or(false);
    let x_api_key_ok = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|k| k == cfg.api_key)
        .unwrap_or(false);
    bearer_ok || x_api_key_ok
}

/// Loomy 对话补全（OpenAI 兼容；鉴权复用 GLM 代理密钥风格）。
async fn api_loomy_chat_completions(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let cfg = loomy_proxy::load_config();
    if !cfg.enabled {
        return glm_proxy_error(
            "Loomy 代理未启用，请在设置中开启".to_string(),
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
    if !loomy_proxy_auth_ok(&headers, &cfg) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match loomy_proxy::chat_completions(body).await {
        Ok(loomy_proxy::LoomyChatCompletion::Json(value)) => json_ok(value),
        Ok(loomy_proxy::LoomyChatCompletion::Stream(upstream)) => {
            let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, upstream.content_type)],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(e) => glm_proxy_error(e, StatusCode::BAD_GATEWAY),
    }
}

/// Loomy 模型列表（OpenAI /v1/models 格式透传，缓存 10 分钟）。
async fn api_loomy_v1_models(headers: HeaderMap) -> Response {
    let cfg = loomy_proxy::load_config();
    if !loomy_proxy_auth_ok(&headers, &cfg) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match loomy_proxy::models_payload().await {
        Ok(value) => json_ok(value),
        Err(e) => glm_proxy_error(e, StatusCode::BAD_GATEWAY),
    }
}

// ---------------------------------------------------------------------------
// 妙手（美团智能体）
//
// 该平台无每日签到功能：checkin-all / checkin-one 与其他平台接口形状一致，
// 统一返回 `{"result":"unsupported","message":"妙手无每日签到功能，积分按月自动刷新"}`，
// 前端无需特殊处理。管理面端点（无网关 Key 鉴权，与 /api/qoder/accounts 同范式）。
// ---------------------------------------------------------------------------

/// 妙手账号列表：本机客户端登录态 + 已导入的池账号。
async fn api_miaoshou_accounts_list() -> Response {
    json_ok(json!({ "accounts": platform_checkin::discover_miaoshou_accounts() }))
}

/// 导入本机妙手（`%APPDATA%\catpaw-moon`）登录态。
async fn api_miaoshou_import_local() -> Response {
    match miaoshou::import_local().await {
        Ok(account) => json_ok(json!({ "ok": true, "account": account })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// 移除已导入的妙手账号（按 uid）。
async fn api_miaoshou_remove_account(Json(body): Json<Value>) -> Response {
    let Some(uid) = body.get("uid").and_then(Value::as_str) else {
        return json_err("缺少 uid".to_string(), StatusCode::BAD_REQUEST);
    };
    if miaoshou::remove_account(uid) {
        json_ok(json!({ "ok": true }))
    } else {
        json_err("账号不存在".to_string(), StatusCode::NOT_FOUND)
    }
}

/// 妙手全账号状态（含积分）。无每日签到：todayCheckedIn 为 null。
async fn api_miaoshou_status() -> Response {
    let mut items = Vec::new();
    for account in platform_checkin::discover_miaoshou_accounts() {
        let mut status = platform_checkin::get_miaoshou_status(&account).await;
        let account_id = account.get("id").cloned().unwrap_or(Value::Null);
        // 🔴 必须双写 id 与 accountId：前端 MiaoshouPage 用 `s.id` 建
        // statusById Map（types.ts 的 MiaoshouStatusItem.id），早期只写
        // accountId 导致 join 全部未命中——账号行的积分徽标/余额/异常徽标
        // 永不渲染，是"界面灰色、不同步"最直接的界面症状（2026-09-21）。
        status["id"] = account_id.clone();
        status["accountId"] = account_id;
        status["email"] = json!(account::account_display_name(&account));
        status["platform"] = json!(platform_checkin::PLATFORM_MIAOSHOU);
        status["localLogin"] = json!(platform_checkin::is_local_login_account(&account));
        status["credit"] = platform_checkin::get_miaoshou_credits(&account).await;
        items.push(status);
    }
    json_ok(json!({
        "installed": platform_checkin::miaoshou_desktop_installed(),
        "accounts": items,
    }))
}

/// 妙手「一键签到」：逐账号返回 unsupported 提示（该平台无每日签到）。
async fn api_miaoshou_checkin_all() -> Response {
    let mut results = Vec::new();
    for account in platform_checkin::discover_miaoshou_accounts() {
        let res = platform_checkin::perform_miaoshou_checkin().await;
        results.push(json!({
            "id": account.get("id").cloned().unwrap_or(Value::Null),
            "label": account.get("nickname").cloned().unwrap_or(Value::Null),
            "result": res.get("result").cloned().unwrap_or(json!("error")),
            "message": res.get("message").cloned().unwrap_or(Value::Null),
            "error": res.get("error").cloned().unwrap_or(Value::Null),
        }));
    }
    json_ok(json!({ "accounts": results }))
}

/// 妙手单账号「签到」：同上，unsupported 提示。
async fn api_miaoshou_checkin_one(Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    };
    let res = platform_checkin::perform_miaoshou_checkin().await;
    json_ok(json!({
        "id": id,
        "result": res.get("result").cloned().unwrap_or(json!("error")),
        "message": res.get("message").cloned().unwrap_or(Value::Null),
        "error": res.get("error").cloned().unwrap_or(Value::Null),
    }))
}

/// 妙手积分汇总（按月刷新的额度余额）。
async fn api_miaoshou_credits() -> Response {
    let mut out = Vec::new();
    for account in platform_checkin::discover_miaoshou_accounts() {
        let credits = platform_checkin::get_miaoshou_credits(&account).await;
        out.push(json!({
            "id": account.get("id").cloned().unwrap_or(Value::Null),
            "label": account.get("nickname").cloned().unwrap_or(Value::Null),
            "credit": credits,
        }));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    json_ok(json!({ "accounts": out }))
}

// ---------------------------------------------------------------------------
// 妙手反代（/miaoshou/v1）：OpenAI 兼容对话（create → agent-stream → SSE/history）
// ---------------------------------------------------------------------------

/// `/miaoshou/v1/models`：妙手可用模型目录。
/// 妙手桌面端模型目录（webui 前端用；不经网关 API Key 鉴权，走 webui 会话）。
///
/// 与 `/miaoshou/v1/models` 的区别：那个面向外部 OpenAI 兼容客户端（需网关
/// 密钥），且 models_payload_async 有 10 分钟缓存；webui 需要"点刷新就看
/// 到最新目录"，因此这里用 `force` 查询参数绕过缓存直拉上游。
/// 妙手反代试调（webui「试一试」用）：与 `/miaoshou/v1/chat/completions`
/// 同一套链路，但走 webui 会话鉴权（不要求外部客户端带网关 API Key）。
async fn api_miaoshou_chat(Json(body): Json<Value>) -> Response {
    let started = std::time::Instant::now();
    // 取账号池第一个可用账号（复用网关链路的同一函数）
    let account = match first_miaoshou_account() {
        Ok(a) => a,
        Err(e) => return json_err(e, StatusCode::BAD_REQUEST),
    };
    match miaoshou::chat_completions(&account, &body).await {
        Ok(value) => {
            log_gateway_request(
                "miaoshou",
                None,
                body.get("model").and_then(Value::as_str).unwrap_or("—"),
                true,
                started.elapsed().as_millis() as u64,
                "",
            );
            json_ok(value)
        }
        Err(e) => {
            log_gateway_request(
                "miaoshou",
                None,
                body.get("model").and_then(Value::as_str).unwrap_or("—"),
                false,
                started.elapsed().as_millis() as u64,
                &e,
            );
            json_err(format!("妙手试调失败：{e}"), StatusCode::BAD_GATEWAY)
        }
    }
}

async fn api_miaoshou_models(RawQuery(query): RawQuery) -> Response {
    let force = query
        .as_deref()
        .map(|q| q.split('&').any(|p| p == "force=1" || p == "force"))
        .unwrap_or(false);
    if force {
        // 绕过 10 分钟缓存：直拉桌面端目录（用户点"刷新"时用）
        let payload = match miaoshou::fetch_models_payload_fresh().await {
            Some(v) => v,
            None => {
                return json_err(
                    "拉取妙手桌面端模型目录失败：请确认已导入本机登录态且桌面端在线".to_string(),
                    StatusCode::BAD_GATEWAY,
                )
            }
        };
        return json_ok(payload);
    }
    json_ok(miaoshou::models_payload_async().await)
}

async fn api_miaoshou_v1_models(headers: HeaderMap) -> Response {
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("miaoshou", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    log_gateway_request("miaoshou", key_id.as_deref(), "models", true, 0, "");
    json_ok(miaoshou::models_payload_async().await)
}

/// 取第一个可用妙手账号（池为空返回 Err）。
fn first_miaoshou_account() -> Result<Value, String> {
    platform_checkin::discover_miaoshou_accounts()
        .into_iter()
        .next()
        .ok_or_else(|| "妙手账号池为空：请先在「妙手」页导入本机登录态".to_string())
}

/// `/miaoshou/v1/chat/completions`：非流式走 history 聚合；流式走 SSE 转换泵。
async fn api_miaoshou_v1_chat_completions(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    let (auth_ok, key_id) = gateway_key_status(&headers);
    if !auth_ok {
        log_gateway_request("miaoshou", key_id.as_deref(), "—", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let account = match first_miaoshou_account() {
        Ok(a) => a,
        Err(e) => {
            log_gateway_request("miaoshou", key_id.as_deref(), "—", false, 0, &e);
            return glm_proxy_error(e, StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(miaoshou::DEFAULT_MODEL)
        .to_string();

    if want_stream {
        // 流式：建会话 → 发起生成 → SSE 连接，帧转换后透传
        let token = match account.get("accessToken").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                let e = "妙手账号缺少 accessToken（请重新导入登录态）".to_string();
                log_gateway_request("miaoshou", key_id.as_deref(), &model, false, 0, &e);
                return glm_proxy_error(e, StatusCode::SERVICE_UNAVAILABLE);
            }
        };
        let prompt = miaoshou::flatten_messages(
            body.get("messages")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or(&[]),
        );
        let chat_id = match miaoshou::create_chat(&token, &prompt).await {
            Ok(id) => id,
            Err(e) => {
                log_gateway_request("miaoshou", key_id.as_deref(), &model, false, started.elapsed().as_millis() as u64, &e);
                return glm_proxy_error(e, StatusCode::BAD_GATEWAY);
            }
        };
        let conv = match miaoshou::kick_agent_stream(&token, &chat_id, &prompt, &model).await {
            Ok(c) => c,
            Err(e) => {
                log_gateway_request("miaoshou", key_id.as_deref(), &model, false, started.elapsed().as_millis() as u64, &e);
                return glm_proxy_error(e, StatusCode::BAD_GATEWAY);
            }
        };
        match miaoshou::stream_connect(&token, &conv).await {
            Ok(resp) => {
                log_gateway_request("miaoshou", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
                let sse = miaoshou_sse_to_openai(resp, &model);
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                    sse,
                )
                    .into_response()
            }
            Err(e) => {
                log_gateway_request("miaoshou", key_id.as_deref(), &model, false, started.elapsed().as_millis() as u64, &e);
                glm_proxy_error(e, StatusCode::BAD_GATEWAY)
            }
        }
    } else {
        match miaoshou::chat_completions(&account, &body).await {
            Ok(value) => {
                log_gateway_request("miaoshou", key_id.as_deref(), &model, true, started.elapsed().as_millis() as u64, "");
                json_ok(value)
            }
            Err(e) => {
                log_gateway_request("miaoshou", key_id.as_deref(), &model, false, started.elapsed().as_millis() as u64, &e);
                glm_proxy_error(e, StatusCode::BAD_GATEWAY)
            }
        }
    }
}

/// 妙手 SSE → OpenAI chunk 转换泵。
///
/// 上游帧（data: 后为 JSON）：
/// - `{"headlessRemoteAgentResp":{"isHistoryMessage":true,...}}` 历史回放（跳过）
/// - `{"currentStatus":"running"}` 状态（跳过）
/// - `{"headlessRemoteAgentResp":{"message":{"type":"assistant","content":[{"type":"text","text":"增量"}]}}}` 正文增量
/// - `{"statusUpdate":"completed|canceled|user_stopped|error|failed|terminated"}` 终止帧
fn miaoshou_sse_to_openai(
    resp: wb_switch_core::modules::qoder::UpstreamResponse,
    model: &str,
) -> axum::body::Body {
    let id = format!("chatcmpl-miaoshou-{}", wb_switch_core::modules::config::now_ms());
    let model = model.to_string();
    // 状态：行缓冲 / UTF-8 carry / 历史已下发 / 历史 finished / 见过增量 / 最近活动 / 已收尾
    let stream = futures_util::stream::unfold(
        (
            resp,
            String::new(),
            Vec::<u8>::new(),
            id,
            model,
            false,
            false,
            false,
            std::time::Instant::now(),
            false,
        ),
        |(mut resp, mut line_buf, mut carry, id, model, mut hist_emitted, mut hist_finished, mut saw_delta, mut last_activity, mut done)| async move {
            if done {
                return None;
            }
            // 构造 OpenAI chunk 帧
            let mk = |delta: Value, finish: Value, id: &str, model: &str| -> String {
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": id, "object": "chat.completion.chunk",
                        "created": wb_switch_core::modules::config::now_ms() / 1000,
                        "model": model,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                    })
                )
            };
            loop {
                // 静默收尾：历史已下发且 finished、5s 无增量 → 终止
                // （妙手对已完成会话只回放历史后挂起，永不发 statusUpdate）
                if hist_emitted && hist_finished && !saw_delta
                    && last_activity.elapsed() >= std::time::Duration::from_secs(5)
                {
                    let out = format!(
                        "{}\n\ndata: [DONE]\n\n",
                        mk(json!({}), json!("stop"), &id, &model)
                    );
                    return Some((Ok::<_, String>(axum::body::Bytes::from(out)), (resp, line_buf, carry, id, model, hist_emitted, hist_finished, saw_delta, last_activity, true)));
                }
                // 吐完行缓冲里的完整行
                while let Some(pos) = line_buf.find('\n') {
                    let line: String = line_buf.drain(..=pos).collect();
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    let Some(payload) = trimmed.strip_prefix("data:").map(|p| p.trim()) else { continue };
                    let payload = payload.trim();
                    if payload.is_empty() {
                        continue;
                    }
                    let Ok(frame) = serde_json::from_str::<Value>(payload) else { continue };
                    last_activity = std::time::Instant::now();

                    // 终止帧
                    if let Some(status) = frame.get("statusUpdate").and_then(Value::as_str) {
                        let out = format!(
                            "{}\n\ndata: [DONE]\n\n",
                            mk(json!({}), json!("stop"), &id, &model)
                        );
                        return Some((Ok::<_, String>(axum::body::Bytes::from(out)), (resp, line_buf, carry, id, model, hist_emitted, hist_finished, saw_delta, last_activity, true)));
                    }

                    let agent = match frame.pointer("/headlessRemoteAgentResp") {
                        Some(a) => a.clone(),
                        None => continue,
                    };

                    // 历史回放：提取 assistant 全文一次性下发
                    if agent.get("isHistoryMessage").and_then(Value::as_bool) == Some(true) {
                        if let Some(msgs) = agent.get("historyMessages").and_then(Value::as_array) {
                            let mut text = String::new();
                            let mut finished = false;
                            for m in msgs {
                                if m.get("type").and_then(Value::as_str) != Some("assistant") {
                                    continue;
                                }
                                if m.get("finished").and_then(Value::as_bool) == Some(true) {
                                    finished = true;
                                }
                                if let Some(parts) = m.get("content").and_then(Value::as_array) {
                                    for p in parts {
                                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                                            text.push_str(t);
                                        }
                                    }
                                }
                            }
                            if !text.is_empty() && !hist_emitted {
                                hist_emitted = true;
                                hist_finished = finished;
                                let out = mk(json!({"content": text}), Value::Null, &id, &model);
                                return Some((Ok::<_, String>(axum::body::Bytes::from(out)), (resp, line_buf, carry, id, model, hist_emitted, hist_finished, saw_delta, last_activity, false)));
                            }
                        }
                        continue;
                    }

                    // 实时增量
                    if let Some(text) = agent
                        .pointer("/message/content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .filter(|t| !t.is_empty())
                    {
                        saw_delta = true;
                        let out = mk(json!({"content": text}), Value::Null, &id, &model);
                        return Some((Ok::<_, String>(axum::body::Bytes::from(out)), (resp, line_buf, carry, id, model, hist_emitted, hist_finished, saw_delta, last_activity, false)));
                    }
                }
                // 行缓冲空了 → 读下一块（短超时以便静默收尾判断运转）
                match tokio::time::timeout(std::time::Duration::from_secs(6), resp.chunk()).await {
                    Ok(Ok(Some(chunk))) => {
                        let text = wb_switch_core::modules::glm_anthropic::decode_stream_chunk(
                            &mut carry,
                            &chunk,
                        );
                        line_buf.push_str(&text);
                    }
                    Ok(Ok(None)) => {
                        let out = format!(
                            "{}\n\ndata: [DONE]\n\n",
                            mk(json!({}), json!("stop"), &id, &model)
                        );
                        return Some((Ok::<_, String>(axum::body::Bytes::from(out)), (resp, line_buf, carry, id, model, hist_emitted, hist_finished, saw_delta, last_activity, true)));
                    }
                    Ok(Err(e)) => {
                        let out = format!(
                            "data: {}\n\ndata: [DONE]\n\n",
                            json!({"error": {"message": format!("妙手上游流中断: {e}"), "type": "upstream_error"}})
                        );
                        return Some((Ok::<_, String>(axum::body::Bytes::from(out)), (resp, line_buf, carry, id, model, hist_emitted, hist_finished, saw_delta, last_activity, true)));
                    }
                    Err(_) => {
                        // 轮询超时：回到循环顶部（让静默收尾判断运转）
                    }
                }
            }
        },
    );
    axum::body::Body::from_stream(stream)
}

// ---------------------------------------------------------------------------
// Grok SSO 自动同步（专用浏览器 + CDP）
// ---------------------------------------------------------------------------

async fn api_grok_sso_status() -> Response {
    let mut status = grok_sso::token_status();
    if let Some(obj) = status.as_object_mut() {
        obj.insert("toolVersion".to_string(), json!(grok_sso::VERSION));
        obj.insert("config".to_string(), json!({
            "enabled": grok_sso::load_config().enabled,
            "syncIntervalMinutes": 30,
        }));
    }
    json_ok(status)
}

async fn api_grok_sso_sync() -> Response {
    // 手动同步：重置「已被接管」标记，把当前浏览器登录态写回 autosync 账号。
    json_ok(grok_sso::run_sync_manual().await)
}

async fn api_grok_sso_open_login() -> Response {
    json_ok(grok_sso::open_login_window().await)
}

// ---------------------------------------------------------------------------
// 注册机（Grok 账号注册专用 Chrome 实例；仅手动触发，零自动）
// ---------------------------------------------------------------------------

/// 只读状态探测：不启动浏览器、不触发注册。
async fn api_grok_register_status() -> Response {
    json_ok(grok_register::status())
}

/// 手动打开注册机：用户点击入口是唯一启动方式。
async fn api_grok_register_open() -> Response {
    json_ok(grok_register::open().await)
}

/// 抓取注册成果：以固定 ID `grok_register` upsert 写入账号池。
async fn api_grok_register_capture() -> Response {
    json_ok(grok_register::capture().await)
}

/// 关闭注册机：只结束命令行带 grok-chrome-profile 的 Chrome 进程。
async fn api_grok_register_close() -> Response {
    json_ok(grok_register::close().await)
}

// ---------------------------------------------------------------------------
// ChatGPT 通道（gpt-reg-review 工具链托管+控制；handler 薄封装，逻辑在 core
// chatgpt_channel 模块。管理面免 key——服务仅绑 127.0.0.1，契约 §4）
// ---------------------------------------------------------------------------

async fn api_chatgpt_channel_status() -> Response {
    json_ok(chatgpt_channel::status_value().await)
}

async fn api_chatgpt_webui_start() -> Response {
    let cfg = chatgpt_channel::load_config();
    match chatgpt_channel::ensure_webui(&cfg).await {
        Ok(port) => json_ok(json!({ "ok": true, "port": port })),
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

async fn api_chatgpt_webui_stop() -> Response {
    let cfg = chatgpt_channel::load_config();
    // 批量注册进行中停止 WebUI 会打断注册循环并释放 in_use 号——core 内已先
    // auto_stop 尽力而为，前端应在停止前二次确认（契约 §3 ⚠️）。
    let stopped = chatgpt_channel::stop_webui(&cfg).await;
    json_ok(json!({ "ok": stopped, "stopped": stopped }))
}

/// 批量注册启动：body（AutoLoopStartReq）透传。target_count 必填 ≥1
/// （0 = 不限量会无限跑），由前端保证；工具链已在跑时回 400 透传。
async fn api_chatgpt_registrar_start(Json(body): Json<Value>) -> Response {
    let cfg = chatgpt_channel::load_config();
    match chatgpt_channel::auto_start(&cfg, body).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

async fn api_chatgpt_registrar_pause() -> Response {
    let cfg = chatgpt_channel::load_config();
    match chatgpt_channel::auto_pause(&cfg).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

async fn api_chatgpt_registrar_resume() -> Response {
    let cfg = chatgpt_channel::load_config();
    match chatgpt_channel::auto_resume(&cfg).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

async fn api_chatgpt_registrar_stop() -> Response {
    let cfg = chatgpt_channel::load_config();
    match chatgpt_channel::auto_stop(&cfg).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_GATEWAY),
    }
}

async fn api_chatgpt_registrar_status() -> Response {
    json_ok(chatgpt_channel::auto_status(&chatgpt_channel::load_config()).await)
}

async fn api_chatgpt_registered() -> Response {
    json_ok(chatgpt_channel::registered(&chatgpt_channel::load_config()).await)
}

async fn api_chatgpt_accounts() -> Response {
    json_ok(chatgpt_channel::accounts(&chatgpt_channel::load_config()).await)
}

async fn api_chatgpt_accounts_sync() -> Response {
    json_ok(chatgpt_channel::sync_pool(&chatgpt_channel::load_config()).await)
}

// ---------------------------------------------------------------------------
// TRAE Work CN 模型反代（V1：目录+权益；对话需客户端会话态暂未开放）
// ---------------------------------------------------------------------------

async fn api_trae_proxy_status() -> Response {
    json_ok(trae_proxy::proxy_status().await)
}

async fn api_trae_proxy_models() -> Response {
    json_ok(trae_proxy::models_catalog().await)
}

/// T3：TRAE key 迁移窗口——trae 独立 key 生成后 30 天内，loomy 旧 key 仍被接受
/// （响应体带 `keyMigrated:true` 提示调用方切换到 trae key）；窗口外只认 trae key。
const TRAE_KEY_MIGRATION_WINDOW_MS: i64 = 30 * 24 * 3600 * 1000;

/// TRAE 反代鉴权（Bearer + x-api-key；空 key 放行，与 glm/loomy/workbuddy 一致）。
fn trae_proxy_auth_ok(headers: &HeaderMap, cfg: &trae_proxy_config::TraeProxyConfig) -> bool {
    if cfg.api_key.is_empty() {
        return true;
    }
    let bearer_ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|k| k == cfg.api_key)
        .unwrap_or(false);
    let x_api_key_ok = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|k| k == cfg.api_key)
        .unwrap_or(false);
    bearer_ok || x_api_key_ok
}

/// TRAE 模型列表（OpenAI /v1/models 格式透传）。
///
/// T3 鉴权：trae 独立 key 优先；迁移窗口内（trae key 生成后 30 天）兼容 loomy 旧
/// key，命中时响应体插 `keyMigrated:true` 提示切换；两者都不中返回 401。
async fn api_trae_v1_models(headers: HeaderMap) -> Response {
    let cfg = trae_proxy_config::load_config();
    let mut key_migrated = false;
    if !trae_proxy_auth_ok(&headers, &cfg) {
        let loomy = loomy_proxy::load_config();
        if loomy_proxy_auth_ok(&headers, &loomy)
            && config::now_ms() - cfg.key_created_at <= TRAE_KEY_MIGRATION_WINDOW_MS
        {
            key_migrated = true;
        } else {
            return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
        }
    }
    let started = std::time::Instant::now();
    let mut payload = trae_proxy::models_payload().await;
    if key_migrated {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("keyMigrated".to_string(), json!(true));
        }
    }
    // T5：TRAE 目录请求计入请求级用量（轻量：ok/延迟/modelCount；目录查询不计费）。
    let model_count = payload
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.len() as u64)
        .unwrap_or(0);
    model_usage::record_channel_call(
        "trae",
        json!({
            "id": format!("trae-models-{}", config::now_ms()),
            "ts": config::now_ms(),
            "channel": "trae",
            "model": "models",
            "requestedModel": "models",
            "ok": true,
            "latencyMs": started.elapsed().as_millis() as u64,
            "credits": 0,
            "cost": 0,
            "accountId": "trae_work_cn",
            "modelCount": model_count,
            "note": "目录查询不计费",
        }),
    );
    json_ok(payload)
}

/// TRAE 对话补全（OpenAI 兼容）：T6 真转发（本机 TRAE ACP 通道）。
///
/// T1 的占位 503 已退役：转发链路落在 core `trae_acp::chat_completions`
/// （connect → initialize → session/new → session/prompt，SSE 解析
/// agent_message_chunk 翻译 OpenAI chunk，断线重连一次）。
///
/// fullAccess 前置门禁（硬约束，默认关闭）：`trae_proxy_config.acp_enabled`
/// 默认 false + `acp_allowed_dirs` 空 = 拒绝全部；未开启/白名单空/目录越界/
/// 无活跃 TRAE 会话 → 503 明示（文案含「未开放/白名单」），绝不返回 200 HTML
/// 假成功。
///
/// 刻意**不带 Json extractor**：畸形 body 也进门禁/参数逻辑（503/400 JSON），
/// 不被 axum 的 422 抢答。方法不匹配（如 GET）由 axum MethodRouter 自动返回 405。
/// TRAE 会话 ACP 转发的统一响应构造（/trae/v1 专用；/workbuddy-native/v1 的
/// 对话已改走 native-first + OpenCode 兜底编排，见 api_workbuddy_native_chat_completions）。
async fn trae_acp_chat_response(body: axum::body::Bytes) -> Response {
    let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    match trae_acp::chat_completions(value).await {
        Ok(trae_acp::TraeAcpChat::Json(value)) => json_ok(value),
        Ok(trae_acp::TraeAcpChat::Stream(upstream)) => {
            let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err((code, msg)) => {
            let status = StatusCode::from_u16(code).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
            glm_proxy_error(msg, status)
        }
    }
}

async fn api_trae_v1_chat_completions(body: axum::body::Bytes) -> Response {
    trae_acp_chat_response(body).await
}

/// native-first 对话响应（/workbuddy-native/v1/chat/completions，契约 §3.2）：
/// native 优先 + OpenCode 兜底，`wbServedBy` 流式放响应头 `x-wb-served-by`、
/// 非流式放 JSON 字段。log_gateway_request 仍打 `workbuddy` tag（note 标
/// servedBy）；record_channel_call：native 路径 channel=workbuddy-native，
/// 兜底路径 channel=workbuddy（T6 口径）。
async fn api_workbuddy_native_chat_completions(body: axum::body::Bytes) -> Response {
    let started = std::time::Instant::now();
    let started_ms = config::now_ms();
    let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let log_model = value
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("—")
        .to_string();
    match workbuddy_proxy::chat_completions_native_first(value).await {
        Ok(workbuddy_proxy::NativeFirstChat::Json { served_by, mut value }) => {
            let latency = started.elapsed().as_millis() as u64;
            // 非流式：JSON 增字段 wbServedBy（契约 §3.2）。
            if let Some(obj) = value.as_object_mut() {
                obj.insert("wbServedBy".to_string(), json!(served_by));
            }
            let actual_model = value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&log_model)
                .to_string();
            log_gateway_request(
                "workbuddy",
                None,
                &actual_model,
                true,
                latency,
                &format!("servedBy={served_by}"),
            );
            record_workbuddy_call(
                served_by_channel(served_by),
                started_ms,
                &actual_model,
                true,
                latency,
                "",
                value.get("usage"),
                &format!("servedBy={served_by}"),
            );
            json_ok(value)
        }
        Ok(workbuddy_proxy::NativeFirstChat::Stream { served_by, rx }) => {
            let latency = started.elapsed().as_millis() as u64;
            log_gateway_request(
                "workbuddy",
                None,
                &log_model,
                true,
                latency,
                &format!("servedBy={served_by}（流式）"),
            );
            // 流式 cost 由 SSE 末尾 usage 帧携带，handler 返回时尚未产出，记 0 并注明。
            record_workbuddy_call(
                served_by_channel(served_by),
                started_ms,
                &log_model,
                true,
                latency,
                "",
                None,
                &format!("servedBy={served_by}；流式：cost 见 SSE 末尾 usage 帧"),
            );
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "text/event-stream".to_string()),
                    (
                        header::HeaderName::from_static("x-wb-served-by"),
                        served_by.to_string(),
                    ),
                ],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err((code, msg)) => {
            let latency = started.elapsed().as_millis() as u64;
            log_gateway_request("workbuddy", None, &log_model, false, latency, &msg);
            record_workbuddy_call(
                "workbuddy",
                started_ms,
                &log_model,
                false,
                latency,
                &msg,
                None,
                "",
            );
            let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
            glm_proxy_error(msg, status)
        }
    }
}

/// wbServedBy → 记账 channel（T6）：native 链路新增 channel=workbuddy-native 行；
/// OpenCode 兜底链路沿用 channel=workbuddy（note 已标 servedBy）。
fn served_by_channel(served_by: &str) -> &'static str {
    if served_by == workbuddy_proxy::SERVED_BY_NATIVE {
        "workbuddy-native"
    } else {
        "workbuddy"
    }
}

/// `?include=custom` 解析（契约 §3.1）：query 形如 `include=custom`（多参数容忍）。
fn native_models_include_custom(query: &str) -> bool {
    query.split('&').any(|kv| {
        let mut parts = kv.splitn(2, '=');
        parts.next() == Some("include") && parts.next() == Some("custom")
    })
}

/// `?refresh=1` 解析（契约 §2.4）：强制重探目录（等价 refresh 端点），多参数容忍。
fn native_models_force_refresh(query: &str) -> bool {
    query.split('&').any(|kv| {
        let mut parts = kv.splitn(2, '=');
        parts.next() == Some("refresh") && parts.next() == Some("1")
    })
}

/// WorkBuddy 内置模型目录（/workbuddy-native/v1/models，契约 §3.1）：
/// ACP session/new 实时探测（TTL 缓存 300s，带 name/multiplier/kind/exposed 与来源标记），
/// ACP 不可达 → builtin 静态回落（source=static-fallback），目录永不为空。
/// 契约 §2.4 增补：`?refresh=1` 绕过 TTL 强制重探（与 refresh 端点同语义——
/// 探测失败同样 502 + 错误串，不做 static 回落：显式刷新要如实报错）。
async fn api_workbuddy_native_models(RawQuery(query): RawQuery) -> Response {
    let query = query.as_deref().unwrap_or("");
    let include_custom = native_models_include_custom(query);
    let payload = if native_models_force_refresh(query) {
        match workbuddy_proxy::refresh_native_catalog().await {
            Ok(catalog) => workbuddy_proxy::catalog_to_payload(&catalog, include_custom),
            Err(e) => return glm_proxy_error(e, StatusCode::BAD_GATEWAY),
        }
    } else {
        workbuddy_proxy::native_models_payload(include_custom).await
    };
    // 目录查询轻量记账（对齐 /trae/v1/models 口径：不计费，注明）。
    let source = payload
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let count = payload
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    model_usage::record_channel_call(
        "workbuddy-native",
        json!({
            "id": format!("wb-native-models-{}", config::now_ms()),
            "ts": config::now_ms(),
            "channel": "workbuddy-native",
            "model": "models",
            "requestedModel": "models",
            "ok": true,
            "latencyMs": 0,
            "credits": 0,
            "cost": 0,
            "accountId": "workbuddy",
            "modelCount": count,
            "note": format!("目录查询不计费（source={source}）"),
        }),
    );
    json_ok(payload)
}

/// POST /api/workbuddy-native/catalog/refresh（契约 §2.4）：绕过 TTL 强制重探
/// 目录（ACP + 产品配置补充源合并），返回 `catalog_to_payload(&catalog, true)`
/// （含 custom 条目，前端按 kind 分组展示）。探测失败 → 502 + 错误串。
async fn api_workbuddy_native_catalog_refresh() -> Response {
    match workbuddy_proxy::refresh_native_catalog().await {
        Ok(catalog) => json_ok(workbuddy_proxy::catalog_to_payload(&catalog, true)),
        Err(e) => glm_proxy_error(e, StatusCode::BAD_GATEWAY),
    }
}

// ---------------------------------------------------------------------------
// WorkBuddy 模型反代（借道本机 OpenCode server）
// ---------------------------------------------------------------------------

/// 401 自诊文案（契约 §2.3）：告诉用户本网关接受哪几类密钥，可自行定位错因。
const WORKBUDDY_AUTH_INVALID_KEY_MSG: &str =
    "无效的 API Key（本网关接受：TRAE / WorkBuddy 页展示的接入密钥，或网关密钥 gw_live_*）";

/// WorkBuddy `/workbuddy/v1/*` 鉴权（契约 §2.3 扩展，三选一命中即过）：
/// ① `cfg.api_key` 精确匹配（Bearer 或 x-api-key；空 key 放行，向后兼容现状）；
/// ② ①未命中时走网关 key 体系 `gateway_admin::verify_request_key`——网关专用
///    Key（gw_live_*，命中会计数+1 并落盘）或 GLM 主密钥（命中无副作用），主密钥
///    来源与 `gateway_key_status` 同口径（`glm_proxy::load_config().api_key`）；
/// ③ 主密钥已由 ② 覆盖，不重复实现。
///
/// ⚠️ 副作用说明：② 仅在 ① 未命中时调用（gw_live_* 命中会 requestCount+1），
/// ① 命中的常规路径零副作用。仅作用于 `/workbuddy/v1/*`；`/workbuddy-native/v1/*`
/// 维持免鉴权现状（契约 §2.3 明示不涉 native 端点）。
fn workbuddy_proxy_auth_ok(headers: &HeaderMap, cfg: &workbuddy_proxy::WorkbuddyProxyConfig) -> bool {
    if cfg.api_key.is_empty() {
        return true;
    }
    // 双头提取对齐既有 wrapper `gateway_key_status`（Bearer 优先，其次 x-api-key）。
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    if presented.map(|k| k == cfg.api_key).unwrap_or(false) {
        return true;
    }
    let glm_master = glm_proxy::load_config().api_key;
    gateway_admin::verify_request_key(presented, &glm_master).0
}

/// 鉴权失败时的「收到的鉴权头长什么样」诊断串（模式同 `opencode_auth_diag`，
/// 该 helper 绑定 opencode 配置无法直接复用）：只输出鉴权头前 6 字符与长度、
/// UA 截断、头名列表与期望 key 的掩码，**绝不落全量密钥**。
fn workbuddy_auth_diag(headers: &HeaderMap, cfg: &workbuddy_proxy::WorkbuddyProxyConfig) -> String {
    let mask = |s: &str| -> String {
        let n = s.chars().count();
        let p: String = s.chars().take(6).collect();
        format!("{p}…(len={n})")
    };
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| mask(v.trim_start_matches("Bearer ")))
        .unwrap_or_else(|| "<无>".to_string());
    let xkey = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(mask)
        .unwrap_or_else(|| "<无>".to_string());
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(40).collect::<String>())
        .unwrap_or_else(|| "<无>".to_string());
    let mut names: Vec<String> = headers.keys().map(|k| k.as_str().to_string()).collect();
    names.sort();
    format!(
        "无效的 API Key ｜ 期望={} 实收 Authorization={auth} x-api-key={xkey} UA={ua} 头列表=[{}]",
        mask(&cfg.api_key),
        names.join(",")
    )
}

async fn api_workbuddy_proxy_status() -> Response {
    json_ok(workbuddy_proxy::proxy_status().await)
}

async fn api_workbuddy_proxy_models() -> Response {
    match workbuddy_proxy::models_catalog().await {
        Ok(value) => json_ok(value),
        Err(e) => {
            // G2：负缓存期是瞬态（看门狗恢复窗口，通常 ≤60s），带机器可读的
            // recovering 标记，避免前端把「恢复中」显示成稳态「读取失败」。
            if workbuddy_proxy::is_neg_cache_error(&e) {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "ok": false, "error": e, "recovering": true })),
                )
                    .into_response()
            } else {
                json_err(e, StatusCode::BAD_GATEWAY)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 模型目录自动同步（/api/model-sync/*，契约 model-sync-contract.md §四）
// 管理端点一律免 key（本机回环惯例，同 /api/workbuddy-proxy/config）。
// ---------------------------------------------------------------------------

/// GET /api/model-sync/status：总览（读 state，秒开不触发拉取）。
async fn api_model_sync_status() -> Response {
    json_ok(model_sync::status_payload())
}

/// POST /api/model-sync/run：立即跑一轮。body 可选 `{gateways:["kimi",…]}` 过滤；
/// busy 时 409；返回本轮完整结果（形状同 log 单轮）。
async fn api_model_sync_run(body: Option<Json<Value>>) -> Response {
    let gateways = body
        .as_ref()
        .and_then(|Json(v)| v.get("gateways"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect::<Vec<_>>()
        });
    let round = model_sync::run_sync("manual", gateways.as_deref()).await;
    if round.get("busy").and_then(Value::as_bool) == Some(true) {
        return json_err(
            round
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("同步已在进行中")
                .to_string(),
            StatusCode::CONFLICT,
        );
    }
    json_ok(round)
}

/// GET /api/model-sync/log?limit=N：环形日志倒序（最新在前），limit 默认 20 上限 50。
async fn api_model_sync_log(RawQuery(query): RawQuery) -> Response {
    let limit = query
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| pair.strip_prefix("limit=").and_then(|v| v.parse::<usize>().ok()))
        .unwrap_or(20)
        .min(model_sync::LOG_MAX_ROUNDS);
    let mut log = model_sync::load_log();
    log.reverse();
    log.truncate(limit);
    json_ok(json!({ "rounds": log }))
}

/// GET /api/model-sync/config：`{enabled, intervalMinutes}`。
async fn api_model_sync_config_get() -> Response {
    let cfg = model_sync::load_config();
    json_ok(json!({
        "enabled": cfg.enabled,
        "intervalMinutes": cfg.interval_minutes,
    }))
}

/// POST /api/model-sync/config：热更新（即写盘，定时轮下一轮现读生效）。
async fn api_model_sync_config_save(Json(body): Json<Value>) -> Response {
    let mut cfg = model_sync::load_config();
    if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if let Some(mins) = body.get("intervalMinutes").and_then(Value::as_i64) {
        cfg.interval_minutes = mins.clamp(0, 60 * 24 * 30);
    }
    if let Err(e) = model_sync::save_config(&cfg) {
        return json_err(e, StatusCode::INTERNAL_SERVER_ERROR);
    }
    json_ok(json!({
        "enabled": cfg.enabled,
        "intervalMinutes": cfg.interval_minutes,
    }))
}

/// model-sync registry 补录端点只收 Manual 网关（glmweb/qoder）：Auto 网关的清单
/// 已动态，手动塞进去既不可调也会被下轮 diff 语义搞乱（契约 §四）。
fn model_sync_registry_manual_only(gateway: &str) -> Result<(), String> {
    let is_manual = model_sync::SYNC_SOURCES.iter().any(|src| {
        src.gateway == gateway && src.mode == model_sync::SourceMode::Manual
    });
    if is_manual {
        Ok(())
    } else {
        Err(format!(
            "网关「{gateway}」不是手动补录型（仅 glmweb / qoder 支持）；auto 型网关的模型清单来自其上游，会自动同步"
        ))
    }
}

/// POST /api/model-sync/registry：`{gateway, modelId, displayName?, note?}` 手动补录。
/// 写入后该网关 models_payload 立即含该条目（合并尾步每次实时读注册表）。
async fn api_model_sync_registry_add(Json(body): Json<Value>) -> Response {
    let Some(gateway) = body.get("gateway").and_then(Value::as_str).map(str::trim) else {
        return json_err("缺少 gateway".to_string(), StatusCode::BAD_REQUEST);
    };
    if gateway.is_empty() {
        return json_err("gateway 不能为空".to_string(), StatusCode::BAD_REQUEST);
    }
    if let Err(e) = model_sync_registry_manual_only(gateway) {
        return json_err(e, StatusCode::BAD_REQUEST);
    }
    let Some(model_id) = body
        .get("modelId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return json_err("缺少 modelId".to_string(), StatusCode::BAD_REQUEST);
    };
    let display_name = body
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let note = body
        .get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    if let Err(e) =
        model_sync::add_registry_entry(gateway, model_id, display_name, "manual", note)
    {
        return json_err(e, StatusCode::INTERNAL_SERVER_ERROR);
    }
    let entries = model_sync::registry_entries_for(gateway);
    json_ok(json!({
        "ok": true,
        "gateway": gateway,
        "modelId": model_id,
        "registry": entries.iter().map(|e| json!({
            "id": e.id,
            "name": e.name,
            "addedTs": e.added_ts,
            "origin": e.origin,
            "note": e.note,
        })).collect::<Vec<_>>(),
    }))
}

/// POST /api/model-sync/registry/remove：`{gateway, modelId}` 撤销补录（G6 恢复原样）。
async fn api_model_sync_registry_remove(Json(body): Json<Value>) -> Response {
    let Some(gateway) = body.get("gateway").and_then(Value::as_str).map(str::trim) else {
        return json_err("缺少 gateway".to_string(), StatusCode::BAD_REQUEST);
    };
    if let Err(e) = model_sync_registry_manual_only(gateway) {
        return json_err(e, StatusCode::BAD_REQUEST);
    }
    let Some(model_id) = body.get("modelId").and_then(Value::as_str).map(str::trim) else {
        return json_err("缺少 modelId".to_string(), StatusCode::BAD_REQUEST);
    };
    if model_sync::remove_registry_entry(gateway, model_id) {
        json_ok(json!({"ok": true, "gateway": gateway, "removed": model_id}))
    } else {
        json_err(
            format!("网关「{gateway}」的注册表中不存在模型「{model_id}」"),
            StatusCode::NOT_FOUND,
        )
    }
}

/// GET /api/workbuddy-proxy/config：接入信息（Base URL / 密钥 / 端口配置）。
/// 只读配置、不触发服务发现，保证页面秒开（发现逻辑可能耗时十几秒）。
async fn api_workbuddy_proxy_config_get() -> Response {
    let cfg = workbuddy_proxy::load_config();
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(json!({
        "enabled": cfg.enabled,
        "endpoint": format!("http://127.0.0.1:{port}/workbuddy/v1"),
        "apiKey": cfg.api_key,
        "serverPort": cfg.server_port,
    }))
}

/// POST /api/workbuddy-proxy/config：保存接入相关配置（启用开关 / 端口 / 密钥重新生成）。
async fn api_workbuddy_proxy_config_save(Json(body): Json<Value>) -> Response {
    let mut cfg = workbuddy_proxy::load_config();
    if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if let Some(port) = body.get("serverPort").and_then(Value::as_u64) {
        cfg.server_port = port.min(65535) as u16;
    }
    if body.get("regenerateKey").and_then(Value::as_bool) == Some(true) {
        cfg.api_key = glm_proxy::generate_api_key();
    } else if let Some(key) = body.get("apiKey").and_then(Value::as_str) {
        let key = key.trim();
        if !key.is_empty() {
            cfg.api_key = key.to_string();
        }
    }
    if let Err(e) = workbuddy_proxy::save_config(&cfg) {
        return json_err(e, StatusCode::INTERNAL_SERVER_ERROR);
    }
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(json!({
        "enabled": cfg.enabled,
        "endpoint": format!("http://127.0.0.1:{port}/workbuddy/v1"),
        "apiKey": cfg.api_key,
        "serverPort": cfg.server_port,
    }))
}

/// GET /api/trae-proxy/config：TRAE 接入信息。
///
/// T3：TRAE 现在有自己的独立密钥体系（`trae_proxy_config`，迁移窗口 30 天内兼容
/// loomy 旧 key）；`apiKey` 保留并置为 trae key 以兼容旧前端（其类型把 keySource
/// 锁死为 "loomy"，需前端实现员在 T4 同步），`traeKey`/`keySource` 反映真实来源。
async fn api_trae_proxy_config_get() -> Response {
    let trae = trae_proxy_config::load_config();
    let status = trae_proxy::proxy_status().await;
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(json!({
        "endpoint": format!("http://127.0.0.1:{port}/trae/v1"),
        "apiKey": trae.api_key,
        "traeKey": trae.api_key,
        "keySource": "trae",
        // trae key 生成后 30 天为迁移窗口截止时刻（配合 /trae/v1/models 的 keyMigrated）。
        "keyMigrationUntil": trae.key_created_at + TRAE_KEY_MIGRATION_WINDOW_MS,
        "chatForwarding": status.get("chatForwarding").cloned().unwrap_or(json!(false)),
        "credential": status.get("credential").cloned().unwrap_or(Value::Null),
        "modelCount": status.get("models").cloned().unwrap_or(Value::Null),
    }))
}

/// `/workbuddy/v1/models` —— **WorkBuddy 内置模型目录优先**（契约 T4）。
///
/// 2026-09-23（用户要求「只保留 WorkBuddy 原本内置的模型」）：原先这里借道 OpenCode server
/// 展开上游 provider 全量清单（实测 591 条 / 14 渠道），与设计边界冲突 ——
/// `docs/gateway-split-20260922.md` 明确 WorkBuddy「**不拥有 provider 清单**（归 OpenCode 页呈现）」。
/// 现改为与 `/workbuddy-native/v1/models` 同源（native ACP 实时目录，name/multiplier/kind），
/// 目录数 591 → 内置模型数。
///
/// 契约 T4 新增回落语义：native 目录**不可用**时（WorkBuddy 未运行 / ACP 失败），
/// 回落 `channel_pick_models_payload()`（fresh 发现 OpenCode server + 展开上游目录，
/// 此即「不可用时重新再去获取模型内容」），保证目录永不为空。
///
/// ⚠️ 对话端点 `/workbuddy/v1/chat/completions` 走 native 优先 + OpenCode 兜底（契约 §3.2）。
/// ⚠️ 别顺手把 `workbuddy_proxy::models_payload()/models_catalog()` 改成只返回内置模型 ——
/// **`opencode.rs` 正在复用它们**，改了会一起打断 OpenCode 网关与 OpenCode 页。
async fn api_workbuddy_v1_models(headers: HeaderMap) -> Response {
    let cfg = workbuddy_proxy::load_config();
    if !workbuddy_proxy_auth_ok(&headers, &cfg) {
        return glm_proxy_error(
            WORKBUDDY_AUTH_INVALID_KEY_MSG.to_string(),
            StatusCode::UNAUTHORIZED,
        );
    }
    // 内置目录优先；目录不可用 → channel_pick 重新拉取（v1_models_payload 内部编排）。
    json_ok(workbuddy_proxy::v1_models_payload().await)
}

/// T5：workbuddy 渠道请求级用量记录（chat 流式/非流式、成功/失败两条路径共用）。
///
/// 写同一份 `glm_usage_records.json`；积分口径（F9）取上游响应的 `usage.cost`
/// 实耗（免费模型为 0，如实记录）。`channel`：`workbuddy`（OpenCode 借道链路，
/// 含兜底）或 `workbuddy-native`（native ACP 链路，T6 新增行，不改既有口径）。
fn record_workbuddy_call(
    channel: &str,
    started_ms: i64,
    model: &str,
    ok: bool,
    latency_ms: u64,
    error: &str,
    usage: Option<&Value>,
    note: &str,
) {
    // cost：响应 usage.cost（缺失/null = 免费模型，如实记 0）。
    let cost = usage
        .and_then(|u| u.get("cost"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let usage = usage.cloned().unwrap_or(Value::Null);
    let mut record = json!({
        "id": format!("wb-{}", started_ms),
        "ts": started_ms,
        "channel": channel,
        "model": model,
        "requestedModel": model,
        "ok": ok,
        "latencyMs": latency_ms,
        "error": if error.is_empty() { Value::Null } else { json!(error) },
        "promptTokens": usage.get("prompt_tokens").cloned().unwrap_or(json!(0)),
        "completionTokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0)),
        "totalTokens": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
        "credits": cost,
        "cost": cost,
        "accountId": "workbuddy",
    });
    if !note.is_empty() {
        if let Some(obj) = record.as_object_mut() {
            obj.insert("note".to_string(), json!(note));
        }
    }
    model_usage::record_channel_call(channel, record);
}

/// WorkBuddy 对话补全（OpenAI 兼容；native 优先 + OpenCode 兜底，契约 §3.2）。
///
/// T4：keyed 路由改 native-first 编排——请求模型在内置目录（或缺省）时走
/// native ACP（session/set_model 真切换），模型不在目录或 native 失败时落
/// OpenCode 借道兜底（重新拉取模型内容）。`/workbuddy/v1` 记账保持
/// channel=workbuddy（T6），note 标 servedBy。
///
/// T5：流式 + 非流式、成功 + 失败全路径插桩——`log_gateway_request`（gateway_admin
/// 请求日志，供渠道健康度 `traffic_source=logs` 消费）+ `record_channel_call`
/// （model_usage 请求级用量统计）。
async fn api_workbuddy_chat_completions(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    let started_ms = config::now_ms();
    let cfg = workbuddy_proxy::load_config();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("workbuddy")
        .to_string();
    if !cfg.enabled {
        // T2：文案去掉「请在设置中开启」——启用开关落在「TRAE / WorkBuddy」页
        // （接 POST /api/workbuddy-proxy/config {enabled}），不存在旧的「设置」入口。
        let msg = "WorkBuddy 代理未启用（可在「TRAE / WorkBuddy」页开启）";
        log_gateway_request("workbuddy", None, &model, false, 0, msg);
        record_workbuddy_call("workbuddy", started_ms, &model, false, 0, msg, None, "");
        return glm_proxy_error(msg.to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    if !workbuddy_proxy_auth_ok(&headers, &cfg) {
        // 契约 §2.3：401 可自诊——请求日志 error 字段记鉴权头诊断串（只前缀+长度，
        // 不落全量密钥），响应 message 换可自诊文案。
        let diag = workbuddy_auth_diag(&headers, &cfg);
        log_gateway_request("workbuddy", None, &model, false, 0, &diag);
        record_workbuddy_call(
            "workbuddy",
            started_ms,
            &model,
            false,
            0,
            WORKBUDDY_AUTH_INVALID_KEY_MSG,
            None,
            "",
        );
        return glm_proxy_error(
            WORKBUDDY_AUTH_INVALID_KEY_MSG.to_string(),
            StatusCode::UNAUTHORIZED,
        );
    }
    match workbuddy_proxy::chat_completions_native_first(body).await {
        Ok(workbuddy_proxy::NativeFirstChat::Json { served_by, mut value }) => {
            let latency = started.elapsed().as_millis() as u64;
            // 非流式：JSON 增字段 wbServedBy（契约 §3.2）。
            if let Some(obj) = value.as_object_mut() {
                obj.insert("wbServedBy".to_string(), json!(served_by));
            }
            let actual_model = value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&model)
                .to_string();
            log_gateway_request(
                "workbuddy",
                None,
                &actual_model,
                true,
                latency,
                &format!("servedBy={served_by}"),
            );
            // T6：/workbuddy/v1 记账保持 channel=workbuddy，note 标 servedBy。
            record_workbuddy_call(
                "workbuddy",
                started_ms,
                &actual_model,
                true,
                latency,
                "",
                value.get("usage"),
                &format!("servedBy={served_by}"),
            );
            json_ok(value)
        }
        Ok(workbuddy_proxy::NativeFirstChat::Stream { served_by, rx }) => {
            let latency = started.elapsed().as_millis() as u64;
            log_gateway_request(
                "workbuddy",
                None,
                &model,
                true,
                latency,
                &format!("servedBy={served_by}（流式）"),
            );
            // 流式 cost 由 SSE 末尾 usage 帧携带，handler 返回时尚未产出，
            // 本记录记 0 并注明（非流式路径照实记录 cost）。
            record_workbuddy_call(
                "workbuddy",
                started_ms,
                &model,
                true,
                latency,
                "",
                None,
                &format!("servedBy={served_by}；流式：cost 见 SSE 末尾 usage 帧"),
            );
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "text/event-stream".to_string()),
                    (
                        header::HeaderName::from_static("x-wb-served-by"),
                        served_by.to_string(),
                    ),
                ],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err((code, msg)) => {
            let latency = started.elapsed().as_millis() as u64;
            log_gateway_request("workbuddy", None, &model, false, latency, &msg);
            record_workbuddy_call("workbuddy", started_ms, &model, false, latency, &msg, None, "");
            let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
            glm_proxy_error(msg, status)
        }
    }
}

// ---------------------------------------------------------------------------
// WorkBuddy 多账号池网关（/workbuddy-pool/v1/*）—— cloud 上游 + 账号池
//
// 契约 docs/workbuddy-gateway-refactor-contract.md §12：新池走**独立路由组**，
// 存量 /workbuddy/v1（OpenCode 借道）与 /workbuddy-native/v1（ACP）原样不动，
// 本期绝不内联切流。
//
// 本段只做薄接线：网关 key 鉴权 → 粘性键 → 选号循环（≤3 次换号）→ token 惰性
// 刷新 → chat_stream → 错误分级换号 / 成功聚合 → 按协议渲染。
// 选号算法、冷却矩阵、粘性 TTL、出站头族、脱敏全部下沉 core 的
// workbuddy_gateway 与 workbuddy_upstream（契约 §13 钉定签名）。
//
// P2 起（契约增补）三个协议入口共用同一份选号循环 `workbuddy_pool_dispatch`：
// chat（OpenAI）/ responses（Codex CLI）/ messages（Claude Code），差异只在
// 进 dispatch 前的协议转换与出 dispatch 后的渲染。
// ---------------------------------------------------------------------------

/// 选号循环上限：一次请求最多换 3 次号（契约 §13「≤3 次换号」）。
const WB_POOL_MAX_PICKS: usize = 3;

/// token 惰性刷新阈值：距过期不足 5 分钟先刷新再发车（契约 §13 refresh_if_needed）。
const WB_POOL_REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;

/// 粘性 TTL 边界（契约 §5：60min 滚动）；config PATCH 的 clamp 用。
const WB_POOL_STICKY_TTL_MIN_MS: i64 = 60_000;
const WB_POOL_STICKY_TTL_MAX_MS: i64 = 3_600_000;

/// 余额巡检间隔下限（契约 §5：默认 15 分钟）。
const WB_POOL_BALANCE_REFRESH_MIN_MINUTES: u64 = 5;

/// 池模式枚举（契约 §5：均衡 / 烧号 / 指定）。
const WB_POOL_MODES: [&str; 3] = ["balance", "rotate", "pinned"];

/// 网关 key 鉴权：`Authorization: Bearer` 或 `x-api-key`，命中「网关专用 Key」
/// 或 GLM 主密钥即放行。
///
/// 双头接受与 `glm_proxy_auth_ok`（本文件 786 行）同口径；密钥来源走网关 key
/// 体系（`gateway_key_status`，本文件 1986 行）—— 池网关没有自己的 api_key
/// 配置字段（契约 §13 的 `GatewayConfig` 无该项），与 grok/kimi/qoder 三个
/// 池网关保持一致。返回 `(是否通过, key id)`，key id 供请求日志归属。
///
/// 只给三个对话入口用（chat / responses / messages，鉴权口径完全一致）；
/// models/status/config 是管理三端点、免 key（见路由注册处的鉴权策略注释）。
fn workbuddy_pool_auth_ok(headers: &HeaderMap) -> (bool, Option<String>) {
    gateway_key_status(headers)
}

/// 区域版池鉴权：请求体 region 参与 key 校验（WorkBuddy 池两区密钥隔离）。
fn workbuddy_pool_auth_ok_region(
    headers: &HeaderMap,
    wanted_region: Option<&str>,
) -> (bool, Option<String>) {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    let cfg = glm_proxy::load_config();
    gateway_admin::verify_request_key_region(presented, &cfg.api_key, wanted_region)
}

/// system 字段拍平为纯文本：兼容字符串与 block 数组两种客户端形态
/// （口径同参考实现 go-gateway/internal/server/messages.go:156 anthropicSystemText）。
fn workbuddy_pool_system_text(body: &Value) -> String {
    match body.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 首条 user 消息文本（content 为字符串或 block 数组都取 text）。
fn workbuddy_pool_first_user_text(body: &Value) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    for m in messages {
        if m.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        return match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
    }
    String::new()
}

/// 粘性键：客户端显式 `conversation_id` 优先，否则用 system + 首条 user 文本派生。
///
/// 口径同参考实现 go-gateway/internal/session/session.go:302 ExtractKey（顶层
/// `conversation_id` → `metadata.conversation_id` → `metadata.user_id`）与
/// session.go:335 SessionKeyFromSeed（FNV-1a 64 位折叠成 `seed-<hex>`）。
///
/// 为什么种子只取「system + 首条 user」而不是整段 messages：多轮对话里 messages
/// 越滚越长，用全量做种子会让同一会话每轮键都不同，粘性恒不命中；只取首轮内容
/// 才能让同一会话全程命中同一账号（契约 §3 粘性）。
/// 两者都取不到时返回空串 = 本次请求不做粘性。
fn workbuddy_pool_sticky_key(body: &Value) -> String {
    if let Some(v) = body.get("conversation_id").and_then(Value::as_str) {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(meta) = body.get("metadata").and_then(Value::as_object) {
        for k in ["conversation_id", "user_id"] {
            if let Some(v) = meta.get(k).and_then(Value::as_str) {
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    let system = workbuddy_pool_system_text(body);
    let first_user = workbuddy_pool_first_user_text(body);
    if system.is_empty() && first_user.is_empty() {
        return String::new();
    }
    let seed = format!("{system}\u{0}{first_user}");
    // FNV-1a 64 位：与参考实现同算法（偏移基/质数一致），跨进程稳定 → 同一会话
    // 在服务重启后仍命中同一账号。
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in seed.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("seed-{hash:x}")
}

/// 请求侧 / 凭证侧的明确错误 → 直接回给客户端的状态码（**不换号**）。
///
/// - `ContextTooLong`：同一请求体发给任何账号都同样失败，换号只会把整个请求体
///   重传 N 遍（参考实现 forward.go:211-233 的结论）；契约 §4 要求立即 400。
/// - `SessionDead`：账号 OAuth 会话已失效（12153），继续换号重发会让客户端以为
///   是请求本身的问题；回 401 提示重新登录该账号。
/// 其余 kind 返回 None = 记账后继续换号。
fn workbuddy_pool_fatal_status(kind: workbuddy_errors::ErrKind) -> Option<StatusCode> {
    use workbuddy_errors::ErrKind;
    match kind {
        ErrKind::ContextTooLong => Some(StatusCode::BAD_REQUEST),
        ErrKind::SessionDead => Some(StatusCode::UNAUTHORIZED),
        _ => None,
    }
}

/// 配置对外形状（camelCase，与前端既有配置端点一致）；status 与 config PATCH
/// 的响应共用，避免两处字段漂移。
fn workbuddy_pool_config_json(cfg: &workbuddy_gateway::GatewayConfig) -> Value {
    json!({
        "mode": cfg.mode,
        "pinnedUid": cfg.pinned_uid,
        "softRateBaseSecs": cfg.soft_rate_base_secs,
        "softRateMaxSecs": cfg.soft_rate_max_secs,
        "breakerThreshold": cfg.breaker_threshold,
        "breakerBaseSecs": cfg.breaker_base_secs,
        "breakerMaxSecs": cfg.breaker_max_secs,
        "maxInFlight": cfg.max_in_flight,
        "stickyTtlMs": cfg.sticky_ttl_ms,
        "sanitizeEnabled": cfg.sanitize_enabled,
        "balanceRefreshMinutes": cfg.balance_refresh_minutes,
    })
}

/// 在途租约守卫：Drop 时自动归还账号的在途名额。
///
/// 为什么用守卫而不是手写 `release`：出口有五六条（刷新失败 / 上游 4xx 致命 /
/// 上游 4xx 换号 / 传输层失败 / 非流式聚合完成），漏一条就把该账号的在途名额
/// 永久漏掉（`max_in_flight` 之下这个账号再也不可选，只能重启服务）。
/// 参考实现 forward.go:93-105 用 `defer` + `handedOff` 标志解决同一问题；
/// Rust 侧不需要那个标志 —— **把租约 move 进流的状态就是所有权移交**，
/// 流结束（EOF）或被 drop（客户端断开）时 Drop 自然归还，恰好一次。
struct WorkbuddyPoolLease {
    uid: String,
}

impl WorkbuddyPoolLease {
    /// 占用租约；名额已满（并发打满 `max_in_flight`）返回 None = 换号。
    fn acquire(uid: &str) -> Option<Self> {
        if workbuddy_gateway::acquire(uid) {
            Some(Self {
                uid: uid.to_string(),
            })
        } else {
            None
        }
    }
}

impl Drop for WorkbuddyPoolLease {
    fn drop(&mut self) {
        workbuddy_gateway::release(&self.uid);
    }
}

/// 共享 dispatch 的聚合产物（契约增补 §1 钉定字段，一字不多一字不少）。
///
/// 三个协议 handler 都由它渲染各自的响应形态：
/// - chat：OpenAI `chat.completion` / `chat.completion.chunk` 帧；
/// - Responses：`response` 对象 / `response.*` 事件帧；
/// - Messages：Anthropic `message` / Anthropic SSE 帧。
///
/// 为什么出口是聚合产物而不是原始流：出站体被 `prepare_body` 强制 `stream:true`，
/// 上游恒以 SSE 回；而三个协议的流式帧序列各不相同（OpenAI `data:` 帧 / Responses
/// 事件序列 / Anthropic 事件序列），「聚合一次 + 各自渲染」比「三种转发器」少两条
/// 会各自漂移的代码路径（增补 §1 对 Responses/Messages 的流式同样定义为
/// 「先聚合上游再按事件序吐帧」）。
struct WorkbuddyPoolReply {
    /// 助手正文全文（上游 `delta.content` 累加）。
    content: String,
    /// 推理段全文（上游 `delta.reasoning_content` 累加；空串 = 上游没吐思考）。
    reasoning: String,
    /// 工具调用（按上游 index 升序；空数组 = 本次没有工具调用）。
    tool_calls: Vec<Value>,
    /// 上游 usage 原样透出（None = 上游没给，渲染层各自补默认值，不谎报 0）。
    usage: Option<Value>,
    /// 实际生效的模型名（上游帧带的优先，其次请求的，最后 `workbuddy`）。
    model_used: String,
    /// 命中的账号 uid（只供观测/排障，不进任何响应体）。
    uid: String,
}

/// WorkBuddy 多账号池的**共享 dispatch**（契约增补 §1「选号循环共享化」）。
///
/// 入参 `payload` 是**已转换成内部 chat 形态**的请求体：chat handler 直接送客户端
/// body，Responses / Messages 两个新 handler 先用
/// `workbuddy_protocols::{responses,anthropic}_req_to_chat` 转一道再进来。因此
/// 粘性键、脱敏、选号、租约、刷新、错误分级只有这一份实现，三个协议共用。
///
/// 流程（契约 §13 + 增补 §1）：粘性键 → 选号循环（≤3 次换号）→ token 惰性刷新
/// → `chat_stream` → 错误分级换号 / 成功则聚合成 [`WorkbuddyPoolReply`]。
///
/// 错误分级（契约 §4）：
/// - `ContextTooLong` / `SessionDead`：`note_error` 记账后**直接回 4xx**，不换号；
/// - 其余 kind：`note_error` 记账 + uid 加入 tried，继续换号；
/// - 传输层失败（`chat_stream` 返回 `Err`）：只换号，不喂熔断。
///
/// `Err` 一侧的 `(StatusCode, String)` 已是面向客户端的最终状态码与文案，三个
/// handler 拿到后只需 `glm_proxy_error`，不必再判错误类型。
/// `key_id` 只用于观测日志归属（`log_gateway_request`，增补 §2.2）。
///
/// 区域化（workbuddy-pool-region 契约 §3.3）：入参 payload 为 `&mut`——请求体可选
/// `region` 字段读出后**立即剥掉**（内部 chat 形态会原样交给 `prepare_body` 转发上游，
/// 非协议字段不外发）。循环前解析区域（显式 region > 区域目录唯一命中 > 无约束），
/// 循环内所有选号走 `pick_account_for_region`；上游错误含 `11102`（model service
/// info not found）→ 翻转区域提示继续在本轮 picks 内重试（T3 自愈）。
async fn workbuddy_pool_dispatch(
    payload: &mut Value,
    key_id: Option<&str>,
) -> Result<WorkbuddyPoolReply, (StatusCode, String)> {
    let started = std::time::Instant::now();
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("workbuddy")
        .to_string();
    // 显式 region 字段（§3.3①）：读出后剥掉，不外发上游。
    let region_hint = payload
        .get("region")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_string);
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("region");
    }
    let cfg = workbuddy_gateway::load_config();
    let sticky_key = workbuddy_pool_sticky_key(payload);
    let mut sticky_uid = if sticky_key.is_empty() {
        None
    } else {
        workbuddy_gateway::sticky_get(&sticky_key)
    };

    // 区域解析（循环前一次，§3.3）：显式 region 不需要目录；无显式时先确保目录缓存
    // 温暖（TTL 10min 内零上游调用，§3.2 T2；过期/失败降级为无约束，可用性优先——§3.1）。
    let mut region_filter = match workbuddy_gateway::RegionFilter::from_hint(region_hint.as_deref()) {
        Some(f) => f,
        None => {
            if let Err(e) = workbuddy_gateway::region_catalog().await {
                eprintln!(
                    "[workbuddy-pool] region catalog refresh failed: {e}（区域解析降级为无约束）"
                );
            }
            workbuddy_gateway::resolve_model_region(&model, None)
        }
    };
    // 日志 region=xx 标注（§3.3：不加新 tag，沿用既有 model 字段备注；随 11102 翻转更新）。
    let log_model = |wanted: workbuddy_gateway::RegionFilter| -> String {
        match wanted.wanted_region() {
            Some(r) => format!("{model} region={r}"),
            None => model.clone(),
        }
    };

    let mut tried: Vec<String> = Vec::new();
    let mut last_err: Option<String> = None;

    for _ in 0..WB_POOL_MAX_PICKS {
        // 选号：粘性号优先，全部走区域约束版 pick_account_for_region（§3.2/§3.3）。
        // 粘性的「按 model 过滤」仍由把其余全部账号放进 tried 实现（契约 §13 口径不变，
        // 冷却/熔断/在途判定对粘性号一视同仁）。
        let picked = match sticky_uid.as_deref() {
            Some(uid) if !tried.iter().any(|t| t == uid) => {
                let accounts = workbuddy_gateway::load_pool_accounts();
                let sticky_region = accounts
                    .iter()
                    .find(|a| a.uid == uid)
                    .map(|a| a.region.clone());
                if workbuddy_gateway::sticky_region_mismatch(sticky_region.as_deref(), region_filter)
                {
                    // T4 粘性区域一致性：粘性号区域与本轮 wanted 不符 → 直接解绑，
                    // 走常规区域选号（不硬塞跨区请求）。
                    workbuddy_gateway::sticky_clear(&sticky_key);
                    sticky_uid = None;
                    workbuddy_gateway::pick_account_for_region(&model, &tried, region_filter)
                } else {
                    // 其余全部账号当 tried：等价只放行粘性号（旧口径）。
                    let others: Vec<String> = accounts
                        .into_iter()
                        .map(|a| a.uid)
                        .filter(|u| u != uid)
                        .collect();
                    match workbuddy_gateway::pick_account_for_region(&model, &others, region_filter)
                    {
                        Some(acc) => Some(acc),
                        None => {
                            workbuddy_gateway::sticky_clear(&sticky_key);
                            sticky_uid = None;
                            workbuddy_gateway::pick_account_for_region(&model, &tried, region_filter)
                        }
                    }
                }
            }
            _ => workbuddy_gateway::pick_account_for_region(&model, &tried, region_filter),
        };
        let Some(mut acc) = picked else {
            // 区域约束下选不到号：给出「区域内无可用账号」的清晰文案（契约 G4），
            // 而不是把请求丢给对侧账号打上游。此前已有失败原因（11102 翻转后仍不可用
            // 等）时保留原文并前置区域标注。
            if let Some(want) = region_filter.wanted_region() {
                let region_note =
                    format!("region={want} 区内无可用账号（该区账号全部冷却/在途或不存在）");
                last_err = Some(match last_err.take() {
                    Some(prev) => format!("{region_note}；{prev}"),
                    None => region_note,
                });
            }
            break;
        };
        let uid = acc.uid.clone();
        tried.push(uid.clone());

        // 在途租约：选中即占名额（`max_in_flight` 的执行者；池内选号本就会跳过
        // 占满的账号，这里防的是「选中到占用之间」的并发窗口）。占不到就换号。
        let Some(lease) = WorkbuddyPoolLease::acquire(&uid) else {
            if sticky_uid.as_deref() == Some(uid.as_str()) {
                workbuddy_gateway::sticky_clear(&sticky_key);
                sticky_uid = None;
            }
            continue;
        };

        // token 惰性刷新：距过期不足 5 分钟先刷；成功则回写账号库（按过期较新者）
        // 并记一次成功，失败换号继续。
        if acc.expires_at > 0 && acc.expires_at < config::now_ms() + WB_POOL_REFRESH_SKEW_MS {
            match workbuddy_upstream::refresh_token(&acc).await {
                Ok((token, expires_at)) => {
                    acc.access_token = token;
                    acc.expires_at = expires_at;
                    workbuddy_gateway::writeback_tokens(&uid, &acc.access_token, expires_at);
                    workbuddy_gateway::note_success(&uid);
                }
                Err(e) => {
                    // 刷新失败只有一句文案（无 status/body），用 classify 从文案里
                    // 认 12153/session 失效等标记：SessionDead 会标需重登出池，
                    // 纯网络失败（code=-1）判 None 不标（契约 §4）。
                    let kind = workbuddy_errors::classify(0, &e);
                    workbuddy_gateway::note_error(&uid, &model, kind, &e);
                    last_err = Some(format!("token 刷新失败：{e}"));
                    if sticky_uid.as_deref() == Some(uid.as_str()) {
                        workbuddy_gateway::sticky_clear(&sticky_key);
                        sticky_uid = None;
                    }
                    continue;
                }
            }
        }

        let out_body =
            workbuddy_upstream::prepare_body(&payload, &acc.region, cfg.sanitize_enabled);
        match workbuddy_upstream::chat_stream(&acc, &out_body).await {
            // 上游 >=400：body 已按 ErrKind 分类
            Ok(workbuddy_upstream::ChatOutcome::Error { status, body }) => {
                let kind = workbuddy_errors::classify(status, &body);
                let friendly = workbuddy_errors::friendly_message(kind, status, &body);
                workbuddy_gateway::note_error(&uid, &model, kind, &body);
                // 11102 跨区不匹配自愈（workbuddy-pool-region 契约 §3.3 T3）：翻转区域
                // 继续在本轮 picks 内重试（tried 机制照旧）。判据取本轮实际使用的
                // acc.region：filter 已有约束则取对侧，无约束（Unknown）则从账号区域反推。
                if body.contains("11102") {
                    let flipped = workbuddy_gateway::region_flip(region_filter, &acc.region);
                    eprintln!(
                        "[workbuddy-pool] 11102 region mismatch: uid={uid} acc_region={} filter→{:?}，翻转区域重试",
                        acc.region, flipped
                    );
                    region_filter = flipped;
                }
                if let Some(fatal) = workbuddy_pool_fatal_status(kind) {
                    let latency = started.elapsed().as_millis() as u64;
                    log_gateway_request(
                        "workbuddy-pool",
                        key_id,
                        &log_model(region_filter),
                        false,
                        latency,
                        &friendly,
                    );
                    // 明确错误直接回客户端（不换号），由 handler 包成错误响应。
                    return Err((fatal, friendly));
                }
                last_err = Some(if friendly.is_empty() {
                    format!("上游返回 {status}")
                } else {
                    friendly
                });
                if sticky_uid.as_deref() == Some(uid.as_str()) {
                    workbuddy_gateway::sticky_clear(&sticky_key);
                    sticky_uid = None;
                }
            }
            // 传输层失败（超时/连接被拒/DNS）：只换号，不喂熔断（契约 §4）
            Err(e) => {
                last_err = Some(e);
                if sticky_uid.as_deref() == Some(uid.as_str()) {
                    workbuddy_gateway::sticky_clear(&sticky_key);
                    sticky_uid = None;
                }
            }
            // 2xx SSE
            Ok(workbuddy_upstream::ChatOutcome::Stream(resp)) => {
                workbuddy_gateway::note_success(&uid);
                if !sticky_key.is_empty() {
                    workbuddy_gateway::sticky_set(&sticky_key, &uid);
                }
                let latency = started.elapsed().as_millis() as u64;
                log_gateway_request(
                    "workbuddy-pool",
                    key_id,
                    &log_model(region_filter),
                    true,
                    latency,
                    "",
                );
                // 租约 move 进聚合：读流期间在途名额持续被占用，聚合结束（含读流
                // 出错提前返回）时 Drop 归还 —— 与 P1 chat handler 的非流式路径
                // 同一口径（那时 `lease` 由 handler 持有到聚合返回）。
                return match workbuddy_pool_aggregate_reply(
                    workbuddy_upstream::ChatOutcome::Stream(resp),
                    &model,
                    &uid,
                    lease,
                )
                .await
                {
                    Ok(reply) => Ok(reply),
                    Err(e) => {
                        // 上游 200 但没吐出有效数据帧（空流 / 只有 [DONE]）：已记过
                        // 一条成功日志，这里补一条失败日志（与 P1 的两条日志同形态）。
                        log_gateway_request(
                            "workbuddy-pool",
                            key_id,
                            &log_model(region_filter),
                            false,
                            started.elapsed().as_millis() as u64,
                            &e,
                        );
                        Err((StatusCode::BAD_GATEWAY, e))
                    }
                };
            }
        }
    }

    // 选号耗尽（池内账号全部冷却/在途/已出池）或 3 次换号仍失败：
    // 一律 503（账号池耗尽的语义，客户端据此稍后重试），文案带最后一次失败原因。
    let detail = last_err.unwrap_or_default();
    let msg = if detail.is_empty() {
        "无可用账号（池内账号全部冷却/在途或已出池）".to_string()
    } else {
        format!("无可用账号：{detail}")
    };
    log_gateway_request(
        "workbuddy-pool",
        key_id,
        &log_model(region_filter),
        false,
        started.elapsed().as_millis() as u64,
        &msg,
    );
    Err((StatusCode::SERVICE_UNAVAILABLE, msg))
}

/// WorkBuddy 多账号池对话补全（OpenAI 兼容；cloud 上游 + 账号池）。
///
/// 本 handler 只做**薄接线**（增补 §1：chat 改调共享 dispatch）：网关 key 鉴权 →
/// 解析 body → [`workbuddy_pool_dispatch`] → 按 `stream` 渲染。选号/换号/租约/
/// 刷新/错误分级全部在 dispatch 内，与 Responses、Messages 两个 handler 共用同一
/// 份实现（契约 §13 + 增补 §1）。
///
/// 错误分级（契约 §4）见 [`workbuddy_pool_dispatch`] 的文档；成功分支的
/// `note_success` + 粘性绑定同样在 dispatch 内完成。
async fn api_workbuddy_pool_chat_completions(
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // 刻意用 Bytes 而非 `Json` extractor：畸形 body 也要能走到鉴权/参数逻辑并回
    // 400 JSON，不被 axum 的 422 抢答（与 trae_acp_chat_response 同口径，
    // 本文件 4130 行起的注释）。
    let Ok(mut payload) = serde_json::from_slice::<Value>(&body) else {
        log_gateway_request(
            "workbuddy-pool",
            None,
            "—",
            false,
            0,
            "请求体不是合法 JSON",
        );
        return glm_proxy_error("请求体不是合法 JSON".to_string(), StatusCode::BAD_REQUEST);
    };
    // 区域密钥隔离：请求体 region（cn/intl）参与鉴权——区域专用 key 跨区即拒，
    // 只用区域提示约束选号不约束钥匙会导致「用国内版 key 打到国际账号」的混淆
    // （与池模型区域切换同口径）。注意 body 先解析才能拿到 region（鉴权前置依赖）。
    let want_region = payload
        .get("region")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_string)
        .filter(|r| !r.is_empty());
    let (auth_ok, key_id) = workbuddy_pool_auth_ok_region(&headers, want_region.as_deref());
    if !auth_ok {
        let detail = match want_region.as_deref() {
            Some(r) => format!("无效的 API Key（该密钥不属于当前区域：{r}）"),
            None => "无效的 API Key".to_string(),
        };
        log_gateway_request("workbuddy-pool", key_id.as_deref(), "—", false, 0, &detail);
        return glm_proxy_error(detail, StatusCode::UNAUTHORIZED);
    }
    let want_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // dispatch 以 &mut 收体：可选 region 字段在内部读出后剥掉，不外发上游
    // （workbuddy-pool-region 契约 §3.3①）。
    match workbuddy_pool_dispatch(&mut payload, key_id.as_deref()).await {
        Ok(reply) if want_stream => workbuddy_pool_sse(&workbuddy_pool_chat_frames(&reply)),
        Ok(reply) => json_ok(workbuddy_pool_chat_value(&reply)),
        Err((status, msg)) => glm_proxy_error(msg, status),
    }
}

/// POST /workbuddy-pool/v1/responses：OpenAI Responses API
/// （Codex CLI `wire_api="responses"`）入口（增补 §3）。
///
/// 鉴权口径同 chat（`workbuddy_pool_auth_ok`，失败 401）：它和 chat/completions
/// 一样是把池内额度烧出去的对外入口；models/status/config 则是免 key 的管理三端点
/// （理由见路由注册处的鉴权策略注释）。
///
/// 流程：`workbuddy_protocols::responses_req_to_chat` 把 `input`/`instructions`/
/// `tools`/`stream` 转成内部 chat payload → [`workbuddy_pool_dispatch`] →
/// 非流式 `responses_resp_from_text` 合成 response 对象；流式按
/// `responses_stream_frames` 的固定事件序吐帧（上游本就是一次性聚合，没有逐 token
/// 增量可转，口径同 `workbuddy_proxy.rs:1014-1069` 的合成 chunk 策略）。
async fn api_workbuddy_pool_responses(headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let (auth_ok, key_id) = workbuddy_pool_auth_ok(&headers);
    if !auth_ok {
        log_gateway_request(
            "workbuddy-pool",
            key_id.as_deref(),
            "—",
            false,
            0,
            "无效的 API Key",
        );
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    // 同 chat：用 Bytes 收畸形 body，回 400 JSON 而不是 axum 的 422。
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        log_gateway_request(
            "workbuddy-pool",
            key_id.as_deref(),
            "—",
            false,
            0,
            "请求体不是合法 JSON",
        );
        return glm_proxy_error("请求体不是合法 JSON".to_string(), StatusCode::BAD_REQUEST);
    };
    let want_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut chat = workbuddy_protocols::responses_req_to_chat(&payload);
    // Responses 的 `model` 就是上游模型名（OpenAI 侧没有 Claude 槽位概念），
    // 缺失时兜底 workbuddy；转换层不回填也要保证上游拿得到模型名。
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("workbuddy")
        .to_string();
    workbuddy_pool_set_model(&mut chat, &model);
    // 请求体可选 region 字段透传（workbuddy-pool-region 契约 §3.3①）：转换层不保留
    // 未知字段，显式带过——dispatch 读出后会剥掉，不外发上游。
    if let Some(r) = payload.get("region") {
        if let Some(obj) = chat.as_object_mut() {
            obj.insert("region".to_string(), r.clone());
        }
    }
    match workbuddy_pool_dispatch(&mut chat, key_id.as_deref()).await {
        Ok(reply) if want_stream => {
            let usage = reply.usage.clone().unwrap_or(Value::Null);
            workbuddy_pool_sse(&workbuddy_protocols::responses_stream_frames(
                &reply.model_used,
                &reply.content,
                &usage,
            ))
        }
        Ok(reply) => {
            let usage = reply.usage.clone().unwrap_or(Value::Null);
            json_ok(workbuddy_protocols::responses_resp_from_text(
                &reply.model_used,
                &reply.content,
                &reply.reasoning,
                &usage,
            ))
        }
        Err((status, msg)) => glm_proxy_error(msg, status),
    }
}

/// POST /workbuddy-pool/v1/messages：Anthropic Messages API（Claude Code /
/// CC Switch）入口（增补 §3）。鉴权口径同 chat（失败 401）。
///
/// 流程：`anthropic_slot_to_model` 把 Claude 槽位名映射成上游模型名 →
/// `anthropic_req_to_chat` 转内部 chat payload（system/messages blocks/tools/
/// max_tokens/stop_sequences 全部归一）→ [`workbuddy_pool_dispatch`] →
/// 非流式 `anthropic_resp_from_text`（带 tools 时从聚合出的 `tool_calls` 嗅探出
/// `tool_use` 块）；流式按 `anthropic_stream_frames` 吐帧。
///
/// 响应体回显**请求里的槽位名**而不是上游真实模型名：Anthropic 客户端按自己请求的
/// 模型做记录/展示，回写上游名会让界面上出现「我问的不是这个模型」。上游真实模型
/// 用于选号/冷却/日志（dispatch 内已按映射后的模型记账，口径同 opencode 网关的
/// 「日志以 actual 为准」）。
async fn api_workbuddy_pool_messages(headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let (auth_ok, key_id) = workbuddy_pool_auth_ok(&headers);
    if !auth_ok {
        log_gateway_request(
            "workbuddy-pool",
            key_id.as_deref(),
            "—",
            false,
            0,
            "无效的 API Key",
        );
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    // 同 chat：用 Bytes 收畸形 body，回 400 JSON 而不是 axum 的 422。
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        log_gateway_request(
            "workbuddy-pool",
            key_id.as_deref(),
            "—",
            false,
            0,
            "请求体不是合法 JSON",
        );
        return glm_proxy_error("请求体不是合法 JSON".to_string(), StatusCode::BAD_REQUEST);
    };
    let want_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let slot = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mapped = workbuddy_protocols::anthropic_slot_to_model(
        &slot,
        &workbuddy_pool_claude_slots(),
    );
    // 缺 model（槽位空串）时兜底 workbuddy —— 与 responses 入口的 unwrap_or 口径一致，
    // 否则空模型名会直达上游（大概率 400）。Anthropic 协议要求 model 必填，这是边界兜底。
    let mapped = if mapped.trim().is_empty() {
        "workbuddy".to_string()
    } else {
        mapped
    };
    let mut chat = workbuddy_protocols::anthropic_req_to_chat(&payload);
    workbuddy_pool_set_model(&mut chat, &mapped);
    // 请求体可选 region 字段透传（workbuddy-pool-region 契约 §3.3①）：同 responses 入口，
    // dispatch 读出后剥掉，不外发上游。
    if let Some(r) = payload.get("region") {
        if let Some(obj) = chat.as_object_mut() {
            obj.insert("region".to_string(), r.clone());
        }
    }
    // 请求带 tools 时，上游可能把 tool_calls 当纯文本吐出（与 grok.rs:552 同款场景）：
    // 经 anthropic_sniff_tool_calls 补齐为 tool_use 块，Claude 客户端才不会只收到
    // 一段含 JSON 的正文。结构化 tool_calls 已由 dispatch 聚合。
    let has_tools = chat
        .get("tools")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    match workbuddy_pool_dispatch(&mut chat, key_id.as_deref()).await {
        Ok(mut reply) if want_stream => {
            let mut tool_calls = reply.tool_calls.clone();
            if has_tools && tool_calls.is_empty() {
                if let Some(calls) = workbuddy_protocols::anthropic_sniff_tool_calls(&reply.content) {
                    tool_calls = calls;
                    reply.content = String::new();
                }
            }
            let usage = reply.usage.clone().unwrap_or(Value::Null);
            // content 空而有 reasoning 时降级为正文（与 responses_resp_from_text
            // 同款口径：思考型模型只出 reasoning 时不丢内容——2026-09-24 E2E
            // 实测 haiku→kimi-k2.7 槽位出现过）。
            let stream_text = if reply.content.is_empty() && !reply.reasoning.is_empty() {
                reply.reasoning.clone()
            } else {
                reply.content.clone()
            };
            workbuddy_pool_sse(&workbuddy_protocols::anthropic_stream_frames(
                &slot, &stream_text, &usage,
            ))
        }
        Ok(mut reply) => {
            let mut tool_calls = reply.tool_calls.clone();
            if has_tools && tool_calls.is_empty() {
                if let Some(calls) = workbuddy_protocols::anthropic_sniff_tool_calls(&reply.content) {
                    tool_calls = calls;
                    reply.content = String::new();
                }
            }
            let usage = reply.usage.clone().unwrap_or(Value::Null);
            json_ok(workbuddy_protocols::anthropic_resp_from_text(
                &slot,
                &reply.content,
                &tool_calls,
                &usage,
            ))
        }
        Err((status, msg)) => glm_proxy_error(msg, status),
    }
}

/// Claude 槽位名 → 上游模型名的默认映射表（增补 §1「映射表配置化」的路由层一侧）。
///
/// 取值对齐参考实现 `go-gateway/internal/server/claude_alias_test.go` 的槽位默认值
/// （同一个 WorkBuddy cloud 上游的模型名）。**未命中的模型名原样透传**（
/// `anthropic_slot_to_model` 的 pinned 语义），所以「一键接入」直写真实模型名的
/// 客户端零损耗；槽位默认值要按账号实际可用模型调整时只改这一张表。
fn workbuddy_pool_claude_slots() -> Value {
    json!({
        "claude-sonnet-4-5": "deepseek-v4-flash",
        "claude-opus-4-1": "glm-5.2",
        "claude-haiku-4-5": "kimi-k2.7",
        "claude-fable-5": "minimax-m3",
    })
}

/// 把模型名写进转换出的 chat payload。
///
/// 两个协议转换函数（`responses_req_to_chat` / `anthropic_req_to_chat`）的 pinned
/// 职责里都没提回填 `model`，而上游缺模型名会直接 400 —— 路由层显式补齐，
/// 转换层以后真回了也只是同值覆盖。
fn workbuddy_pool_set_model(chat: &mut Value, model: &str) {
    if let Some(obj) = chat.as_object_mut() {
        obj.insert("model".to_string(), json!(model));
    }
}

/// 聚合产物 → OpenAI 非流式 `chat.completion`。
///
/// 口径同 P1 的聚合器输出：消息体带 role/content，
/// 有推理段时补 `reasoning_content`，有工具调用时补 `tool_calls` 并把 finish_reason
/// 抬成 `tool_calls`，`usage` 上游给了才带（缺失不谎报 0）。
///
/// `id`/`created` 本地生成（`chatcmpl-wbpool-<now_ms>` / `now_ms()/1000`）：聚合
/// 产物按增补 §1 只回模型名，不回上游帧的 id，而这两个值对客户端都是不透明的。
fn workbuddy_pool_chat_value(reply: &WorkbuddyPoolReply) -> Value {
    let has_tools = !reply.tool_calls.is_empty();
    let finish_reason = if has_tools { "tool_calls" } else { "stop" };
    let mut message = json!({ "role": "assistant", "content": reply.content });
    if !reply.reasoning.is_empty() {
        message["reasoning_content"] = json!(reply.reasoning);
    }
    if has_tools {
        message["tool_calls"] = json!(reply.tool_calls);
    }
    let mut out = json!({
        "id": format!("chatcmpl-wbpool-{}", config::now_ms()),
        "object": "chat.completion",
        "created": config::now_ms() / 1000,
        "model": reply.model_used,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
    });
    if let Some(u) = &reply.usage {
        out["usage"] = u.clone();
    }
    out
}

/// 聚合产物 → OpenAI 流式 SSE 帧（每帧 `data: {json}\n\n`，`data: [DONE]` 收尾）。
///
/// 合成 chunk 的口径仿 `workbuddy_proxy.rs:1014-1069 chat_completions_stream`
/// （OpenCode 侧本就是一次性聚合后手搓三帧）：role 帧 → 推理帧（有才吐）→
/// content 帧 → tool_calls 帧（每个调用一帧）→ 收尾帧（finish_reason + usage）。
///
/// 为什么 chat 流式也走聚合：dispatch 是三个协议共用的同一份选号循环，出口按增补
/// §1 统一为聚合产物，各协议的流式帧由各自的渲染函数生成；上游 200 但没吐有效帧
/// 时 dispatch 已经回 502，不会给客户端吐一条假流。
fn workbuddy_pool_chat_frames(reply: &WorkbuddyPoolReply) -> Vec<String> {
    /// 一帧 OpenAI `chat.completion.chunk`（usage 只在收尾帧带）。
    fn frame(
        id: &str,
        created: i64,
        model: &str,
        delta: Value,
        finish: Option<&str>,
        usage: Option<&Value>,
    ) -> String {
        let mut chunk = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        if let Some(u) = usage {
            chunk["usage"] = u.clone();
        }
        format!("data: {chunk}\n\n")
    }

    let id = format!("chatcmpl-wbpool-{}", config::now_ms());
    let created = config::now_ms() / 1000;
    let mut frames = vec![frame(
        &id,
        created,
        &reply.model_used,
        json!({"role": "assistant"}),
        None,
        None,
    )];
    if !reply.reasoning.is_empty() {
        frames.push(frame(
            &id,
            created,
            &reply.model_used,
            json!({"reasoning_content": reply.reasoning}),
            None,
            None,
        ));
    }
    if !reply.content.is_empty() {
        frames.push(frame(
            &id,
            created,
            &reply.model_used,
            json!({"content": reply.content}),
            None,
            None,
        ));
    }
    // BTreeMap 已按 index 升序，逐个工具调用一帧（多个并行调用不串槽）。
    for call in &reply.tool_calls {
        frames.push(frame(
            &id,
            created,
            &reply.model_used,
            json!({"tool_calls": [call]}),
            None,
            None,
        ));
    }
    let finish = if reply.tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    frames.push(frame(
        &id,
        created,
        &reply.model_used,
        json!({}),
        Some(finish),
        reply.usage.as_ref(),
    ));
    frames.push("data: [DONE]\n\n".to_string());
    frames
}

/// 帧序列 → `text/event-stream` 响应（三个协议的流式出口共用）。
///
/// 一次性整体写出而不是逐帧转流：三个协议的流式都是「聚合成型后吐帧」，没有逐 token
/// 增量可转（同源取舍见 `workbuddy_proxy.rs:1014-1069`）；客户端按 SSE 规范自行
/// 拼帧，观感与逐帧推送一致。
fn workbuddy_pool_sse(frames: &[String]) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream".to_string())],
        axum::body::Body::from(frames.concat()),
    )
        .into_response()
}

/// 取流式 tool_call 分片的 index：兼容数字 / 字符串数字 / 缺省三种上游形态
/// （口径同参考实现 upstream/sse.rs:239 index_of_tool_call —— 只认数字时多个
/// 并行工具调用会被静默合并进槽位 0）。
fn workbuddy_pool_tool_call_index(call: &serde_json::Map<String, Value>) -> i64 {
    match call.get("index") {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok().filter(|&n| n >= 0).unwrap_or(0),
        _ => 0,
    }
}

/// 把流式 tool_call 片段合并到累计对象：id/type/function.name 直覆盖，
/// function.arguments 拼接（口径同参考实现 upstream/sse.rs:252 merge_tool_call_delta）。
fn workbuddy_pool_merge_tool_call(merged: &mut Value, delta: &serde_json::Map<String, Value>) {
    let Some(m) = merged.as_object_mut() else {
        return;
    };
    if let Some(v) = delta.get("id").and_then(Value::as_str) {
        if !v.is_empty() {
            m.insert("id".to_string(), Value::String(v.to_string()));
        }
    }
    if let Some(v) = delta.get("type").and_then(Value::as_str) {
        if !v.is_empty() {
            m.insert("type".to_string(), Value::String(v.to_string()));
        }
    }
    let Some(df) = delta.get("function").and_then(Value::as_object) else {
        return;
    };
    if m.get("function").and_then(Value::as_object).is_none() {
        m.insert("function".to_string(), Value::Object(serde_json::Map::new()));
    }
    let Some(mf) = m.get_mut("function").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(v) = df.get("name").and_then(Value::as_str) {
        if !v.is_empty() {
            mf.insert("name".to_string(), Value::String(v.to_string()));
        }
    }
    if let Some(v) = df.get("arguments").and_then(Value::as_str) {
        if !v.is_empty() {
            let prev = mf.get("arguments").and_then(Value::as_str).unwrap_or("");
            mf.insert("arguments".to_string(), Value::String(format!("{prev}{v}")));
        }
    }
}

/// 上游 SSE 聚合器：把 delta 帧累加成 [`WorkbuddyPoolReply`] 的字段。
///
/// 解析段仿 `grok::chat_completions_unified`（core grok.rs:800-890）：按行取
/// `data:` 帧 → JSON 解析 → 指针取值累加；另兼容 tool_calls 的 index 合并与
/// `usage` 透出（参考实现 upstream/sse.rs:83 aggregate）。
struct WorkbuddyPoolAggregate {
    /// 客户端请求的模型名：上游帧没带 model 时回填，避免响应里出现空模型。
    requested_model: String,
    /// 上游帧带的模型名：优先于 requested_model 回填（上游可能改写模型名）。
    upstream_model: String,
    content: String,
    reasoning: String,
    usage: Option<Value>,
    valid_events: usize,
    tool_calls: std::collections::BTreeMap<i64, Value>,
}

impl WorkbuddyPoolAggregate {
    fn new(model: &str) -> Self {
        Self {
            requested_model: model.to_string(),
            upstream_model: String::new(),
            content: String::new(),
            reasoning: String::new(),
            usage: None,
            valid_events: 0,
            tool_calls: std::collections::BTreeMap::new(),
        }
    }

    /// 喂一行 SSE（已去掉行尾换行）；遇到 `data: [DONE]` 返回 false = 可停止读取。
    fn feed_line(&mut self, line: &str) -> bool {
        let Some(payload) = line.strip_prefix("data:") else {
            return true; // 注释行 / 空行 / event: 行：忽略
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            return false; // 上游显式结束，之后的数据一律忽略
        }
        if payload.is_empty() {
            return true;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            return true; // 解析失败静默跳过（不计有效事件）
        };
        self.valid_events += 1;
        if let Some(v) = chunk.get("model").and_then(Value::as_str) {
            if !v.is_empty() && self.upstream_model.is_empty() {
                self.upstream_model = v.to_string();
            }
        }
        if let Some(u) = chunk.get("usage") {
            if u.is_object() {
                self.usage = Some(u.clone());
            }
        }
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return true;
        };
        for choice in choices {
            let Some(c) = choice.as_object() else {
                continue;
            };
            let Some(delta) = c.get("delta").and_then(Value::as_object) else {
                continue;
            };
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                self.content.push_str(text);
            }
            if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
                self.reasoning.push_str(r);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let Some(call) = call.as_object() else {
                        continue;
                    };
                    let idx = workbuddy_pool_tool_call_index(call);
                    let slot = self
                        .tool_calls
                        .entry(idx)
                        .or_insert_with(|| json!({ "index": idx }));
                    workbuddy_pool_merge_tool_call(slot, call);
                }
            }
        }
        true
    }

    /// 合成共享 dispatch 的聚合产物（增补 §1 的 [`WorkbuddyPoolReply`]）。
    ///
    /// OpenAI 特有的 id/created/finish_reason/role 不在这里出：三个协议各渲染一遍
    /// 的话同一份语义要写三份（必然漂移），统一在渲染层按协议生成。
    fn into_reply(self, uid: &str) -> Result<WorkbuddyPoolReply, String> {
        if self.valid_events == 0 {
            // 上游 200 但没有任何有效数据帧（空流 / 只有 [DONE]）：不合成空 content
            // 的假成功（口径同参考实现 sse.rs:189）。
            return Err("上游流未返回任何有效数据帧".to_string());
        }
        let model = if self.upstream_model.is_empty() {
            if self.requested_model.is_empty() {
                "workbuddy".to_string()
            } else {
                self.requested_model
            }
        } else {
            self.upstream_model
        };
        Ok(WorkbuddyPoolReply {
            content: self.content,
            reasoning: self.reasoning,
            // BTreeMap 按 index 升序输出，多个并行工具调用不串槽。
            tool_calls: self.tool_calls.into_values().collect(),
            usage: self.usage,
            model_used: model,
            uid: uid.to_string(),
        })
    }
}

/// 非流式聚合（三个协议共用）：reqwest 行读上游 SSE，累加 content /
/// reasoning_content / tool_calls 后合成 [`WorkbuddyPoolReply`]。
///
/// 参数收 `ChatOutcome` 而不是裸 `reqwest::Response`：server crate 不直接依赖
/// reqwest，写不出后者的类型签名（`ChatOutcome` 在依赖链里可命名）。
/// 读块同样走 `read_stream_chunk`（60s 空闲超时），与流式转发路径同一套断流口径；
/// chunk 边界与 SSE 帧边界不对齐，必须自己按 `\n` 切行（跨 chunk 的半行留在缓冲里），
/// 按字节累积再逐行 `from_utf8_lossy`，避免多字节字符被 chunk 边界切断后乱码。
///
/// `_lease` 的所有权在这里交接（P1 的 chat handler 是自己在栈上持有到聚合返回）：
/// 读流期间在途名额持续被占用，函数返回（含读流出错）时 Drop 恰好归还一次。
async fn workbuddy_pool_aggregate_reply(
    outcome: workbuddy_upstream::ChatOutcome,
    model: &str,
    uid: &str,
    _lease: WorkbuddyPoolLease,
) -> Result<WorkbuddyPoolReply, String> {
    // 只应在成功分支进入；Error 变体是兜底（理论上到不了）。
    let mut resp = match outcome {
        workbuddy_upstream::ChatOutcome::Stream(resp) => resp,
        workbuddy_upstream::ChatOutcome::Error { status, body } => {
            let kind = workbuddy_errors::classify(status, &body);
            return Err(workbuddy_errors::friendly_message(kind, status, &body));
        }
    };
    let mut agg = WorkbuddyPoolAggregate::new(model);
    let mut buf: Vec<u8> = Vec::new();
    'read: while let Some(chunk) = workbuddy_upstream::read_stream_chunk(
        &mut resp,
        workbuddy_upstream::CHAT_IDLE_TIMEOUT,
    )
    .await?
    {
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw);
            if !agg.feed_line(line.trim_end_matches(['\r', '\n'])) {
                break 'read;
            }
        }
    }
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        agg.feed_line(line.trim_end_matches(['\r', '\n']));
    }
    agg.into_reply(uid)
}

/// GET /workbuddy-pool/v1/models[?region=cn|intl]：**按区聚合**（workbuddy-pool-region
/// 契约 §3.4）。cn 区取首个成功 cn 账号动态目录（全失败回落 builtin 内置表，
/// source=fallback）；intl 区取首个成功 intl 账号动态目录（无 intl 账号/全失败回落
/// `intl_static_models()`，source=static——本机当前无国际版账号时这正是正常态，
/// 契约 §五-6）。每条目加 `region:"cn"|"intl"` 字段；跨区同名（glm-5.3）保留两条；
/// 两区独立成败互不阻塞（并发拉取）。保持 OpenAI list 扁平形状（region/regions 均
/// 为附加字段，不带 region 参数的响应 = 两区全量，回归红线 1）。
///
/// `?region=cn|intl` 过滤：data 只含单区条目，regions 元数据只含单区；缺席/非法值 =
/// 两区全量。
///
/// 旧版「首个成功账号目录 + merge_intl_static_models 混入同一扁平列表」的混入语义
/// 退役（区域混杂、无隔离，契约 §一缺口 1）——聚合纯函数在 core
/// `workbuddy_gateway::{region_models_part, assemble_region_models}`（fetch 注入，G1 单测覆盖）。
///
/// 免网关 key（与 status/config 同属管理三端点，理由见路由注册处的鉴权策略注释）：
/// 契约 §13 路由表只对 chat/completions 标注「鉴权(网关 key)」，且本文件 WorkBuddy
/// 家族另外两个模型目录端点（`/workbuddy-native/v1/models` 本文件 256 行、
/// `/api/workbuddy-proxy/models` 本文件 262 行）同样免 key。
async fn api_workbuddy_pool_models(RawQuery(query): RawQuery) -> Response {
    let only = workbuddy_pool_models_region_query(query.as_deref().unwrap_or(""));
    let accounts = workbuddy_gateway::load_pool_accounts();
    let want_cn = only != workbuddy_gateway::RegionFilter::Intl;
    let want_intl = only != workbuddy_gateway::RegionFilter::Cn;

    // 两区独立成败，互不阻塞（§3.1/§3.4）：并发拉取；被过滤的区直接跳过（None）。
    let (cn_fetch, intl_fetch) = tokio::join!(
        async {
            if want_cn {
                workbuddy_pool_fetch_region_models(&accounts, "cn").await
            } else {
                None
            }
        },
        async {
            if want_intl {
                workbuddy_pool_fetch_region_models(&accounts, "intl").await
            } else {
                None
            }
        },
    );

    // cn 回落 = builtin 内置表（§3.4）；intl 回落 = 静态表。聚合与 region 标注全在
    // core 纯函数里（region_models_part 负责条目加 region 字段与区内去重）。
    // 观测（不加新 tag，走 error 备注）：各区末次失败原因 + 目录来源与条目数。
    let mut note = String::new();
    for (fetch, region) in [(&cn_fetch, "cn"), (&intl_fetch, "intl")] {
        if let Some((uid, Err(e))) = fetch {
            note.push_str(&format!("{region} 区动态目录失败 uid={uid}: {e} | "));
        }
    }
    let builtin = workbuddy_proxy::builtin_models_payload();
    let builtin_items = builtin
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let cn_part = workbuddy_gateway::region_models_part(
        workbuddy_gateway::RegionFilter::Cn,
        cn_fetch,
        builtin_items,
    );
    let intl_static = workbuddy_upstream::intl_static_models();
    let intl_part = workbuddy_gateway::region_models_part(
        workbuddy_gateway::RegionFilter::Intl,
        intl_fetch,
        intl_static,
    );
    note.push_str(&format!(
        "cn={}:{} intl={}:{}",
        cn_part.source,
        cn_part.items.len(),
        intl_part.source,
        intl_part.items.len()
    ));
    log_gateway_request("workbuddy-pool", None, "models", true, 0, &note);

    let payload = workbuddy_gateway::assemble_region_models(cn_part, intl_part, only);
    json_ok(payload)
}

/// `?region=cn|intl` 解析：合法值 → 单区过滤；缺席/非法 → Unknown（两区全量，
/// 客户端容错优先）。
fn workbuddy_pool_models_region_query(query: &str) -> workbuddy_gateway::RegionFilter {
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k == "region" {
            if let Some(f) = workbuddy_gateway::RegionFilter::from_hint(Some(v)) {
                return f;
            }
        }
    }
    workbuddy_gateway::RegionFilter::Unknown
}

/// `GET/POST /api/workbuddy-pool/region-key?region=cn|intl`：取（或按需创建）该区
/// 专用接入密钥。GET 找不到该区启用中的 key 时返回 `{key:null, created:false}`；
/// POST（或 `?create=1`）时找不到即按该区新建并返回 `{key, created:true}`。
/// 设计动机：用户要「国内版一个密钥、国际版一个密钥」的明确区分，不能共用一把。
async fn api_workbuddy_pool_region_key(RawQuery(query): RawQuery) -> Response {
    let create = query
        .as_deref()
        .unwrap_or_default()
        .split('&')
        .any(|pair| pair == "create=1" || pair == "create=true");
    let region = query
        .as_deref()
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| pair.strip_prefix("region="))
        .map(str::trim)
        .map(str::to_ascii_lowercase);
    let Some(region) = region.filter(|r| r == "cn" || r == "intl") else {
        return json_err("缺少合法 region 参数（cn|intl）".to_string(), StatusCode::BAD_REQUEST);
    };
    let region_name = if region == "cn" { "国内版" } else { "国际版" };
    // 已有该区启用中的 key → 直接复用（多张卡不会互相顶号）。
    if let Some(existing) = gateway_admin::load_keys().into_iter().find(|k| {
        k.get("enabled").and_then(Value::as_bool).unwrap_or(true)
            && k.get("region").and_then(Value::as_str) == Some(region.as_str())
    }) {
        let mut out = existing.clone();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("created".to_string(), json!(false));
        }
        return json_ok(json!({ "ok": true, "key": out }));
    }
    if !create {
        return json_ok(json!({ "ok": true, "key": Value::Null, "created": false }));
    }
    match gateway_admin::create_key_scoped(&format!("WorkBuddy {region_name}专用"), Some(&region)) {
        Ok(key) => json_ok(json!({ "ok": true, "key": key, "created": true })),
        Err(e) => json_err(e, StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// 单区动态目录拉取（§3.4）：该区**首个成功**账号的 `fetch_models`（打对应区域端点，
/// 账号 region 由登录域派生）；全部失败 → `(末次账号 uid, Err)`；该区无账号 → None
/// （两区独立成败，调用方按区各自回落）。
async fn workbuddy_pool_fetch_region_models(
    accounts: &[workbuddy_gateway::PoolAccount],
    region: &str,
) -> Option<(String, Result<Value, String>)> {
    let mut last: Option<(String, Result<Value, String>)> = None;
    for acc in accounts.iter().filter(|a| a.region == region) {
        match workbuddy_upstream::fetch_models(acc).await {
            Ok(v) => return Some((acc.uid.clone(), Ok(v))),
            Err(e) => last = Some((acc.uid.clone(), Err(e))),
        }
    }
    last
}

/// GET /workbuddy-pool/v1/status：池状态 + 当前配置（grok 式 `pool_status()` +
/// `load_config()` 合并成一份 JSON，前端一个请求拿全）。
///
/// 增 regions 聚合（workbuddy-pool-region 契约 §3.5）：`regions.cn/intl` 的
/// `accounts/healthy/models`——在 core `pool_status()`（status_value）内完成，本端点
/// 原样透传。
///
/// ⚠️ 必须**扁平**返回（契约 §13「pool_status() + config」= 同层合并），不要在池数据
/// 外面套 `{"pool": ...}` 信封：前端 `WorkbuddyPage.tsx:163` 拿到响应体直接
/// `setPoolStatus(res)`，随后 :476/:481/:487/:493/:499/:525 读 `poolStatus.total` /
/// `.healthy` / `.cooling` / `.modelCooling` / `.breaker` / `.accounts`（类型见
/// `api.ts:1175 WorkbuddyPoolStatusResponse extends WorkbuddyPoolStatus`，这些字段
/// 声明在**响应体顶层**）。套信封会让五个计数器恒显「—」、账号表恒显「暂无账号」。
/// 同 Grok 池的既有约定：`api_grok_pool_overview`（本文件 1700 行）也是
/// `json_ok(grok_pool::pool_overview())` 扁平返回。regions 是**新增附加字段**，
/// 不改任何既有字段的层级（红线：前端直接 setPoolStatus）。
async fn api_workbuddy_pool_status() -> Response {
    let cfg = workbuddy_gateway::load_config();
    let mut out = workbuddy_gateway::pool_status();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("config".to_string(), workbuddy_pool_config_json(&cfg));
        // 访问密钥：面板「接入信息」区块展示用（前端 `poolStatus.apiKey`，缺省则显示
        // 「未获取到」门槛说明）。取 GLM 主密钥 —— 本路由组的鉴权接受它或网关专用 Key
        // （`gateway_key_status`）。与 `/api/glm-proxy/config`（本文件 5621 行）同一
        // 口径：本服务仅绑 127.0.0.1，不构成新的暴露面。
        obj.insert(
            "apiKey".to_string(),
            json!(glm_proxy::load_config().api_key),
        );
    }
    json_ok(out)
}

/// POST /workbuddy-pool/v1/config：PATCH 配置（clamp 边界同契约 §5），保存后返回新配置。
///
/// 边界：mode 枚举校验（非法值整体拒绝，不静默改写模式）、soft_rate_max >= base、
/// breaker_max >= base、breaker_threshold >= 1、sticky_ttl 60_000..3_600_000、
/// balance_refresh_minutes >= 5。
async fn api_workbuddy_pool_config_save(Json(body): Json<Value>) -> Response {
    let mut cfg = workbuddy_gateway::load_config();
    if let Some(mode) = body.get("mode").and_then(Value::as_str) {
        if !WB_POOL_MODES.contains(&mode) {
            return json_err(
                format!("mode 仅支持 {}（收到 {mode}）", WB_POOL_MODES.join(" / ")),
                StatusCode::BAD_REQUEST,
            );
        }
        cfg.mode = mode.to_string();
    }
    if let Some(v) = body.get("pinnedUid").and_then(Value::as_str) {
        cfg.pinned_uid = v.trim().to_string();
    }
    if let Some(v) = body.get("softRateBaseSecs").and_then(Value::as_u64) {
        cfg.soft_rate_base_secs = v;
    }
    if let Some(v) = body.get("softRateMaxSecs").and_then(Value::as_u64) {
        cfg.soft_rate_max_secs = v;
    }
    // 上限不得低于下限：否则指数退避永远到不了顶，语义自相矛盾。
    if cfg.soft_rate_max_secs < cfg.soft_rate_base_secs {
        cfg.soft_rate_max_secs = cfg.soft_rate_base_secs;
    }
    if let Some(v) = body.get("breakerThreshold").and_then(Value::as_i64) {
        cfg.breaker_threshold = v.max(1);
    }
    if let Some(v) = body.get("breakerBaseSecs").and_then(Value::as_u64) {
        cfg.breaker_base_secs = v;
    }
    if let Some(v) = body.get("breakerMaxSecs").and_then(Value::as_u64) {
        cfg.breaker_max_secs = v;
    }
    if cfg.breaker_max_secs < cfg.breaker_base_secs {
        cfg.breaker_max_secs = cfg.breaker_base_secs;
    }
    // max_in_flight 不 clamp：core 侧 0 = 不限（workbuddy_gateway.rs:121），
    // 强行抬到 1 会把「不限」变成「只允许 1 个在途」。
    if let Some(v) = body.get("maxInFlight").and_then(Value::as_i64) {
        cfg.max_in_flight = v.max(0);
    }
    if let Some(v) = body.get("stickyTtlMs").and_then(Value::as_i64) {
        cfg.sticky_ttl_ms = v.clamp(WB_POOL_STICKY_TTL_MIN_MS, WB_POOL_STICKY_TTL_MAX_MS);
    }
    if let Some(v) = body.get("sanitizeEnabled").and_then(Value::as_bool) {
        cfg.sanitize_enabled = v;
    }
    if let Some(v) = body.get("balanceRefreshMinutes").and_then(Value::as_u64) {
        // 0 = 关闭巡检（core normalized 与 patrol_interval_ms 均认 0）；其余钳 [5, 1440]
        cfg.balance_refresh_minutes = if v == 0 {
            0
        } else {
            v.clamp(WB_POOL_BALANCE_REFRESH_MIN_MINUTES, 1440)
        };
    }
    if let Err(e) = workbuddy_gateway::save_config(&cfg) {
        return json_err(e, StatusCode::INTERNAL_SERVER_ERROR);
    }
    json_ok(json!({
        "ok": true,
        "config": workbuddy_pool_config_json(&cfg),
    }))
}

// ---------------------------------------------------------------------------
// OpenCode 网关（/opencode/v1）—— 本机 OpenCode server 的**免费模型专用**入口
//
// 与 WorkBuddy 网关同级、独立密钥/独立配置；server 进程与看门狗由 workbuddy_proxy 统一管理，
// 本网关只做「免费收敛 + 独立鉴权 + 对话委托」。
// ---------------------------------------------------------------------------

/// 鉴权判定：`Some(true)` = 用了本网关自有密钥；`Some(false)` = 用了兼容密钥（主网关 /v1 的）；
/// `None` = 不通过。空密钥视为放行（与传统各网关一致）。
fn opencode_proxy_auth_result(
    headers: &HeaderMap,
    cfg: &opencode::OpenCodeProxyConfig,
) -> Option<bool> {
    if cfg.api_key.is_empty() {
        return Some(true);
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let xkey = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let got = bearer.or(xkey);
    let Some(got) = got else {
        return None;
    };
    if got == cfg.api_key {
        return Some(true);
    }
    // 兼容：WorkBuddy 对 `vendor = "ainb"` 的多条目历史上可能只发出主网关密钥，
    // 导致本网关（后加、独立密钥）被判 401。此处接受主网关密钥以免阻塞接入，
    // 并在调用侧记一条日志（不是静默放行）。
    if got == glm_proxy::load_config().api_key {
        return Some(false);
    }
    None
}

/// 鉴权失败时输出「收到的鉴权头长什么样」的诊断串（**只输出前缀与长度，不落全量密钥**），
/// 用于快速定位「客户端到底发没发密钥 / 发的是哪个密钥」。
fn opencode_auth_diag(headers: &HeaderMap, cfg: &opencode::OpenCodeProxyConfig) -> String {
    let mask = |s: &str| -> String {
        let n = s.chars().count();
        let p: String = s.chars().take(6).collect();
        format!("{p}…(len={n})")
    };
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| mask(v.trim_start_matches("Bearer ")))
        .unwrap_or_else(|| "<无>".to_string());
    let xkey = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(mask)
        .unwrap_or_else(|| "<无>".to_string());
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(40).collect::<String>())
        .unwrap_or_else(|| "<无>".to_string());
    let mut names: Vec<String> = headers.keys().map(|k| k.as_str().to_string()).collect();
    names.sort();
    format!(
        "无效的 API Key ｜ 期望={} 实收 Authorization={auth} x-api-key={xkey} UA={ua} 头列表=[{}]",
        mask(&cfg.api_key),
        names.join(",")
    )
}

/// 兼容旧调用点的布尔包装（当前无调用点，保留以便未来复用）。
#[allow(dead_code)]
fn opencode_proxy_auth_ok(headers: &HeaderMap, cfg: &opencode::OpenCodeProxyConfig) -> bool {
    opencode_proxy_auth_result(headers, cfg).is_some()
}

/// 「客户端选错模型」用 400，「上游/server 故障」用 502 —— 便于调用方区分该改模型还是该重试。
fn opencode_error_status(msg: &str) -> StatusCode {
    if msg.contains("仅提供免费模型") || msg.contains("未指定模型") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    }
}

/// /opencode/v1 上的 DuMate 档位模型转发（1.0.10）。
///
/// 复用 dumate 的核心实现（JSON / SSE 分流 + 错误状态映射），差异只有两点：
/// ① 日志记在 `opencode` 渠道下；② **不重复鉴权**（外层 opencode handler 已完成）。
async fn opencode_dumate_bridge(
    model: &str,
    body: &Value,
    started: std::time::Instant,
) -> Response {
    if !dumate::load_config().enabled {
        let msg = "DuMate 网关未启用（可在「DuMate」页开启）".to_string();
        log_gateway_request("opencode", None, model, false, 0, &msg);
        return glm_proxy_error(msg, StatusCode::SERVICE_UNAVAILABLE);
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream {
        match dumate::chat_completions_stream(model, body).await {
            Ok((upstream, actual)) => {
                log_gateway_request(
                    "opencode",
                    None,
                    &actual,
                    true,
                    started.elapsed().as_millis() as u64,
                    "dumate 档位转发",
                );
                let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                });
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }
            Err(e) => {
                let status = dumate_error_status(&e.message);
                log_gateway_request(
                    "opencode",
                    None,
                    model,
                    false,
                    started.elapsed().as_millis() as u64,
                    &e.message,
                );
                glm_proxy_error(e.message, status)
            }
        }
    } else {
        match dumate::chat_completions_json(model, body).await {
            Ok(value) => {
                let actual = value
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(model)
                    .to_string();
                log_gateway_request(
                    "opencode",
                    None,
                    &actual,
                    true,
                    started.elapsed().as_millis() as u64,
                    "dumate 档位转发",
                );
                json_ok(value)
            }
            Err(e) => {
                let status = dumate_error_status(&e.message);
                log_gateway_request(
                    "opencode",
                    None,
                    model,
                    false,
                    started.elapsed().as_millis() as u64,
                    &e.message,
                );
                glm_proxy_error(e.message, status)
            }
        }
    }
}

/// GET /opencode/v1/models：只列免费模型。
async fn api_opencode_v1_models(headers: HeaderMap) -> Response {
    let cfg = opencode::load_config();
    if !cfg.enabled {
        let msg = "OpenCode 网关未启用（可在「OpenCode」页开启）";
        log_gateway_request("opencode", None, "models", false, 0, msg);
        return glm_proxy_error(msg.to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    match opencode_proxy_auth_result(&headers, &cfg) {
        Some(true) => {}
        Some(false) => log_gateway_request(
            "opencode",
            None,
            "models",
            true,
            0,
            "鉴权通过（兼容主网关密钥；建议在「OpenCode」页用本网关自有密钥）",
        ),
        None => {
            let diag = opencode_auth_diag(&headers, &cfg);
            log_gateway_request("opencode", None, "models", false, 0, &diag);
            return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
        }
    }
    match opencode::models_payload().await {
        Ok(mut value) => {
            // 1.0.10：并入 DuMate 的 4 个档位模型（dumate-lite/turbo/ultra/points），
            // 让同一 base_url + 同一把 key 就能选用；取自 dumate 自己的静态目录，
            // 前置展示便于客户端一眼看到。
            if dumate::load_config().enabled {
                let extra = dumate::models_payload()
                    .get("data")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !extra.is_empty() {
                    if let Some(list) = value.get_mut("data").and_then(Value::as_array_mut) {
                        let mut merged = extra;
                        merged.append(list);
                        *list = merged;
                    }
                }
            }
            let n = value.get("data").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            log_gateway_request("opencode", None, "models", true, 0, "");
            let _ = n;
            json_ok(value)
        }
        Err(e) => {
            log_gateway_request("opencode", None, "models", false, 0, &e);
            glm_proxy_error(e, StatusCode::BAD_GATEWAY)
        }
    }
}

/// POST /opencode/v1/chat/completions：只放行免费模型。
async fn api_opencode_chat_completions(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let started = std::time::Instant::now();
    let cfg = opencode::load_config();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if !cfg.enabled {
        let msg = "OpenCode 网关未启用（可在「OpenCode」页开启）";
        log_gateway_request("opencode", None, &model, false, 0, msg);
        return glm_proxy_error(msg.to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    match opencode_proxy_auth_result(&headers, &cfg) {
        Some(true) => {}
        Some(false) => log_gateway_request(
            "opencode",
            None,
            &model,
            true,
            0,
            "鉴权通过（兼容主网关密钥；建议在「OpenCode」页用本网关自有密钥）",
        ),
        None => {
            let diag = opencode_auth_diag(&headers, &cfg);
            log_gateway_request("opencode", None, &model, false, 0, &diag);
            return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
        }
    }

    // 1.0.10：DuMate 的 4 个档位模型（dumate-lite/turbo/ultra/points）也走本入口，
    // 以 `dumate-` 前缀分流到 dumate 核心（外层已鉴权，不再重复校验）。
    if model.starts_with("dumate-") {
        return opencode_dumate_bridge(&model, &body, started).await;
    }

    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream {
        match opencode::chat_completions_stream(body).await {
            // 0.1.85：fallback 可能已换模型，日志与 SSE 帧都以 actual 为准
            Ok((upstream, actual)) => {
                log_gateway_request(
                    "opencode",
                    None,
                    &actual,
                    true,
                    started.elapsed().as_millis() as u64,
                    "",
                );
                let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                });
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }
            Err(e) => {
                log_gateway_request(
                    "opencode",
                    None,
                    &model,
                    false,
                    started.elapsed().as_millis() as u64,
                    &e,
                );
                glm_proxy_error(e.clone(), opencode_error_status(&e))
            }
        }
    } else {
        match opencode::chat_completions_json(body).await {
            Ok(value) => {
                let actual = value
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(&model)
                    .to_string();
                log_gateway_request(
                    "opencode",
                    None,
                    &actual,
                    true,
                    started.elapsed().as_millis() as u64,
                    "",
                );
                json_ok(value)
            }
            Err(e) => {
                log_gateway_request(
                    "opencode",
                    None,
                    &model,
                    false,
                    started.elapsed().as_millis() as u64,
                    &e,
                );
                glm_proxy_error(e.clone(), opencode_error_status(&e))
            }
        }
    }
}

async fn api_opencode_proxy_status() -> Response {
    json_ok(opencode::proxy_status().await)
}

/// GET /api/opencode-proxy/config：接入信息（Base URL / 密钥 / 启用态）。
/// 只读配置、不触发服务发现，保证页面秒开。
async fn api_opencode_proxy_config_get() -> Response {
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(opencode::config_info(port))
}

/// POST /api/opencode-proxy/config：启用开关 / 重新生成密钥 / 手动设置密钥。
async fn api_opencode_proxy_config_save(Json(body): Json<Value>) -> Response {
    let mut cfg = opencode::load_config();
    if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if body.get("regenerateKey").and_then(Value::as_bool) == Some(true) {
        cfg.api_key = glm_proxy::generate_api_key();
    } else if let Some(key) = body.get("apiKey").and_then(Value::as_str) {
        let key = key.trim();
        if !key.is_empty() {
            cfg.api_key = key.to_string();
        }
    }
    if let Err(e) = opencode::save_config(&cfg) {
        return json_err(e, StatusCode::INTERNAL_SERVER_ERROR);
    }
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(opencode::config_info(port))
}

// ---------------------------------------------------------------------------
// DuMate（百度搭子）网关（/dumate/v1）+ 管理面（/api/dumate/*）
//
// 契约 docs/dumate-channel-contract.md §3.2：鉴权三态对齐 opencode（自有密钥
// 或主网关密钥皆放行）；chat handler 对齐 api_opencode_chat_completions 的
// enabled 检查 / JSON / SSE 分流；成功请求写 gateway_admin 日志（渠道健康
// 看板的流量数据源，channel_health 的 dumate 行 traffic_source="logs"）。
// ---------------------------------------------------------------------------

/// 鉴权判定：`Some(true)` = 用了本网关自有密钥；`Some(false)` = 用了兼容密钥（主网关 /v1 的）；
/// `None` = 不通过。空密钥视为放行（与传统各网关一致）。
fn dumate_proxy_auth_result(headers: &HeaderMap, cfg: &dumate::DumateProxyConfig) -> Option<bool> {
    if cfg.api_key.is_empty() {
        return Some(true);
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let xkey = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let got = bearer.or(xkey);
    let Some(got) = got else {
        return None;
    };
    if got == cfg.api_key {
        return Some(true);
    }
    // 兼容：客户端历史上可能只配了主网关密钥。接受主网关密钥以免阻塞接入，
    // 并在调用侧记一条日志（不是静默放行）。
    if got == glm_proxy::load_config().api_key {
        return Some(false);
    }
    None
}

/// 鉴权失败时的「收到的鉴权头长什么样」诊断串（模式同 `opencode_auth_diag`，
/// 该 helper 绑定各自网关配置类型无法直接复用）：只输出鉴权头前 6 字符与长度、
/// UA 截断、头名列表与期望 key 的掩码，**绝不落全量密钥**。
fn dumate_auth_diag(headers: &HeaderMap, cfg: &dumate::DumateProxyConfig) -> String {
    let mask = |s: &str| -> String {
        let n = s.chars().count();
        let p: String = s.chars().take(6).collect();
        format!("{p}…(len={n})")
    };
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| mask(v.trim_start_matches("Bearer ")))
        .unwrap_or_else(|| "<无>".to_string());
    let xkey = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(mask)
        .unwrap_or_else(|| "<无>".to_string());
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(40).collect::<String>())
        .unwrap_or_else(|| "<无>".to_string());
    let mut names: Vec<String> = headers.keys().map(|k| k.as_str().to_string()).collect();
    names.sort();
    format!(
        "无效的 API Key ｜ 期望={} 实收 Authorization={auth} x-api-key={xkey} UA={ua} 头列表=[{}]",
        mask(&cfg.api_key),
        names.join(",")
    )
}

/// 「客户端请求体问题」用 400（便于调用方区分该改请求还是该重试），
/// 「上游/引擎故障」用 502 —— 口径对齐 opencode_error_status。
fn dumate_error_status(msg: &str) -> StatusCode {
    if msg.contains("不能为空") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    }
}

/// POST /dumate/v1/messages：Anthropic Messages 协议兼容入口（1.0.13）。
///
/// 为什么需要：ZCode 等客户端的 provider **统一使用 `anthropic-messages` 协议**
/// （实测其 11 个 provider 全为该类型），而 DuMate 通道原本只提供 OpenAI 风格接口。
/// 本 handler 复用与 `/dumate/v1/chat/completions` 相同的鉴权，再走 `dumate::anthropic_messages`。
async fn api_dumate_anthropic_messages(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let started = std::time::Instant::now();
    let cfg = dumate::load_config();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if !cfg.enabled {
        let msg = "DuMate 网关未启用（可在「DuMate」页开启）";
        log_gateway_request("dumate", None, &model, false, 0, msg);
        return anthropic_error(msg.to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    match dumate_proxy_auth_result(&headers, &cfg) {
        Some(true) => {}
        Some(false) => log_gateway_request(
            "dumate",
            None,
            &model,
            true,
            0,
            "鉴权通过（兼容主网关密钥）",
        ),
        None => {
            let diag = dumate_auth_diag(&headers, &cfg);
            log_gateway_request("dumate", None, &model, false, 0, &diag);
            return anthropic_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
        }
    }

    match dumate::anthropic_messages(body).await {
        Ok(dumate::DumateAnthropicReply::Json(value)) => {
            log_gateway_request(
                "dumate",
                None,
                &model,
                true,
                started.elapsed().as_millis() as u64,
                "anthropic",
            );
            json_ok(value)
        }
        Ok(dumate::DumateAnthropicReply::Stream(upstream)) => {
            log_gateway_request(
                "dumate",
                None,
                &model,
                true,
                started.elapsed().as_millis() as u64,
                "anthropic-stream",
            );
            let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(
                    header::CONTENT_TYPE,
                    "text/event-stream; charset=utf-8".to_string(),
                )],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(e) => {
            let status = anthropic_error_status(&e);
            log_gateway_request(
                "dumate",
                None,
                &model,
                false,
                started.elapsed().as_millis() as u64,
                &e,
            );
            anthropic_error(e, status)
        }
    }
}

/// GET /dumate/v1/models：静态 4 模型目录（lite/turbo/ultra/points）。
async fn api_dumate_v1_models(headers: HeaderMap) -> Response {
    let cfg = dumate::load_config();
    if !cfg.enabled {
        let msg = "DuMate 网关未启用（可在「DuMate」页开启）";
        log_gateway_request("dumate", None, "models", false, 0, msg);
        return glm_proxy_error(msg.to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    match dumate_proxy_auth_result(&headers, &cfg) {
        Some(true) => {}
        Some(false) => log_gateway_request(
            "dumate",
            None,
            "models",
            true,
            0,
            "鉴权通过（兼容主网关密钥；建议在「DuMate」页用本网关自有密钥）",
        ),
        None => {
            let diag = dumate_auth_diag(&headers, &cfg);
            log_gateway_request("dumate", None, "models", false, 0, &diag);
            return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
        }
    }
    log_gateway_request("dumate", None, "models", true, 0, "");
    json_ok(dumate::models_payload())
}

/// POST /dumate/v1/chat/completions：JSON / SSE 分流；引擎同步返回最终消息
/// （SSE 为聚合后合成帧，见 dumate::chat_completions_stream）。
async fn api_dumate_chat_completions(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let started = std::time::Instant::now();
    let cfg = dumate::load_config();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if !cfg.enabled {
        let msg = "DuMate 网关未启用（可在「DuMate」页开启）";
        log_gateway_request("dumate", None, &model, false, 0, msg);
        return glm_proxy_error(msg.to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    match dumate_proxy_auth_result(&headers, &cfg) {
        Some(true) => {}
        Some(false) => log_gateway_request(
            "dumate",
            None,
            &model,
            true,
            0,
            "鉴权通过（兼容主网关密钥；建议在「DuMate」页用本网关自有密钥）",
        ),
        None => {
            let diag = dumate_auth_diag(&headers, &cfg);
            log_gateway_request("dumate", None, &model, false, 0, &diag);
            return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
        }
    }

    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream {
        match dumate::chat_completions_stream(&model, &body).await {
            Ok((upstream, actual)) => {
                log_gateway_request(
                    "dumate",
                    None,
                    &actual,
                    true,
                    started.elapsed().as_millis() as u64,
                    "",
                );
                let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                });
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream".to_string())],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }
            Err(e) => {
                let status = dumate_error_status(&e.message);
                log_gateway_request(
                    "dumate",
                    None,
                    &model,
                    false,
                    started.elapsed().as_millis() as u64,
                    &e.message,
                );
                glm_proxy_error(e.message, status)
            }
        }
    } else {
        match dumate::chat_completions_json(&model, &body).await {
            Ok(value) => {
                let actual = value
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(&model)
                    .to_string();
                log_gateway_request(
                    "dumate",
                    None,
                    &actual,
                    true,
                    started.elapsed().as_millis() as u64,
                    "",
                );
                json_ok(value)
            }
            Err(e) => {
                let status = dumate_error_status(&e.message);
                log_gateway_request(
                    "dumate",
                    None,
                    &model,
                    false,
                    started.elapsed().as_millis() as u64,
                    &e.message,
                );
                glm_proxy_error(e.message, status)
            }
        }
    }
}

/// GET /api/dumate/status：渠道状态聚合（桌面端 / 引擎 / 凭据 / 网关配置 / 积分）。
async fn api_dumate_status() -> Response {
    json_ok(dumate::status().await)
}

/// GET /api/dumate/credits：积分概览（quota_overview 组装）。
async fn api_dumate_credits() -> Response {
    json_ok(dumate::get_credits().await)
}

/// GET /api/dumate/config：接入信息（Base URL / 密钥掩码 / 启用态），只读不触发发现。
async fn api_dumate_config_get() -> Response {
    json_ok(dumate::config_info())
}

/// POST /api/dumate/config：启用开关 / 重新生成或手填密钥 / 引擎静态地址与 inapp key。
async fn api_dumate_config_post(Json(body): Json<Value>) -> Response {
    let mut cfg = dumate::load_config();
    if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if body.get("regenerateKey").and_then(Value::as_bool) == Some(true) {
        cfg.api_key = glm_proxy::generate_api_key();
    } else if let Some(key) = body.get("apiKey").and_then(Value::as_str) {
        let key = key.trim();
        if !key.is_empty() {
            cfg.api_key = key.to_string();
        }
    }
    if let Some(url) = body.get("opencodeUrl").and_then(Value::as_str) {
        cfg.opencode_url = url.trim().to_string();
    }
    if let Some(key) = body.get("inappKey").and_then(Value::as_str) {
        cfg.inapp_key = key.trim().to_string();
    }
    if let Err(e) = dumate::save_config(&cfg) {
        return json_err(e, StatusCode::INTERNAL_SERVER_ERROR);
    }
    json_ok(dumate::config_info())
}

/// POST /api/dumate/checkin：本机账号一键签到（status → claim，幂等）。
/// 凭据缺失时也照常执行——上游链路会给出「请打开 DuMate 客户端重新登录」的人话报错。
async fn api_dumate_checkin() -> Response {
    let account = platform_checkin::discover_dumate_account().unwrap_or_else(|| {
        json!({
            "id": format!("{}:local", platform_checkin::PLATFORM_DUMATE),
            "email": "DuMate 本机账号",
        })
    });
    let res = platform_checkin::perform_dumate_checkin(&account).await;
    json_ok(json!({
        "id": account.get("id").cloned().unwrap_or(Value::Null),
        "result": res.get("result").cloned().unwrap_or(json!("error")),
        "message": res.get("message").cloned().unwrap_or(Value::Null),
        "error": res.get("error").cloned().unwrap_or(Value::Null),
    }))
}

/// GET /api/dumate/checkin-status：今日签到状态（loginBonusInfo 直查）。
async fn api_dumate_checkin_status() -> Response {
    let mut v = platform_checkin::get_dumate_status().await;
    // 前端契约（§4.1）按顶层读 totalTimes/totalPoints/signInDays；上游字段在 raw 里，
    // 这里拍平到顶层（raw 保留兜底），否则前端数字与 7 天打点全部落空。
    if let (Some(raw), Some(obj)) = (v.get("raw").cloned(), v.as_object_mut()) {
        if let Some(raw_obj) = raw.as_object() {
            for key in ["totalTimes", "totalPoints", "signInDays", "hasIssued"] {
                if let Some(val) = raw_obj.get(key) {
                    obj.entry(key.to_string()).or_insert(val.clone());
                }
            }
        }
    }
    json_ok(v)
}

// ---------------------------------------------------------------------------
// DuMate 多账号池（契约 unified-accounts-contract.md §1.2/§1.5）
// ---------------------------------------------------------------------------

/// GET /api/dumate/accounts：池列表（含 dumate:local 本机行，isLocal 标记）。
/// 不回传 cookie 明文，只回 cookieCount；credits 为最新缓存（可能为 null）。
async fn api_dumate_accounts_list() -> Response {
    json_ok(json!({ "accounts": dumate_accounts::list_entries() }))
}

/// POST /api/dumate/accounts：添加池账号。body `{name, cookies(数组或
/// {cookies:[...]}), proxy?, note?}`。
async fn api_dumate_accounts_add(Json(body): Json<Value>) -> Response {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    let proxy = body.get("proxy").and_then(Value::as_str).unwrap_or("");
    let note = body.get("note").and_then(Value::as_str).unwrap_or("");
    let Some(cookies_json) = body.get("cookies") else {
        return json_err("缺少 cookies 字段（数组或 {cookies:[...]}）".to_string(), StatusCode::BAD_REQUEST);
    };
    match dumate_accounts::add_account(name, cookies_json, proxy, note) {
        Ok(acc) => json_ok(json!({"ok": true, "account": dumate_accounts::summary_of(&acc)})),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/dumate/accounts/update：更新池账号。body `{id, name?, proxy?, note?}`。
async fn api_dumate_accounts_update(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    }
    match dumate_accounts::update_account(
        id,
        body.get("name").and_then(Value::as_str),
        body.get("proxy").and_then(Value::as_str),
        body.get("note").and_then(Value::as_str),
    ) {
        Ok(()) => json_ok(json!({"ok": true})),
        Err(e) => json_err(e, StatusCode::NOT_FOUND),
    }
}

/// POST /api/dumate/accounts/remove：删除池账号。body `{id}`。
async fn api_dumate_accounts_remove(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    }
    match dumate_accounts::remove_account(id) {
        Ok(()) => json_ok(json!({"ok": true})),
        Err(e) => json_err(e, StatusCode::NOT_FOUND),
    }
}

/// POST /api/dumate/accounts/import-client：把本机桌面端当前登录态导入池
/// （按 bce-user-info 值去重，重复报「已是同一登录态」）。
async fn api_dumate_accounts_import_client() -> Response {
    match dumate_accounts::import_from_client() {
        Ok(acc) => json_ok(json!({"ok": true, "account": dumate_accounts::summary_of(&acc)})),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/dumate/account/checkin：单账号签到。body `{id}`（local/pool 都走它；
/// `dumate:local` 路由到本机客户端凭据路径，`dumate:pool_*` 走池账号）。
async fn api_dumate_account_checkin(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    if id.is_empty() {
        return json_err("缺少 id".to_string(), StatusCode::BAD_REQUEST);
    }
    let local_id = format!("{}:local", platform_checkin::PLATFORM_DUMATE);
    if id == local_id {
        let account = platform_checkin::discover_dumate_account()
            .unwrap_or_else(|| json!({"id": local_id, "email": "DuMate 本机账号"}));
        let res = platform_checkin::perform_dumate_checkin(&account).await;
        return json_ok(json!({
            "id": id,
            "result": res.get("result").cloned().unwrap_or(json!("error")),
            "message": res.get("message").cloned().unwrap_or(Value::Null),
            "error": res.get("error").cloned().unwrap_or(Value::Null),
        }));
    }
    match dumate_accounts::find_account(&id) {
        Some(_) => json_ok(dumate_accounts::account_checkin(&id).await),
        None => json_err(format!("DuMate 账号不存在: {id}"), StatusCode::NOT_FOUND),
    }
}

/// GET /api/accounts/unified：统一账号管理聚合（全平台统一 schema + summary，
/// 契约 unified-accounts-contract.md §2；workbuddy 不跳过）。
async fn api_unified_accounts() -> Response {
    json_ok(unified_accounts::unified_accounts().await)
}

/// Loomy 对话补全（Anthropic Claude Messages 协议兼容）。
async fn api_loomy_anthropic_messages(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let cfg = loomy_proxy::load_config();
    if !cfg.enabled {
        return anthropic_error(
            "Loomy 代理未启用，请在设置中开启".to_string(),
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
    if !loomy_proxy_auth_ok(&headers, &cfg) {
        return anthropic_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match loomy_proxy::anthropic_messages(body).await {
        Ok(loomy_proxy::LoomyChatCompletion::Json(value)) => json_ok(value),
        Ok(loomy_proxy::LoomyChatCompletion::Stream(upstream)) => {
            let stream = futures_util::stream::unfold(upstream.rx, |mut rx| async move {
                rx.recv().await.map(|chunk| (chunk, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, upstream.content_type)],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(e) => {
            let status = anthropic_error_status(&e);
            anthropic_error(e, status)
        }
    }
}

async fn api_loomy_protection_status() -> Response {
    json_ok(account_protection::get_loomy_protection_status())
}

async fn api_loomy_protection_keepalive_now() -> Response {
    let report = account_protection::run_loomy_keepalive_cycle().await;
    json_ok(json!({
        "ok": true,
        "report": report,
    }))
}

async fn api_loomy_protection_cooldown_clear(Json(body): Json<Value>) -> Response {
    let account_id = body.get("accountId").and_then(Value::as_str).unwrap_or("");
    let cleared = account_protection::clear_account_cooling(account_id);
    json_ok(json!({
        "ok": true,
        "cleared": cleared,
    }))
}

/// AutoClaw 网关状态（UI 用）。
// ---------------------------------------------------------------------------
// GLM Plan 网关（1.4.0 新通道：GLM Coding Plan 账号池 → OpenAI/Anthropic）
// ---------------------------------------------------------------------------

fn glmplan_mask_key(key: &str) -> String {
    if key.len() <= 10 {
        "*".repeat(key.len())
    } else {
        format!("{}...{}", &key[..6], &key[key.len() - 4..])
    }
}

fn glmplan_key_ok(headers: &HeaderMap) -> bool {
    let cfg = glmplan::load_config();
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    matches!(got, Some(k) if k == cfg.api_key)
}

async fn api_glmplan_status() -> Response {
    let cfg = glmplan::load_config();
    let stats = glmplan::cache_stats();
    let accounts: Vec<Value> = cfg
        .accounts
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "label": a.label,
                "provider": a.provider,
                "providerLabel": glmplan::provider_label(&a.provider),
                "apiKeyMasked": glmplan_mask_key(&a.api_key),
                "enabled": a.enabled,
                "lastError": a.last_error,
                "cooling": a.cooldown_until_ms > glmplan::now_ms(),
            })
        })
        .collect();
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(json!({
        "enabled": cfg.enabled,
        "apiKey": cfg.api_key,
        "apiKeyMasked": glmplan_mask_key(&cfg.api_key),
        "defaultProvider": cfg.default_provider,
        "defaultModel": cfg.default_model,
        "models": cfg.models,
        "accounts": accounts,
        "cache": {
            "enabled": cfg.cache_enabled,
            "hits": stats.hits,
            "misses": stats.misses,
            "entries": stats.entries,
        },
        "endpoint": format!("http://127.0.0.1:{port}/glmplan/v1"),
    }))
}

async fn api_glmplan_test(Json(body): Json<Value>) -> Response {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("glm-4.6").to_string();
    let prompt = body.get("prompt").and_then(Value::as_str).unwrap_or("只回复 OK").to_string();
    let payload = json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": 512,
    });
    let started = std::time::Instant::now();
    match glmplan::chat_completions(payload).await {
        Ok(v) => {
            let content = v
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let served = v
                .pointer("/model")
                .and_then(Value::as_str)
                .unwrap_or(&model)
                .to_string();
            json_ok(json!({
                "ok": true,
                "content": content,
                "model": served,
                "latencyMs": started.elapsed().as_millis() as u64,
            }))
        }
        Err((status, err)) => json_ok(json!({
            "ok": false,
            "error": err,
            "status": status,
        })),
    }
}

async fn api_glmplan_config_save(Json(body): Json<Value>) -> Response {
    let mut cfg = glmplan::load_config();
    if let Some(b) = body.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = b;
    }
    if let Some(p) = body.get("defaultProvider").and_then(Value::as_str) {
        if glmplan::provider_info(p).is_some() {
            cfg.default_provider = p.to_string();
        }
    }
    if let Some(m) = body.get("defaultModel").and_then(Value::as_str) {
        if !m.is_empty() {
            cfg.default_model = m.to_string();
        }
    }
    if let Some(b) = body.get("cacheEnabled").and_then(Value::as_bool) {
        cfg.cache_enabled = b;
    }
    if let Some(b) = body.get("spoof").and_then(Value::as_bool) {
        cfg.spoof = b;
    }
    if body.get("regenerateKey").and_then(Value::as_bool).unwrap_or(false) {
        cfg.api_key = glmplan::gen_api_key();
    }
    glmplan::save_config(&cfg);
    json_ok(json!({ "ok": true }))
}

async fn api_glmplan_accounts_add(Json(body): Json<Value>) -> Response {
    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if glmplan::provider_info(&provider).is_none() {
        return json_err(
            "provider 必须为 bigmodel（国内版）或 zai（国际版）".to_string(),
            StatusCode::BAD_REQUEST,
        );
    }
    let api_key = body
        .get("apiKey")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if api_key.is_empty() {
        return json_err("缺少 apiKey".to_string(), StatusCode::BAD_REQUEST);
    }
    let label = body
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("未命名")
        .trim()
        .to_string();
    let mut cfg = glmplan::load_config();
    if cfg.accounts.iter().any(|a| a.api_key == api_key) {
        return json_err("该 API Key 已在账号池中".to_string(), StatusCode::BAD_REQUEST);
    }
    let id = glmplan::gen_id();
    cfg.accounts.push(glmplan::PlanAccount {
        id: id.clone(),
        label,
        provider,
        api_key,
        enabled: true,
        added_at: glmplan::now_ms(),
        last_error: None,
        cooldown_until_ms: 0,
    });
    if !cfg.enabled {
        cfg.enabled = true;
    }
    glmplan::save_config(&cfg);
    json_ok(json!({ "ok": true, "id": id }))
}

async fn api_glmplan_accounts_remove(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    let mut cfg = glmplan::load_config();
    let before = cfg.accounts.len();
    cfg.accounts.retain(|a| a.id != id);
    if cfg.accounts.len() == before {
        return json_err("账号不存在".to_string(), StatusCode::NOT_FOUND);
    }
    glmplan::save_config(&cfg);
    json_ok(json!({ "ok": true }))
}

async fn api_glmplan_accounts_enable(Json(body): Json<Value>) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let mut cfg = glmplan::load_config();
    match cfg.accounts.iter_mut().find(|a| a.id == id) {
        Some(a) => {
            a.enabled = enabled;
            a.cooldown_until_ms = 0;
            glmplan::save_config(&cfg);
            json_ok(json!({ "ok": true }))
        }
        None => json_err("账号不存在".to_string(), StatusCode::NOT_FOUND),
    }
}

async fn api_glmplan_models(headers: HeaderMap, RawQuery(raw_query): RawQuery) -> Response {
    if !glmplan_key_ok(&headers) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let cfg = glmplan::load_config();
    let region = raw_query.as_deref().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("region=").map(|v| v.to_string()))
    });
    json_ok(glmplan::models_payload_filtered(&cfg, region.as_deref()))
}

async fn api_glmplan_chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !glmplan_key_ok(&headers) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if streaming {
        return match glmplan::chat_stream(body).await {
            Ok(resp) => {
                let ct = resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .cloned()
                    .unwrap_or(header::HeaderValue::from_static("text/event-stream"));
                let stream = futures_util::stream::unfold(resp, |mut r| async move {
                    match r.chunk().await {
                        Ok(Some(c)) => Some((Ok::<_, String>(c), r)),
                        Ok(None) => None,
                        Err(e) => Some((Err(format!("GLM Plan 流中断: {e}")), r)),
                    }
                });
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, ct)],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }
            Err((s, e)) => {
                glm_proxy_error(e, StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY))
            }
        };
    }
    match glmplan::chat_completions(body).await {
        Ok(v) => json_ok(v),
        Err((s, e)) => glm_proxy_error(e, StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY)),
    }
}

async fn api_glmplan_messages(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !glmplan_key_ok(&headers) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match glmplan::anthropic_messages(body).await {
        Ok((status, text)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            [(header::CONTENT_TYPE, "application/json")],
            text,
        )
            .into_response(),
        Err((s, e)) => glm_proxy_error(e, StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY)),
    }
}

async fn api_glm_proxy_status() -> Response {
    json_ok(glm_proxy::gateway_status().await)
}

/// 网关状态（OpenAI 服务风格端点，供外部工具/脚本监控；需代理密钥）。
async fn api_glm_v1_status(headers: HeaderMap) -> Response {
    let cfg = glm_proxy::load_config();
    if !cfg.enabled {
        return glm_proxy_error("GLM 代理未启用".to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    if !glm_proxy_auth_ok(&headers, &cfg) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let mut status = glm_proxy::gateway_status().await;
    status["proxy"] = json!({
        "enabled": true,
        "endpoint": format!(
            "http://127.0.0.1:{}/v1",
            BOUND_PORT.get().copied().unwrap_or(57890)
        ),
        "models": glm_proxy::models_payload()["data"],
    });
    json_ok(status)
}

async fn api_glm_proxy_config_get() -> Response {
    let cfg = glm_proxy::load_config();
    let port = BOUND_PORT.get().copied().unwrap_or(57890);
    json_ok(json!({
        "enabled": cfg.enabled,
        "apiKey": cfg.api_key,
        "endpoint": format!("http://127.0.0.1:{port}/v1"),
        "autoclawReady": glm_proxy::autoclaw_ready(),
        "speedFirst": cfg.speed_first,
        "speedFirstTarget": cfg.speed_first_target,
        "speedFirstSources": glm_proxy::SPEED_FIRST_SOURCES,
        "accountMode": cfg.account_mode,
        "pinnedAccountId": cfg.pinned_account_id,
        "accountModes": glm_proxy::ACCOUNT_MODES,
        "models": glm_proxy::model_routes().iter().map(|r| {
            let mut item = r.meta_summary();
            item["id"] = json!(r.body_model);
            item["alias"] = json!(r.alias);
            item["routeId"] = json!(r.route_id);
            item["region"] = json!(r.region.as_str());
            item
        }).collect::<Vec<_>>(),
    }))
}

async fn api_glm_proxy_config_save(Json(body): Json<Value>) -> Response {
    let mut cfg = glm_proxy::load_config();
    if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if let Some(speed_first) = body.get("speedFirst").and_then(Value::as_bool) {
        cfg.speed_first = speed_first;
    }
    if let Some(target) = body.get("speedFirstTarget").and_then(Value::as_str) {
        let target = target.trim();
        if !target.is_empty() {
            cfg.speed_first_target = target.to_string();
        }
    }
    if let Some(mode) = body.get("accountMode").and_then(Value::as_str) {
        let mode = mode.trim();
        if !glm_proxy::ACCOUNT_MODES.contains(&mode) {
            return json_err(
                format!("未知的账号策略: {mode}"),
                StatusCode::BAD_REQUEST,
            );
        }
        cfg.account_mode = mode.to_string();
    }
    if let Some(pinned) = body.get("pinnedAccountId").and_then(Value::as_str) {
        cfg.pinned_account_id = pinned.trim().to_string();
    }
    if cfg.account_mode == glm_proxy::ACCOUNT_MODE_PINNED
        && cfg.pinned_account_id.trim().is_empty()
    {
        return json_err(
            "「固定账号」策略需要先选择一个账号".to_string(),
            StatusCode::BAD_REQUEST,
        );
    }
    if body.get("regenerateKey").and_then(Value::as_bool) == Some(true) {
        cfg.api_key = glm_proxy::generate_api_key();
    } else if let Some(key) = body.get("apiKey").and_then(Value::as_str) {
        let key = key.trim();
        if !key.is_empty() {
            cfg.api_key = key.to_string();
        }
    }
    match glm_proxy::save_config(&cfg) {
        Ok(()) => api_glm_proxy_config_get().await,
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_platform_accounts() -> Response {
    let list = platform_checkin::all_checkin_accounts();
    let mut items = Vec::new();
    for acc in &list {
        if platform_checkin::account_platform(acc) == platform_checkin::PLATFORM_WORKBUDDY {
            continue;
        }
        let status = checkin::get_checkin_status_for(acc).await;
        let mut item = checkin_status_item(acc, status);
        item["credit"] = platform_checkin::get_platform_credits(acc).await;
        items.push(item);
    }
    json_ok(json!({ "accounts": items }))
}

async fn api_travel_status() -> Response {
    travel::reconcile_due_travel(None).await;
    let items = account::load_accounts()
        .iter()
        .map(|acc| {
            let id = acc.get("id").and_then(Value::as_str).unwrap_or("");
            let mut value = travel::travel_display(id);
            value["accountId"] = acc.get("id").cloned().unwrap_or(Value::Null);
            value["email"] = json!(account::account_display_name(acc));
            value
        })
        .collect::<Vec<_>>();
    json_ok(json!({ "accounts": items }))
}

async fn api_travel_config() -> Response {
    json_ok(config::load_travel_config())
}

async fn api_save_travel_config(Json(body): Json<Value>) -> Response {
    let submitted = body.get("config").unwrap_or(&body);
    match config::save_travel_config(submitted) {
        Ok(()) => {
            let saved = config::load_travel_config();
            if saved.get("enabled").and_then(Value::as_bool) == Some(true) {
                tokio::spawn(async {
                    let _ = travel::run_travel_cycle().await;
                    let _ = travel::run_travel_claim_cycle().await;
                });
            }
            json_ok(saved)
        }
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_refresh_token(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(refresh::refresh_account_token(acc).await)
}

// ---------------------------------------------------------------------------
// 更新
// ---------------------------------------------------------------------------

async fn api_update_check() -> Response {
    json_ok(update::update_check(None, false).await)
}

async fn api_update_config() -> Response {
    json_ok(update::load_github_config())
}

async fn api_save_update_config(Json(body): Json<Value>) -> Response {
    match update::save_github_config(&body) {
        Ok(()) => json_ok(json!({ "ok": true, "config": update::load_github_config() })),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 静态前端
// ---------------------------------------------------------------------------

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".js") || path.ends_with(".mjs") {
        "text/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}

/// 该路径是否属于「API 命名空间」——这些前缀下不存在前端 SPA 页面，
/// 静态兜底必须返回 404 而不是 index.html。
///
/// ⚠️ 单段路径 `/kimi`、`/grok`、`/qoder` 同时是前端页面（见 `src/App.tsx` 的
/// Route），所以这三个前缀只有**带斜杠的子路径**（`/kimi/v1/...`）才算 API；
/// 而 `/api`、`/v1` 没有同名页面，精确匹配也算 API。
/// T1（P0-F15）：补充登记 `trae`、`workbuddy`、`loomy`、`glmweb`、`miaoshou` 五个
/// 反代网关前缀——它们同样只有带斜杠的子路径才是 API（单段 `/loomy`、`/glmweb`、
/// `/miaoshou` 是前端页面，因要求前缀后紧跟 `/` 不会误伤；`/trae`、`/workbuddy`
/// 无同名页面，但只要不带斜杠就不是已注册路由）。未登记时这些前缀下的未知子路径
/// （如 `/trae/v1/bar`、`/loomy/v1/embeddings`）会落进 SPA 兜底返回 200 HTML，
/// 把「路由不存在」伪装成成功。
fn is_api_namespace(path: &str) -> bool {
    const EXACT: [&str; 2] = ["api", "v1"];
    // 这些前缀 + `/` 才算 API 命名空间；单段同名路径是前端页面（如 `/qoder`、`/kimi`、
    // `/loomy`、`/glmweb`、`/miaoshou`），因为要求路径后面紧跟 `/`，故不会误伤页面。
    // ⚠️ 新增网关路由时务必在此登记，否则未知子路径会落进 SPA 兜底返回 200 HTML，
    //    把「路由不存在」伪装成成功（排查时极具误导性）。
    //    注意：`qwen` 一条**覆盖不到** `qwen3.8-flash`（要求前缀后紧跟 `/`），必须单独登记。
    const WITH_SUB: [&str; 16] = [
        "api",
        "v1",
        "kimi",
        "grok",
        "qoder",
        "qwen",
        "qwen-flash",
        // 该路由已于 0.1.70 下线（协议不可行），此处登记是为了让残留路径返回 404 JSON，
        // 而不是被 SPA 兜底伪装成 200 HTML。
        "qwen3.8-flash",
        // T1：五个模型反代网关前缀（trae/workbuddy/loomy/glmweb/miaoshou）。
        // 已注册路由（/trae/v1/models、/workbuddy/v1/*、/loomy/v1/*、/glmweb/v1/*、
        // /miaoshou/v1/*）由 axum 先行匹配，行为不变；此处只兜住「未注册子路径」，
        // 让它们返回 404 JSON（未知接口）而不是 200 HTML 假成功。
        "trae",
        "workbuddy",
        "loomy",
        "glmweb",
        "miaoshou",
        // WorkBuddy 桌面端自带模型通道（/workbuddy-native/v1/*）。
        "workbuddy-native",
        // OpenCode 网关（/opencode/v1/*，2026-09-22 三拆新增）。
        // ⚠️ `opencode` 这一条只兜 `/opencode/<子路径>`；`/opencode`（前端页面）无尾斜杠，不受影响。
        "opencode",
        // DuMate（百度搭子）网关（/dumate/v1/*，2026-10-01 接入；`/dumate` 单段是前端页面）。
        "dumate",
    ];

    if EXACT.contains(&path) {
        return true;
    }
    WITH_SUB
        .iter()
        .any(|p| path.starts_with(p) && path.as_bytes().get(p.len()) == Some(&b'/'))
}

async fn static_handler(uri: Uri) -> Response {
    let mut path = uri.path().trim_start_matches('/').to_string();
    if path.is_empty() || path == "index.html" {
        path = "index.html".to_string();
    }

    // 1. 若请求匹配具体静态资源，直接返回该资源
    if let Some(f) = Assets::get(&path) {
        let cache_control = if path == "index.html" {
            Some("no-cache")
        } else if is_hashed_asset(&path) {
            Some("public, max-age=31536000, immutable")
        } else {
            None
        };
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type(&path));
        if let Some(value) = cache_control {
            builder = builder.header(header::CACHE_CONTROL, value);
        }
        return builder.body(Body::from(f.data.into_owned())).unwrap();
    }

    // 2. 静态资源专属路径（如 assets/* 或带常见静态后缀），若找不到必须返回 404，
    // 绝不能回退到 index.html（否则浏览器将 HTML 误作为 JS 执行会抛 SyntaxError 导致白屏）。
    if path.starts_with("assets/")
        || path.ends_with(".js")
        || path.ends_with(".mjs")
        || path.ends_with(".css")
        || path.ends_with(".png")
        || path.ends_with(".svg")
        || path.ends_with(".ico")
        || path.ends_with(".woff2")
    {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from("404 Not Found"))
            .unwrap();
    }

    // 3. API 命名空间绝不能回退到 index.html。否则「路由漏注册 / 路径写错」会
    // 表现为 200 + HTML：调用方拿到假成功，部署健康检查恒为真（2026-09-16 根因）。
    // 宁可 404，也不要静默返回一个语义完全不同的文档。
    if is_api_namespace(&path) {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(
                json!({
                    "ok": false,
                    "code": "not_found",
                    "error": format!("未知接口: /{path}"),
                })
                .to_string(),
            ))
            .unwrap();
    }

    // 4. 其余路径为前端 SPA 路由（如 /token-stats, /credit-stats, /settings 等），
    // 统一回退到 index.html，且 Content-Type 必须是 text/html（绝不能是 octet-stream 触发下载）。
    if let Some(index_file) = Assets::get("index.html") {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(index_file.data.into_owned()))
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("not found"))
        .unwrap()
}

/// vite 带内容 hash 的产物名（末段为 8 位 [0-9A-Za-z_-] 的 js/css）。
fn is_hashed_asset(path: &str) -> bool {
    let Some(dot) = path.rfind('.') else {
        return false;
    };
    if !matches!(&path[dot + 1..], "js" | "css") {
        return false;
    }
    let stem_tail = &path[..dot];
    let segment = stem_tail.rsplit('-').next().unwrap_or("");
    segment.len() == 8
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::checkin_status_item;
    use serde_json::json;

    #[test]
    fn web_checkin_status_keeps_account_identity() {
        let item = checkin_status_item(
            &json!({"id": "account-1", "email": "user@example.com"}),
            json!({"ok": true, "todayCheckedIn": true}),
        );

        assert_eq!(item["accountId"], "account-1");
        assert_eq!(item["email"], "user@example.com");
        assert_eq!(item["todayCheckedIn"], true);
    }

    #[test]
    fn web_checkin_status_preserves_failure_state() {
        let item = checkin_status_item(
            &json!({"id": "account-2"}),
            json!({"ok": false, "todayCheckedIn": false, "error": "status failed"}),
        );

        assert_eq!(item["accountId"], "account-2");
        assert_eq!(item["ok"], false);
        assert_eq!(item["error"], "status failed");
    }
}


#[cfg(test)]
mod context_guard_tests {
    use super::*;

    fn msg(role: &str, text: &str) -> Value {
        json!({"role": role, "content": text})
    }

    #[test]
    fn token_estimation_is_conservative_for_chinese() {
        // 中文按 1.6 token/字符：1000 汉字 ≈ 1600 tokens
        let zh = "上下".repeat(500);
        assert_eq!(estimate_tokens(&zh), 1600);
        // 英文按 0.3 token/字符：1000 字母 ≈ 300 tokens
        let en = "a".repeat(1000);
        assert_eq!(estimate_tokens(&en), 300);
        // 同样字符数，中文估值必须是英文的 5 倍以上（IDE 的英式启发式正栽在这）
        assert!(estimate_tokens(&zh) > estimate_tokens(&en) * 5);
    }

    #[test]
    fn guard_trims_oldest_but_keeps_system_and_latest() {
        let mut body = json!({
            "messages": [
                msg("system", "你是助手"),
                msg("user", &"旧".repeat(40000)),
                msg("assistant", &"旧回复".repeat(40000)),
                msg("user", "最新问题"),
            ]
        });
        let (trimmed, before, after) = guard_context_budget(&mut body);
        assert!(trimmed, "超预算必须裁剪");
        assert!(before > QWEN_INPUT_TOKEN_BUDGET);
        assert!(after <= QWEN_INPUT_TOKEN_BUDGET, "裁剪后必须在预算内");
        let msgs = body["messages"].as_array().unwrap();
        // system 与最新 user 必须保留
        assert_eq!(msgs.first().unwrap()["role"], "system");
        assert_eq!(msgs.last().unwrap()["content"], "最新问题");
        // 旧的必须被丢掉
        assert!(msgs.len() < 4, "应丢掉最旧消息，实际剩 {}", msgs.len());
    }

    #[test]
    fn guard_leaves_small_requests_untouched() {
        let original = json!({"messages": [msg("system", "你是助手"), msg("user", "你好")]});
        let mut body = original.clone();
        let (trimmed, before, after) = guard_context_budget(&mut body);
        assert!(!trimmed);
        assert_eq!(before, after);
        assert_eq!(body, original, "小请求一个字节都不能动");
    }

    #[test]
    fn guard_accounts_for_tools_schema() {
        // tools 的 JSON schema 也占 token：即使 messages 不长，巨型 tools 也要触发裁剪
        let big_tools: Vec<Value> = (0..20)
            .map(|i| json!({"type":"function","function":{
                "name": format!("tool_{i}"),
                "description": "工".repeat(3000),
                "parameters": {"type":"object","properties":{"x":{"type":"string","description":"参数".repeat(500)}}}
            }}))
            .collect();
        let mut body = json!({
            "messages": [msg("user", &"内容".repeat(30000))],
            "tools": big_tools,
        });
        let (trimmed, before, _) = guard_context_budget(&mut body);
        assert!(trimmed, "tools 占比大时必须裁剪");
        assert!(before > QWEN_INPUT_TOKEN_BUDGET);
    }
}
