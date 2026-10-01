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
    self::create_key_scoped(name, None)
}

/// 创建密钥；`region` 为 Some("cn")/Some("intl") 时按区域归属（WorkBuddy 池
/// 国内版/国际版专用密钥，跨区请求被拒），None 为通用密钥（无区域约束）。
pub fn create_key_scoped(name: &str, region: Option<&str>) -> Result<Value, String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut keys = load_keys();
    let region = region
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|r| !r.is_empty());
    let key = json!({
        "id": uuid::Uuid::new_v4().simple().to_string(),
        "name": if name.is_empty() { "默认密钥".to_string() } else { name.to_string() },
        "key": generate_key(),
        "region": region,
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
                // 区域归属（null=通用；cn/intl=区域专用，WorkBuddy 池两区密钥隔离）
                "region": k.get("region").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// 校验请求密钥：命中"网关专用 Key"或 GLM 代理主密钥。
/// 返回 (是否有效, 命中的 key id 或主密钥标记)。
pub fn verify_request_key(header_value: Option<&str>, glm_master: &str) -> (bool, Option<String>) {
    self::verify_request_key_region(header_value, glm_master, None)
}

/// 密钥归属是否匹配请求区域（纯函数，脱离 IO 便于测试）：
/// - 通用 key（region 空/识别不出）恒匹配——向后兼容；
/// - cn/intl 专用 key 只在 wanted 相同或 wanted 为空（无约束）时匹配。
fn key_region_matches(key_region: Option<&str>, wanted: Option<&str>) -> bool {
    let norm = |v: Option<&str>| -> Option<String> {
        v.map(str::trim)
            .map(str::to_ascii_lowercase)
            .filter(|r| !r.is_empty())
    };
    let key = norm(key_region).filter(|r| r == "cn" || r == "intl");
    match (key, norm(wanted)) {
        (None, _) => true,          // 通用 key：无区域约束
        (Some(_), None) => true,    // 无区域请求：不限制归属 key
        (Some(k), Some(w)) => k == w,
    }
}

/// 带区域约束的密钥校验（WorkBuddy 池两区密钥隔离）：
/// - `wanted_region` 为 Some("cn"/"intl") 时，区域专用 key 只有归属相同才通过；
///   通用 key（region 为空/非 cn/intl）与 GLM 主密钥**仍然通过**（兼容既有用户）；
/// - 区域专用 key 跨区使用一律拒绝，返回 (false, None)。
pub fn verify_request_key_region(
    header_value: Option<&str>,
    glm_master: &str,
    wanted_region: Option<&str>,
) -> (bool, Option<String>) {
    let Some(presented) = header_value else {
        return (false, None);
    };
    let wanted = wanted_region
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|r| !r.is_empty());
    // 主密钥仍然可用（向后兼容，无区域约束）
    if !glm_master.is_empty() && presented == glm_master {
        return (true, Some("__master__".to_string()));
    }
    let mut matched_id: Option<String> = None;
    for key in load_keys() {
        if key.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if key.get("key").and_then(Value::as_str) != Some(presented) {
            continue;
        }
        // 区域约束：key 归属 cn/intl 且与请求区不同 → 直接拒绝（不做计数）。
        let key_region = key
            .get("region")
            .and_then(Value::as_str);
        if !key_region_matches(key_region, wanted.as_deref()) {
            return (false, None);
        }
        matched_id = key.get("id").and_then(Value::as_str).map(String::from);
        break;
    }
    if let Some(id) = matched_id {
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
        return (true, Some(id));
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_region_matching_matrix() {
        // 通用 key：不受任何区域请求约束（向后兼容存量通用密钥）
        assert!(key_region_matches(None, Some("cn")));
        assert!(key_region_matches(None, Some("intl")));
        assert!(key_region_matches(None, None));
        // 区域 key：同区放行、跨区拒绝；大小写与空白容错
        assert!(key_region_matches(Some("cn"), Some("cn")));
        assert!(key_region_matches(Some("intl"), Some("intl")));
        assert!(!key_region_matches(Some("cn"), Some("intl")));
        assert!(!key_region_matches(Some("intl"), Some("cn")));
        assert!(key_region_matches(Some("CN"), Some("cn")));
        assert!(key_region_matches(Some(" intl "), Some("INTL")));
        // 无区域请求时连区域 key 也放行（models/status 等免 key 端点不经过此路径，
        // 这里保证老客户端不带 region 也能用）
        assert!(key_region_matches(Some("cn"), None));
        // 未知归属按通用处理（防御）
        assert!(key_region_matches(Some("legacy"), Some("cn")));
        assert!(key_region_matches(Some(""), Some("intl")));
    }
}
