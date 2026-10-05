# -*- coding: utf-8 -*-
"""一次性补丁：MiniMax 账号行实时积分 + 模型目录展示。
① minimax.rs：MmAccount 加 balance/credit_consumed/balance_at 字段；
   credits_summary（remaining/consumed/expire 明细）；probe 后由 api 层写回。
② api.rs：probe 写回余额；status 暴露 balance/creditConsumed + 真实模型表。
③ MiniMaxPage：账号行显示积分余额；新增「模型目录」Section。
"""
import io

# ── 1) minimax.rs ──
p = 'crates/wb-switch-core/src/modules/minimax.rs'
t = io.open(p, encoding='utf-8').read()

# 1a) 结构体字段
old = '''    /// auth.json records 里含 oauth 的完整键（含 \\x00 后缀）
    pub auth_key: String,
}'''
new = '''    /// auth.json records 里含 oauth 的完整键（含 \\x00 后缀）
    pub auth_key: String,
    /// 最近一次探测的积分余额（credit/details 未过期求和；探测时写回）
    pub balance: Option<f64>,
    /// 同一轮探测的已消耗积分
    pub credit_consumed: Option<f64>,
    pub balance_at: u64,
}'''
assert old in t, 'struct anchor'
t = t.replace(old, new, 1)

# 1b) load_config 解析
old = '''                    source: a.get("source").and_then(Value::as_str).unwrap_or("").to_string(),
                    auth_path: a.get("auth_path").and_then(Value::as_str).unwrap_or("").to_string(),
                    auth_key: a.get("auth_key").and_then(Value::as_str).unwrap_or("").to_string(),
                })'''
new = '''                    source: a.get("source").and_then(Value::as_str).unwrap_or("").to_string(),
                    auth_path: a.get("auth_path").and_then(Value::as_str).unwrap_or("").to_string(),
                    auth_key: a.get("auth_key").and_then(Value::as_str).unwrap_or("").to_string(),
                    balance: a.get("balance").and_then(Value::as_f64),
                    credit_consumed: a.get("credit_consumed").and_then(Value::as_f64),
                    balance_at: a.get("balance_at").and_then(Value::as_u64).unwrap_or(0),
                })'''
assert old in t, 'load anchor'
t = t.replace(old, new, 1)

# 1c) save_config 序列化
old = '''                "source": a.source, "auth_path": a.auth_path, "auth_key": a.auth_key,
            })'''
new = '''                "source": a.source, "auth_path": a.auth_path, "auth_key": a.auth_key,
                "balance": a.balance, "credit_consumed": a.credit_consumed, "balance_at": a.balance_at,
            })'''
assert old in t, 'save anchor'
t = t.replace(old, new, 1)

# 1d) credits_remaining → credit_summary（明细：remaining/consumed/min_expire）
old = '''/// desktop 余额口径：credit/details 的 remaining_amount 求和（积分 30 天到期）。
pub async fn credits_remaining(acc: &MmAccount) -> Option<f64> {
    let v = cloud_get(acc, "/minimax-cloud/api/v1/credit/details", None).await.ok()?;
    // 注意：details 在响应顶层（不在 data 下）
    let arr = v.get("details").and_then(Value::as_array)?;
    let now = now_ms();
    let sum: f64 = arr
        .iter()
        .filter(|g| g.get("expire_at_ms").and_then(Value::as_i64).map(|e| e as u64 > now).unwrap_or(true))
        .filter_map(|g| {
            g.get("remaining_amount")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok())
        })
        .sum();
    Some(sum)
}'''
new = '''/// desktop 积分明细：credit/details（响应顶层 details[]，金额为字符串）。
/// 返回 (未过期剩余合计, 已消耗合计, 最早到期时间戳)。
pub async fn credit_summary(acc: &MmAccount) -> Option<(f64, f64, u64)> {
    let v = cloud_get(acc, "/minimax-cloud/api/v1/credit/details", None).await.ok()?;
    // 注意：details 在响应顶层（不在 data 下）
    let arr = v.get("details").and_then(Value::as_array)?;
    let now = now_ms();
    let mut remaining = 0.0f64;
    let mut consumed = 0.0f64;
    let mut min_expire = u64::MAX;
    let mut any = false;
    for g in arr {
        let expire = g.get("expire_at_ms").and_then(Value::as_i64).unwrap_or(0) as u64;
        let rem = g
            .get("remaining_amount")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let used = g
            .get("consumed_amount")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        consumed += used;
        if expire > now {
            remaining += rem;
            any = true;
        }
        if expire > 0 {
            min_expire = min_expire.min(expire);
        }
    }
    any.then_some((remaining, consumed, if min_expire == u64::MAX { 0 } else { min_expire }))
}

/// desktop 余额口径：credit/details 的 remaining_amount 求和（积分 30 天到期）。
pub async fn credits_remaining(acc: &MmAccount) -> Option<f64> {
    credit_summary(acc).await.map(|(r, _, _)| r)
}'''
assert old in t, 'credits anchor'
t = t.replace(old, new, 1)

io.open(p, 'w', encoding='utf-8').write(t)
print('minimax.rs ok')

# ── 2) api.rs：probe 写回 + status 暴露 ──
p = 'crates/wb-switch-server/src/api.rs'
t = io.open(p, encoding='utf-8').read()

old = '''    let balance = if status == 200 {
            // desktop 来源：余额口径 = credit/details 未过期积分求和
            if acc.source == "desktop" {
                minimax::credits_remaining(&acc).await
            } else {
                minimax::membership_balance(&acc).await.ok()
            }
        } else {
            None
        };'''
new = '''    let mut balance = None;
    let mut consumed_out = None;
    if status == 200 {
        if acc.source == "desktop" {
            // desktop：credit/details 明细（剩余+已消耗），写回账号供页面直显
            if let Some((remaining, consumed, _expire)) = minimax::credit_summary(&acc).await {
                balance = Some(remaining);
                consumed_out = Some(consumed);
                let mut cfg2 = minimax::load_config();
                if let Some(a) = cfg2.accounts.iter_mut().find(|x| x.id == acc.id) {
                    a.balance = Some(remaining);
                    a.credit_consumed = Some(consumed);
                    a.balance_at = minimax::now_ms();
                }
                minimax::save_config(&cfg2);
            }
        } else {
            balance = minimax::membership_balance(&acc).await.ok();
        }
    }'''
assert old in t, 'probe balance anchor'
t = t.replace(old, new, 1)

old = '''    json_ok(json!({
        "ok": status == 200, "account": acc.label, "status": status,
        "detail": detail, "balance": balance,
        "latencyMs": started.elapsed().as_millis() as u64,
    }))'''
new = '''    json_ok(json!({
        "ok": status == 200, "account": acc.label, "status": status,
        "detail": detail, "balance": balance, "consumed": consumed_out,
        "latencyMs": started.elapsed().as_millis() as u64,
    }))'''
assert old in t, 'probe resp anchor'
t = t.replace(old, new, 1)

# status 暴露 balance/creditConsumed + 真实模型表
old = '''                "signinLastDate": a.signin_last_date, "signinPointsTotal": a.signin_points_total,
            })'''
new = '''                "signinLastDate": a.signin_last_date, "signinPointsTotal": a.signin_points_total,
                "balance": a.balance, "creditConsumed": a.credit_consumed, "balanceAt": a.balance_at,
            })'''
assert old in t, 'status accounts anchor'
t = t.replace(old, new, 1)

old = '''        "defaultModel": "MiniMax-M3",
        "models": ["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"],'''
new = '''        "defaultModel": "MiniMax-M3.1-Flash-Preview",
        "models": minimax::desktop_models().as_array().cloned().unwrap_or_default().iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>(),'''
assert old in t, 'status models anchor'
t = t.replace(old, new, 1)

io.open(p, 'w', encoding='utf-8').write(t)
print('api.rs ok')

# ── 3) accounts_add initializer 补新字段（否则编译失败）──
old = '''        source: String::new(),
        auth_path: String::new(),
        auth_key: String::new(),
    });
    if !cfg.enabled {
        cfg.enabled = true;
    }
    minimax::save_config(&cfg);
    json_ok(json!({ "ok": true, "id": id }))'''
new = '''        source: String::new(),
        auth_path: String::new(),
        auth_key: String::new(),
        balance: None,
        credit_consumed: None,
        balance_at: 0,
    });
    if !cfg.enabled {
        cfg.enabled = true;
    }
    minimax::save_config(&cfg);
    json_ok(json!({ "ok": true, "id": id }))'''
assert old in t, 'add initializer anchor'
t = t.replace(old, new, 1)
io.open(p, 'w', encoding='utf-8').write(t)
print('initializer ok')
