//! TRAE 反代独立配置（`~/.wb-switch/trae_proxy_config.json`）。
//!
//! T3（P0-F14）：TRAE 反代此前没有自己的密钥体系，`/trae/v1/models` 直接复用
//! Loomy 代理的 api_key。本模块给 TRAE 一份独立配置：缺失时自动生成随机 key
//! 并立即落盘（仿 `workbuddy_proxy`），同时记录 key 生成时间
//! （`key_created_at`，additive 字段）——`/trae/v1/models` 的 30 天迁移窗口
//! 依赖它：窗口内 loomy 旧 key 仍被接受（响应体带 `keyMigrated:true`）。
//!
//! T6（P0-F16）fullAccess 前置门禁同样读本配置：`acp_enabled` 默认 false、
//! `acp_allowed_dirs` 默认空（= 拒绝全部转发），本周期不开启。

use serde_json::{json, Value};
use std::path::PathBuf;

use crate::modules::config::{atomic_write, now_ms, store_dir};

// ---------------------------------------------------------------------------
// 配置（~/.wb-switch/trae_proxy_config.json）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TraeProxyConfig {
    /// TRAE 反代独立密钥（缺失时自动生成）。
    pub api_key: String,
    /// T6 ACP 转发总开关（默认 false：功能默认关闭）。
    pub acp_enabled: bool,
    /// T6 ACP 允许转发的目录白名单（空 = 拒绝全部）。
    pub acp_allowed_dirs: Vec<String>,
    /// key 生成时间（ms 时间戳）。T3 迁移窗口（生成后 30 天内兼容 loomy 旧 key）
    /// 的起算点；旧配置文件缺省时回落为本次加载时刻并补写落盘。
    pub key_created_at: i64,
}

fn config_path() -> PathBuf {
    store_dir().join("trae_proxy_config.json")
}

pub fn load_config() -> TraeProxyConfig {
    match std::fs::read_to_string(config_path())
        .ok()
        .as_deref()
        .and_then(parse_config)
    {
        Some(cfg) => cfg,
        None => {
            // 缺失文件 / api_key 为空 / key_created_at 缺失：生成随机 key 并立即落盘
            // （与 workbuddy_proxy 同范式）。
            let cfg = TraeProxyConfig {
                api_key: crate::modules::glm_proxy::generate_api_key(),
                acp_enabled: false,
                acp_allowed_dirs: Vec::new(),
                key_created_at: now_ms(),
            };
            let _ = save_config(&cfg);
            cfg
        }
    }
}

/// 解析配置文本；需要重建（api_key 为空 / key_created_at 缺失或非法）时返回 None，
/// 由调用方走「生成并落盘」分支。纯函数，便于测试。
fn parse_config(text: &str) -> Option<TraeProxyConfig> {
    let v: Value = serde_json::from_str(text).ok()?;
    let api_key = v
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if api_key.is_empty() {
        return None;
    }
    let key_created_at = v.get("key_created_at").and_then(Value::as_i64).unwrap_or(0);
    if key_created_at <= 0 {
        return None;
    }
    Some(TraeProxyConfig {
        api_key,
        // T6 硬门禁口径：默认关闭 + 白名单空。
        acp_enabled: v.get("acp_enabled").and_then(Value::as_bool).unwrap_or(false),
        acp_allowed_dirs: v
            .get("acp_allowed_dirs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        key_created_at,
    })
}

pub fn save_config(cfg: &TraeProxyConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "api_key": cfg.api_key,
        "acp_enabled": cfg.acp_enabled,
        "acp_allowed_dirs": cfg.acp_allowed_dirs,
        "key_created_at": cfg.key_created_at,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_all_fields() {
        let cfg = parse_config(
            r#"{"api_key":"k123","acp_enabled":true,"acp_allowed_dirs":["/a","/b"],"key_created_at":1700000000000}"#,
        )
        .expect("合法配置应解析成功");
        assert_eq!(cfg.api_key, "k123");
        assert!(cfg.acp_enabled);
        assert_eq!(cfg.acp_allowed_dirs, vec!["/a".to_string(), "/b".to_string()]);
        assert_eq!(cfg.key_created_at, 1_700_000_000_000);
    }

    #[test]
    fn parse_rejects_missing_key_or_timestamp() {
        // api_key 缺失/为空 → None（load_config 走生成分支）。
        assert!(parse_config(r#"{"key_created_at":1}"#).is_none());
        assert!(parse_config(r#"{"api_key":"","key_created_at":1}"#).is_none());
        // key_created_at 缺失/非法 → None（迁移窗口无法起算，需重建）。
        assert!(parse_config(r#"{"api_key":"k"}"#).is_none());
        assert!(parse_config(r#"{"api_key":"k","key_created_at":0}"#).is_none());
        // 非 JSON → None。
        assert!(parse_config("not json").is_none());
    }

    #[test]
    fn parse_defaults_gate_fields_closed() {
        // T6 硬门禁口径：旧文件缺省时 acp_enabled=false、白名单空。
        let cfg = parse_config(r#"{"api_key":"k","key_created_at":1}"#).unwrap();
        assert!(!cfg.acp_enabled);
        assert!(cfg.acp_allowed_dirs.is_empty());
    }
}
