# -*- coding: utf-8 -*-
"""一次性补丁：zcode.rs 追加 auto_switch 模块（额度耗尽自动切换）。"""
import io

p = 'crates/wb-switch-core/src/modules/zcode.rs'
t = io.open(p, encoding='utf-8').read()

assert 'pub fn auto_switch_state' not in t, 'auto_switch already present'

anchor = '#[cfg(test)]'
section = '''// ---------------------------------------------------------------------------
// 1.5.2 多账号自动管理：额度耗尽自动切换（2026-10-03）
// ---------------------------------------------------------------------------
//
// 闭环：后台每 5 分钟查当前登录账号的 Coding Plan 剩余额度（quota_for 双通道，
// 3-5s）→ 低于阈值时在账号池里找"还有余额"的下一个账号 → 热切换（复用已存
// 凭据，无需重新登录，ZCode 下一次鉴权即用新号，会话不中断）→ 全部耗尽时
// 桌面通知提示人工补充。
//
// 可行性边界（实测沉淀）：
// - 凭据复用：热切换只写 credentials.json + 删 plan 缓存 + 换虚拟 deviceMid，
//   不触碰 ZCode 进程 → 无需重新登录；但 ZCode 内存里的旧 token 在其下次
//   鉴权前仍会用旧号跑完当前请求（对会话透明）。
// - 池内账号凭据失效（JWT 过期且 refresh 失败）→ 该账号判 invalid 跳过并在
//   状态里给出明确原因，需要人工在 ZCode 里重新登录一次该账号再入库。
// - 额度"未知"（两通道都拿不到数值）不触发切换，避免误切。

const AUTO_SWITCH_STATE_FILE: &str = "zcode_autoswitch.json";

fn auto_switch_state_path() -> PathBuf {
    store_dir().join(AUTO_SWITCH_STATE_FILE)
}

/// 自动切换配置（存池文件顶层，与账号池同生命周期）。
#[derive(Clone, Debug)]
pub struct AutoSwitchConfig {
    pub enabled: bool,
    /// remaining（tokens）≤ 该阈值即视为耗尽并触发切换；0 = 彻底用完才切。
    pub threshold_tokens: f64,
}

pub fn auto_switch_config() -> AutoSwitchConfig {
    let pool = read_pool(&pool_file()).unwrap_or_else(|_| json!({"accounts": []}));
    let cfg = pool.get("autoSwitch").cloned().unwrap_or(json!({}));
    AutoSwitchConfig {
        enabled: cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        threshold_tokens: cfg
            .get("thresholdTokens")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .max(0.0),
    }
}

pub fn save_auto_switch_config(enabled: bool, threshold_tokens: f64) -> Result<(), String> {
    let path = pool_file();
    let mut pool = read_pool(&path)?;
    pool["autoSwitch"] = json!({
        "enabled": enabled,
        "thresholdTokens": threshold_tokens.max(0.0),
    });
    save_pool(&path, &pool)
}

fn load_auto_state() -> Value {
    std::fs::read_to_string(auto_switch_state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({
            "lastTickAt": null, "lastState": "idle",
            "lastSwitchAt": null, "lastSwitchTo": null,
            "lastMessage": null, "allExhaustedNotifiedAt": null,
            "invalidUntil": {},
            "events": [],
        }))
}

fn save_auto_state(state: &Value) {
    if let Some(dir) = auto_switch_state_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        auto_switch_state_path(),
        serde_json::to_string_pretty(state).unwrap_or_default(),
    );
}

fn push_event(state: &mut Value, message: &str) {
    if let Some(arr) = state.get_mut("events").and_then(Value::as_array_mut) {
        arr.insert(0, json!({"ts": now_ms(), "message": message}));
        arr.truncate(20);
    }
}

/// 额度响应 → 通道数值余额合计（全部通道拿不到数值 → None=未知）。
fn quota_available_numeric(quota: &Value) -> Option<f64> {
    let mut total = 0.0f64;
    let mut any = false;
    for ch in quota.get("channels").and_then(Value::as_array).into_iter().flatten() {
        if ch.get("error").is_some() && ch.get("items").is_none() {
            continue;
        }
        for it in ch.get("items").and_then(Value::as_array).into_iter().flatten() {
            if let Some(v) = json_num(it.get("remaining")) {
                total += v;
                any = true;
            }
        }
    }
    any.then_some(total)
}

/// 单轮自动切换检查（幂等，可被后台循环与手动端点共用）。
pub async fn auto_switch_tick() -> Value {
    let started = Instant::now();
    let cfg = auto_switch_config();
    let mut state = load_auto_state();
    state["lastTickAt"] = json!(now_ms());
    if !cfg.enabled {
        state["lastState"] = json!("disabled");
        save_auto_state(&state);
        return json!({"ok": true, "action": "skipped", "reason": "disabled"});
    }

    // 清理过期的 invalid 标记（6h 冷却后允许再试）
    let now = now_ms();
    if let Some(map) = state.get_mut("invalidUntil").and_then(Value::as_object_mut) {
        map.retain(|_, until| until.as_u64().unwrap_or(0) > now);
    }

    // ── 当前登录账号额度 ──
    let Some(live) = live_credentials() else {
        state["lastState"] = json!("no-login");
        state["lastMessage"] = json!("ZCode 未登录（credentials.json 缺失），请先登录一个账号");
        save_auto_state(&state);
        return json!({"ok": true, "action": "none", "reason": "no-login"});
    };
    if !is_logged_in(&live) {
        state["lastState"] = json!("no-login");
        state["lastMessage"] = json!("当前登录态已失效，请在 ZCode 重新登录");
        crate::modules::notify::notify(
            "zcode-autoswitch-nologin",
            "ZCode 账号自动管理",
            "当前登录态已失效，请在 ZCode 重新登录后再启用自动切换。",
        );
        save_auto_state(&state);
        return json!({"ok": true, "action": "none", "reason": "logged-out"});
    }

    let live_hash = canonical_hash(&live);
    let accounts = list_accounts();
    // 当前账号 = 哈希完全一致；无一致时视为"未入库的登录态"（只监控不切换来源）。
    let current_id = {
        let pool = read_pool(&pool_file()).unwrap_or_else(|_| json!({"accounts": []}));
        pool.get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .find(|acc| {
                acc.get("hash").and_then(Value::as_str) == Some(live_hash.as_str())
            })
            .and_then(|acc| acc.get("id").and_then(Value::as_str).map(String::from))
    };

    let live_quota = quota_for(&live).await;
    let live_avail = quota_available_numeric(&live_quota);
    if let Some(id) = &current_id {
        save_quota_cache_entry(id, &live_quota);
    }
    save_quota_cache_entry("zcode:live", &live_quota);

    let Some(avail) = live_avail else {
        state["lastState"] = json!("unknown");
        state["lastMessage"] = json!("额度查询不可用（两通道均未返回数值），本轮不判断不切换");
        save_auto_state(&state);
        return json!({"ok": true, "action": "none", "reason": "quota-unknown",
                      "quota": live_quota});
    };

    if avail > cfg.threshold_tokens {
        state["lastState"] = json!("ok");
        state["lastMessage"] = json!(format!("当前账号余额 {:.0} tokens，高于阈值 {:.0}", avail, cfg.threshold_tokens));
        state["allExhaustedNotifiedAt"] = json!(null);
        save_auto_state(&state);
        return json!({"ok": true, "action": "none", "reason": "ok", "available": avail});
    }

    // ── 耗尽 → 找下一个有余额的账号 ──
    let invalid_map = state
        .get("invalidUntil")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut tried: Vec<String> = Vec::new();
    for acc in &accounts {
        let Some(id) = acc.get("id").and_then(Value::as_str).map(String::from) else {
            continue;
        };
        if Some(&id) == current_id.as_ref() {
            continue;
        }
        if invalid_map.get(&id).is_some() {
            continue;
        }
        // 凭据级死号（JWT 过期且无 API Key 可用）直接跳过
        if acc
            .pointer("/identity/jwtExpired")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            tried.push(format!("{}（JWT 已过期）", id));
            continue;
        }
        let Some(creds) = pool_credentials(&id) else {
            tried.push(format!("{}（凭据缺失）", id));
            continue;
        };
        // 优先用 30 分钟内的缓存，避免逐号打网络
        let cached = quota_cache_entry(&id)
            .filter(|e| now.saturating_sub(e.get("fetchedAt").and_then(Value::as_u64).unwrap_or(0)) < 30 * 60 * 1000)
            .and_then(|e| e.get("quota").cloned());
        let quota = match cached {
            Some(q) => q,
            None => {
                let q = quota_for(&creds).await;
                save_quota_cache_entry(&id, &q);
                q
            }
        };
        match quota_available_numeric(&quota) {
            Some(remaining) if remaining > cfg.threshold_tokens => {
                tried.push(format!("{}（余额 {:.0}）", id, remaining));
                // 找到目标 → 热切换（复用已存凭据，无需重新登录）
                match switch_account(&id, false, false, true) {
                    Ok(res) => {
                        let name = acc
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(&id)
                            .to_string();
                        let msg = format!(
                            "当前账号额度耗尽（剩 {:.0} tokens），已自动热切换到「{}」（余额 {:.0} tokens）。ZCode 无需重新登录，下一次鉴权生效。",
                            avail, name, remaining
                        );
                        state["lastState"] = json!("switched");
                        state["lastSwitchAt"] = json!(now_ms());
                        state["lastSwitchTo"] = json!(name);
                        state["lastMessage"] = json!(msg.clone());
                        state["allExhaustedNotifiedAt"] = json!(null);
                        push_event(&mut state, &msg);
                        save_auto_state(&state);
                        crate::modules::notify::notify(
                            "zcode-autoswitch",
                            "ZCode 账号已自动切换",
                            &msg,
                        );
                        let mut out = res;
                        out["action"] = json!("switched");
                        out["availableBefore"] = json!(avail);
                        return out;
                    }
                    Err(e) => {
                        // 切换失败：若像登录态问题，冷却该账号 6h 后试下一个
                        let soft = e.contains("凭据缺失") || e.contains("登录态") || e.contains("失效");
                        if soft {
                            if let Some(m) = state.get_mut("invalidUntil").and_then(Value::as_object_mut) {
                                m.insert(id.clone(), json!(now_ms() + 6 * 3600 * 1000));
                            }
                        }
                        tried.push(format!("{}（切换失败: {}）", id, e));
                    }
                }
            }
            Some(remaining) => {
                tried.push(format!("{}（余额 {:.0}，低于阈值）", id, remaining));
            }
            None => {
                tried.push(format!("{}（额度未知）", id));
            }
        }
    }

    // ── 全部耗尽 ──
    let msg = format!(
        "所有账号额度均不足（当前剩 {:.0}，阈值 {:.0}）。候选：{}。请手动补充新账号或等待额度刷新（每日/周期重置）。",
        avail,
        cfg.threshold_tokens,
        if tried.is_empty() { "无其他池账号".to_string() } else { tried.join("；") }
    );
    state["lastState"] = json!("all-exhausted");
    state["lastMessage"] = json!(msg.clone());
    push_event(&mut state, &msg);
    save_auto_state(&state);
    // notify 自带 10 分钟同类节流，不会刷屏
    crate::modules::notify::notify("zcode-autoswitch-exhausted", "ZCode 账号自动管理", &msg);
    json!({
        "ok": true, "action": "all-exhausted", "available": avail,
        "message": msg, "tookMs": started.elapsed().as_millis() as u64,
    })
}

/// 后台循环：每 5 分钟一轮（额度查询 3-5s，网络成本可忽略）。
pub fn spawn_auto_switch_loop() {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(120)).await;
        loop {
            let _ = auto_switch_tick().await;
            tokio::time::sleep(Duration::from_secs(5 * 60)).await;
        }
    });
}

/// 自动切换状态（供 status / UI）。
pub fn auto_switch_state() -> Value {
    let mut st = load_auto_state();
    st["config"] = {
        let cfg = auto_switch_config();
        json!({"enabled": cfg.enabled, "thresholdTokens": cfg.threshold_tokens})
    };
    st
}

#[cfg(test)]
'''
# 插在 #[cfg(test)] 之前
assert anchor in t
t = t.replace(anchor, section, 1)
io.open(p, 'w', encoding='utf-8').write(t)
print('auto_switch section added')
