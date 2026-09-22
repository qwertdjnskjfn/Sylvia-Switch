//! ChatGLM 网页协议网关（移植自 XxxXTeam/glm2api）。
//!
//! 把 `chatglm.cn` 网页接口封装为 OpenAI 兼容接口（`/glmweb/v1/*`）。
//! 协议要点（glm2api 源码逆向核对）：
//! - 请求签名：`X-Sign = md5("{timestamp}-{nonce}-8a1317a7468aa3ad86e997d08f3f31cb")`，
//!   timestamp 混淆：数字和 mod 10 写入倒数第二位；
//! - access token：`POST /user-api/user/refresh`（Bearer = refresh_token）换
//!   1 小时 access token（缓存到过期前 60s）；游客模式 `POST /user-api/guest/access`
//!   免登录获取临时凭据；
//! - chat：`POST /backend-api/assistant/stream`（SSE），body 带 assistant_id，
//!   messages 拍平为 `User:/Assistant:` 转写单条 user 消息；SSE 事件
//!   `parts[].content[]` 按 text/think/code/image 渲染（think→reasoning_content）；
//! - 会话不保留：每次全新 conversation_id，可选请求后删除会话。
//!
//! token 池落盘 `~/.wb-switch/glmweb_tokens.txt`（每行一个 refresh_token，
//! 上游轮换时自动写回对应行；"guest" 行为游客槽位）。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, now_ms, store_dir};

const GLM_BASE: &str = "https://chatglm.cn/chatglm";
const SIGN_SECRET: &str = "8a1317a7468aa3ad86e997d08f3f31cb";
const DEFAULT_ASSISTANT_ID: &str = "65940acff94777010aa6b796";
const GUEST_MARKER: &str = "guest";
/// access token 有效期 1h，提前 60s 视为过期。
const ACCESS_TTL_SECS: i64 = 3600 - 60;
/// v0.1.46：默认模型改为 **GLM5.3Flash**。请求未显式传 model 时用它。
pub const DEFAULT_MODEL: &str = "glm-5.3-flash";
/// 续期阈值：access token 剩余寿命低于此值即需续期（6 小时）。
/// chatglm 的 access 只有 1h，这里同时用于「refresh token 是否需要重新抓取」的提示口径。
pub const RENEW_THRESHOLD_MS: i64 = 6 * 60 * 60 * 1000;

static WRITE_LOCK: Mutex<()> = Mutex::new(());
/// v0.1.46：sync_token 的读-改-写串行锁（与 WRITE_LOCK 分层，避免自锁死）。
static SYNC_LOCK: Mutex<()> = Mutex::new(());
static TOKEN_POOL: Mutex<Option<Vec<GlmAccount>>> = Mutex::new(None);

/// 凭据脱敏：只留前 8 位 + 长度，日志/接口一律用它。
fn mask_token(token: &str) -> String {
    let len = token.len();
    format!("{}…({})", &token[..len.min(8)], len)
}

/// 上游「空闲读」上限（秒）：连续这么久收不到任何字节才判定断流。
///
/// 🔴 同 loomy_proxy：聊天端点是 **SSE 流**，绝不能用请求级 `.timeout()`
/// ——它算「连接 + 响应体读完」的总时长，会把超过该时长的正常长回答掐断，
/// 且 reqwest 只回 `error decoding response body`，看起来像上游断流。
/// `read_timeout` 只在「连续无数据」时触发，读到数据即重置。
const UPSTREAM_READ_IDLE_SECS: u64 = 300;

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy() // 国内上游直连，不继承系统代理
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(UPSTREAM_READ_IDLE_SECS))
            .build()
            .expect("failed to build glmweb client")
    })
}

fn tokens_path() -> PathBuf {
    store_dir().join("glmweb_tokens.txt")
}

/// 可选配置文件：`~/.wb-switch/glmweb_config.json`，形如 `{"defaultModel":"glm-5.3-flash"}`。
fn config_path() -> PathBuf {
    store_dir().join("glmweb_config.json")
}

/// 默认模型：配置文件优先，回落到 `DEFAULT_MODEL`（GLM5.3Flash）。
pub fn default_model() -> String {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("defaultModel").and_then(Value::as_str).map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

// ---------------------------------------------------------------------------
// 签名与请求头
// ---------------------------------------------------------------------------

/// 客户端签名算法（build_sign）：timestamp 数字和 mod 10 写入倒数第二位，
/// `X-Sign = md5("{ts}-{nonce}-{secret}")`。
fn build_sign() -> (String, String, String) {
    let now_ms = now_ms().to_string();
    let digits: Vec<u32> = now_ms.chars().filter_map(|c| c.to_digit(10)).collect();
    let checksum = digits.iter().sum::<u32>().saturating_sub(*digits.get(digits.len().saturating_sub(2)).unwrap_or(&0)) % 10;
    let len = now_ms.len();
    let timestamp = format!("{}{}{}", &now_ms[..len - 2], checksum, &now_ms[len - 1..]);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    hasher.update(format!("{timestamp}-{nonce}-{SIGN_SECRET}"));
    let sign = format!("{:x}", hasher.finalize());
    (timestamp, nonce, sign)
}

/// 浏览器伪装头（与 glm2api get_browser_headers 一致）。
fn browser_headers() -> Vec<(&'static str, String)> {
    vec![
        ("Accept", "text/event-stream".to_string()),
        ("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8".to_string()),
        ("App-Name", "chatglm".to_string()),
        ("Cache-Control", "no-cache".to_string()),
        ("Content-Type", "application/json".to_string()),
        ("Origin", "https://chatglm.cn".to_string()),
        ("Pragma", "no-cache".to_string()),
        (
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36 Edg/143.0.0.0".to_string(),
        ),
        ("X-App-Fr", "browser_extension".to_string()),
        ("X-App-Platform", "pc".to_string()),
        ("X-App-Version", "0.0.1".to_string()),
        ("X-Lang", "zh".to_string()),
    ]
}

fn signed_headers(token: String, accept_default: bool) -> reqwest::header::HeaderMap {
    let (timestamp, nonce, sign) = build_sign();
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in browser_headers() {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    if accept_default {
        headers.insert("Accept", "application/json, text/plain, */*".parse().unwrap());
    }
    headers.insert("Authorization", format!("Bearer {token}").parse().unwrap());
    headers.insert("X-Device-Id", uuid::Uuid::new_v4().simple().to_string().parse().unwrap());
    headers.insert("X-Nonce", nonce.clone().parse().unwrap());
    headers.insert("X-Request-Id", uuid::Uuid::new_v4().simple().to_string().parse().unwrap());
    headers.insert("X-Sign", sign.parse().unwrap());
    headers.insert("X-Timestamp", timestamp.parse().unwrap());
    headers
}

// ---------------------------------------------------------------------------
// token 池（refresh token 文本文件；guest = 游客槽位）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct GlmAccount {
    /// refresh token；空串 = 游客槽位。
    refresh_token: String,
    is_guest: bool,
    cached_access: String,
    access_expires_at_ms: i64,
}

fn load_pool() -> Vec<GlmAccount> {
    if let Ok(guard) = TOKEN_POOL.lock() {
        if let Some(pool) = guard.as_ref() {
            return pool.clone();
        }
    }
    let pool = load_pool_from_disk();
    if let Ok(mut guard) = TOKEN_POOL.lock() {
        *guard = Some(pool.clone());
    }
    pool
}

fn load_pool_from_disk() -> Vec<GlmAccount> {
    std::fs::read_to_string(tokens_path())
        .map(|text| {
            text.lines()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .map(|line| {
                    let is_guest = line.eq_ignore_ascii_case(GUEST_MARKER);
                    GlmAccount {
                        refresh_token: if is_guest { String::new() } else { line.to_string() },
                        is_guest,
                        cached_access: String::new(),
                        access_expires_at_ms: 0,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_pool(pool: &[GlmAccount]) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let content = pool
        .iter()
        .map(|a| if a.is_guest { GUEST_MARKER.to_string() } else { a.refresh_token.clone() })
        .collect::<Vec<_>>()
        .join("\n");
    let _ = atomic_write(&tokens_path(), &content);
    if let Ok(mut guard) = TOKEN_POOL.lock() {
        *guard = Some(pool.to_vec());
    }
}

/// 添加 refresh token（guest 为游客槽位）；重复忽略。
pub fn add_token(token: &str) -> Result<Value, String> {
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        return Err("token 不能为空".to_string());
    }
    let is_guest = trimmed.eq_ignore_ascii_case(GUEST_MARKER);
    let mut pool = load_pool();
    if pool.iter().any(|a| !a.is_guest && a.refresh_token == trimmed) {
        return Err("该 token 已在池中".to_string());
    }
    pool.push(GlmAccount {
        refresh_token: if is_guest { String::new() } else { trimmed },
        is_guest,
        cached_access: String::new(),
        access_expires_at_ms: 0,
    });
    save_pool(&pool);
    Ok(json!({"ok": true, "count": pool.len(), "guestSlots": pool.iter().filter(|a| a.is_guest).count()}))
}

pub fn remove_token(index: usize) -> bool {
    let mut pool = load_pool();
    if index >= pool.len() {
        return false;
    }
    pool.remove(index);
    save_pool(&pool);
    true
}

/// v0.1.46：幂等写回（浏览器同步工具专用）。
///
/// 与 `add_token` 的区别 —— 扩展每 20 分钟调一次，所以必须满足：
/// - **幂等**：token 已在池中时**不报错、不新增**，仅刷新缓存并返回健康信息；
/// - **先校验后落盘**：`validate=true` 时先真实调用上游 refresh，**失败则不写入**，
///   避免把坏凭据写进池子（这也是"绝不因抓取失败而降级"的落点）；
/// - **认轮换**：上游若下发新的 refresh token，以新值写回；
/// - **并发安全**：读-改-写经 `SYNC_LOCK` 串行，落盘经 `WRITE_LOCK` + `atomic_write`。
pub async fn sync_token(token: &str, validate: bool, source: Option<&str>) -> Result<Value, String> {
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        return Err("token 不能为空".to_string());
    }
    if trimmed.eq_ignore_ascii_case(GUEST_MARKER) {
        return Err("不接受游客槽位作为同步凭据：游客模式没有登录态可续期".to_string());
    }

    // 1) 锁外先做真校验（网络 I/O 不占用锁）
    let mut probe = GlmAccount {
        refresh_token: trimmed.clone(),
        is_guest: false,
        cached_access: String::new(),
        access_expires_at_ms: 0,
    };
    if validate {
        refresh_access_token(&mut probe).await?;
    }

    // 2) 锁内读-改-写
    let _sync = SYNC_LOCK.lock().unwrap();
    let effective = probe.refresh_token.clone();
    let rotated = effective != trimmed;
    let expires_at = probe.access_expires_at_ms;

    let mut pool = load_pool();
    let added = !pool.iter().any(|a| !a.is_guest && a.refresh_token == effective);
    if added {
        pool.push(GlmAccount {
            refresh_token: effective.clone(),
            is_guest: false,
            cached_access: String::new(),
            access_expires_at_ms: 0,
        });
    }
    if let Some(slot) = pool.iter_mut().find(|a| !a.is_guest && a.refresh_token == effective) {
        if validate {
            slot.cached_access = probe.cached_access.clone();
            slot.access_expires_at_ms = expires_at;
        }
    }
    save_pool(&pool);

    let now = now_ms();
    Ok(json!({
        "ok": true,
        "added": added,
        "rotated": rotated,
        "validated": validate,
        "source": source.unwrap_or("browser"),
        "account": {
            "preview": mask_token(&effective),
            "accessExpiresAt": if expires_at > 0 { json!(expires_at) } else { Value::Null },
            "needRenew": expires_at == 0 || expires_at - now < RENEW_THRESHOLD_MS,
        },
        "pool": pool_health(),
    }))
}

/// v0.1.46：池健康快照（含"是否已退化为游客模式"）。
pub fn pool_health() -> Value {
    let pool = load_pool();
    let now = now_ms();
    let accounts = pool.len();
    let guest_slots = pool.iter().filter(|a| a.is_guest).count();
    let refresh_accounts = accounts - guest_slots;
    json!({
        "accounts": accounts,
        "guestSlots": guest_slots,
        "refreshAccounts": refresh_accounts,
        "withCachedAccess": pool.iter().filter(|a| !a.cached_access.is_empty()).count(),
        "needRenew": pool.iter().filter(|a| {
            !a.is_guest && (a.access_expires_at_ms == 0 || a.access_expires_at_ms - now < RENEW_THRESHOLD_MS)
        }).count(),
        "defaultModel": default_model(),
        "renewThresholdMs": RENEW_THRESHOLD_MS,
        "degraded": refresh_accounts == 0,
        "now": now,
    })
}

pub fn list_tokens_masked() -> Vec<Value> {
    load_pool()
        .iter()
        .enumerate()
        .map(|(index, account)| {
            json!({
                "index": index,
                "type": if account.is_guest { "guest" } else { "refresh" },
                "preview": if account.is_guest {
                    "游客槽位".to_string()
                } else {
                    let token = &account.refresh_token;
                    format!("{}…({})", &token[..token.len().min(8)], token.len())
                },
                "hasCachedAccess": !account.cached_access.is_empty(),
                "accessExpiresAt": if account.access_expires_at_ms > 0 { json!(account.access_expires_at_ms) } else { Value::Null },
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// token 刷新（正式 refresh + 游客 access）
// ---------------------------------------------------------------------------

async fn refresh_access_token(account: &mut GlmAccount) -> Result<(), String> {
    let (url, use_empty_body) = if account.is_guest || account.refresh_token.is_empty() {
        (format!("{GLM_BASE}/user-api/guest/access"), true)
    } else {
        (format!("{GLM_BASE}/user-api/user/refresh"), false)
    };
    let token_for_sign = if account.is_guest { String::new() } else { account.refresh_token.clone() };
    let mut headers = signed_headers(token_for_sign, true);
    if use_empty_body {
        headers.insert("Referer", "https://chatglm.cn/".parse().unwrap());
    }
    let resp = http_client()
        .post(&url)
        .headers(headers)
        .body(if use_empty_body { Vec::new() } else { b"{}".to_vec() })
        .send()
        .await
        .map_err(|e| format!("GLM token 刷新请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("GLM token 刷新失败（{status}）：{}", text.chars().take(150).collect::<String>()));
    }
    let payload: Value = serde_json::from_str(&text).map_err(|e| format!("GLM token 刷新响应解析失败: {e}"))?;
    let code = payload.get("code").or_else(|| payload.get("status")).and_then(Value::as_i64);
    let access = payload
        .pointer("/result/access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("GLM token 刷新响应缺少 access_token: {}", text.chars().take(120).collect::<String>()))?
        .to_string();
    if let Some(code) = code {
        if code != 0 {
            return Err(format!("GLM token 刷新业务失败 code={code}"));
        }
    }
    let new_refresh = payload
        .pointer("/result/refresh_token")
        .and_then(Value::as_str)
        .map(String::from);
    account.cached_access = access;
    account.access_expires_at_ms = now_ms() + ACCESS_TTL_SECS * 1000;
    // 上游轮换 refresh token 时写回（正式账号）
    if !account.is_guest {
        if let Some(new_refresh) = new_refresh {
            if !new_refresh.is_empty() && new_refresh != account.refresh_token {
                account.refresh_token = new_refresh;
            }
        }
    }
    save_pool(&load_pool_positional_replace(account));
    Ok(())
}

/// 按位置写回（token 池按行对应账号；匿名结构无法用 uid 定位）。
fn load_pool_positional_replace(updated: &GlmAccount) -> Vec<GlmAccount> {
    let mut pool = load_pool();
    if let Some(slot) = pool
        .iter_mut()
        .find(|a| a.refresh_token == updated.refresh_token && a.is_guest == updated.is_guest)
    {
        slot.cached_access = updated.cached_access.clone();
        slot.access_expires_at_ms = updated.access_expires_at_ms;
        slot.refresh_token = updated.refresh_token.clone();
    }
    pool
}

/// 轮转取 access token：从上次位置开始找缓存有效或可刷新的账号。
async fn next_access_token() -> Result<(String, usize), String> {
    let mut pool = load_pool();
    if pool.is_empty() {
        // 自动兜底游客模式
        pool.push(GlmAccount {
            refresh_token: String::new(),
            is_guest: true,
            cached_access: String::new(),
            access_expires_at_ms: 0,
        });
        save_pool(&pool);
    }
    static CURSOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let start = CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % pool.len();
    let mut last_error = String::new();
    for index in 0..pool.len() {
        let slot = (start + index) % pool.len();
        let account = &mut pool[slot];
        let now = now_ms();
        if !account.cached_access.is_empty() && account.access_expires_at_ms - 60_000 > now {
            return Ok((account.cached_access.clone(), slot));
        }
        match refresh_access_token(account).await {
            Ok(()) => return Ok((account.cached_access.clone(), slot)),
            Err(e) => last_error = e,
        }
    }
    Err(format!("全部 {} 个 GLM 账号均不可用：{}", pool.len(), last_error))
}

/// 使指定位置缓存的 access token 失效（401 时）。
fn invalidate_access(slot: usize) {
    let mut pool = load_pool();
    if let Some(account) = pool.get_mut(slot) {
        account.cached_access.clear();
        account.access_expires_at_ms = 0;
    }
    save_pool(&pool);
}

// ---------------------------------------------------------------------------
// chat 协议
// ---------------------------------------------------------------------------

/// OpenAI messages → glm2api 同款转写（system/System + role:text 行 + "Assistant: "）。
/// 当请求含有 tools 时，将工具定义格式化为标准 Tool Calling 提示词注入到开头。
fn format_messages(messages: &[Value], tools: Option<&[Value]>) -> String {
    let mut transcript: Vec<String> = Vec::new();

    if let Some(tool_list) = tools {
        if !tool_list.is_empty() {
            let tools_json = serde_json::to_string_pretty(tool_list).unwrap_or_else(|_| "[]".to_string());
            transcript.push(format!(
                "System: [AVAILABLE_TOOLS]\nYou have access to the following tools:\n{tools_json}\n\nWhen you need to call one or more tools, you MUST respond ONLY with a JSON block in the following format:\n```json\n{{\n  \"tool_calls\": [\n    {{\n      \"name\": \"<tool_name>\",\n      \"arguments\": {{ /* arguments object */ }}\n    }}\n  ]\n}}\n```\nIf no tool call is needed, respond directly with plain text.\n[/AVAILABLE_TOOLS]"
            ));
        }
    }

    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| {
                    if b.is_string() {
                        b.as_str().map(String::from)
                    } else {
                        b.get("text").and_then(Value::as_str).map(String::from)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let text = text.trim().to_string();

        // 兼容 assistant 历史带 tool_calls
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            let calls_summary = serde_json::to_string(tool_calls).unwrap_or_default();
            transcript.push(format!("Assistant: [TOOL_CALLS: {calls_summary}]"));
            continue;
        }

        // 兼容 tool 结果回复
        if role == "tool" {
            let tool_id = message.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
            transcript.push(format!("User: [TOOL_RESULT id={tool_id}]: {text}"));
            continue;
        }

        if text.is_empty() {
            continue;
        }
        let title = match role {
            "system" | "developer" => "System",
            "assistant" => "Assistant",
            "user" => "User",
            other => other,
        };
        transcript.push(format!("{title}: {text}"));
    }
    let mut prompt = transcript.join("\n\n");
    prompt.push_str("\n\nAssistant: ");
    prompt
}

/// 解析文本中是否包含标准 tool_calls JSON 结构。
pub(crate) fn try_extract_tool_calls(text: &str) -> Option<Vec<Value>> {
    let trimmed = text.trim();
    // 1. 尝试从 ```json ... ``` 代码块中提取
    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + 7..];
        if let Some(end) = after.find("```") {
            let block = after[..end].trim();
            if let Ok(v) = serde_json::from_str::<Value>(block) {
                if let Some(calls) = parse_tool_calls_json(&v) {
                    return Some(calls);
                }
            }
        }
    }
    // 2. 尝试整体解析为 JSON
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if let Some(calls) = parse_tool_calls_json(&v) {
            return Some(calls);
        }
    }
    // 3. 尝试搜索首个 {"tool_calls" 结构
    if let Some(start) = trimmed.find("{\"tool_calls\"") {
        let candidate = &trimmed[start..];
        // 尝试逐步向后寻找合法的 JSON 闭合
        if let Some(end) = candidate.rfind('}') {
            if let Ok(v) = serde_json::from_str::<Value>(&candidate[..=end]) {
                if let Some(calls) = parse_tool_calls_json(&v) {
                    return Some(calls);
                }
            }
        }
    }
    None
}

fn parse_tool_calls_json(val: &Value) -> Option<Vec<Value>> {
    let raw_calls = if let Some(arr) = val.get("tool_calls").and_then(Value::as_array) {
        arr
    } else if val.is_array() {
        val.as_array()?
    } else {
        return None;
    };
    let mut out = Vec::new();
    for (i, item) in raw_calls.iter().enumerate() {
        let name = item.pointer("/function/name")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)?;
        let raw_args = item.pointer("/function/arguments").or_else(|| item.get("arguments"));
        let args_str = match raw_args {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "{}".to_string(),
        };
        let call_id = item.get("id").and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("call_{}_{}", uuid::Uuid::new_v4().simple(), i));
        out.push(json!({
            "id": call_id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": args_str,
            }
        }));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// chat 请求体（glm2api `_open_chat_stream` 同款结构）。
fn build_chat_body(messages: &[Value], model: &str, tools: Option<&[Value]>) -> Result<Value, String> {
    let prompt = format_messages(messages, tools);
    if prompt.trim().is_empty() {
        return Err("messages 不能为空".to_string());
    }
    // thinking / deep-research 模型开启 chat_mode
    let lower = model.to_lowercase();
    let chat_mode = if lower.contains("zero") || lower.contains("deep-research") || lower.contains("think") {
        json!("think")
    } else {
        json!(null)
    };
    Ok(json!({
        "assistant_id": DEFAULT_ASSISTANT_ID,
        "conversation_id": "",
        "project_id": "",
        "chat_type": "user_chat",
        "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
        "meta_data": {
            "channel": "",
            "chat_mode": chat_mode,
            "draft_id": "",
            "if_plus_model": true,
            "input_question_type": "xxxx",
            "is_networking": false,
            "is_test": false,
            "platform": "pc",
            "quote_log_id": "",
            "cogview": {"rm_label_watermark": false},
        },
    }))
}

/// SSE 事件 → (正文增量, 推理增量, 是否结束, conversation_id)。
/// 累计快照 → 增量推进。
///
/// 🔴 chatglm 的 SSE `parts` 是**累计快照**：每一帧都带「到目前为止的全文」，
/// 而不是新增的那一段。早期实现把每帧全文当作 delta 直接下发，导致正文被重复 N 次
/// （实测「只回复：AB」会输出 `ABABAB`）。
///
/// 这里统一做转换：返回本次真正需要下发的新增内容，并把状态推进到最新快照。
fn advance(prev: &mut String, next: &str) -> String {
    if next.starts_with(prev.as_str()) {
        // 正常累计：只取新增后缀
        let delta = next[prev.len()..].to_string();
        *prev = next.to_string();
        delta
    } else if prev.starts_with(next) {
        // 上游重发了更旧的快照：不下发
        String::new()
    } else {
        // 非累计语义（如分段输出）：整体下发并累加
        prev.push_str(next);
        next.to_string()
    }
}

/// parts[].content[] 按 text/think/code/image 渲染（glm2api _render_full_output）。
fn extract_from_event(event: &Value) -> (Option<String>, Option<String>, bool, Option<String>) {
    let conversation_id = event
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(String::from);
    let status = event.get("status").and_then(Value::as_str).unwrap_or("");
    let finished = status == "finish" || status == "intervene";
    if let Some(error) = event.get("error") {
        let message = error
            .get("message")
            .or_else(|| error.get("detail"))
            .map(|v| v.to_string())
            .unwrap_or_else(|| event.to_string());
        // 错误事件视为结束并携带正文=None（由调用方报错）
        return (Some(format!("__GLM_ERROR__{message}")), None, true, conversation_id);
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    if let Some(parts) = event.get("parts").and_then(Value::as_array) {
        for part in parts {
            if let Some(contents) = part.get("content").and_then(Value::as_array) {
                for content in contents {
                    match content.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = content.get("text").and_then(Value::as_str) {
                                text.push_str(t);
                            }
                        }
                        Some("think") => {
                            if let Some(t) = content.get("think").and_then(Value::as_str) {
                                reasoning.push_str(t);
                            }
                        }
                        Some("code") => {
                            if let Some(code) = content.get("code").and_then(Value::as_str) {
                                text.push_str(&format!("```python\n{code}\n```"));
                            }
                        }
                        Some("execution_output") => {
                            if let Some(t) = content.get("content").and_then(Value::as_str) {
                                text.push_str(t);
                            }
                        }
                        Some("image") => {
                            if let Some(images) = content.get("image").and_then(Value::as_array) {
                                for image in images {
                                    if let Some(url) = image.get("image_url").and_then(Value::as_str) {
                                        text.push_str(&format!("![image]({url})"));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    // 多 parts 分隔（glm2api 用 \n\n 连接多逻辑段）
    let content = (!text.is_empty()).then_some(text);
    let reasoning = (!reasoning.is_empty()).then_some(reasoning);
    (content, reasoning, finished, conversation_id)
}

// ---------------------------------------------------------------------------
// OpenAI 兼容输出
// ---------------------------------------------------------------------------

pub struct GlmWebStream {
    pub rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, String>>,
}

/// chat completions 统一入口（流式/非流式）。
pub fn chat_completions(
    body: Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<GlmChatOutput, String>> + Send>> {
    Box::pin(async move {
        let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        // v0.1.46：缺省模型由陈旧的 `glm-4` 改为可配置的 GLM5.3Flash（`default_model()`）。
        let fallback_model = default_model();
        let raw_model = body
            .get("model")
            .and_then(Value::as_str)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(&fallback_model);

        // 统一模型别名归一化与 AutoClaw 目录兼容
        let model = match raw_model.to_ascii_lowercase().as_str() {
            "auto" | "zai_auto" | "auto-fast" | "zai_auto-fast" => "glm-5.3-flash",
            "glm-5.3" | "zaicoding_glm-5.3" => "glm-5.3",
            "glm-5.3-flash" | "zai_glm-5.3-flash" => "glm-5.3-flash",
            other => other,
        }.to_string();

        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tools = body.get("tools").and_then(Value::as_array).cloned();
        let has_tools = tools.as_ref().map(|t| !t.is_empty()).unwrap_or(false);
        let chat_body = build_chat_body(&messages, &model, tools.as_deref())?;

        let (access, slot) = next_access_token().await?;
        let started = std::time::Instant::now();
        let open_chat = |token: String| {
            let mut headers = signed_headers(token, false);
            headers.insert("Accept", "text/event-stream".parse().unwrap());
            http_client()
                .post(format!("{GLM_BASE}/backend-api/assistant/stream"))
                .headers(headers)
                .json(&chat_body)
                // 🔴 不得链请求级 `.timeout()`：SSE 长回答会被总时长掐断
                // （见 UPSTREAM_READ_IDLE_SECS）。超时统一在 client 层表达。
                .send()
        };
        let mut resp = match open_chat(access.clone()).await {
            Ok(resp) => resp,
            Err(e) => {
                invalidate_access(slot);
                return Err(format!("GLM chat 请求失败: {e}"));
            }
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // token 过期：失效缓存重取一次
            invalidate_access(slot);
            let (fresh, fresh_slot) = next_access_token().await?;
            resp = open_chat(fresh)
                .await
                .map_err(|e| format!("GLM chat 重试请求失败: {e}"))?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                invalidate_access(fresh_slot);
                return Err("GLM 登录态失效且刷新后仍 401".to_string());
            }
        }
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "GLM 上游返回 {status}：{}",
                text.chars().take(200).collect::<String>()
            ));
        }

        let return_model = raw_model.to_string();

        if want_stream {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, String>>(16);
            tokio::spawn(async move {
                pump_stream(resp, return_model, has_tools, tx).await;
            });
            Ok(GlmChatOutput::Stream(GlmWebStream { rx }))
        } else {
            // 非流式：聚合 SSE 为完整 JSON
            let mut content = String::new();
            let mut reasoning = String::new();
            let mut conversation_id = String::new();
            let mut buffer = String::new();
            let mut carry: Vec<u8> = Vec::new();
            let mut error_message = String::new();
            let mut done = false;
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        buffer.push_str(&super::glm_anthropic::decode_stream_chunk(
                            &mut carry, &chunk,
                        ));
                        while let Some(pos) = buffer.find("\n\n") {
                            let block: String = buffer.drain(..pos + 2).collect();
                            for line in block.lines() {
                                let Some(payload) = line.strip_prefix("data:") else { continue };
                                let payload = payload.trim();
                                if payload.is_empty() || payload == "[DONE]" {
                                    continue;
                                }
                                let Ok(event) = serde_json::from_str::<Value>(payload) else { continue };
                                let (c, r, event_done, cid) = extract_from_event(&event);
                                if event_done {
                                    done = true;
                                }
                                if let Some(cid) = cid {
                                    conversation_id = cid;
                                }
                                if let Some(c) = c {
                                    if let Some(message) = c.strip_prefix("__GLM_ERROR__") {
                                        error_message = message.to_string();
                                    } else {
                                        // 上游是累计快照：advance 会取相对已有的新增部分（去重核心）
                                        advance(&mut content, &c);
                                    }
                                }
                                if let Some(r) = r {
                                    advance(&mut reasoning, &r);
                                }
                                if done || !error_message.is_empty() {
                                    break;
                                }
                            }
                            if done || !error_message.is_empty() {
                                break;
                            }
                        }
                        if done || !error_message.is_empty() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        error_message = format!("GLM 流中断: {e}");
                        break;
                    }
                }
            }
            let latency = started.elapsed().as_millis() as u64;
            if !error_message.is_empty() {
                return Err(format!("GLM 上游错误：{error_message}"));
            }
            let _ = latency;

            let final_id = if conversation_id.is_empty() {
                format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
            } else {
                conversation_id
            };

            // 如果客户端声明了 tools 且模型输出了结构化 tool_calls，封装为标准工具调用响应
            if has_tools {
                if let Some(tool_calls) = try_extract_tool_calls(&content) {
                    return Ok(GlmChatOutput::Json(json!({
                        "id": final_id,
                        "object": "chat.completion",
                        "created": now_ms() / 1000,
                        "model": return_model,
                        "choices": [{
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": Value::Null,
                                "tool_calls": tool_calls,
                                "reasoning_content": if reasoning.is_empty() { Value::Null } else { json!(reasoning) },
                            },
                            "finish_reason": "tool_calls",
                        }],
                        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
                    })));
                }
            }

            Ok(GlmChatOutput::Json(json!({
                "id": final_id,
                "object": "chat.completion",
                "created": now_ms() / 1000,
                "model": return_model,
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": content,
                        "reasoning_content": if reasoning.is_empty() { Value::Null } else { json!(reasoning) },
                    },
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            })))
        }
    })
}

/// 流式泵：GLM SSE → OpenAI chunk SSE（think→reasoning_content，支持 tool_calls 转译）。
async fn pump_stream(
    mut resp: reqwest::Response,
    model: String,
    has_tools: bool,
    tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, String>>,
) {
    use bytes::Bytes;
    let mut buffer = String::new();
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk_id = String::new();
    let mut started = false;
    // 上游 parts 是累计快照，这里记住已下发的进度，用来算真正的增量
    let mut emitted_text = String::new();
    let mut emitted_reasoning = String::new();
    let send_frame = |frame_chunk_id: &str, delta: Value, finish: Value| -> String {
        let frame = json!({
            "id": frame_chunk_id, "object": "chat.completion.chunk", "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        format!("data: {frame}\n\n")
    };
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                buffer.push_str(&super::glm_anthropic::decode_stream_chunk(
                    &mut carry, &chunk,
                ));
                while let Some(pos) = buffer.find("\n\n") {
                    let block: String = buffer.drain(..pos + 2).collect();
                    for line in block.lines() {
                        let Some(payload) = line.strip_prefix("data:") else { continue };
                        let payload = payload.trim();
                        if payload.is_empty() {
                            continue;
                        }
                        if payload == "[DONE]" {
                            let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                            return;
                        }
                        let Ok(event) = serde_json::from_str::<Value>(payload) else { continue };
                        let (content, reasoning, done, conversation_id) = extract_from_event(&event);
                        if let Some(id) = conversation_id {
                            if chunk_id.is_empty() {
                                chunk_id = id;
                            }
                        }
                        if chunk_id.is_empty() {
                            chunk_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
                        }
                        if let Some(message) = content.as_deref().and_then(|c| c.strip_prefix("__GLM_ERROR__")) {
                            let error_frame = json!({"error": {"message": message, "type": "upstream_error"}});
                            let _ = tx
                                .send(Ok(Bytes::from(format!("data: {error_frame}\n\n"))))
                                .await;
                            return;
                        }
                        if !started {
                            started = true;
                            let frame = send_frame(&chunk_id, json!({"role": "assistant", "content": ""}), Value::Null);
                            if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                                return;
                            }
                        }
                        if let Some(reasoning) = reasoning {
                            let delta = advance(&mut emitted_reasoning, &reasoning);
                            if !delta.is_empty() {
                                let frame = send_frame(&chunk_id, json!({"reasoning_content": delta}), Value::Null);
                                if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                                    return;
                                }
                            }
                        }
                        if let Some(content) = content {
                            let delta = advance(&mut emitted_text, &content);
                            if !delta.is_empty() {
                                let frame = send_frame(&chunk_id, json!({"content": delta}), Value::Null);
                                if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                                    return;
                                }
                            }
                        }
                        if done {
                            if has_tools {
                                if let Some(tool_calls) = try_extract_tool_calls(&emitted_text) {
                                    let frame = send_frame(&chunk_id, json!({"tool_calls": tool_calls}), json!("tool_calls"));
                                    let _ = tx.send(Ok(Bytes::from(frame))).await;
                                    let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                                    return;
                                }
                            }
                            let frame = send_frame(&chunk_id, json!({}), json!("stop"));
                            let _ = tx.send(Ok(Bytes::from(frame))).await;
                            let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                            return;
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = tx.send(Err(format!("GLM 流中断: {e}"))).await;
                return;
            }
        }
    }
    if started {
        if has_tools {
            if let Some(tool_calls) = try_extract_tool_calls(&emitted_text) {
                let frame = send_frame(&chunk_id, json!({"tool_calls": tool_calls}), json!("tool_calls"));
                let _ = tx.send(Ok(Bytes::from(frame))).await;
                let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                return;
            }
        }
        let frame = send_frame(&chunk_id, json!({}), json!("stop"));
        let _ = tx.send(Ok(Bytes::from(frame))).await;
        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
    }
}

/// 流式/非流式输出的统一封装。
pub enum GlmChatOutput {
    Json(Value),
    Stream(GlmWebStream),
}

/// 模型清单（对齐 AutoClaw 模型目录 + 网页支持的官方模型）。
pub fn models_payload() -> Value {
    let models = [
        "auto",
        "auto-fast",
        "glm-5.3-flash",
        "glm-5.3",
        "cogView-4-250304",
        "glm-5.2",
        "glm-5.1",
        "glm-5v-turbo",
        "glm-5-turbo",
        "glm-5",
        "glm-4.7-flash",
        "glm-4.7",
        "glm-4.6v-flash",
        "glm-4.6",
        "glm-4.5",
        "glm-4.1v-thinking-flashx",
        "glm-4",
        "glm-4-flash",
        "glm-4-air",
        "glm-4v",
        "glm-zero-preview",
        "glm-deep-research",
        "glm-image-1",
    ];
    json!({
        "object": "list",
        "data": models
            .iter()
            .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "chatglm"}))
            .collect::<Vec<_>>(),
    })
}

/// 健康探测：token 池状态。
pub fn pool_status() -> Value {
    let pool = load_pool();
    json!({
        "accounts": pool.len(),
        "guestSlots": pool.iter().filter(|a| a.is_guest).count(),
        "refreshAccounts": pool.iter().filter(|a| !a.is_guest).count(),
        "withCachedAccess": pool.iter().filter(|a| !a.cached_access.is_empty()).count(),
    })
}

// ---------------------------------------------------------------------------
// 签到与账号查询对外接口
// ---------------------------------------------------------------------------

/// 检查 token 池中是否有可用且非游客的 refresh token
pub fn has_usable_token() -> bool {
    load_pool().iter().any(|a| !a.is_guest && !a.refresh_token.is_empty())
}

/// 获取 token 池中所有非游客的 refresh token 列表
pub fn get_refresh_tokens() -> Vec<String> {
    load_pool()
        .into_iter()
        .filter(|a| !a.is_guest && !a.refresh_token.is_empty())
        .map(|a| a.refresh_token)
        .collect()
}

/// 获取当前或下一个有效的 access token
pub async fn get_current_or_next_access_token() -> Result<String, String> {
    next_access_token().await.map(|(token, _)| token)
}

/// 按特定的 refresh_token 获取有效的 access token（优先使用缓存，必要时自动刷新）
pub async fn get_access_token_for_refresh(refresh_token: &str) -> Result<String, String> {
    let mut pool = load_pool();
    if let Some(account) = pool.iter_mut().find(|a| !a.is_guest && a.refresh_token == refresh_token) {
        let now = now_ms();
        if !account.cached_access.is_empty() && account.access_expires_at_ms - 60_000 > now {
            return Ok(account.cached_access.clone());
        }
        refresh_access_token(account).await?;
        return Ok(account.cached_access.clone());
    }
    let mut temp = GlmAccount {
        refresh_token: refresh_token.to_string(),
        is_guest: false,
        cached_access: String::new(),
        access_expires_at_ms: 0,
    };
    refresh_access_token(&mut temp).await?;
    Ok(temp.cached_access)
}

/// 获取首个已登录的正式账号 access token（排除游客）
pub async fn get_authenticated_access_token() -> Result<String, String> {
    let mut pool = load_pool();
    for account in pool.iter_mut() {
        if !account.is_guest && !account.refresh_token.is_empty() {
            let now = now_ms();
            if !account.cached_access.is_empty() && account.access_expires_at_ms - 60_000 > now {
                return Ok(account.cached_access.clone());
            }
            refresh_access_token(account).await?;
            return Ok(account.cached_access.clone());
        }
    }
    Err("未配置有效的正式 ChatGLM refresh token".to_string())
}

/// 为外部模块提供带有签名的请求头
pub fn signed_headers_for(token: String, accept_default: bool) -> reqwest::header::HeaderMap {
    signed_headers(token, accept_default)
}

/// 获取 ChatGLM 用户的个人信息、手机号及当前剩余积分
pub async fn fetch_user_info(access_token: &str) -> Result<Value, String> {
    let url = format!("{GLM_BASE}/user-api/user/info");
    let headers = signed_headers(access_token.to_string(), true);
    let resp = http_client()
        .get(&url)
        .headers(headers)
        .send()
        .await
        .map_err(|e| format!("ChatGLM 用户信息请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("ChatGLM 用户信息请求失败（{status}）：{}", text.chars().take(150).collect::<String>()));
    }
    let val: Value = serde_json::from_str(&text).map_err(|e| format!("解析用户信息失败: {e}"))?;
    Ok(val)
}

/// 调用桌面端每日登录打卡签到接口（获取每日积分）
pub async fn perform_daily_checkin(access_token: &str) -> Result<Value, String> {
    let url = format!("{GLM_BASE}/member-api/member/daily_login_score");
    let headers = signed_headers(access_token.to_string(), true);
    let resp = http_client()
        .post(&url)
        .headers(headers)
        .body(b"{}".to_vec())
        .send()
        .await
        .map_err(|e| format!("ChatGLM 签到打卡请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("ChatGLM 签到请求失败（{status}）：{}", text.chars().take(150).collect::<String>()));
    }
    let val: Value = serde_json::from_str(&text).map_err(|e| format!("解析签到打卡响应失败: {e}"))?;
    Ok(val)
}

