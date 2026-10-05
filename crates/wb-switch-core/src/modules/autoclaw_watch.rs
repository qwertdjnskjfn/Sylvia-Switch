//! AutoClaw 账号封禁监控（1.5.0 新能力，2026-10-01）。
//!
//! 背景：410004 封禁发生在 chat 计费层，登录态/任务列表探活接口依然 200，
//! 网关状态卡因此长期"绿"而 chat 全灭（2026-09-28 四账号封禁事故实测）。
//! 本模块做三件事：
//! 1. 主动探测：后台每 30 分钟对账号池内所有账号发一次最小 chat 请求，
//!    按 HTTP 状态分类（ok / banned 410004 / quota 810001 / invalid 401 / error）；
//! 2. 状态落盘与追踪：`~/.wb-switch/autoclaw_watch.json` 记录每账号当前态、
//!    首次进入时间与最近状态迁移历史（容量 20）；
//! 3. 立即告警：账号**进入**封禁态的瞬间发 Windows toast（notify 节流），
//!    同时落 account_protection 12h 冷却（渠道健康页"冷却中"徽标可见）。
//!
//! 探测请求本身是真实的 chat 计费请求（max_tokens=16，消耗极小）；探测频率
//! 30 分钟/轮对风控的增量暴露可忽略（对比正常对话流量）。

use serde_json::{json, Value};
use std::time::{Duration, Instant};

const WATCH_PATH: &str = "autoclaw_watch.json";
const PROBE_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// 单条状态历史最多保留条数
const HISTORY_CAP: usize = 20;

#[derive(Clone, Debug)]
pub struct WatchEntry {
    pub id: String,
    pub label: String,
    pub region: String,
    /// ok | banned | quota | invalid | error
    pub state: String,
    pub code: Value,
    pub detail: String,
    pub first_seen_at: u64,
    pub last_checked_at: u64,
    pub history: Vec<Value>,
}

fn watch_path() -> std::path::PathBuf {
    crate::modules::config::store_dir().join(WATCH_PATH)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 读取监控状态（供状态端点/前端）。
pub fn watch_state() -> Value {
    let v = std::fs::read_to_string(watch_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({"updatedAt": 0, "accounts": []}));
    v
}

fn load_entries() -> Vec<WatchEntry> {
    let v = std::fs::read_to_string(watch_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({"updatedAt": 0, "accounts": []}));
    v.get("accounts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some(WatchEntry {
                        id: e.get("id")?.as_str()?.to_string(),
                        label: e
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("未命名")
                            .to_string(),
                        region: e
                            .get("region")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        state: match e.get("state").and_then(Value::as_str) {
                            Some("banned") => "banned".to_string(),
                            Some("quota") => "quota".to_string(),
                            Some("invalid") => "invalid".to_string(),
                            Some("error") => "error".to_string(),
                            _ => "ok".to_string(),
                        },
                        code: e.get("code").cloned().unwrap_or(Value::Null),
                        detail: e
                            .get("detail")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        first_seen_at: e
                            .get("first_seen_at")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        last_checked_at: e
                            .get("last_checked_at")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        history: e
                            .get("history")
                            .and_then(|h| h.as_array().cloned())
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_entries(entries: &[WatchEntry], updated_at: u64) {
    let accounts: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "id": e.id, "label": e.label, "region": e.region,
                "state": e.state, "code": e.code, "detail": e.detail,
                "firstSeenAt": e.first_seen_at, "lastCheckedAt": e.last_checked_at,
                "history": e.history,
            })
        })
        .collect();
    let v = json!({"updatedAt": updated_at, "accounts": accounts});
    if let Some(dir) = watch_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(watch_path(), serde_json::to_string_pretty(&v).unwrap_or_default());
}

/// 状态分类：HTTP 状态码 + 响应原文 → 归一化状态。
pub fn classify(status: u16, body: &str) -> (&'static str, Value, String) {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("code").cloned())
        .unwrap_or(Value::Null);
    let detail = body.chars().take(160).collect::<String>();
    if status == 200 {
        ("ok", Value::Null, String::new())
    } else if status == 401 {
        ("invalid", json!("401"), "登录态失效".to_string())
    } else if status == 403 && (body.contains("410004") || body.contains("banned") || body.contains("封禁")) {
        // 保留上游原始 code（Number）与原始响应片段，而非硬编码文案
        ("banned", code.clone(), format!("上游封禁: {detail}"))
    } else if status == 403 && (body.contains("810001") || body.contains("pay-view")) {
        ("quota", code.clone(), format!("额度用尽（pay-view）: {detail}"))
    } else {
        ("error", code, format!("HTTP {status}: {detail}"))
    }
}

/// 立即探测一轮：更新状态、迁移时通知、封禁落冷却。返回本轮快照。
pub async fn probe_now() -> Value {
    let started = Instant::now();
    // 注意：必须用存储层 load_accounts（含 accessToken）；list_for_display 是
    // 展示层，会剥掉凭据导致探测 0 账号（2026-10-02 实测踩坑）。
    let accounts = crate::modules::autoclaw_accounts::load_accounts();
    let mut prev = load_entries();
    let now = now_ms();
    let mut next: Vec<WatchEntry> = Vec::new();
    let mut transition_banned: Vec<String> = Vec::new();

    for acc in &accounts {
        let id = acc
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let label = acc
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("未命名")
            .to_string();
        let region = acc
            .get("region")
            .and_then(Value::as_str)
            .unwrap_or("domestic")
            .to_string();
        let token = acc
            .get("accessToken")
            .or_else(|| acc.get("token"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if token.is_empty() {
            continue;
        }
        let (status, body) = crate::modules::glm_proxy::probe_account_chat(&region, &token).await;
        let (state_s, code, detail) = classify(status, &body);
        let state = state_s.to_string();

        let existing = prev
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .unwrap_or_else(|| WatchEntry {
                id: id.clone(),
                label: label.clone(),
                region: region.clone(),
                state: "ok".to_string(),
                code: Value::Null,
                detail: String::new(),
                first_seen_at: now,
                last_checked_at: 0,
                history: Vec::new(),
            });

        let mut history = existing.history.clone();
        if existing.state != state {
            history.insert(
                0,
                json!({"ts": now, "from": existing.state, "to": state, "detail": detail}),
            );
            history.truncate(HISTORY_CAP);
        }

        // 处于封禁态 → 持续保证冷却在位（幂等；重启丢冷却后由此恢复）
        // 状态迁移（首次进入）额外发 toast 告警，避免每轮重复打扰。
        // 本机账号在展示层以「本机实时」条目出现（discover id），与池 id 不同，
        // 故 localDiscoveryId 也要同步落冷却，否则该账号在页面看不到封禁。
        if state == "banned" {
            crate::modules::account_protection::mark_account_cooling(
                &id,
                &label,
                "上游 403 封禁（410004）",
                720,
            );
            if let Some(discovery_id) = acc.get("localDiscoveryId").and_then(Value::as_str) {
                crate::modules::account_protection::mark_account_cooling(
                    discovery_id,
                    &label,
                    "上游 403 封禁（410004）",
                    720,
                );
            }
            if existing.state != "banned" {
                transition_banned.push(label.clone());
                crate::modules::notify::notify(
                    "autoclaw-ban",
                    "AutoClaw 账号封禁警告",
                    &format!("账号 {label}（{region}）已被上游封禁（410004），GLM 代理请求将持续失败。请打开渠道健康页确认，并考虑停止使用该账号。"),
                );
            }
        }

        let first_seen_at = if existing.state == state {
            existing.first_seen_at
        } else {
            now
        };
        next.push(WatchEntry {
            id,
            label,
            region,
            state,
            code,
            detail,
            first_seen_at,
            last_checked_at: now,
            history,
        });
    }

    save_entries(&next, now);
    if !transition_banned.is_empty() {
        crate::modules::notify::notify(
            "autoclaw-ban-summary",
            "AutoClaw 封禁监控",
            &format!("本轮新进入封禁态：{}", transition_banned.join("、")),
        );
    }
    json!({
        "ok": true,
        "probed": next.len(),
        "tookMs": started.elapsed().as_millis() as u64,
        "newlyBanned": transition_banned,
        "accounts": next.iter().map(|e| json!({
            "id": e.id, "label": e.label, "region": e.region, "state": e.state,
            "code": e.code, "detail": e.detail,
            "firstSeenAt": e.first_seen_at, "lastCheckedAt": e.last_checked_at,
        })).collect::<Vec<_>>(),
    })
}

/// 后台循环：每 30 分钟探测一轮。
pub fn spawn_watch_loop() {
    tokio::spawn(async move {
        // 启动后 90 秒先跑一轮（等服务初始化完成）
        tokio::time::sleep(Duration::from_secs(90)).await;
        loop {
            let _ = probe_now().await;
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_all_known_states() {
        assert_eq!(classify(200, "{}").0, "ok");
        assert_eq!(
            classify(403, r#"{"code":410004,"message":"账号已被封禁"}"#).0,
            "banned"
        );
        assert_eq!(
            classify(403, r#"{"code":410004,"message":"The account has been banned"}"#).0,
            "banned"
        );
        assert_eq!(
            classify(403, r#"{"action":{"kind":"pay-view"},"code":810001}"#).0,
            "quota"
        );
        assert_eq!(classify(401, r#"{"error":"Invalid token"}"#).0, "invalid");
        assert_eq!(classify(500, "boom").0, "error");
    }

    #[test]
    fn classify_keeps_raw_detail() {
        let (_, code, detail) = classify(403, r#"{"code":410004,"message":"账号已被封禁"}"#);
        assert_eq!(code, json!(410004));
        assert!(detail.contains("410004"));
    }
}
