# -*- coding: utf-8 -*-
"""一次性补丁：minimax.rs 桌面端登录态只读接入（结构体/加载/现读 token/发现/导入）。"""
import io

p = 'crates/wb-switch-core/src/modules/minimax.rs'
t = io.open(p, encoding='utf-8').read()

# 1) 结构体字段
old = '''    /// 最近一次签到日期（YYYY-MM-DD），幂等用
    pub signin_last_date: String,
    pub signin_points_total: u64,
}'''
new = '''    /// 最近一次签到日期（YYYY-MM-DD），幂等用
    pub signin_last_date: String,
    pub signin_points_total: u64,
    /// 凭据来源：""=手动粘贴；"desktop"=桌面端 auth.json 只读共享
    pub source: String,
    /// source=desktop 时：auth.json 绝对路径（每次调用现读 accessToken，
    /// 桌面端自行续期后自动跟上；绝不代刷新——refresh 一次性轮换 +
    /// generation 跨进程锁，代刷新会把桌面端挤下线）
    pub auth_path: String,
    /// auth.json records 里含 oauth 的完整键（含 \\x00 后缀）
    pub auth_key: String,
}'''
assert old in t, 'struct anchor'
t = t.replace(old, new, 1)

# 2) load_config 解析
old = '''                    signin_last_date: a.get("signin_last_date").and_then(Value::as_str).unwrap_or("").to_string(),
                    signin_points_total: a.get("signin_points_total").and_then(Value::as_u64).unwrap_or(0),
                })'''
new = '''                    signin_last_date: a.get("signin_last_date").and_then(Value::as_str).unwrap_or("").to_string(),
                    signin_points_total: a.get("signin_points_total").and_then(Value::as_u64).unwrap_or(0),
                    source: a.get("source").and_then(Value::as_str).unwrap_or("").to_string(),
                    auth_path: a.get("auth_path").and_then(Value::as_str).unwrap_or("").to_string(),
                    auth_key: a.get("auth_key").and_then(Value::as_str).unwrap_or("").to_string(),
                })'''
assert old in t, 'load anchor'
t = t.replace(old, new, 1)

# 3) save_config 序列化
old = '''                "signin_last_date": a.signin_last_date, "signin_points_total": a.signin_points_total,
            })'''
new = '''                "signin_last_date": a.signin_last_date, "signin_points_total": a.signin_points_total,
                "source": a.source, "auth_path": a.auth_path, "auth_key": a.auth_key,
            })'''
assert old in t, 'save anchor'
t = t.replace(old, new, 1)

# 4) effective_token 助手
old = 'fn account_host(acc: &MmAccount) -> String {'
new = '''/// 账号当前生效 token：desktop 来源每次现读 auth.json（桌面端续期后自动跟上）；
/// 手动来源用存储值。auth.json 的 records 是对象，键含 \\x00 后缀——auth_key 为空
/// 时取第一个含 accessToken 的条目；读失败回落存储值。
pub fn effective_token(acc: &MmAccount) -> String {
    if acc.source == "desktop" && !acc.auth_path.is_empty() {
        if let Ok(text) = std::fs::read_to_string(&acc.auth_path) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(obj) = v.get("records").and_then(Value::as_object) {
                    let key = if acc.auth_key.is_empty() {
                        obj.keys().next().cloned().unwrap_or_default()
                    } else {
                        acc.auth_key.clone()
                    };
                    if let Some(tok) = obj
                        .get(&key)
                        .and_then(|r| r.get("accessToken"))
                        .and_then(Value::as_str)
                    {
                        return tok.to_string();
                    }
                    // 键不匹配（桌面端轮换后键名变化）：取首个含 accessToken 的条目
                    if let Some((_, r)) = obj.iter().find(|(_, r)| r.get("accessToken").and_then(Value::as_str).is_some()) {
                        if let Some(tok) = r.get("accessToken").and_then(Value::as_str) {
                            return tok.to_string();
                        }
                    }
                }
            }
        }
    }
    acc.token.clone()
}

fn account_host(acc: &MmAccount) -> String {'''
assert old in t, 'effective anchor'
t = t.replace(old, new, 1)

# 5) 调用点替换（4 处 acc.token 请求头）
n = t.count('.header("token", &acc.token)')
t = t.replace('.header("token", &acc.token)', '.header("token", effective_token(acc))')
print(f'token call sites replaced: {n}')

# 6) 桌面端发现与导入（挂在 normalize_region 之前）
old = "fn normalize_region(region: Option<&str>) -> Option<&'static str> {"
new = '''/// 扫描桌面端登录态（~/.minimax/auth/prod/{cn,en}/mcode-public/auth.json，只读）。
pub fn discover_desktop() -> Value {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    for (dir, region) in [("cn", "domestic"), ("en", "international")] {
        let path = std::path::PathBuf::from(&home)
            .join(".minimax")
            .join("auth")
            .join("prod")
            .join(dir)
            .join("mcode-public")
            .join("auth.json");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(recs) = v.get("records").and_then(Value::as_object) else { continue };
        let Some((key, rec)) = recs.iter().find(|(_, r)| r.get("accessToken").and_then(Value::as_str).is_some()) else {
            continue;
        };
        let label = desktop_shared_label(dir);
        out.push(json!({
            "region": region,
            "regionLabel": provider_label(region),
            "authPath": path.display().to_string(),
            "authKey": key,
            "label": label,
            "userId": label,
            "expiresAtMs": rec.get("expiresAtMs").cloned().unwrap_or(Value::Null),
        }));
    }
    json!({ "ok": !out.is_empty(), "accounts": out })
}

fn desktop_shared_label(region_dir: &str) -> String {
    // %APPDATA%/MiniMax/minimax-agent[-cn]-config.json 的 sharedUser.userID
    let base = std::env::var("APPDATA").unwrap_or_default();
    let cfg = if region_dir == "cn" {
        std::path::PathBuf::from(&base).join("MiniMax").join("minimax-agent-cn-config.json")
    } else {
        std::path::PathBuf::from(&base).join("MiniMax").join("minimax-agent-config.json")
    };
    std::fs::read_to_string(&cfg)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            v.pointer("/sharedUser/userID")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| {
                    v.pointer("/sharedUser/realUserID")
                        .map(|x| x.to_string())
                })
        })
        .unwrap_or_else(|| "桌面端".to_string())
}

/// 从桌面端登录态导入账号（只读共享；不落 refresh token）。uuid/device_id
/// 本机生成且随账号持久化（格式合规：uuid v4 / 纯数字 device_id）。
pub fn capture_from_desktop() -> Value {
    let disc = discover_desktop();
    let Some(list) = disc.get("accounts").and_then(Value::as_array).cloned() else {
        return json!({ "ok": false, "error": "未找到桌面端登录态（~/.minimax/auth/prod/*/mcode-public/auth.json）" });
    };
    let mut cfg = load_config();
    let mut added = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    for d in &list {
        let path = d.get("authPath").and_then(Value::as_str).unwrap_or("").to_string();
        let key = d.get("authKey").and_then(Value::as_str).unwrap_or("").to_string();
        if cfg.accounts.iter().any(|a| a.auth_path == path && a.auth_key == key) {
            skipped.push(format!("{}（已在池）", d.get("label").and_then(Value::as_str).unwrap_or("?")));
            continue;
        }
        let region = d.get("region").and_then(Value::as_str).unwrap_or(PROVIDER_BIGMODEL).to_string();
        let id = gen_id();
        cfg.accounts.push(MmAccount {
            id: id.clone(),
            label: format!("桌面端-{}", d.get("label").and_then(Value::as_str).unwrap_or(region.as_str())),
            region,
            token: String::new(), // desktop 来源每次现读
            uuid: uuid::Uuid::new_v4().to_string(),
            device_id: format!("1{:07}", now_ms() % 10_000_000),
            user_id: d.get("userId").and_then(Value::as_str).unwrap_or_default().to_string(),
            agent_id: String::new(),
            screen_w: "1707".into(),
            screen_h: "1067".into(),
            base_url: String::new(),
            enabled: true,
            added_at: now_ms(),
            last_error: None,
            cooldown_until_ms: 0,
            signin_last_date: String::new(),
            signin_points_total: 0,
            source: "desktop".into(),
            auth_path: path,
            auth_key: key,
        });
        added += 1;
    }
    if added > 0 && !cfg.enabled {
        cfg.enabled = true;
    }
    save_config(&cfg);
    json!({ "ok": true, "added": added, "skipped": skipped })
}

fn normalize_region(region: Option<&str>) -> Option<&'static str> {'''
assert old in t, 'discover anchor'
t = t.replace(old, new, 1)

io.open(p, 'w', encoding='utf-8').write(t)
print('minimax.rs desktop integration done')
