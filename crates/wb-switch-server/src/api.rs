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
    account, account_protection, auth_file, channel_health, checkin, config, credit_usage, credits, export_import,
    autoclaw_accounts, glm_proxy, loomy, loomy_proxy, model_probe, model_usage, oauth, platform_checkin, platform_switch, process, refresh,
    gateway_admin, gateway_overview, glm_web, grok, grok_pool, kimi, qoder, qwen, qwen_mcp, qwen_web, grok_sso, miaoshou, session, switch, token_stats, toolcall_monitor, travel, update, trae_acp, trae_accounts, trae_proxy, trae_proxy_config, workbuddy_proxy, opencode,
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
        .route("/api/platform/accounts", get(api_platform_accounts))
        .route("/v1/chat/completions", post(api_glm_chat_completions))
        .route("/v1/messages", post(api_glm_anthropic_messages))
        .route("/v1/models", get(api_glm_models))
        .route(
            "/api/glm-proxy/config",
            get(api_glm_proxy_config_get).post(api_glm_proxy_config_save),
        )
        .route("/api/glm-proxy/status", get(api_glm_proxy_status))
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
        .route("/api/workbuddy-proxy/status", get(api_workbuddy_proxy_status))
        .route("/api/workbuddy-proxy/models", get(api_workbuddy_proxy_models))
        .route(
            "/api/workbuddy-proxy/config",
            get(api_workbuddy_proxy_config_get).post(api_workbuddy_proxy_config_save),
        )
        .route("/api/trae-proxy/config", get(api_trae_proxy_config_get))
        .route("/workbuddy/v1/chat/completions", post(api_workbuddy_chat_completions))
        .route("/workbuddy/v1/models", get(api_workbuddy_v1_models))
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
fn is_logged_path(path: &str) -> bool {
    let prefixes = [
        "/v1/", "/qwen/", "/qwen-flash/", "/qoder/", "/kimi/", "/glmweb/",
        "/grok/", "/loomy/", "/cuckoo/", "/trae/", "/api/", "/workbuddy/",
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

async fn api_glm_models(headers: HeaderMap) -> Response {
    let cfg = glm_proxy::load_config();
    if !cfg.enabled {
        return glm_proxy_error("GLM 代理未启用".to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    if !glm_proxy_auth_ok(&headers, &cfg) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    json_ok(glm_proxy::models_payload())
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
    match gateway_admin::create_key(name) {
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
/// TRAE/WorkBuddy 会话 ACP 转发的统一响应构造（/trae/v1 与
/// /workbuddy-native/v1 两个路由共用：本质都是本机 WorkBuddy 内嵌
/// codebuddy 会话，模型为该会话自带模型）。
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

/// WorkBuddy 桌面端自带模型通道（/workbuddy-native/v1）：与 /trae/v1 同一
/// ACP 实现，路由名明确指向「WorkBuddy 内置模型」而非 OpenCode 借道模型。
async fn api_workbuddy_native_chat_completions(body: axum::body::Bytes) -> Response {
    trae_acp_chat_response(body).await
}

/// WorkBuddy 自带模型目录：实时探测内嵌 codebuddy 会话的 --model。
/// 与 /workbuddy/v1/models（OpenCode 借道，594 个）是两套模型体系。
async fn api_workbuddy_native_models() -> Response {
    let model = trae_acp::builtin_model().unwrap_or_else(|| "deepseek-v4.1-flash".to_string());
    json_ok(json!({
        "object": "list",
        "data": [{
            "id": model,
            "object": "model",
            "created": 0,
            "owned_by": "workbuddy",
        }],
    }))
}

// ---------------------------------------------------------------------------
// WorkBuddy 模型反代（借道本机 OpenCode server）
// ---------------------------------------------------------------------------

fn workbuddy_proxy_auth_ok(headers: &HeaderMap, cfg: &workbuddy_proxy::WorkbuddyProxyConfig) -> bool {
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

async fn api_workbuddy_v1_models(headers: HeaderMap) -> Response {
    let cfg = workbuddy_proxy::load_config();
    if !workbuddy_proxy_auth_ok(&headers, &cfg) {
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match workbuddy_proxy::models_payload().await {
        Ok(value) => json_ok(value),
        Err(e) => glm_proxy_error(e, StatusCode::BAD_GATEWAY),
    }
}

/// T5：workbuddy 渠道请求级用量记录（chat 流式/非流式、成功/失败两条路径共用）。
///
/// 写同一份 `glm_usage_records.json`（channel=workbuddy）；积分口径（F9）取
/// 上游响应的 `usage.cost` 实耗（免费模型为 0，如实记录）。
fn record_workbuddy_call(
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
        "channel": "workbuddy",
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
    model_usage::record_channel_call("workbuddy", record);
}

/// WorkBuddy 对话补全（OpenAI 兼容；借道本机 OpenCode server）。
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
        record_workbuddy_call(started_ms, &model, false, 0, msg, None, "");
        return glm_proxy_error(msg.to_string(), StatusCode::SERVICE_UNAVAILABLE);
    }
    if !workbuddy_proxy_auth_ok(&headers, &cfg) {
        log_gateway_request("workbuddy", None, &model, false, 0, "无效的 API Key");
        record_workbuddy_call(started_ms, &model, false, 0, "无效的 API Key", None, "");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    let stream = body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stream {
        match workbuddy_proxy::chat_completions_stream(body).await {
            Ok(upstream) => {
                let latency = started.elapsed().as_millis() as u64;
                log_gateway_request("workbuddy", None, &model, true, latency, "");
                // 流式 cost 由 SSE 末尾 usage 帧携带，handler 返回时尚未产出，
                // 本记录记 0 并注明（非流式路径照实记录 cost）。
                record_workbuddy_call(
                    started_ms,
                    &model,
                    true,
                    latency,
                    "",
                    None,
                    "流式：cost 见 SSE 末尾 usage 帧",
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
                let latency = started.elapsed().as_millis() as u64;
                log_gateway_request("workbuddy", None, &model, false, latency, &e);
                record_workbuddy_call(started_ms, &model, false, latency, &e, None, "");
                glm_proxy_error(e, StatusCode::BAD_GATEWAY)
            }
        }
    } else {
        match workbuddy_proxy::chat_completions_json(body).await {
            Ok(value) => {
                let latency = started.elapsed().as_millis() as u64;
                let actual_model = value
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(&model)
                    .to_string();
                log_gateway_request("workbuddy", None, &actual_model, true, latency, "");
                record_workbuddy_call(
                    started_ms,
                    &actual_model,
                    true,
                    latency,
                    "",
                    value.get("usage"),
                    "",
                );
                json_ok(value)
            }
            Err(e) => {
                let latency = started.elapsed().as_millis() as u64;
                log_gateway_request("workbuddy", None, &model, false, latency, &e);
                record_workbuddy_call(started_ms, &model, false, latency, &e, None, "");
                glm_proxy_error(e, StatusCode::BAD_GATEWAY)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OpenCode 网关（/opencode/v1）—— 本机 OpenCode server 的**免费模型专用**入口
//
// 与 WorkBuddy 网关同级、独立密钥/独立配置；server 进程与看门狗由 workbuddy_proxy 统一管理，
// 本网关只做「免费收敛 + 独立鉴权 + 对话委托」。
// ---------------------------------------------------------------------------

fn opencode_proxy_auth_ok(headers: &HeaderMap, cfg: &opencode::OpenCodeProxyConfig) -> bool {
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

/// 「客户端选错模型」用 400，「上游/server 故障」用 502 —— 便于调用方区分该改模型还是该重试。
fn opencode_error_status(msg: &str) -> StatusCode {
    if msg.contains("仅提供免费模型") || msg.contains("未指定模型") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
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
    if !opencode_proxy_auth_ok(&headers, &cfg) {
        log_gateway_request("opencode", None, "models", false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }
    match opencode::models_payload().await {
        Ok(value) => {
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
    if !opencode_proxy_auth_ok(&headers, &cfg) {
        log_gateway_request("opencode", None, &model, false, 0, "无效的 API Key");
        return glm_proxy_error("无效的 API Key".to_string(), StatusCode::UNAUTHORIZED);
    }

    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream {
        match opencode::chat_completions_stream(body).await {
            Ok(upstream) => {
                log_gateway_request(
                    "opencode",
                    None,
                    &model,
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
    const WITH_SUB: [&str; 15] = [
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
