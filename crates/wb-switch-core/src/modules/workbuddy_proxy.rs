//! WorkBuddy 模型反代：借道本机 OpenCode server 转发对话 + 模型目录。
//!
//! WorkBuddy 桌面端的 AI 内核是内嵌 OpenCode（启动时随机端口
//! `opencode serve --hostname=127.0.0.1 --port=<N>`），其 provider 表由
//! WorkBuddy 注入登录态（实测 `GET /provider` 可见 `connected` 清单，
//! 含 opencode/imodel/deepseek 等 11 个已鉴权 provider）。模型对话即
//! OpenCode 标准 API：
//! - `POST /session` 建会话 → `POST /session/{id}/message`（fire）→
//!   `GET /session/{id}/message` 轮询取 assistant 回复（含 tokens/cost）。
//!
//! 端口发现：扫描本机监听端口中 `GET /global/health` 返回
//! `{"healthy":true,"version":"..."}` 的 OpenCode server（响应体带
//! opencode 特征）。也可在配置里写死端口。
//!
//! OpenAI 兼容层：`/workbuddy/v1/chat/completions` 把 messages 拍平为
//! 单条 text（agent 场景），非流式聚合后合成 chunk 帧（与 glm_proxy / loomy_proxy
//! 相同的策略——OpenCode 的 message 提交是 fire-and-poll，无逐 token 流）。

use bytes::Bytes;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, now_ms, store_dir};

const HEALTH_TIMEOUT_SECS: u64 = 3;

// ---------------------------------------------------------------------------
// 配置（~/.wb-switch/workbuddy_proxy_config.json）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct WorkbuddyProxyConfig {
    pub enabled: bool,
    pub api_key: String,
    /// opencode server 端口（0 = 自动发现）。
    pub server_port: u16,
}

fn config_path() -> PathBuf {
    store_dir().join("workbuddy_proxy_config.json")
}

pub fn load_config() -> WorkbuddyProxyConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| WorkbuddyProxyConfig {
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            api_key: v
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            server_port: v
                .get("server_port")
                .and_then(Value::as_u64)
                .map(|p| p as u16)
                .unwrap_or(0),
        })
        .unwrap_or_else(|| {
            let cfg = WorkbuddyProxyConfig {
                enabled: true,
                api_key: crate::modules::glm_proxy::generate_api_key(),
                server_port: 0,
            };
            let _ = save_config(&cfg);
            cfg
        })
}

pub fn save_config(cfg: &WorkbuddyProxyConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "enabled": cfg.enabled,
        "api_key": cfg.api_key,
        "server_port": cfg.server_port,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Server 发现
// ---------------------------------------------------------------------------

static PORT_CACHE: Mutex<Option<(i64, u16)>> = Mutex::new(None);
const PORT_TTL_MS: i64 = 60 * 1000;

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::new())
}

/// 探测端口是否为 opencode server（/global/health 特征）。
async fn is_opencode_server(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/global/health");
    let resp = http_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(HEALTH_TIMEOUT_SECS))
        .send()
        .await;
    match resp {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(v) => v.get("healthy").and_then(Value::as_bool) == Some(true),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// 发现 opencode server 端口：配置端口优先，否则扫描候选常见端口 + 监听端口。
pub async fn discover_server() -> Result<u16, String> {
    {
        let guard = PORT_CACHE.lock().unwrap();
        if let Some((at, port)) = guard.as_ref() {
            if now_ms() - at < PORT_TTL_MS {
                return Ok(*port);
            }
        }
    }
    let cfg = load_config();
    if cfg.server_port > 0 {
        if is_opencode_server(cfg.server_port).await {
            let mut guard = PORT_CACHE.lock().unwrap();
            *guard = Some((now_ms(), cfg.server_port));
            return Ok(cfg.server_port);
        }
        return Err(format!(
            "配置的 opencode 端口 {} 未响应（WorkBuddy 未运行？）",
            cfg.server_port
        ));
    }
    // 优先从 opencode serve 进程的命令行抓端口（最可靠，无端口范围假设）。
    if let Ok(out) = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'opencode\\s+serve' } | ForEach-Object { $_.CommandLine }",
        ])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            for token in line.split_whitespace() {
                if let Some(port_str) = token.strip_prefix("--port=") {
                    if let Ok(port) = port_str.parse::<u16>() {
                        if is_opencode_server(port).await {
                            let mut guard = PORT_CACHE.lock().unwrap();
                            *guard = Some((now_ms(), port));
                            return Ok(port);
                        }
                    }
                }
            }
        }
    }
    // 兜底：扫描当前监听的本地端口（含常见临时端口与 4000-5000 段）。
    let mut candidates = Vec::new();
    if let Ok(out) = std::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 && parts[3] == "LISTENING" {
                if let Some(local) = parts[1].rsplit(':').next() {
                    if let Ok(port) = local.parse::<u16>() {
                        // 排除知名端口；opencode serve 默认 4096 起，动态端口 49152+
                        if (port >= 4096 && port < 49152) || (49152..=65535).contains(&port) {
                            if !candidates.contains(&port) {
                                candidates.push(port);
                            }
                        }
                    }
                }
            }
        }
    }
    // 按端口从小到大尝试，最多 80 个。
    candidates.sort_unstable();
    candidates.truncate(80);
    for chunk in candidates.chunks(16) {
        let results = futures_util::future::join_all(
            chunk.iter().map(|p| is_opencode_server(*p)),
        )
        .await;
        for (idx, ok) in results.iter().enumerate() {
            if *ok {
                let port = chunk[idx];
                let mut guard = PORT_CACHE.lock().unwrap();
                *guard = Some((now_ms(), port));
                return Ok(port);
            }
        }
    }
    Err("未发现运行中的 OpenCode server（请启动 WorkBuddy 并打开一个 AI 会话）".to_string())
}

// ---------------------------------------------------------------------------
// 模型目录（provider 接口，仅已鉴权 provider）
// ---------------------------------------------------------------------------

/// 模型目录：从 /provider 抓取（只列 connected provider 的模型）。
pub async fn models_catalog() -> Result<Value, String> {
    let port = discover_server().await?;
    let url = format!("http://127.0.0.1:{port}/provider");
    let resp = http_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("读取 provider 失败: {e}"))?;
    let data: Value = resp.json().await.map_err(|e| e.to_string())?;
    let connected: Vec<String> = data
        .get("connected")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let mut providers = Vec::new();
    if let Some(all) = data.get("all").and_then(Value::as_array) {
        for provider in all {
            let Some(pid) = provider.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !connected.iter().any(|c| c == pid) {
                continue;
            }
            let mut models = Vec::new();
            if let Some(map) = provider.get("models").and_then(Value::as_object) {
                for (mid, model) in map {
                    models.push(json!({
                        "id": mid,
                        "name": model.get("name").cloned().unwrap_or(Value::Null),
                        "reasoning": model.pointer("/capabilities/reasoning").cloned().unwrap_or(Value::Bool(false)),
                        "context": model.pointer("/limit/context").cloned().unwrap_or(Value::Null),
                    }));
                }
            }
            models.sort_by(|a, b| {
                let ai = a.get("id").and_then(Value::as_str).unwrap_or("");
                let bi = b.get("id").and_then(Value::as_str).unwrap_or("");
                ai.cmp(bi)
            });
            providers.push(json!({
                "id": pid,
                "name": provider.get("name").cloned().unwrap_or(Value::Null),
                "models": models,
            }));
        }
    }
    providers.sort_by(|a, b| {
        let ai = a.get("id").and_then(Value::as_str).unwrap_or("");
        let bi = b.get("id").and_then(Value::as_str).unwrap_or("");
        ai.cmp(bi)
    });
    Ok(json!({"ok": true, "providers": providers}))
}

/// OpenAI /v1/models 形态。
pub async fn models_payload() -> Result<Value, String> {
    let catalog = models_catalog().await?;
    let mut data = Vec::new();
    if let Some(providers) = catalog.get("providers").and_then(Value::as_array) {
        for provider in providers {
            let pid = provider.get("id").and_then(Value::as_str).unwrap_or("unknown");
            if let Some(models) = provider.get("models").and_then(Value::as_array) {
                for model in models {
                    if let Some(id) = model.get("id").and_then(Value::as_str) {
                        // 模型 ID 用 "provider/model" 形式（与 opencode 一致）。
                        data.push(json!({
                            "id": format!("{pid}/{id}"),
                            "object": "model",
                            "created": 0,
                            "owned_by": pid,
                        }));
                    }
                }
            }
        }
    }
    Ok(json!({"object": "list", "data": data}))
}

/// 状态探测。
pub async fn proxy_status() -> Value {
    let cfg = load_config();
    let server = match discover_server().await {
        Ok(port) => json!({"ok": true, "port": port}),
        Err(error) => json!({"ok": false, "error": error}),
    };
    let catalog = models_catalog().await.ok();
    let model_count = catalog
        .as_ref()
        .and_then(|c| c.get("providers"))
        .and_then(Value::as_array)
        .map(|providers| {
            providers
                .iter()
                .filter_map(|p| p.get("models"))
                .filter_map(Value::as_array)
                .map(|m| m.len())
                .sum::<usize>()
        })
        .unwrap_or(0);
    let providers_value = catalog
        .as_ref()
        .and_then(|c| c.get("providers"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "enabled": cfg.enabled,
        "server": server,
        "providers": providers_value,
        "modelCount": model_count,
    })
}

// ---------------------------------------------------------------------------
// Chat 转发（OpenAI 兼容）
// ---------------------------------------------------------------------------

fn flatten_messages(messages: &[Value]) -> String {
    let mut chunks = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = match message.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(Value::as_str) == Some("text") {
                        p.get("text").and_then(Value::as_str).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !content.trim().is_empty() {
            chunks.push(format!("[{role}] {content}"));
        }
    }
    chunks.join("\n\n")
}

/// 非流式对话补全：建会话 → 发消息 → 轮询取 assistant 回复。
pub async fn chat_completions_json(body: Value) -> Result<Value, String> {
    let port = discover_server().await?;
    let base = format!("http://127.0.0.1:{port}");
    let model = body.get("model").and_then(Value::as_str).unwrap_or("opencode/mimo-v2.5-free");
    let (provider_id, model_id) = match model.split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => ("opencode".to_string(), model.to_string()),
    };
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let prompt = flatten_messages(&messages);
    if prompt.trim().is_empty() {
        return Err("messages 不能为空".to_string());
    }
    let client = http_client();
    // 1) 建会话。
    let session: Value = client
        .post(format!("{base}/session"))
        .json(&json!({}))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("创建会话失败: {e}"))?
        .json()
        .await
        .map_err(|e| format!("解析会话响应失败: {e}"))?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "创建会话失败：响应缺少 id".to_string())?
        .to_string();
    // 2) 提交消息（fire-and-poll：POST 立即返回，回复异步产出）。
    let payload = json!({
        "parts": [{"type": "text", "text": prompt}],
        "model": {"providerID": provider_id, "modelID": model_id},
        "agent": "build",
    });
    let submit = client
        .post(format!("{base}/session/{session_id}/message"))
        .json(&payload)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;
    match submit {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // 会话已建立但提交失败：清理会话后报错。
            let _ = client
                .delete(format!("{base}/session/{session_id}"))
                .send()
                .await;
            return Err(format!("提交消息失败 {status}: {}", truncate(&text, 200)));
        }
        Err(e) => {
            let _ = client
                .delete(format!("{base}/session/{session_id}"))
                .send()
                .await;
            return Err(format!("提交消息失败: {e}"));
        }
    }
    // 3) 轮询取 assistant 回复（上限 5 分钟）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut tokens = Value::Null;
    let mut cost = Value::Null;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let messages: Vec<Value> = match client
            .get(format!("{base}/session/{session_id}/message"))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) => resp.json().await.unwrap_or_default(),
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                continue;
            }
        };
        // 最后一条 assistant 消息即当前轮回复。
        if let Some(last) = messages.iter().rev().find(|m| {
            m.pointer("/info/role").and_then(Value::as_str) == Some("assistant")
        }) {
            let completed = last
                .pointer("/info/time/completed")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let texts: Vec<String> = last
                .get("parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            if completed > 0 {
                tokens = last.pointer("/info/tokens").cloned().unwrap_or(Value::Null);
                cost = last.pointer("/info/cost").cloned().unwrap_or(Value::Null);
                let reply = texts.join("");
                // 清理会话（不留垃圾）。
                let _ = client
                    .delete(format!("{base}/session/{session_id}"))
                    .send()
                    .await;
                return Ok(json!({
                    "id": format!("chatcmpl-{}", session_id),
                    "object": "chat.completion",
                    "created": now_ms() / 1000,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": reply},
                        "finish_reason": "stop",
                    }],
                    "usage": {
                        "prompt_tokens": tokens.get("input").cloned().unwrap_or(json!(0)),
                        "completion_tokens": tokens.get("output").cloned().unwrap_or(json!(0)),
                        "total_tokens": tokens.get("total").cloned().unwrap_or(json!(0)),
                        "cost": cost,
                    },
                }));
            }
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
    let _ = client
        .delete(format!("{base}/session/{session_id}"))
        .send()
        .await;
    Err("等待回复超时（模型可能排队中或不可用）".to_string())
}

/// 流式响应封装（OpenAI chunk SSE 字节流）。
///
/// 原为跨模块共用的流式封装类型，现已内聚到本模块并使用中性命名。**结构与行为完全不变**。
pub struct WorkbuddyUpstreamStream {
    pub rx: tokio::sync::mpsc::Receiver<Result<Bytes, String>>,
}

/// 流式：非流式结果合成 OpenAI chunk 帧（OpenCode message 提交无逐 token 流）。
pub async fn chat_completions_stream(
    body: Value,
) -> Result<WorkbuddyUpstreamStream, String> {
    let completion = chat_completions_json(body).await?;
    let model = completion
        .get("model")
        .cloned()
        .unwrap_or(json!("workbuddy"));
    let content = completion
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = completion.get("usage").cloned().unwrap_or(Value::Null);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    tokio::spawn(async move {
        let frame = |delta: Value, finish: Option<&str>| {
            json!({
                "id": format!("chatcmpl-workbuddy-{}", now_ms()),
                "object": "chat.completion.chunk",
                "created": now_ms() / 1000,
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
            .to_string()
        };
        let _ = tx.send(Ok(Bytes::from(format!(
            "data: {}\n\n",
            frame(json!({"role": "assistant", "content": ""}), None)
        )))).await;
        if !content.is_empty() {
            let _ = tx.send(Ok(Bytes::from(format!(
                "data: {}\n\n",
                frame(json!({"content": content}), None)
            )))).await;
        }
        let mut usage_json = usage;
        if let Some(obj) = usage_json.as_object_mut() {
            obj.insert("stream".to_string(), json!(true));
        }
        let _ = tx.send(Ok(Bytes::from(format!(
            "data: {}\n\n",
            json!({
                "id": format!("chatcmpl-workbuddy-{}", now_ms()),
                "object": "chat.completion.chunk",
                "created": now_ms() / 1000,
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": usage_json,
            })
            .to_string()
        )))).await;
        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
    });
    Ok(WorkbuddyUpstreamStream { rx })
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
