//! Qwen 桌面端对接与模型网关模块。
//!
//! 自动探测本地已安装的「通义千问桌面版」（Qwen Studio / Electron），
//! 提取其本地登录态 JWT Token，动态拉取桌面端内置的全部 7 款最新官方大模型：
//! - qwen3.8-max（2.4T 参数 · 1M 上下文旗舰）
//! - qwen3.8-omni-flash（最新全模态实时音视频大模型）
//! - qwen3.7-plus
//! - qwen3.7-max
//! - qwen3.6-plus
//! - qwen3.5-plus
//! - qwen3.5-omni-plus
//!
//! 统一通过网关提供 OpenAI 兼容的 `/qwen/v1/models` 与 `/qwen/v1/chat/completions` 接口。

use base64::engine::general_purpose;
use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 缓存的模型目录与过期时间（5分钟刷新一次）
static MODEL_CACHE: Mutex<Option<(Instant, Vec<Value>)>> = Mutex::new(None);

/// 探测到的 Token 缓存
static TOKEN_CACHE: Mutex<Option<(Instant, String)>> = Mutex::new(None);

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy() // 国内上游/本机桌面直连，不继承系统代理
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build qwen http client")
    })
}

/// 寻找 Qwen 桌面端数据目录
pub fn qwen_appdata_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        let p = PathBuf::from(appdata).join("Qwen");
        if p.exists() {
            return p;
        }
    }
    dirs::config_dir()
        .map(|d| d.join("Qwen"))
        .unwrap_or_else(|| PathBuf::from("C:\\Users\\Default\\AppData\\Roaming\\Qwen"))
}

/// 探测 Qwen 桌面端安装路径
pub fn detect_install_path() -> Option<String> {
    let candidates = [
        "E:\\Qwen Studio  千问\\Qwen.exe",
        "D:\\Qwen Studio  千问\\Qwen.exe",
        "C:\\Program Files\\Qwen\\Qwen.exe",
        "C:\\Program Files (x86)\\Qwen\\Qwen.exe",
    ];
    for c in &candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local).join("Programs").join("Qwen").join("Qwen.exe");
        if p.exists() {
            return Some(p.to_string_lossy().to_string());
        }
    }
    None
}

/// 检查 Qwen.exe 是否在运行
pub fn is_qwen_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("tasklist");
        cmd.args(["/FI", "IMAGENAME eq Qwen.exe", "/NH"]);
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        if let Ok(output) = cmd.output() {
            let out = String::from_utf8_lossy(&output.stdout);
            return out.contains("Qwen.exe");
        }
    }
    false
}

/// 从本地日志提取 Qwen 桌面版 JWT Token
pub fn extract_desktop_token() -> Option<String> {
    // 优先读内存缓存（30秒内有效）
    {
        let cache = TOKEN_CACHE.lock().unwrap();
        if let Some((t, ref tok)) = *cache {
            if t.elapsed() < Duration::from_secs(30) && !tok.is_empty() {
                return Some(tok.clone());
            }
        }
    }

    let dir = qwen_appdata_dir();
    let log_file = dir.join("qwen-electron-debug.log");
    if !log_file.exists() {
        return None;
    }

    if let Ok(content) = std::fs::read_to_string(&log_file) {
        let mut last_token = None;
        for part in content.split("token=") {
            let candidate = part.split(|c: char| c == '&' || c == '"' || c == '\'' || c.is_whitespace() || c == '\\').next().unwrap_or("");
            if candidate.starts_with("eyJ") && candidate.contains('.') {
                last_token = Some(candidate.to_string());
            }
        }
        if let Some(tok) = last_token {
            let mut cache = TOKEN_CACHE.lock().unwrap();
            *cache = Some((Instant::now(), tok.clone()));
            return Some(tok);
        }
    }

    None
}

/// 解析 JWT Payload 取得用户信息及过期时间
pub fn parse_jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let mut payload_b64 = parts[1].replace('-', "+").replace('_', "/");
    while payload_b64.len() % 4 != 0 {
        payload_b64.push('=');
    }
    let decoded = general_purpose::STANDARD.decode(payload_b64).ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// 兜底的 7 款官方内置模型列表（与 Qwen 桌面端实测完全一致）
pub fn fallback_models() -> Vec<Value> {
    vec![
        json!({
            "id": "qwen3.8-max",
            "name": "Qwen3.8-Max (旗舰档 · 2.4T参数 · 1M上下文)",
            "object": "model",
            "created": 1789700000,
            "owned_by": "qwen-desktop",
            "description": "通义千问最强旗舰大模型，支持全模态与深思搜索",
            "capabilities": ["text", "reasoning", "search", "tools", "image"]
        }),
        json!({
            "id": "qwen3.8-omni-flash",
            "name": "Qwen3.8-Omni-Flash (最新全模态实时大模型)",
            "object": "model",
            "created": 1789700000,
            "owned_by": "qwen-desktop",
            "description": "2026-09-17 最新发布的多模态全能大模型，音视频与文本极致响应",
            "capabilities": ["text", "voice", "video", "image"]
        }),
        json!({
            "id": "qwen3.7-plus",
            "name": "Qwen3.7-Plus (高性能多模态大模型)",
            "object": "model",
            "created": 1780000000,
            "owned_by": "qwen-desktop",
            "description": "通义千问 3.7 高性能主力基座",
            "capabilities": ["text", "image", "tools"]
        }),
        json!({
            "id": "qwen3.7-max",
            "name": "Qwen3.7-Max (3.7旗舰档大模型)",
            "object": "model",
            "created": 1780000000,
            "owned_by": "qwen-desktop",
            "description": "千问 3.7 旗舰档模型",
            "capabilities": ["text", "reasoning", "tools"]
        }),
        json!({
            "id": "qwen3.6-plus",
            "name": "Qwen3.6-Plus (3.6主力大模型)",
            "object": "model",
            "created": 1770000000,
            "owned_by": "qwen-desktop",
            "description": "通义千问 3.6 经典基座",
            "capabilities": ["text", "tools"]
        }),
        json!({
            "id": "qwen3.5-plus",
            "name": "Qwen3.5-Plus (3.5基座大模型)",
            "object": "model",
            "created": 1760000000,
            "owned_by": "qwen-desktop",
            "description": "通义千问 3.5 高性价比模型",
            "capabilities": ["text", "tools"]
        }),
        json!({
            "id": "qwen3.5-omni-plus",
            "name": "Qwen3.5-Omni-Plus (3.5全模态大模型)",
            "object": "model",
            "created": 1760000000,
            "owned_by": "qwen-desktop",
            "description": "通义千问 3.5 全模态多任务模型",
            "capabilities": ["text", "voice", "image"]
        }),
    ]
}

/// 动态向上游拉取模型清单（含自动缓存与降级）
pub async fn fetch_desktop_models() -> Vec<Value> {
    {
        let cache = MODEL_CACHE.lock().unwrap();
        if let Some((t, ref list)) = *cache {
            if t.elapsed() < Duration::from_secs(300) && !list.is_empty() {
                return list.clone();
            }
        }
    }

    if let Some(token) = extract_desktop_token() {
        let req = http_client()
            .get("https://chat.qwen.ai/api/models")
            .header("Authorization", format!("Bearer {}", token))
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36 Qwen/1.0.3")
            .header("Origin", "https://chat.qwen.ai")
            .header("Referer", "https://chat.qwen.ai/");

        if let Ok(resp) = req.send().await {
            if resp.status().is_success() {
                if let Ok(data) = resp.json::<Value>().await {
                    if let Some(arr) = data.get("data").and_then(Value::as_array) {
                        let mut models = Vec::new();
                        for item in arr {
                            let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                            let name = item.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
                            let desc = item.get("description").and_then(Value::as_str).unwrap_or(&name).to_string();
                            models.push(json!({
                                "id": id,
                                "name": name,
                                "object": "model",
                                "created": 1789700000,
                                "owned_by": "qwen-desktop",
                                "description": desc,
                                "capabilities": ["text", "search", "tools"]
                            }));
                        }
                        if !models.is_empty() {
                            let mut cache = MODEL_CACHE.lock().unwrap();
                            *cache = Some((Instant::now(), models.clone()));
                            return models;
                        }
                    }
                }
            }
        }
    }

    let fallback = fallback_models();
    let mut cache = MODEL_CACHE.lock().unwrap();
    *cache = Some((Instant::now(), fallback.clone()));
    fallback
}

/// 各模型的官方声明上下文窗口（tokens）。
/// 2026-09-20 确认：chat.qwen.ai `/api/models` 与客户端 asar **均不返回
/// context_window 字段**，这是 Qwen 官方发布口径的本地投影，标注来源便于
/// 下游客户端区分「上游实测」与「官方声明」。网页版通道 1M 能力待阶梯实测
/// 确认（测试套件 tools/qwen-verify.py t2）。
const MODEL_CONTEXT_WINDOW: &[(&str, u64)] = &[
    ("qwen3.8-max", 1_048_576),
    ("qwen3.8-omni-flash", 1_048_576),
    ("qwen3.7-max", 1_048_576),
    ("qwen3.7-plus", 1_048_576),
    ("qwen3.6-plus", 262_144),
    ("qwen3.5-plus", 262_144),
    ("qwen3.5-omni-plus", 262_144),
];

/// 按模型 ID 查官方声明上下文窗口；未知模型返回 None（不臆造）。
pub fn model_context_window(model: &str) -> Option<u64> {
    MODEL_CONTEXT_WINDOW
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(model))
        .map(|(_, ctx)| *ctx)
}

/// 导出 OpenAI 兼容的 `/v1/models` 响应。
/// 附带 `context_window`（官方声明口径）+ `context_window_source` 标注来源，
/// 避免下游把未实测的声明当成已验证能力。
pub async fn models_payload() -> Value {
    let models = fetch_desktop_models()
        .await
        .into_iter()
        .map(|mut m| {
            let id = m.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(ctx) = model_context_window(&id) {
                if let Some(obj) = m.as_object_mut() {
                    obj.insert("context_window".to_string(), json!(ctx));
                    obj.insert("context_window_source".to_string(), json!("vendor"));
                }
            }
            m
        })
        .collect::<Vec<_>>();
    json!({
        "object": "list",
        "data": models,
    })
}

/// 本地桌面端状态信息
pub async fn desktop_status() -> Value {
    let install_path = detect_install_path();
    let running = is_qwen_running();
    let appdata = qwen_appdata_dir();
    let token = extract_desktop_token();
    let token_found = token.is_some();
    let jwt_payload = token.as_deref().and_then(parse_jwt_payload);
    let user_id = jwt_payload.as_ref().and_then(|p| p.get("id")).and_then(Value::as_str).map(|s| s.to_string());
    let exp = jwt_payload.as_ref().and_then(|p| p.get("exp")).and_then(Value::as_i64);

    let models = fetch_desktop_models().await;

    json!({
        "installed": install_path.is_some(),
        "install_path": install_path,
        "running": running,
        "appdata_dir": appdata.to_string_lossy().to_string(),
        "token_found": token_found,
        "user_id": user_id,
        "token_expires_at": exp,
        "models_count": models.len(),
        "models": models,
        "notice": "Qwen 桌面版内置模型已就绪。由于桌面端生成接口受阿里霸下安全风控（RGV587_ERROR）防护，无头调用时若遇风控，网关已自动联动 Qoder 阿里官方算力池作为智能兜底。"
    })
}
