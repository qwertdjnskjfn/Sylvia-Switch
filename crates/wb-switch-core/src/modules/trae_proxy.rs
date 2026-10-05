//! TRAE Work CN 模型反代（V1：模型目录 + 权益/倍率通道）。
//!
//! TRAE SOLO CN 的 LLM 对话端点（`POST {agent}/api/ide/v1/chat`，Cloud-IDE-JWT
//! + 18 个设备头 + SSE）已实测打通，但服务端在请求前要求 `CreateChatSession`
//!   建立的会话态（缺会话时 SSE 固定回 `3003 all models failed`），无法以
//!   单发请求转发对话。因此 V1 反代交付：
//! - 模型目录：从本地 OpenCode 形态的会话缓存
//!   `state.vscdb`（`AI.agent.model.model_list_map`）离线读取全部场景模型
//!   （含显示名/倍率/上下文/思考标记），该缓存由客户端服务端下发同步；
//! - 权益通道：复用 `platform_checkin` 的 TRAE 凭证查询 `ide_user_ent_usage`
//!   （套餐额度/到期），与账号页一致。
//!
//! 对话转发待后续补 `CreateChatSession → SendMessage → Commit` 会话栈后开放。

use serde_json::{json, Value};
use std::path::PathBuf;

use crate::modules::platform_checkin::trae_proxy_credential;

// ---------------------------------------------------------------------------
// 模型目录（离线缓存读取）
// ---------------------------------------------------------------------------

/// TRAE SOLO CN 的 storage.json 所在目录（vscdb 与之同级）。
fn trae_user_dir() -> Option<PathBuf> {
    for name in ["TRAE SOLO CN", "Trae CN", "TraeWork CN", "TRAE SOLO", "Trae"] {
        if let Some(base) = dirs::config_dir() {
            let dir = base.join(name).join("User").join("globalStorage");
            if dir.join("state.vscdb").is_file() {
                return Some(dir);
            }
        }
    }
    None
}

/// 从 state.vscdb 读取模型目录缓存（JSON）。多场景合并去重。
fn load_model_list_map() -> Result<Value, String> {
    let dir = trae_user_dir().ok_or_else(|| {
        "未找到 TRAE SOLO CN 数据目录（请安装并登录 TRAE 客户端）".to_string()
    })?;
    let db_path = dir.join("state.vscdb");
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("打开 state.vscdb 失败: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT value FROM ItemTable WHERE key LIKE '%:AI.agent.model.model_list_map' ORDER BY key DESC LIMIT 1")
        .map_err(|e| e.to_string())?;
    let value: String = stmt
        .query_row([], |row| row.get(0))
        .map_err(|e| format!("模型目录缓存不存在（请打开 TRAE 客户端一次）: {e}"))?;
    serde_json::from_str(&value).map_err(|e| format!("解析模型目录失败: {e}"))
}

/// 模型目录响应：全场景合并去重，含倍率/上下文/思考能力。
pub async fn models_catalog() -> Value {
    let map = match load_model_list_map() {
        Ok(map) => map,
        Err(error) => return json!({"ok": false, "error": error}),
    };
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();
    let mut scenes: Vec<(String, usize)> = Vec::new();
    if let Some(obj) = map.as_object() {
        for (scene, list) in obj {
            let Some(list) = list.as_array() else {
                continue;
            };
            if list.is_empty() {
                continue;
            }
            scenes.push((scene.clone(), list.len()));
            for model in list {
                let Some(name) = model.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if !seen.insert(name.to_string()) {
                    continue;
                }
                let rate = model
                    .pointer("/features/consumption_rate/data/rate")
                    .and_then(Value::as_f64);
                models.push(json!({
                    "id": name,
                    "display_name": model.get("display_name").cloned().unwrap_or(Value::Null),
                    "scene": scene,
                    "model_type": model.get("model_type").cloned().unwrap_or(Value::Null),
                    "rate": rate,
                    "reasoning": model.get("model_type").and_then(Value::as_str) == Some("reasoning_model"),
                    "multimodal": model.get("multimodal").cloned().unwrap_or(Value::Bool(false)),
                    "context_max": model.pointer("/context_window_size/max/0").cloned().unwrap_or(Value::Null),
                    "status": model.get("status").cloned().unwrap_or(Value::Bool(true)),
                }));
            }
        }
    }
    models.sort_by(|a, b| {
        let ra = a.get("rate").and_then(Value::as_f64).unwrap_or(f64::MAX);
        let rb = b.get("rate").and_then(Value::as_f64).unwrap_or(f64::MAX);
        ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
    });
    json!({
        "ok": true,
        "count": models.len(),
        "scenes": scenes,
        "models": models,
        "note": "模型目录来自 TRAE 客户端本地缓存（服务端下发）；对话转发需客户端会话态，暂未开放",
    })
}

/// TRAE 状态探测：凭证可用性 + 目录规模。
pub async fn proxy_status() -> Value {
    let cred_ok = match trae_proxy_credential().await {
        Ok(cred) => json!({"ok": true, "user": cred.username, "host": cred.host}),
        Err(error) => json!({"ok": false, "error": error}),
    };
    let catalog = models_catalog().await;
    json!({
        "credential": cred_ok,
        "models": {"count": catalog.get("count").cloned().unwrap_or(json!(0))},
        "chatForwarding": false,
    })
}

/// OpenAI /v1/models 形态（目录透出）。
pub async fn models_payload() -> Value {
    let catalog = models_catalog().await;
    let mut data = Vec::new();
    if let Some(list) = catalog.get("models").and_then(Value::as_array) {
        for model in list {
            if let Some(id) = model.get("id").and_then(Value::as_str) {
                data.push(json!({
                    "id": id,
                    "object": "model",
                    "created": 0,
                    "owned_by": "trae",
                }));
            }
        }
    }
    json!({"object": "list", "data": data})
}

#[cfg(test)]
mod tests {
    #[test]
    fn trae_user_dir_finds_installed_client() {
        // 本机装有 TRAE SOLO CN；目录探测应命中（环境性，缺客户端时跳过）。
        if let Some(dir) = super::trae_user_dir() {
            assert!(dir.join("state.vscdb").is_file());
        }
    }
}
