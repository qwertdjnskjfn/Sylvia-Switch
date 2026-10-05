# -*- coding: utf-8 -*-
"""一次性补丁：minimax.rs 桌面端账号信息升级（真实 deviceID/模型表/技能/同步）。"""
import io

p = 'crates/wb-switch-core/src/modules/minimax.rs'
t = io.open(p, encoding='utf-8').read()

# 1) desktop_shared + desktop_models 助手（挂在 desktop_shared_label 前）
old = 'fn desktop_shared_label(region_dir: &str) -> String {'
new = '''/// 桌面端 sharedUser（%APPDATA%/MiniMax/minimax-agent[-cn]-config.json）。
fn desktop_shared(region_dir: &str) -> Value {
    let base = std::env::var("APPDATA").unwrap_or_default();
    let cfg = if region_dir == "cn" {
        std::path::PathBuf::from(&base).join("MiniMax").join("minimax-agent-cn-config.json")
    } else {
        std::path::PathBuf::from(&base).join("MiniMax").join("minimax-agent-config.json")
    };
    std::fs::read_to_string(&cfg)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("sharedUser").cloned())
        .unwrap_or(json!({}))
}

/// 桌面端账号可用模型（~/.minimax/config.yaml 的 provider.minimax.models，
/// 粗解析：models: 块内 6 空格缩进键名；defaultModel 提到最前）。
pub fn desktop_models() -> Value {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let text = std::fs::read_to_string(
        std::path::PathBuf::from(home).join(".minimax").join("config.yaml"),
    )
    .unwrap_or_default();
    let mut models: Vec<String> = Vec::new();
    let mut in_models = false;
    for line in text.lines() {
        if line.trim_end() == "    models:" {
            in_models = true;
            continue;
        }
        if in_models {
            if line.starts_with("      ") && line.contains(':') && !line.trim_start().starts_with('-') {
                let name = line.trim().trim_end_matches(':').to_string();
                if !name.is_empty() && !models.contains(&name) {
                    models.push(name);
                }
            } else if !line.starts_with("      ") && !line.trim().is_empty() {
                in_models = false;
            }
        }
        if line.starts_with("defaultModel:") {
            let dm = line.split(':').nth(1).unwrap_or("").trim().to_string();
            if let Some(short) = dm.rsplit('/').next() {
                if !short.is_empty() && !models.contains(&short) {
                    models.insert(0, short.to_string());
                }
            }
        }
    }
    json!(models)
}

fn desktop_shared_label(region_dir: &str) -> String {'''
assert old in t, 'shared label anchor'
t = t.replace(old, new, 1)

# 2) discover 补 userName/deviceID/tokenExpired
old = '''        let label = desktop_shared_label(dir);
        out.push(json!({
            "region": region,
            "regionLabel": provider_label(region),
            "authPath": path.display().to_string(),
            "authKey": key,
            "label": label,
            "userId": label,
            "expiresAtMs": rec.get("expiresAtMs").cloned().unwrap_or(Value::Null),
        }));'''
new = '''        let shared = desktop_shared(dir);
        let label = shared
            .get("userName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push(json!({
            "region": region,
            "regionLabel": provider_label(region),
            "authPath": path.display().to_string(),
            "authKey": key,
            "label": label,
            "userId": shared.get("userID").cloned().unwrap_or(Value::Null),
            "realUserID": shared.get("realUserID").cloned().unwrap_or(Value::Null),
            "deviceID": shared.get("deviceID").cloned().unwrap_or(Value::Null),
            "expiresAtMs": rec.get("expiresAtMs").cloned().unwrap_or(Value::Null),
            "tokenExpired": rec
                .get("expiresAtMs")
                .and_then(Value::as_u64)
                .map(|e| e < now_ms())
                .unwrap_or(false),
        }));'''
assert old in t, 'discover anchor'
t = t.replace(old, new, 1)

# 3) capture 用真实 deviceID
old = '''            token: String::new(), // desktop 来源每次现读
            uuid: uuid::Uuid::new_v4().to_string(),
            device_id: format!("1{:07}", now_ms() % 10_000_000),
            user_id: d.get("userId").and_then(Value::as_str).unwrap_or_default().to_string(),'''
new = '''            token: String::new(), // desktop 来源每次现读
            uuid: uuid::Uuid::new_v4().to_string(),
            // 真实桌面端 deviceID（sharedUser，纯数字）——比本机生成更可信
            device_id: d
                .get("deviceID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            user_id: d.get("userId").and_then(Value::as_str).unwrap_or_default().to_string(),'''
assert old in t, 'capture device anchor'
t = t.replace(old, new, 1)

# 4) capture 末尾同步已有 desktop 账号
old = '''    if added > 0 && !cfg.enabled {
        cfg.enabled = true;
    }
    save_config(&cfg);
    json!({ "ok": true, "added": added, "skipped": skipped })
}'''
new = '''    // 同步已有 desktop 账号身份（userName/deviceID 随桌面端更新而变化）
    for a in cfg.accounts.iter_mut().filter(|a| a.source == "desktop") {
        let dir = if a.region == PROVIDER_ZAI { "en" } else { "cn" };
        let shared = desktop_shared(dir);
        if let Some(n) = shared.get("userName").and_then(Value::as_str) {
            if !n.is_empty() {
                a.label = format!("桌面端-{n}");
            }
        }
        if let Some(did) = shared.get("deviceID").and_then(Value::as_str) {
            if !did.is_empty() {
                a.device_id = did.to_string();
            }
        }
        if let Some(uid) = shared.get("userID").and_then(Value::as_str) {
            if !uid.is_empty() {
                a.user_id = uid.to_string();
            }
        }
    }
    if added > 0 && !cfg.enabled {
        cfg.enabled = true;
    }
    save_config(&cfg);
    json!({ "ok": true, "added": added, "skipped": skipped })
}

/// 桌面端账号信息离线聚合（不依赖 token）：身份 + 模型表 + 内置技能。
pub fn desktop_details() -> Value {
    let disc = discover_desktop();
    let skills: Vec<String> = std::fs::read_dir(
        std::path::PathBuf::from(
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_default(),
        )
        .join(".minimax")
        .join(".builtin-skills"),
    )
    .map(|rd| {
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .collect()
    })
    .unwrap_or_default();
    json!({
        "accounts": disc.get("accounts").cloned().unwrap_or(json!([])),
        "models": desktop_models(),
        "builtinSkills": skills,
        "note": "积分/签到面板需要有效 token（桌面端惰性续期：在桌面端做一次操作后自动可用）",
    })
}'''
assert old in t, 'sync anchor'
t = t.replace(old, new, 1)

io.open(p, 'w', encoding='utf-8').write(t)
print('minimax.rs upgraded')
