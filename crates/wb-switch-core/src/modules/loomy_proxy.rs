//! Loomy 模型代理：把 Loomy 订阅里的模型暴露为 OpenAI 兼容接口。
//!
//! 上游是讯飞 Loomy 积分网关 `https://loomyad.xunfei.cn/api/v1`
//! （OpenAI 兼容协议，2026-09-15 实测）：
//! - `GET /models` —— 模型目录（`token: <session>` 头）；
//! - `POST /chat/completions` —— 对话补全（`Authorization: Bearer <session>`），
//!   响应 usage 带 `points_consumed`（真实扣分）。
//!
//! 凭证：本机 Loomy `auth-session.json`（明文 session）+ 账号池
//! `~/.wb-switch/loomy_accounts.json`，401/100002 顺延下一个账号。
//! session 无续期机制（14 天有效期，登录时固定 expire），失效账号标记
//! invalid 并提示重新导入。仅供本机 127.0.0.1 使用。

use bytes::Bytes;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::loomy::{loomy_headers, LOOMY_API_HOST};

const UPSTREAM: &str = "https://loomyad.xunfei.cn/api/v1";

// ---------------------------------------------------------------------------
// 配置（~/.wb-switch/loomy_proxy_config.json）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct LoomyProxyConfig {
    pub enabled: bool,
    pub api_key: String,
}

fn config_path() -> PathBuf {
    store_dir().join("loomy_proxy_config.json")
}

pub fn load_config() -> LoomyProxyConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| LoomyProxyConfig {
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            api_key: v
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .unwrap_or_else(|| {
            let cfg = LoomyProxyConfig {
                enabled: true,
                api_key: crate::modules::glm_proxy::generate_api_key(),
            };
            let _ = save_config(&cfg);
            cfg
        })
}

pub fn save_config(cfg: &LoomyProxyConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "enabled": cfg.enabled,
        "api_key": cfg.api_key,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

static POOL_CURSOR: AtomicUsize = AtomicUsize::new(0);
/// 记录最近一次代理错误（状态页展示）。
static LAST_ERROR: Mutex<Option<(i64, String)>> = Mutex::new(None);

pub(crate) fn note_error(error: &str) {
    let mut guard = LAST_ERROR.lock().unwrap();
    *guard = Some((now_ms(), error.to_string()));
}

pub(crate) fn last_error() -> Option<Value> {
    let guard = LAST_ERROR.lock().unwrap();
    guard.as_ref().map(|(ts, e)| json!({"ts": ts, "error": e}))
}

// ---------------------------------------------------------------------------
// 模型列表
// ---------------------------------------------------------------------------

/// 上游模型目录（缓存 10 分钟）。
static MODELS_CACHE: Mutex<Option<(i64, Value)>> = Mutex::new(None);
const MODELS_TTL_MS: i64 = 10 * 60 * 1000;

/// 获取过滤冷却账号并按游标轮询排序的可用账号列表。
pub(crate) fn ordered_pool() -> Vec<(String, String, Option<String>)> {
    let all = crate::modules::loomy::pool_sessions();
    if all.is_empty() {
        return Vec::new();
    }
    // 过滤掉当前正处于冷却保护期的账号
    let available: Vec<_> = all
        .into_iter()
        .filter(|(id, _, _)| !crate::modules::account_protection::is_account_cooling(id))
        .collect();
    if available.is_empty() {
        return Vec::new();
    }
    let start = POOL_CURSOR.fetch_add(1, Ordering::Relaxed) % available.len();
    let mut ordered = Vec::with_capacity(available.len());
    for i in 0..available.len() {
        ordered.push(available[(start + i) % available.len()].clone());
    }
    ordered
}

/// 上游「空闲读」上限（秒）：连续这么久**收不到任何字节**才判定断流。
///
/// 🔴 为什么不用 reqwest 的请求级 `.timeout()`（2026-09-18 事故根因）：
/// 请求级 timeout 覆盖「连接 + 直到响应体**读完**」的**总时长**，对流式补全
/// 是致命的——长任务（agent 长思考 + 长输出）一旦超过该时长，客户端会在
/// 上游**仍正常输出**时单方面掐断 body，reqwest 只回一句
/// `error decoding response body`，于是被误判成「上游断流 / 上下文过长」。
/// 实测时间线：请求 02:30:37 开始 → 300s 后 02:35:37 报错，与总超时严丝合缝。
///
/// `read_timeout` 是「两次读到数据之间」的空闲上限，**读到数据即重置**：
/// 长回答不会被总时长误杀，而真卡死（上游一个字节都不再吐）仍会及时断开。
const UPSTREAM_READ_IDLE_SECS: u64 = 300;

/// 构造注入官方拟真 UA 的 HTTP 客户端（全局单例连接池，复用 TCP/TLS 保持长连接）。
///
/// ⚠️ 超时一律在 **client 层**用 connect + read 表达，禁止在单个请求上叠
/// `.timeout()`——那会重新引入上面的总时长掐断问题。
fn upstream_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let ua = crate::modules::account_protection::simulated_loomy_user_agent();
        reqwest::Client::builder()
            .no_proxy() // 国内上游直连，不继承系统代理
            .user_agent(ua)
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(UPSTREAM_READ_IDLE_SECS))
            .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
            .pool_idle_timeout(Some(std::time::Duration::from_secs(90)))
            .pool_max_idle_per_host(10)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// 获取单账号并发槽位，若忙碌则在 2.5 秒内自适应排队等待
async fn acquire_concurrency_for_account(
    account_id: &str,
    max_concurrency: usize,
) -> Result<crate::modules::account_protection::ConcurrencyGuard, String> {
    if let Some(guard) =
        crate::modules::account_protection::try_acquire_concurrency_slot(account_id, max_concurrency)
    {
        return Ok(guard);
    }
    let wait_start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(2500);
    while wait_start.elapsed() < timeout {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(guard) =
            crate::modules::account_protection::try_acquire_concurrency_slot(account_id, max_concurrency)
        {
            return Ok(guard);
        }
    }
    Err(format!(
        "账号「{account_id}」并发槽位已满（上限 {max_concurrency}），已自动顺延下一个账号"
    ))
}

/// 拉取 Loomy 模型目录（任一有效账号的 session 即可）。
pub async fn fetch_models() -> Result<Value, String> {
    {
        let guard = MODELS_CACHE.lock().unwrap();
        if let Some((at, value)) = guard.as_ref() {
            if now_ms() - at < MODELS_TTL_MS {
                return Ok(value.clone());
            }
        }
    }
    let pool = ordered_pool();
    if pool.is_empty() {
        return Err("Loomy 可用账号池为空（无有效账号或全部账号处于安全冷却保护中）".to_string());
    }
    let mut last_error = String::new();
    for (_, session, _) in &pool {
        let url = format!("{UPSTREAM}/models");
        let headers = loomy_headers(session);
        let resp = crate::modules::config::http_request(&url, "GET", None, Some(&headers)).await;
        // /models 返回 OpenAI list 格式（object: "list"），不经过积分网关的业务码包装。
        if resp.get("object").and_then(Value::as_str) == Some("list") {
            let mut guard = MODELS_CACHE.lock().unwrap();
            *guard = Some((now_ms(), resp.clone()));
            return Ok(resp);
        }
        last_error = resp
            .get("desc")
            .or_else(|| resp.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误")
            .to_string();
    }
    Err(format!("获取 Loomy 模型列表失败：{last_error}"))
}

/// OpenAI `/v1/models` 风格响应（供代理端点直接透出）。
pub async fn models_payload() -> Result<Value, String> {
    fetch_models().await
}

/// 状态探测：账号池规模 + 模型目录可达性。
pub async fn proxy_status() -> Value {
    let cfg = load_config();
    let pool = ordered_pool();
    let models = match fetch_models().await {
        Ok(value) => {
            let count = value
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            json!({"ok": true, "count": count})
        }
        Err(error) => json!({"ok": false, "error": error}),
    };
    json!({
        "enabled": cfg.enabled,
        "upstream": UPSTREAM,
        "poolSize": pool.len(),
        "models": models,
        "lastError": last_error(),
    })
}

// ---------------------------------------------------------------------------
// Chat 转发（OpenAI 兼容）
// ---------------------------------------------------------------------------

/// 针对 Loomy 上游网关做参数兼容与安全清洗：
/// 1. 模型名统一归一化（不区分大小写匹配真实 ID，如 `glm-5.3-flash` -> `GLM-5.3-Flash`）；
/// 2. `max_tokens` 安全钳位：上游硬上限为 131072（128K），超限上游直接报 400，此处自动安全钳位；
/// 3. 空 content 与无效属性修正。
pub(crate) fn sanitize_loomy_payload(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };

    // 1. 模型名统一归一化映射
    if let Some(m) = obj.get("model").and_then(Value::as_str) {
        let trimmed = m.trim();
        if trimmed.eq_ignore_ascii_case("glm-5.3-flash")
            || trimmed.eq_ignore_ascii_case("glm-5.3")
            || trimmed.eq_ignore_ascii_case("glm5.3-flash")
            || trimmed.eq_ignore_ascii_case("glm5.3")
            || trimmed.eq_ignore_ascii_case("GLM-5.3-Flash")
        {
            obj.insert("model".to_string(), json!("GLM-5.3-Flash"));
        } else if trimmed.eq_ignore_ascii_case("kimi-k2.6") {
            obj.insert("model".to_string(), json!("Kimi-k2.6"));
        } else if trimmed.eq_ignore_ascii_case("minimax-m3") {
            obj.insert("model".to_string(), json!("MiniMax-M3"));
        } else if trimmed.eq_ignore_ascii_case("deepseek-v4-flash-0731") {
            obj.insert("model".to_string(), json!("deepseek-v4-flash-0731"));
        } else if trimmed.eq_ignore_ascii_case("spark-x") {
            obj.insert("model".to_string(), json!("spark-x"));
        }
    }

    // 2. max_tokens 安全钳位
    if let Some(mt) = obj.get("max_tokens").and_then(Value::as_i64) {
        if mt > 131072 {
            obj.insert("max_tokens".to_string(), json!(131072));
        } else if mt <= 0 {
            obj.remove("max_tokens");
        }
    }

    // 3. 消息 content 与 tool_calls 兼容
    if let Some(msgs) = obj.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in msgs {
            if let Some(msg_obj) = msg.as_object_mut() {
                if msg_obj.get("content").map(Value::is_null).unwrap_or(false)
                    && !msg_obj.contains_key("tool_calls")
                {
                    msg_obj.insert("content".to_string(), json!(""));
                }
            }
        }
    }
}

/// 非流式对话补全：透传上游 OpenAI 格式响应。
pub async fn chat_completions_json(mut body: Value) -> Result<Value, String> {
    sanitize_loomy_payload(&mut body);
    let pool = ordered_pool();
    if pool.is_empty() {
        return Err("Loomy 可用账号池为空（无有效账号或全部账号处于安全冷却保护中）".to_string());
    }
    let prot_cfg = crate::modules::account_protection::load_protection_config();
    let max_concurrency = if prot_cfg.enabled {
        prot_cfg.max_concurrency_per_account.max(1)
    } else {
        16
    };
    let started = now_ms();
    let model = body.get("model").and_then(Value::as_str).unwrap_or("spark-x");
    let mut last_error = String::new();

    for (account_id, session, store_id) in &pool {
        let guard = match acquire_concurrency_for_account(account_id, max_concurrency).await {
            Ok(g) => g,
            Err(e) => {
                last_error = e;
                continue;
            }
        };

        match send_once(session, &body).await {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.map_err(|e| format!("读取上游响应失败: {e}"))?;
                if status.is_success() {
                    drop(guard);
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|e| format!("上游响应解析失败: {e}"))?;
                    record_usage(&value, model, account_id, started);
                    return Ok(value);
                }
                drop(guard);
                let error = upstream_error(status, &text);

                // 1. 触发限频 / 风控 / 积分耗尽：自动将该账号置入冷却期并避开
                let is_rate_limit = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::FORBIDDEN
                    || text.contains("100003")
                    || text.contains("100004")
                    || text.contains("频控")
                    || text.contains("积分不足");
                if is_rate_limit {
                    crate::modules::account_protection::mark_account_cooling(
                        account_id,
                        account_id,
                        &format!("上游限频或额度不足 ({status})"),
                        prot_cfg.cooldown_minutes,
                    );
                    last_error = error;
                    continue;
                }

                // 2. 登录失效：尝试侦测本地客户端是否已刷新新 session 并静默自愈
                if status == reqwest::StatusCode::UNAUTHORIZED || text.contains("100002") {
                    let (synced, new_session_opt) = crate::modules::loomy::sync_local_session_if_changed();
                    if synced && new_session_opt.is_some() {
                        let fresh_session = new_session_opt.unwrap();
                        if let Ok(retry_resp) = send_once(&fresh_session, &body).await {
                            let retry_status = retry_resp.status();
                            if retry_status.is_success() {
                                if let Ok(retry_text) = retry_resp.text().await {
                                    if let Ok(value) = serde_json::from_str::<Value>(&retry_text) {
                                        record_usage(&value, model, account_id, started);
                                        return Ok(value);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(id) = store_id {
                        crate::modules::loomy::mark_account_error(id, "session 失效，请重新导入");
                    }
                    last_error = error;
                    continue;
                }

                // 服务端瞬时错误 (5xx)：顺延下一个账号容灾
                if status.is_server_error() {
                    last_error = error;
                    continue;
                }

                return Err(error);
            }
            Err(error) => {
                drop(guard);
                last_error = error;
            }
        }
    }
    let message = format!("全部 {} 个可用 Loomy 账号均不可用：{last_error}", pool.len());
    note_error(&message);
    Err(message)
}

fn upstream_error(status: reqwest::StatusCode, text: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(desc) = value.get("desc").and_then(Value::as_str) {
            return format!("Loomy 网关返回 {status}: {desc}");
        }
    }
    format!(
        "Loomy 网关返回 {status}: {}",
        text.chars().take(300).collect::<String>()
    )
}

async fn send_once(session: &str, body: &Value) -> Result<reqwest::Response, String> {
    let url = format!("{UPSTREAM}/chat/completions");
    let client = upstream_client();
    client
        .post(&url)
        .header("Authorization", format!("Bearer {}", session.trim()))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream, */*")
        .header("Accept-Language", "zh-CN,zh;q=0.9")
        .json(body)
        // 🔴 此处**不得**再链 `.timeout(...)`：请求级超时含响应体读取，
        // 会把超过该时长的正常长流掐断（见 UPSTREAM_READ_IDLE_SECS 注释）。
        // 超时策略统一由 upstream_client() 的 connect + read 表达。
        .send()
        .await
        .map_err(|e| format!("Loomy 网关请求失败: {}", super::glm_anthropic::error_chain(&e)))
}

/// 成功调用落库到模型用量（积分取上游 usage.points_consumed）。
fn record_usage(value: &Value, model: &str, account_id: &str, started: i64) {
    let usage = value.get("usage").cloned().unwrap_or(Value::Null);
    let credits = usage.get("points_consumed").and_then(Value::as_f64);
    let prompt = usage.get("prompt_tokens").and_then(Value::as_f64).unwrap_or(0.0);
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.pointer("/prompt_tokens_details/cache_read"))
        .or_else(|| usage.get("cached_tokens"))
        .or_else(|| usage.get("cache_read_input_tokens"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let uncached = (prompt - cached).max(0.0);
    let cache_hit = cached > 0.0;
    crate::modules::model_usage::record_usage(json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "ts": started,
        "model": model,
        "provider": "loomy",
        "accountId": account_id,
        "accountLabel": account_id,
        "promptTokens": json!(prompt),
        "completionTokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0)),
        "reasoningTokens": usage.pointer("/completion_tokens_details/reasoning_tokens").cloned().unwrap_or(Value::Null),
        "totalTokens": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
        "cachedTokens": json!(cached),
        "uncachedPromptTokens": json!(uncached),
        "cacheHit": json!(cache_hit),
        "credits": credits.map(|v| json!(v)).unwrap_or(Value::Null),
        "creditsPending": credits.is_none(),
        "ledgerId": Value::Null,
        "latencyMs": now_ms().saturating_sub(started),
        "ttfbMs": Value::Null,
        "ok": true,
    }));
}

/// 流式对话补全：透传上游 SSE（Loomy 网关本身支持 stream）。
pub async fn chat_completions_stream(mut body: Value) -> Result<LoomyUpstreamStream, String> {
    sanitize_loomy_payload(&mut body);
    let pool = ordered_pool();
    if pool.is_empty() {
        return Err("Loomy 可用账号池为空（无有效账号或全部账号处于安全冷却保护中）".to_string());
    }
    let prot_cfg = crate::modules::account_protection::load_protection_config();
    let max_concurrency = if prot_cfg.enabled {
        prot_cfg.max_concurrency_per_account.max(1)
    } else {
        16
    };
    let started = now_ms();
    let model = body.get("model").and_then(Value::as_str).unwrap_or("spark-x").to_string();
    let mut last_error = String::new();

    for (account_id, session, store_id) in &pool {
        let guard = match acquire_concurrency_for_account(account_id, max_concurrency).await {
            Ok(g) => g,
            Err(e) => {
                last_error = e;
                continue;
            }
        };

        match send_once(session, &body).await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let content_type = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("text/event-stream")
                        .to_string();
                    let account_id = account_id.clone();
                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
                    let guard_holder = guard;
                    let mut resp = resp;

                    // 首块预读：上游常在「思考阶段」（一个字节都没输出）被网关切断。
                    // 此时 HTTP 已是 200，若直接把流交给客户端，客户端只会看到
                    // 「200 之后的流内 error」，判定为 PROVIDER_BUSINESS_ERROR 且
                    // retryable=false，整轮直接失败。
                    // 先读首块 → 失败则换下一个账号重试；全部账号都失败时本函数返回
                    // Err，由路由层映射为 5xx，客户端据此自动重试。
                    let first_chunk = match resp.chunk().await {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => {
                            last_error = format!("账号「{account_id}」上游返回空流");
                            continue;
                        }
                        Err(e) => {
                            // 取完整 source 链：reqwest 的 Display 只有
                            // `error decoding response body` 这层包装，真因在 source 里。
                            last_error = super::glm_anthropic::upstream_interrupt_message(
                                &super::glm_anthropic::error_chain(&e),
                            );
                            continue;
                        }
                    };

                    tokio::spawn(async move {
                        let _guard = guard_holder; // 持有并发槽位直到流结束
                        let mut sse_text = String::new();
                        let mut carry: Vec<u8> = Vec::new();
                        let mut pending: Option<Bytes> = Some(first_chunk);
                        loop {
                            let next = match pending.take() {
                                Some(chunk) => Ok(Some(chunk)),
                                None => {
                                    // 8 秒无新数据（如处于深度思考推理中）则往下游发送 SSE 注释保活帧，
                                    // 避免客户端由于空闲读取超时（Read Timeout）而单方面切断连接。
                                    match tokio::time::timeout(std::time::Duration::from_secs(8), resp.chunk()).await {
                                        Ok(chunk_res) => chunk_res,
                                        Err(_) => {
                                            if tx.send(Ok(Bytes::from_static(b": keep-alive\n\n"))).await.is_err() {
                                                break;
                                            }
                                            continue;
                                        }
                                    }
                                }
                            };
                            match next {
                                Ok(Some(chunk)) => {
                                    sse_text.push_str(&super::glm_anthropic::decode_stream_chunk(
                                        &mut carry, &chunk,
                                    ));
                                    if tx.send(Ok(chunk)).await.is_err() {
                                        break;
                                    }
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    // 上游流中断：reqwest 原文（`error decoding response body`
                                    // 等）对使用者无意义，复用 GLM 通路的翻译逻辑给出可行动说明。
                                    // ⚠️ 必须传 source 链，否则「超时 / 连接被重置」都会被外层
                                    // 包装文案吃掉，只剩含糊的「响应被切断」。
                                    let _ = tx
                                        .send(Err(super::glm_anthropic::upstream_interrupt_message(
                                            &super::glm_anthropic::error_chain(&e),
                                        )))
                                        .await;
                                    break;
                                }
                            }
                        }
                        // 流结束：先把未凑齐的码点尾部排空进 sse_text
                        if !carry.is_empty() {
                            sse_text.push_str(&String::from_utf8_lossy(&carry));
                        }
                        // 流结束：提取扣分落库
                        let sse_usage = sse_text
                            .lines()
                            .rev()
                            .find_map(|line| {
                                let payload = line.strip_prefix("data: ")?.trim();
                                if payload.is_empty() || payload == "[DONE]" {
                                    return None;
                                }
                                let v: Value = serde_json::from_str(payload).ok()?;
                                v.get("usage").filter(|u| u.is_object()).cloned()
                            });
                        if let Some(usage) = sse_usage {
                            let num = |key: &str| usage.get(key).and_then(Value::as_f64).unwrap_or(0.0);
                            let credits = usage.get("points_consumed").and_then(Value::as_f64);
                            let prompt = num("prompt_tokens");
                            let cached = usage
                                .pointer("/prompt_tokens_details/cached_tokens")
                                .or_else(|| usage.pointer("/prompt_tokens_details/cache_read"))
                                .or_else(|| usage.get("cached_tokens"))
                                .or_else(|| usage.get("cache_read_input_tokens"))
                                .and_then(Value::as_f64)
                                .unwrap_or(0.0);
                            let uncached = (prompt - cached).max(0.0);
                            let cache_hit = cached > 0.0;
                            crate::modules::model_usage::record_usage(json!({
                                "id": uuid::Uuid::new_v4().to_string(),
                                "ts": started,
                                "model": model,
                                "provider": "loomy",
                                "accountId": account_id,
                                "accountLabel": account_id,
                                "promptTokens": json!(prompt),
                                "completionTokens": json!(num("completion_tokens")),
                                "reasoningTokens": usage.pointer("/completion_tokens_details/reasoning_tokens").cloned().unwrap_or(Value::Null),
                                "totalTokens": json!(num("total_tokens")),
                                "cachedTokens": json!(cached),
                                "uncachedPromptTokens": json!(uncached),
                                "cacheHit": json!(cache_hit),
                                "credits": credits.map(|v| json!(v)).unwrap_or(Value::Null),
                                "creditsPending": credits.is_none(),
                                "ledgerId": Value::Null,
                                "latencyMs": now_ms().saturating_sub(started),
                                "ttfbMs": Value::Null,
                                "ok": true,
                            }));
                        }
                    });
                    return Ok(LoomyUpstreamStream { content_type, rx, _concurrency_guard: None });
                }
                drop(guard);
                let text = resp.text().await.unwrap_or_default();
                let is_rate_limit = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::FORBIDDEN
                    || text.contains("100003")
                    || text.contains("100004")
                    || text.contains("频控")
                    || text.contains("积分不足");
                if is_rate_limit {
                    crate::modules::account_protection::mark_account_cooling(
                        account_id,
                        account_id,
                        &format!("上游限频或额度不足 ({status})"),
                        prot_cfg.cooldown_minutes,
                    );
                    last_error = upstream_error(status, &text);
                    continue;
                }
                if status == reqwest::StatusCode::UNAUTHORIZED || text.contains("100002") {
                    let (synced, new_session_opt) = crate::modules::loomy::sync_local_session_if_changed();
                    if synced && new_session_opt.is_some() {
                        let fresh_session = new_session_opt.unwrap();
                        if let Ok(retry_resp) = send_once(&fresh_session, &body).await {
                            let retry_status = retry_resp.status();
                            if retry_status.is_success() {
                                return Box::pin(chat_completions_stream(body)).await;
                            }
                        }
                    }
                    if let Some(id) = store_id {
                        crate::modules::loomy::mark_account_error(id, "session 失效，请重新导入");
                    }
                    last_error = upstream_error(status, &text);
                    continue;
                }
                if status.is_server_error() {
                    last_error = upstream_error(status, &text);
                    continue;
                }
                return Err(upstream_error(status, &text));
            }
            Err(error) => {
                drop(guard);
                last_error = error;
            }
        }
    }
    let message = format!("全部 {} 个可用 Loomy 账号均不可用：{last_error}", pool.len());
    note_error(&message);
    Err(message)
}

/// 流式响应封装（上游 SSE 字节流原样透传）。
pub struct LoomyUpstreamStream {
    pub content_type: String,
    pub rx: tokio::sync::mpsc::Receiver<Result<Bytes, String>>,
    pub _concurrency_guard: Option<crate::modules::account_protection::ConcurrencyGuard>,
}

/// 流式/非流式响应的统一封装。
pub enum LoomyChatCompletion {
    Json(Value),
    Stream(LoomyUpstreamStream),
}

pub async fn chat_completions(body: Value) -> Result<LoomyChatCompletion, String> {
    let stream = body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stream {
        Ok(LoomyChatCompletion::Stream(
            chat_completions_stream(body).await?,
        ))
    } else {
        Ok(LoomyChatCompletion::Json(chat_completions_json(body).await?))
    }
}

/// Anthropic Messages 协议兼容转发。
pub async fn anthropic_messages(body: Value) -> Result<LoomyChatCompletion, String> {
    super::loomy_anthropic::messages(body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_normalizes_glm_model_and_clamps_max_tokens() {
        let mut body = json!({
            "model": "glm-5.3-flash",
            "messages": [
                {"role": "user", "content": "hello"}
            ],
            "max_tokens": 200000
        });
        sanitize_loomy_payload(&mut body);
        assert_eq!(body["model"], json!("GLM-5.3-Flash"));
        assert_eq!(body["max_tokens"], json!(131072));
    }

    #[test]
    fn sanitize_removes_invalid_max_tokens_and_fixes_null_content() {
        let mut body = json!({
            "model": "kimi-k2.6",
            "messages": [
                {"role": "user", "content": null}
            ],
            "max_tokens": 0
        });
        sanitize_loomy_payload(&mut body);
        assert_eq!(body["model"], json!("Kimi-k2.6"));
        assert!(!body.as_object().unwrap().contains_key("max_tokens"));
        assert_eq!(body["messages"][0]["content"], json!(""));
    }
}
