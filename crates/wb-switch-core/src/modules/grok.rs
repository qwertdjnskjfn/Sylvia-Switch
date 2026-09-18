//! Grok 融合网关（移植并汲取 chenyme/grok2api 开源项目核心协议设计）。
//!
//! 把多个 Grok 账号（Grok Web SSO Cookie、Grok Build OAuth/API Key、Grok Console 凭证）
//! 汇聚为统一的 OpenAI 兼容接口（`/grok/v1/chat/completions` + `/grok/v1/models`），
//! 支持多账号轮询调度、自动故障转移与 429 冷却降级。
//!
//! 核心支持能力：
//! - Grok Web SSO 网页端会话反代，自动伪造现代 Chrome 浏览器指纹与同源请求头；
//! - Grok Build 官方开发者接口高速直连；
//! - 完整支持流式 SSE（`stream: true`）与非流式响应；
//! - 智能识别并实时转译深度思考过程（Reasoning / Thinking）与联网搜索（Web Search）；
//! - 账号池落盘至 `~/.wb-switch/grok_accounts.json`，支持去重、启用/禁用、在线探活与一键测试。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::modules::config::{atomic_write, now_ms, store_dir};

const GROK_WEB_BASE: &str = "https://grok.com";
const GROK_BUILD_BASE: &str = "https://api.x.ai";

/// 账号写入全局互斥锁
static WRITE_LOCK: Mutex<()> = Mutex::new(());
/// 轮询调度游标
static ROUND_ROBIN_INDEX: AtomicUsize = AtomicUsize::new(0);

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .build()
            .expect("failed to build grok client")
    })
}

// ---------------------------------------------------------------------------
// 账号池存储（~/.wb-switch/grok_accounts.json）
// ---------------------------------------------------------------------------

fn accounts_path() -> PathBuf {
    store_dir().join("grok_accounts.json")
}

pub fn load_accounts() -> Vec<Value> {
    std::fs::read_to_string(accounts_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("accounts").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

pub fn save_accounts(accounts: &[Value]) -> Result<(), String> {
    let path = accounts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "accounts": accounts,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

/// 脱敏凭证信息，用于安全展示
fn mask_secret(secret: &str) -> String {
    let s = secret.trim();
    if s.len() <= 12 {
        return "******".to_string();
    }
    format!("{}...{}", &s[..6], &s[s.len() - 4..])
}

/// 写入/更新一个账号（按 id 去重）
pub fn upsert_account(entry: Value) -> Result<Value, String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let now = now_ms();
    let mut stored = entry.clone();

    if let Some(existing) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
    {
        for field in [
            "name",
            "provider",
            "token",
            "refreshToken",
            "tier",
            "status",
            "enabled",
            "error",
            "cooldownUntil",
        ] {
            if let Some(value) = entry.get(field) {
                if !value.is_null() {
                    existing[field] = value.clone();
                }
            }
        }
        existing["updatedAt"] = json!(now);
        stored = existing.clone();
    } else {
        stored["addedAt"] = json!(now);
        stored["updatedAt"] = json!(now);
        if stored.get("enabled").is_none() {
            stored["enabled"] = json!(true);
        }
        if stored.get("status").is_none() {
            stored["status"] = json!("ok");
        }
        accounts.push(stored.clone());
    }

    save_accounts(&accounts)?;
    Ok(stored)
}

/// 删除账号
pub fn remove_account(id: &str) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let before = accounts.len();
    accounts.retain(|a| a.get("id").and_then(Value::as_str) != Some(id));
    if accounts.len() == before {
        return false;
    }
    save_accounts(&accounts).is_ok()
}

/// 启用/禁用账号
pub fn set_account_enabled(id: &str, enabled: bool) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(item) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    {
        item["enabled"] = json!(enabled);
        item["updatedAt"] = json!(now_ms());
        return save_accounts(&accounts).is_ok();
    }
    false
}

/// 导入账号（支持 SSO Cookie、xAI API Key 等）
pub fn import_account(
    name: &str,
    provider: &str,
    token: &str,
    tier: Option<&str>,
) -> Result<Value, String> {
    let clean_token = token.trim();
    if clean_token.is_empty() {
        return Err("凭证内容不能为空".to_string());
    }

    let clean_provider = match provider.to_lowercase().as_str() {
        "build" | "api" | "oauth" => "build",
        "console" => "console",
        _ => "web",
    };

    let id = format!("grok_{}_{}", clean_provider, &uuid::Uuid::new_v4().to_string()[..8]);
    let display_name = if name.trim().is_empty() {
        match clean_provider {
            "build" => format!("Grok Build ({})", &clean_token[..clean_token.len().min(6)]),
            "console" => "Grok Console 账号".to_string(),
            _ => "Grok Web 账号".to_string(),
        }
    } else {
        name.trim().to_string()
    };

    let entry = json!({
        "id": id,
        "name": display_name,
        "provider": clean_provider,
        "token": clean_token,
        "tier": tier.unwrap_or("free"),
        "enabled": true,
        "status": "ok",
        "error": null,
        "cooldownUntil": 0,
        "lastUsedAt": null,
        "addedAt": now_ms(),
        "updatedAt": now_ms(),
    });

    upsert_account(entry)
}

/// 获取面向前端列表展示的脱敏数据
pub fn list_for_display() -> Vec<Value> {
    let accounts = load_accounts();
    let now = now_ms();
    accounts
        .into_iter()
        .map(|mut acc| {
            if let Some(token) = acc.get("token").and_then(Value::as_str) {
                acc["tokenMasked"] = json!(mask_secret(token));
            }
            let cooldown_until = acc
                .get("cooldownUntil")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            acc["isCooling"] = json!(cooldown_until > now);
            acc
        })
        .collect()
}

/// 标记账号冷却（如遇到 429 / 限流 / 超时）。
///
/// v1.5.0：同时更新健康分（限流/超时扣分）与连续失败计数，
/// 交由 grok_pool 的策略阈值推导风险等级。
fn mark_account_cooldown(id: &str, cooldown_seconds: u64, error_msg: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let policy = crate::modules::grok_pool::load_policy();
    let mut accounts = load_accounts();
    if let Some(item) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    {
        let until = now_ms() + (cooldown_seconds * 1000) as i64;
        item["cooldownUntil"] = json!(until);
        item["status"] = json!("cooldown");
        item["error"] = json!(error_msg);
        crate::modules::grok_pool::ensure_health_fields(item);
        // 限流与超时按不同权重扣分。
        let is_rate_limit = error_msg.contains("429") || error_msg.contains("限流") || error_msg.contains("使用人数");
        let penalty = if is_rate_limit {
            policy.rate_limit_penalty
        } else {
            policy.timeout_penalty
        };
        let cur = item.get("healthScore").and_then(Value::as_i64).unwrap_or(100);
        let h = crate::modules::grok_pool::next_health(cur, -penalty);
        item["healthScore"] = json!(h);
        item["riskLevel"] = json!(crate::modules::grok_pool::risk_level(h, &policy));
        let cf = item.get("consecutiveFailures").and_then(Value::as_i64).unwrap_or(0) + 1;
        item["consecutiveFailures"] = json!(cf);
        if is_rate_limit {
            let rc = item.get("rateLimitCount").and_then(Value::as_i64).unwrap_or(0) + 1;
            item["rateLimitCount"] = json!(rc);
            item["lastErrorKind"] = json!("rate_limit");
        } else {
            item["lastErrorKind"] = json!("timeout");
        }
        item["updatedAt"] = json!(now_ms());
        let acc_name = item.get("name").and_then(Value::as_str).unwrap_or("账号").to_string();
        let _ = save_accounts(&accounts);

        // C2：限流是风控升温的早期信号，值得单独告警（notify 内部 10 分钟节流防刷屏）。
        if is_rate_limit {
            crate::modules::notify::notify(
                "grok-pool",
                "Grok 账号触发限流",
                &format!("账号「{acc_name}」遇到限流，已自动冷却降级；系统将避让该账号直至恢复。"),
            );
        }
    }
}

/// 标记账号错误（如 401 凭证失效）。
///
/// v1.5.0：带指数退避 —— 首次 30 分钟，之后 2h / 6h / 24h（上限）。
/// 过了 errorRetryAt 账号会重新进入候选池尝试，成功即自动恢复；
/// 这样既不会永久跳过"被误判"的账号，也不会让真失效账号每轮都白试。
fn mark_account_error(id: &str, error_msg: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(item) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    {
        let level = item.get("errorRetryLevel").and_then(Value::as_i64).unwrap_or(0);
        let new_level = (level + 1).min(4);
        let backoff_ms: i64 = match new_level {
            1 => 30 * 60 * 1000,
            2 => 2 * 3600 * 1000,
            3 => 6 * 3600 * 1000,
            _ => 24 * 3600 * 1000,
        };
        item["status"] = json!("error");
        item["error"] = json!(error_msg);
        item["errorRetryLevel"] = json!(new_level);
        item["errorRetryAt"] = json!(now_ms() + backoff_ms);
        // v1.5.0：致命失效重扣健康分。
        let policy = crate::modules::grok_pool::load_policy();
        crate::modules::grok_pool::ensure_health_fields(item);
        let cur = item.get("healthScore").and_then(Value::as_i64).unwrap_or(100);
        let h = crate::modules::grok_pool::next_health(cur, -policy.fatal_penalty);
        item["healthScore"] = json!(h);
        item["riskLevel"] = json!(crate::modules::grok_pool::risk_level(h, &policy));
        let cf = item.get("consecutiveFailures").and_then(Value::as_i64).unwrap_or(0) + 1;
        item["consecutiveFailures"] = json!(cf);
        item["lastErrorKind"] = json!("fatal");
        item["updatedAt"] = json!(now_ms());
        let _ = save_accounts(&accounts);
    }
}

/// 记录账号调用成功
fn mark_account_success(id: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(item) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    {
        item["status"] = json!("ok");
        item["error"] = Value::Null;
        // v1.5.0：成功后清除 error 退避状态（账号已恢复健康）。
        item["errorRetryLevel"] = Value::Null;
        item["errorRetryAt"] = Value::Null;
        item["lastUsedAt"] = json!(now_ms());
        // v1.5.0：成功回补健康分、清零连续失败。
        let policy = crate::modules::grok_pool::load_policy();
        crate::modules::grok_pool::ensure_health_fields(item);
        let cur = item.get("healthScore").and_then(Value::as_i64).unwrap_or(100);
        let h = crate::modules::grok_pool::next_health(cur, policy.success_delta);
        item["healthScore"] = json!(h);
        item["riskLevel"] = json!(crate::modules::grok_pool::risk_level(h, &policy));
        item["consecutiveFailures"] = json!(0);
        item["lastSuccessAt"] = json!(now_ms());
        item["updatedAt"] = json!(now_ms());
        let _ = save_accounts(&accounts);
    }
}

// ---------------------------------------------------------------------------
// 模型目录与映射
// ---------------------------------------------------------------------------

pub fn models_payload() -> Value {
    let models = vec![
        json!({
            "id": "grok-4.6",
            "object": "model",
            "created": 1780000000,
            "owned_by": "xai",
            "description": "Grok 4.6 最新旗舰（需 Build 通道账号）"
        }),
        json!({
            "id": "grok-4",
            "object": "model",
            "created": 1745000000,
            "owned_by": "xai",
            "description": "Grok 4 旗舰基座模型，推理能力全面升级"
        }),
        json!({
            "id": "grok-4-fast",
            "object": "model",
            "created": 1745000000,
            "owned_by": "xai",
            "description": "Grok 4 高速版，速度与智能平衡"
        }),
        json!({
            "id": "grok-4-thinking",
            "object": "model",
            "created": 1745000000,
            "owned_by": "xai",
            "description": "Grok 4 深度思考模式，完整思维链推演"
        }),
        json!({
            "id": "grok-beta",
            "object": "model",
            "created": 1710000000,
            "owned_by": "xai",
            "description": "Grok 官方通用主力模型"
        }),
        json!({
            "id": "grok-2",
            "object": "model",
            "created": 1720000000,
            "owned_by": "xai",
            "description": "Grok 2 标准大模型，具备高智能推理与编程表现"
        }),
        json!({
            "id": "grok-2-mini",
            "object": "model",
            "created": 1720000000,
            "owned_by": "xai",
            "description": "Grok 2 轻量版，兼具极速首字延迟与轻快响应"
        }),
        json!({
            "id": "grok-2-vision",
            "object": "model",
            "created": 1720000000,
            "owned_by": "xai",
            "description": "Grok 2 多模态视觉模型，支持图文理解"
        }),
        json!({
            "id": "grok-3",
            "object": "model",
            "created": 1735000000,
            "owned_by": "xai",
            "description": "Grok 3 新一代旗舰基座模型"
        }),
        json!({
            "id": "grok-3-mini",
            "object": "model",
            "created": 1735000000,
            "owned_by": "xai",
            "description": "Grok 3 轻量极速推理模型"
        }),
        json!({
            "id": "grok-3-thinking",
            "object": "model",
            "created": 1735000000,
            "owned_by": "xai",
            "description": "Grok 3 深度思考模式（输出完整思维链与详细推演）"
        }),
        json!({
            "id": "grok-chat-fast",
            "object": "model",
            "created": 1715000000,
            "owned_by": "xai",
            "description": "Grok 网页端极速通道（Fast Mode）"
        }),
        json!({
            "id": "grok-chat-auto",
            "object": "model",
            "created": 1715000000,
            "owned_by": "xai",
            "description": "Grok 网页端自适应通道（Auto Mode）"
        }),
        json!({
            "id": "grok-chat-expert",
            "object": "model",
            "created": 1715000000,
            "owned_by": "xai",
            "description": "Grok 网页端专家深度推理通道（Expert Mode）"
        }),
    ];

    json!({
        "object": "list",
        "data": models
    })
}

// ---------------------------------------------------------------------------
// 核心协议转译与通信引擎
// ---------------------------------------------------------------------------

pub enum GrokChatOutput {
    Json(Value),
    Stream(reqwest::Response),
}

/// 调度选出一个可用的 Grok 账号
fn pick_available_account() -> Result<Value, String> {
    let accounts = load_accounts();
    if accounts.is_empty() {
        return Err("当前 Grok 网关尚未配置任何可用账号，请先进入「Grok 网关」页面添加账号凭证".to_string());
    }

    let now = now_ms();
    let candidates: Vec<&Value> = accounts
        .iter()
        .filter(|a| {
            let enabled = a.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let cooldown = a.get("cooldownUntil").and_then(Value::as_i64).unwrap_or(0);
            // v1.5.0：error 账号不再永久跳过 —— 过了 errorRetryAt 退避期后重新
            // 纳入候选尝试，成功即自动恢复。历史无 errorRetryAt 字段的账号
            // （retry_at=0）会被立即纳入，实现"假 error 自愈"。
            let errored = a.get("status").and_then(Value::as_str) == Some("error");
            let retry_at = a.get("errorRetryAt").and_then(Value::as_i64).unwrap_or(0);
            let blocked_by_error = errored && retry_at > now;
            enabled && cooldown <= now && !blocked_by_error
        })
        .collect();

    if candidates.is_empty() {
        return Err("所有 Grok 账号当前均处于冷却或禁用状态，请稍后重试或补充新账号".to_string());
    }

    // v1.5.0：驻留优先 —— 若当前有未过期的驻留账号且可用，直接复用它，
    // 避免每条请求都 round-robin 切号（高频身份横跳会触发风控）。
    if let Some(resident_id) = crate::modules::grok_sso::resident_account_id() {
        if let Some(acc) = candidates
            .iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(resident_id.as_str()))
        {
            return Ok((*acc).clone());
        }
    }

    let idx = ROUND_ROBIN_INDEX.fetch_add(1, Ordering::Relaxed) % candidates.len();
    Ok(candidates[idx].clone())
}

/// 从 OpenAI messages 列表解析出最新用户提示词和历史上下文
fn extract_prompt_from_messages(messages: &[Value]) -> (String, String) {
    let mut system_prompt = String::new();
    let mut conversation_text = String::new();

    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = if let Some(text) = m.get("content").and_then(Value::as_str) {
            text.to_string()
        } else if let Some(arr) = m.get("content").and_then(Value::as_array) {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t);
                }
            }
            parts.join("\n")
        } else {
            String::new()
        };

        if role == "system" {
            if !system_prompt.is_empty() {
                system_prompt.push('\n');
            }
            system_prompt.push_str(&content);
        } else {
            if !conversation_text.is_empty() {
                conversation_text.push_str("\n\n");
            }
            let role_label = match role {
                "assistant" => "Assistant",
                _ => "User",
            };
            conversation_text.push_str(&format!("{}: {}", role_label, content));
        }
    }

    if conversation_text.is_empty() {
        conversation_text = "你好".to_string();
    }

    (system_prompt, conversation_text)
}

/// 执行统一的 chat_completions 请求
pub async fn chat_completions_unified(payload: Value) -> Result<GrokChatOutput, String> {
    // v1.1.0：流式请求也走 WebReply 统一路径，最后按 payload.stream 合成输出。
    let want_stream = payload.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("grok-2")
        .to_string();

    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let (system_prompt, prompt) = extract_prompt_from_messages(&messages);

    // 账号挑选与单次重试
    let mut last_err = String::new();
    for attempt in 0..2 {
        let account = match pick_available_account() {
            Ok(acc) => acc,
            Err(e) => return Err(e),
        };

        let account_id = account.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let provider = account.get("provider").and_then(Value::as_str).unwrap_or("web");
        let token = account.get("token").and_then(Value::as_str).unwrap_or_default();

        let req_res = match provider {
            "build" => {
                let resp = send_build_request(&account_id, token, &model, &payload).await?;
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                Ok(crate::modules::grok::WebReply { status, body })
            }
            _ => {
                send_web_request(&account_id, token, &model, &system_prompt, &prompt).await
            }
        };

        match req_res {
            Ok(reply) => {
                mark_account_success(&account_id);
                let mut req_res_body = Some(reply.body);
                let upstream_status = reply.status;

                // v1.1.0：WebReply 无逐 token 流（CDP fetch 一次性返回），流式请求
                // 走下方统一解析后合成 OpenAI chunk 帧（与 glm_proxy / loomy_proxy 同策略）。
                let _ = upstream_status;

                // 非流式：聚合上游流/数据为完整 chat.completion JSON
                let mut content = String::new();
                let mut reasoning = String::new();
                let completion_id = format!("chatcmpl-{}", &uuid::Uuid::new_v4().to_string()[..12]);

                // v1.1.0：WebReply.body（CDP fetch 为完整 JSON / 直连为 SSE 文本）统一按行解析。
                let resp_text = req_res_body.take().unwrap_or_default();
                {
                    let text: &str = &resp_text;
                    for line in text.lines().chain(std::iter::once("")) {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let data = if let Some(d) = trimmed.strip_prefix("data:") {
                            d.trim()
                        } else {
                            trimmed
                        };
                        if data == "[DONE]" || data.is_empty() {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            // Grok Web 或 OpenAI SSE 格式提取
                            if let Some(delta) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                                content.push_str(delta);
                            } else if let Some(msg) = v.pointer("/result/response/modelResponse/message").and_then(Value::as_str) {
                                content.push_str(msg);
                            } else if let Some(t) = v.pointer("/result/response/token").and_then(Value::as_str) {
                                content.push_str(t);
                            }
                            // 思考链提取
                            if let Some(r) = v.pointer("/choices/0/delta/reasoning_content").and_then(Value::as_str) {
                                reasoning.push_str(r);
                            } else if let Some(r_str) = v.pointer("/result/response/reasoningToken").and_then(Value::as_str) {
                                reasoning.push_str(r_str);
                            }
                        }
                    }
                }

                let final_message_content = if !reasoning.is_empty() {
                    format!("<think>\n{}\n</think>\n\n{}", reasoning, content)
                } else if content.is_empty() {
                    "Grok 服务已响应（无文本返回内容）".to_string()
                } else {
                    content
                };

                let result_json = json!({
                    "id": completion_id,
                    "object": "chat.completion",
                    "created": now_ms() / 1000,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": final_message_content,
                            "reasoning_content": if reasoning.is_empty() { Value::Null } else { json!(reasoning) }
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": {
                        "prompt_tokens": prompt.len() / 3,
                        "completion_tokens": final_message_content.len() / 3,
                        "total_tokens": (prompt.len() + final_message_content.len()) / 3
                    }
                });

                return Ok(GrokChatOutput::Json(result_json));
            }
            Err(e) => {
                last_err = e;
                if attempt == 0 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
            }
        }
    }

    Err(if last_err.is_empty() {
        "Grok 请求未能成功完成".to_string()
    } else {
        last_err
    })
}

// ---------------------------------------------------------------------------
// 上游 HTTP 请求发送封装
// ---------------------------------------------------------------------------

async fn send_build_request(
    account_id: &str,
    token: &str,
    model: &str,
    payload: &Value,
) -> Result<reqwest::Response, String> {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", GROK_BUILD_BASE);

    let auth_header = if token.starts_with("Bearer ") {
        token.to_string()
    } else {
        format!("Bearer {}", token)
    };

    let mut upstream_payload = payload.clone();
    if upstream_payload.get("model").is_none() {
        upstream_payload["model"] = json!(model);
    }
    // 强制上游流式便于统一响应
    upstream_payload["stream"] = json!(true);

    let resp = client
        .post(&url)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/json")
        .json(&upstream_payload)
        .send()
        .await
        .map_err(|e| format!("Grok Build 连接失败: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 429 {
        mark_account_cooldown(account_id, 60, "遇到 429 请求频率限制");
        return Err("Grok Build 账号遇到 429 频率限制，已自动冷却调度".to_string());
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        mark_account_error(account_id, "Grok Build API Key 无效或权限不足");
        return Err("Grok Build 凭据无效或已失效".to_string());
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Grok Build 上游错误 ({}): {}", status, text));
    }

    Ok(resp)
}

fn normalize_web_cookie(raw_cookie: &str) -> String {
    let trimmed = raw_cookie.trim();
    if trimmed.contains("sso=") {
        trimmed.to_string()
    } else {
        format!("sso={}; sso-rw={}", trimmed, trimmed)
    }
}

/// CDP fetch 桥的返回（合成流式响应用）。
pub struct WebReply {
    pub status: u16,
    pub body: String,
}

async fn send_web_request(
    account_id: &str,
    token: &str,
    model: &str,
    system_prompt: &str,
    prompt: &str,
) -> Result<WebReply, String> {
    // 注：直连路径已移除；model / is_thinking_model 等参数不再使用（Web 模型由页面决定）。
    let _ = model;
    let cookie_val = normalize_web_cookie(token);

    let full_prompt = if !system_prompt.is_empty() {
        format!("系统指令: {}\n\n{}", system_prompt, prompt)
    } else {
        prompt.to_string()
    };

    // v1.5.0：web 通道唯一走 UI 驱动 + 账号路由（grok_sso::ui_chat_with_account）。
    //
    // 不再回退直连（2026-09-17 修正）：直连必被 Cloudflare 403，且当专用浏览器
    // 未运行时会把「浏览器挂了」误标成「SSO 过期」，污染账号状态 —— 实测导致多
    // 个 web 账号被错误标记为 error。ui_chat_with_account 内部会 ensure_browser
    // 自动拉起专用浏览器，因此浏览器崩溃也能自愈。
    match crate::modules::grok_sso::ui_chat_with_account(account_id, &cookie_val, &full_prompt).await {
        Ok(reply_text) => Ok(WebReply {
            status: 200,
            body: json!({
                "result": {
                    "response": {
                        "modelResponse": { "message": reply_text },
                        "modelResponseTokens": Value::Null,
                    }
                }
            })
            .to_string(),
        }),
        Err(chat_err) => {
            eprintln!(
                "[grok] UI 驱动对话失败 ({:?}): {}",
                chat_err.kind, chat_err.message
            );
            // 按错误类型分流：临时问题只冷却，账号失效才标 error。
            if chat_err.is_account_fatal() {
                mark_account_error(account_id, &format!("UI 驱动失败: {}", chat_err.message));
            } else {
                let secs = chat_err.cooldown_seconds();
                if secs > 0 {
                    mark_account_cooldown(account_id, secs, &chat_err.message);
                }
            }
            Err(format!("Grok Web UI 驱动失败：{}", chat_err.message))
        }
    }
}

// ---------------------------------------------------------------------------
// 网页前端在线测试接口
// ---------------------------------------------------------------------------

pub async fn test_chat(
    model: &str,
    prompt: &str,
    enable_thinking: bool,
    account_id: Option<&str>,
) -> Result<Value, String> {
    let start = Instant::now();

    let target_account = if let Some(id) = account_id {
        let accounts = load_accounts();
        accounts
            .into_iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| format!("未找到指定的账号: {}", id))?
    } else {
        pick_available_account()?
    };

    let selected_id = target_account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let account_name = target_account
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Grok 账号")
        .to_string();
    let provider = target_account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("web")
        .to_string();

    let actual_model = if enable_thinking {
        "grok-3-thinking"
    } else if model.is_empty() {
        "grok-2"
    } else {
        model
    };

    let payload = json!({
        "model": actual_model,
        "stream": false,
        "messages": [
            { "role": "user", "content": prompt }
        ]
    });

    let token = target_account
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let chat_res = match provider.as_str() {
        "build" => {
            let resp = send_build_request(&selected_id, token, actual_model, &payload).await?;
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Ok(crate::modules::grok::WebReply { status, body })
        }
        _ => send_web_request(&selected_id, token, actual_model, "", prompt).await,
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    match chat_res {
        Ok(reply) => {
            mark_account_success(&selected_id);
            let mut collected = String::new();
            let mut reasoning = String::new();

            // v1.1.0：WebReply.body 统一按行解析（CDP=完整 JSON / 直连=SSE 文本）。
            let resp_text = reply.body;
            {
                let text: &str = &resp_text;
                for line in text.lines().chain(std::iter::once("")) {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let data = if let Some(d) = trimmed.strip_prefix("data:") {
                        d.trim()
                    } else {
                        trimmed
                    };
                    if data == "[DONE]" || data.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(delta) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                            collected.push_str(delta);
                        } else if let Some(msg) = v.pointer("/result/response/modelResponse/message").and_then(Value::as_str) {
                            collected.push_str(msg);
                        } else if let Some(t) = v.pointer("/result/response/token").and_then(Value::as_str) {
                            collected.push_str(t);
                        }
                        if let Some(r) = v.pointer("/choices/0/delta/reasoning_content").and_then(Value::as_str) {
                            reasoning.push_str(r);
                        } else if let Some(r_str) = v.pointer("/result/response/reasoningToken").and_then(Value::as_str) {
                            reasoning.push_str(r_str);
                        }
                    }
                }
            }

            let reply = if !reasoning.is_empty() {
                format!("<think>\n{}\n</think>\n\n{}", reasoning, collected)
            } else if collected.is_empty() {
                "连接成功，Grok 服务已响应".to_string()
            } else {
                collected
            };

            Ok(json!({
                "ok": true,
                "reply": reply,
                "model": actual_model,
                "provider": provider,
                "accountId": selected_id,
                "accountName": account_name,
                "durationMs": duration_ms,
                "tokens": reply.len() / 3,
            }))
        }
        Err(e) => {
            // v1.5.0：不再在此处 mark_account_error —— send_web_request /
            // send_build_request 内部已按错误类型正确分流标记（临时问题冷却、
            // 失效才 error）。此处二次标记会把刚冷却的账号误打成 error（实测
            // 超时导致 de34bdcc 被标 fatal 即此因）。
            let _ = &selected_id;
            Err(format!("Grok 试调失败: {e}"))
        }
    }
}
