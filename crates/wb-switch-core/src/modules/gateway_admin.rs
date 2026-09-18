//! Qoder / Kimi 网关的对外 API Key 管理与请求日志（统一模块）。
//!
//! 移植自两个原项目的管理台能力：
//! - QoderGateway：SQLite 存 API Key + 请求计数；
//! - kimi2api：API Keys 管理（创建/复制/删除/请求数/上次使用）+ `/v1/*`
//!   请求日志（状态、模型、Key、Kimi 账号、耗时、错误摘要）。
//!
//! 设计：两网关共用一份 Key 库与日志库（都挂在 Sylvia Switch 本服务上，
//! 密钥语义一致：与 GLM 代理主密钥并行的一套"网关专用密钥"）。
//! Key 与日志均落盘 `~/.wb-switch/`，日志保留最近 500 条、脱敏正文。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{atomic_write, now_ms, store_dir};

static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn api_keys_path() -> PathBuf {
    store_dir().join("gateway_api_keys.json")
}

fn request_logs_path() -> PathBuf {
    store_dir().join("gateway_request_logs.json")
}

const MAX_LOGS: usize = 500;

// ---------------------------------------------------------------------------
// API Key 管理
// ---------------------------------------------------------------------------

pub fn load_keys() -> Vec<Value> {
    std::fs::read_to_string(api_keys_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("keys").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn save_keys(keys: &[Value]) -> Result<(), String> {
    let path = api_keys_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "keys": keys,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

/// 生成 `gw_live_` 前缀随机密钥（与 QoderGateway 的 `qg_live_` 风格对齐）。
fn generate_key() -> String {
    format!("gw_live_{}", uuid::Uuid::new_v4().simple())
}

pub fn create_key(name: &str) -> Result<Value, String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut keys = load_keys();
    let key = json!({
        "id": uuid::Uuid::new_v4().simple().to_string(),
        "name": if name.is_empty() { "默认密钥".to_string() } else { name.to_string() },
        "key": generate_key(),
        "createdAt": now_ms(),
        "lastUsedAt": Value::Null,
        "requestCount": 0,
        "enabled": true,
    });
    keys.push(key.clone());
    save_keys(&keys)?;
    Ok(key)
}

pub fn delete_key(id: &str) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut keys = load_keys();
    let before = keys.len();
    keys.retain(|k| k.get("id").and_then(Value::as_str) != Some(id));
    if keys.len() == before {
        return false;
    }
    save_keys(&keys).is_ok()
}

pub fn list_keys_masked() -> Vec<Value> {
    load_keys()
        .iter()
        .map(|k| {
            let full = k.get("key").and_then(Value::as_str).unwrap_or_default();
            json!({
                "id": k.get("id").cloned().unwrap_or(Value::Null),
                "name": k.get("name").cloned().unwrap_or(Value::Null),
                "keyPreview": format!("{}…{}", &full[..full.len().min(12)], &full[full.len()-4..]),
                "createdAt": k.get("createdAt").cloned().unwrap_or(Value::Null),
                "lastUsedAt": k.get("lastUsedAt").cloned().unwrap_or(Value::Null),
                "requestCount": k.get("requestCount").cloned().unwrap_or(json!(0)),
                "enabled": k.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            })
        })
        .collect()
}

/// 校验请求密钥：命中"网关专用 Key"或 GLM 代理主密钥。
/// 返回 (是否有效, 命中的 key id 或主密钥标记)。
pub fn verify_request_key(header_value: Option<&str>, glm_master: &str) -> (bool, Option<String>) {
    let Some(presented) = header_value else {
        return (false, None);
    };
    // 主密钥仍然可用（向后兼容）
    if !glm_master.is_empty() && presented == glm_master {
        return (true, Some("__master__".to_string()));
    }
    for key in load_keys() {
        if key.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if key.get("key").and_then(Value::as_str) == Some(presented) {
            // 计数 + 上次使用
            let _guard = WRITE_LOCK.lock().unwrap();
            let mut keys = load_keys();
            for k in keys.iter_mut() {
                if k.get("key").and_then(Value::as_str) == Some(presented) {
                    k["requestCount"] = json!(k.get("requestCount").and_then(Value::as_u64).unwrap_or(0) + 1);
                    k["lastUsedAt"] = json!(now_ms());
                }
            }
            let _ = save_keys(&keys);
            return (true, key.get("id").and_then(Value::as_str).map(String::from));
        }
    }
    (false, None)
}

// ---------------------------------------------------------------------------
// 请求日志（kimi2api 风格）
// ---------------------------------------------------------------------------

/// 记录一次 `/qoder/v1/*` 或 `/kimi/v1/*` 请求。
pub fn record_request(entry: Value) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut logs = load_logs();
    logs.push(entry);
    if logs.len() > MAX_LOGS {
        let drop = logs.len() - MAX_LOGS;
        logs.drain(..drop);
    }
    let path = request_logs_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(&json!({ "logs": logs })) {
        let _ = atomic_write(&path, &content);
    }
}

pub fn load_logs() -> Vec<Value> {
    std::fs::read_to_string(request_logs_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("logs").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// 日志列表（倒序 + 可选筛选：网关/gateway、状态 ok/err、模型）。
pub fn list_logs(gateway: Option<&str>, limit: usize) -> Vec<Value> {
    let mut logs: Vec<Value> = load_logs()
        .into_iter()
        .rev()
        .filter(|entry| match gateway {
            Some(g) => entry.get("gateway").and_then(Value::as_str) == Some(g),
            None => true,
        })
        .take(limit.min(MAX_LOGS))
        .collect();
    // 简短字段脱敏：prompt 不入库，本来就只有摘要
    for entry in &mut logs {
        if let Some(error) = entry.get("error").and_then(Value::as_str) {
            entry["error"] = json!(error.chars().take(200).collect::<String>());
        }
    }
    logs
}

/// 最近 24h 概览（kimi2api Dashboard 的"最近 24 小时请求统计"）。
pub fn overview() -> Value {
    let cutoff = now_ms() - 24 * 3600 * 1000;
    let logs = load_logs();
    let recent: Vec<&Value> = logs.iter().filter(|l| l.get("ts").and_then(Value::as_i64).unwrap_or(0) >= cutoff).collect();
    let total = recent.len();
    let ok = recent
        .iter()
        .filter(|l| l.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    let mut by_gateway: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for entry in &recent {
        let gateway = entry.get("gateway").and_then(Value::as_str).unwrap_or("?").to_string();
        *by_gateway.entry(gateway).or_insert(0) += 1;
    }
    let keys = load_keys();
    json!({
        "requests24h": total,
        "ok24h": ok,
        "err24h": total - ok,
        "byGateway": by_gateway,
        "keyCount": keys.len(),
        "accountPools": {
            "qoder": crate::modules::qoder::load_accounts().len(),
            "kimi": crate::modules::kimi::load_accounts().len(),
        },
    })
}

