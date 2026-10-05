//! ZCode 账号池渠道（契约 docs/zcode-channel-contract.md）。
//!
//! 把 ZCode（智谱 CLI 客户端）的登录身份做成可保存/切换的账号池：
//! - 登录态 = `{data_root}/.zcode/v2/credentials.json`（值多为 `enc:v1:` 加密）；
//! - `enc:v1:{nonce}.{tag}.{ct}`（URL_SAFE_NO_PAD）= AES-256-GCM，
//!   key = SHA256(secret)，secret = `zcode-credential-fallback:{platform}:{home}:{username}`
//!   （env ZCODE_CREDENTIAL_SECRET 可覆盖）——算法逆向自 pjpv/zcode-switch，
//!   2026-10-01 本机实测解密成功（zcodefeedbackclientid → fb_ UUID）；
//! - 切换 = 换写 credentials.json（原子写）+ 每账号独立 deviceMid（telemetry-state.json，
//!   防遥测关联）+ 删 coding-plan-cache.json（套餐缓存重置）；新代际
//!   （provider_config.json 存在）不碰 config.json；
//! - 额度三通道：BigModel monitor（按 plan api-key）+ ZaiBilling（按 zcodejwttoken
//!   或 provider_config start-plan apiKey），请求头全家桶逆向自 zcode-switch。
//!
//! 安全边界：credentials.json 是 ZCode 的命根子——只整体读出改写、原子落盘，
//! 从不重加密未触碰的键；ZCode 运行中切换默认拒绝（force 才杀进程）。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::modules::config::{atomic_write, now_ms, store_dir};

pub const CLIENT_VERSION: &str = "3.11.2";
const MONITOR_URL: &str = "https://open.bigmodel.cn/api/monitor/usage/quota/limit";
const ZAI_BILLING_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/balance";
const PLAN_KEY_SUFFIX: &str = ":api-key";
const RELAY_PASS_KEY: &str = "web-remote-control:external-relay:pass_hash";

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn abs_env(name: &str) -> Option<PathBuf> {
    let raw = std::env::var(name).ok()?;
    let p = PathBuf::from(raw.trim());
    (p.is_absolute()).then_some(p)
}

/// ZCode 数据根：ZCODE_SWITCH_DATA_ROOT → setting.json dataBaseDir →
/// ZCODE_DATA_BASE_DIR → home（契约 §0 顺序）。
pub fn data_root() -> PathBuf {
    if let Some(p) = abs_env("ZCODE_SWITCH_DATA_ROOT") {
        return p;
    }
    if let Ok(text) = std::fs::read_to_string(home_dir().join(".zcode").join("v2").join("setting.json")) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(d) = v.get("dataBaseDir").and_then(Value::as_str).map(|s| s.trim().to_string()) {
                if !d.is_empty() {
                    let p = PathBuf::from(&d);
                    if p.is_absolute() {
                        return p;
                    }
                }
            }
        }
    }
    if let Some(p) = abs_env("ZCODE_DATA_BASE_DIR") {
        return p;
    }
    home_dir()
}

pub fn credentials_file() -> PathBuf {
    data_root().join(".zcode").join("v2").join("credentials.json")
}

fn telemetry_file() -> PathBuf {
    data_root().join(".zcode").join("v2").join("telemetry-state.json")
}

fn plan_cache_file() -> PathBuf {
    data_root().join(".zcode").join("v2").join("coding-plan-cache.json")
}

fn new_gen() -> bool {
    data_root().join(".zcode").join("v2").join("provider_config.json").exists()
}

// ---------------------------------------------------------------------------
// enc:v1 加解密
// ---------------------------------------------------------------------------

const ENC_PREFIX: &str = "enc:v1:";

fn windows_username() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "unknown".to_string())
}

/// 凭据解密密钥（契约 §0）：env 覆盖 → `zcode-credential-fallback:win32:{home}:{username}`。
pub fn secret() -> String {
    if let Ok(s) = std::env::var("ZCODE_CREDENTIAL_SECRET") {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    format!(
        "zcode-credential-fallback:win32:{}:{}",
        home_dir().display(),
        windows_username()
    )
}

fn derive_key(secret: &str) -> [u8; 32] {
    let d = Sha256::digest(secret.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    out
}

pub fn is_encrypted(v: &str) -> bool {
    v.starts_with(ENC_PREFIX)
}

/// 解密 `enc:v1:{nonce}.{tag}.{ct}`（URL_SAFE_NO_PAD）。失败给中文人话。
pub fn decrypt_with_secret(value: &str, secret: &str) -> Result<String, String> {
    let body = value
        .strip_prefix(ENC_PREFIX)
        .ok_or_else(|| "不是 enc:v1 格式".to_string())?;
    let parts: Vec<&str> = body.split('.').collect();
    if parts.len() != 3 {
        return Err("enc:v1 格式不正确（应为 nonce.tag.ct 三段）".to_string());
    }
    let b64 = |s: &str| -> Result<Vec<u8>, String> {
        URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|e| format!("base64 解码失败: {e}"))
    };
    let nonce = b64(parts[0])?;
    let tag = b64(parts[1])?;
    let ct = b64(parts[2])?;
    if nonce.len() != 12 {
        return Err("nonce 长度异常（应为 12 字节）".to_string());
    }
    let key = derive_key(secret);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("密钥初始化失败: {e}"))?;
    let mut buf = ct;
    buf.extend_from_slice(&tag);
    let pt = cipher
        .decrypt(Nonce::from_slice(&nonce), buf.as_slice())
        .map_err(|_| "解密失败（密钥不匹配或数据损坏）".to_string())?;
    String::from_utf8(pt).map_err(|e| format!("明文非 UTF-8: {e}"))
}

/// 解密（明文值原样透传；失败返回 None）。
pub fn decrypt_opt(value: Option<&str>) -> Option<String> {
    let v = value?;
    if is_encrypted(v) {
        decrypt_with_secret(v, &secret()).ok()
    } else {
        Some(v.to_string())
    }
}

/// 加密（单测 round-trip 与 capture 重写用；GCM nonce 随机 12 字节）。
pub fn encrypt_with_secret(plain: &str, secret: &str) -> Result<String, String> {
    let key = derive_key(secret);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("密钥初始化失败: {e}"))?;
    let mut nonce_bytes = [0u8; 12];
    let _ = getrandom_fill(&mut nonce_bytes);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plain.as_bytes())
        .map_err(|e| format!("加密失败: {e}"))?;
    // GCM 密文末 16 字节是 tag；按 ZCode 的 nonce.tag.ct 三段拆开。
    let (ct_body, tag) = ct.split_at(ct.len() - 16);
    Ok(format!(
        "{}{}.{}.{}",
        ENC_PREFIX,
        URL_SAFE_NO_PAD.encode(nonce_bytes),
        URL_SAFE_NO_PAD.encode(tag),
        URL_SAFE_NO_PAD.encode(ct_body)
    ))
}

fn getrandom_fill(buf: &mut [u8]) -> Result<(), String> {
    // 轻量随机：优先 RtlGenRandom 语义太重，这里用时间+地址熵混合足够（单测用）。
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut seed = Sha256::digest(format!("{t}:{:p}", buf.as_ptr()).as_bytes());
    for (i, b) in buf.iter_mut().enumerate() {
        *b = seed[i % seed.len()];
        if i % seed.len() == seed.len() - 1 {
            seed = Sha256::digest(seed);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 身份解析
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ZcodeIdentity {
    pub provider: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub user_id: Option<String>,
    pub jwt_expired: bool,
}

impl ZcodeIdentity {
    pub fn label(&self) -> Option<String> {
        self.display_name
            .clone()
            .or_else(|| self.username.clone())
            .or_else(|| self.email.clone())
    }
}

fn decode_jwt_exp_ms(jwt: &str) -> Option<i64> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let exp = v.get("exp")?;
    exp.as_i64()
        .or_else(|| exp.as_f64().map(|f| f as i64))
        .map(|s| s.saturating_mul(1000))
}

/// 身份解析（契约 §0）：active_provider → user_info → JWT 兜底。
pub fn identity_of(creds: &Value) -> ZcodeIdentity {
    let mut id = ZcodeIdentity { provider: "bigmodel".into(), ..Default::default() };
    let Some(map) = creds.as_object() else { return id };
    let sec = secret();
    if let Some(ap) = map.get("oauth:active_provider").and_then(Value::as_str) {
        if let Some(p) = decrypt_opt(Some(ap)).filter(|p| !p.trim().is_empty()) {
            id.provider = p.trim().to_string();
        }
    }
    let ui_key = format!("oauth:{}:user_info", id.provider);
    if let Some(ui) = map.get(ui_key.as_str()).and_then(Value::as_str).and_then(|s| decrypt_opt(Some(s))) {
        if let Ok(v) = serde_json::from_str::<Value>(&ui) {
            id.username = v.get("username").and_then(Value::as_str).map(String::from);
            id.display_name = v.get("displayName").and_then(Value::as_str).map(String::from);
            id.email = v
                .get("email")
                .and_then(Value::as_str)
                .or_else(|| v.get("rawProfile").and_then(|r| r.get("email")).and_then(Value::as_str))
                .map(String::from);
            if let Some(uid) = v.get("id").filter(|x| !x.is_null()) {
                id.user_id = uid.as_str().map(String::from).or_else(|| serde_json::to_string(uid).ok());
            }
        }
    }
    let at_key = format!("oauth:{}:access_token", id.provider);
    if let Some(at) = map.get(at_key.as_str()).and_then(Value::as_str).and_then(|s| decrypt_opt(Some(s))) {
        if let Some(exp) = decode_jwt_exp_ms(&at) {
            id.jwt_expired = now_ms() >= exp;
            if id.user_id.is_none() {
                let payload = at.split('.').nth(1).unwrap_or("").trim_end_matches('=').to_string();
                if let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload.as_bytes()) {
                    if let Ok(jwt) = serde_json::from_slice::<Value>(&bytes) {
                        id.user_id = jwt
                            .get("user_id")
                            .or_else(|| jwt.get("sub"))
                            .and_then(Value::as_str)
                            .map(String::from);
                    }
                }
            }
        }
    }
    id
}

/// 身份信号是否足够（至少有 username/displayName/email/user_id 之一）。
fn identity_has_signal(i: &ZcodeIdentity) -> bool {
    i.user_id.as_deref().map(str::trim).is_some_and(|s| !s.is_empty())
        || i.username.as_deref().map(str::trim).is_some_and(|s| !s.is_empty())
        || i.email.as_deref().map(str::trim).is_some_and(|s| !s.is_empty())
}

/// 身份匹配（对齐 zcode-switch：provider 相同 && 任一信号相等）。
fn identity_matches(a: &ZcodeIdentity, b: &ZcodeIdentity) -> bool {
    if a.provider != b.provider {
        return false;
    }
    let eq = |x: &Option<String>, y: &Option<String>| {
        match (x, y) {
            (Some(x), Some(y)) => !x.trim().is_empty() && x.trim() == y.trim(),
            _ => false,
        }
    };
    eq(&a.user_id, &b.user_id) || eq(&a.username, &b.username) || eq(&a.email, &b.email)
}

/// canonical 指纹：剔除 device* 前缀键后的 JSON SHA256（契约 §0 去重口径）。
pub fn canonical_hash(v: &Value) -> String {
    let filtered = match v.as_object() {
        Some(map) if map.keys().any(|k| k.starts_with("device")) => {
            let kept: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(k, _)| !k.starts_with("device"))
                .map(|(k, val)| (k.clone(), val.clone()))
                .collect();
            Value::Object(kept)
        }
        _ => v.clone(),
    };
    let bytes = serde_json::to_vec(&filtered).unwrap_or_default();
    let d = Sha256::digest(&bytes);
    d.iter().map(|b| format!("{b:02x}")).collect::<String>().chars().take(16).collect()
}

// ---------------------------------------------------------------------------
// live 读写
// ---------------------------------------------------------------------------

pub fn live_credentials() -> Option<Value> {
    let text = std::fs::read_to_string(credentials_file()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn is_logged_in(creds: &Value) -> bool {
    let id = identity_of(creds);
    let has_token = creds
        .as_object()
        .map(|m| m.keys().any(|k| k.starts_with("oauth:") && k.ends_with(":access_token")))
        .unwrap_or(false);
    has_token || id.user_id.is_some()
}

fn desktop_running() -> bool {
    #[cfg(windows)]
    {
        let out = no_window_cmd("tasklist")
            .args(["/FI", "IMAGENAME eq ZCode.exe", "/FO", "CSV", "/NH"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.to_lowercase().starts_with("\"zcode.exe\"")),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
fn no_window_cmd(prog: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new(prog);
    c.creation_flags(0x0800_0000);
    c
}

fn kill_zcode() -> Result<bool, String> {
    #[cfg(windows)]
    {
        if !desktop_running() {
            return Ok(true);
        }
        let _ = no_window_cmd("taskkill").args(["/F", "/IM", "ZCode.exe"]).output();
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if !desktop_running() {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        Ok(false)
    }
    #[cfg(not(windows))]
    {
        Ok(true)
    }
}

fn zcode_launch_candidates() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from(r"C:\Program Files\ZCode\ZCode.exe")];
    if let Ok(l) = std::env::var("LOCALAPPDATA") {
        v.push(PathBuf::from(l).join("Programs").join("ZCode").join("ZCode.exe"));
    }
    v
}

/// 拉起 ZCode：走全链路径解析（env > claim-state 持久化 > 运行进程回溯 > 候选），
/// 成功返回实际启动的 exe 路径，失败返回带原因的错误（调用方把原因透传给前端）。
fn launch_zcode_detailed() -> Result<String, String> {
    let Some(exe) = resolve_zcode_exe() else {
        return Err(
            "未找到 ZCode.exe：已尝试 ZCODE_EXE 环境变量、历史安装路径、运行中进程回溯与默认安装位置。\
             凭据已写入，请手动启动 ZCode；或在 /zcode 页确认安装路径后重试"
                .to_string(),
        );
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new(&exe)
            .creation_flags(0x0000_0008 | 0x0000_0200)
            .spawn()
            .map_err(|e| format!("拉起 ZCode 失败（{exe}）: {e}"))?;
        Ok(exe)
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(&exe)
            .spawn()
            .map_err(|e| format!("拉起 ZCode 失败（{exe}）: {e}"))?;
        Ok(exe)
    }
}

// ---------------------------------------------------------------------------
// 设备身份虚拟化
// ---------------------------------------------------------------------------

fn ensure_virtual_device_mid(id: &str) -> Result<String, String> {
    let accounts_path = pool_file();
    let mut pool = read_pool(&accounts_path)?;
    let existing = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for acc in &existing {
        if acc.get("id").and_then(Value::as_str) == Some(id) {
            if let Some(m) = acc.get("virtualDeviceMid").and_then(Value::as_str) {
                if !m.trim().is_empty() {
                    return Ok(m.to_string());
                }
            }
        }
    }
    let mid = uuid::Uuid::new_v4().to_string();
    if let Some(arr) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for acc in arr.iter_mut() {
            if acc.get("id").and_then(Value::as_str) == Some(id) {
                acc["virtualDeviceMid"] = json!(mid);
                acc["updatedAt"] = json!(now_ms());
            }
        }
    }
    save_pool(&accounts_path, &pool)?;
    Ok(mid)
}

fn write_live_device_mid(mid: &str) -> Result<(), String> {
    let path = telemetry_file();
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if !v.is_object() {
        v = json!({});
    }
    if v.get("deviceMid").and_then(Value::as_str) == Some(mid) {
        return Ok(());
    }
    v["deviceMid"] = json!(mid);
    atomic_write(&path, &(serde_json::to_string(&v).unwrap_or_default() + "\n"))
        .map_err(|e| format!("写 telemetry-state 失败: {e}"))
}

// ---------------------------------------------------------------------------
// 账号池
// ---------------------------------------------------------------------------

const POOL_FILE: &str = "zcode_accounts.json";

fn pool_file() -> PathBuf {
    store_dir().join(POOL_FILE)
}

fn read_pool(path: &PathBuf) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).or(Ok(json!({"accounts": []}))),
        Err(_) => Ok(json!({"accounts": []})),
    }
}

fn save_pool(path: &PathBuf, pool: &Value) -> Result<(), String> {
    atomic_write(path, &serde_json::to_string_pretty(pool).unwrap_or_default())
        .map_err(|e| format!("写账号池失败: {e}"))
}

pub fn list_accounts() -> Vec<Value> {
    let pool = read_pool(&pool_file()).unwrap_or_else(|_| json!({"accounts": []}));
    pool.get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|acc| {
            let creds = acc.get("credentials").cloned().unwrap_or(json!({}));
            let id = identity_of(&creds);
            json!({
                "id": acc.get("id").cloned().unwrap_or(Value::Null),
                "name": acc.get("name").cloned().unwrap_or(Value::Null),
                "addedAt": acc.get("addedAt").cloned().unwrap_or(Value::Null),
                "updatedAt": acc.get("updatedAt").cloned().unwrap_or(Value::Null),
                "virtualDeviceMid": acc.get("virtualDeviceMid").cloned().unwrap_or(Value::Null),
                "proxy": acc.get("proxy").cloned().unwrap_or(json!("")),
                "identity": {
                    "provider": id.provider,
                    "label": id.label(),
                    "email": id.email,
                    "jwtExpired": id.jwt_expired,
                },
            })
        })
        .collect()
}

/// 捕获当前登录态入池（同 hash → 更新名字，不重复入库）。
pub fn capture_current(name: Option<&str>) -> Result<Value, String> {
    let Some(creds) = live_credentials() else {
        return Err("未找到 ZCode 登录态（credentials.json 不存在或不可读）。请先在 ZCode 中登录".to_string());
    };
    if !is_logged_in(&creds) {
        return Err("ZCode 当前未登录（credentials.json 缺少 access_token）。请先在 ZCode 中登录".to_string());
    }
    let hash = canonical_hash(&creds);
    let identity = identity_of(&creds);
    let default_name = identity
        .label()
        .or_else(|| identity.email.clone())
        .unwrap_or_else(|| format!("ZCode 账号 {}", hash.get(0..4).unwrap_or("")));
    let name = name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or(default_name);
    let path = pool_file();
    let mut pool = read_pool(&path)?;
    let arr = pool.as_object_mut().unwrap().entry("accounts").or_insert(json!([]));
    let accounts = arr.as_array_mut().ok_or("账号池格式损坏")?;
    let capture_identity = identity_of(&creds);
    for acc in accounts.iter_mut() {
        // 去重两级：①哈希完全一致；②身份一致（热切换注入中继密钥/上游更新无关键
        // 都会改变哈希——同身份重复捕获必须更新原条目而不是新增，否则池越切越多）。
        let same_hash = acc.get("hash").and_then(Value::as_str) == Some(hash.as_str());
        let existing_creds = acc.get("credentials").cloned().unwrap_or(json!({}));
        let same_identity = {
            let ei = identity_of(&existing_creds);
            identity_has_signal(&capture_identity)
                && identity_has_signal(&ei)
                && identity_matches(&ei, &capture_identity)
        };
        if same_hash || same_identity {
            acc["name"] = json!(name);
            acc["credentials"] = creds.clone();
            acc["hash"] = json!(hash);
            acc["updatedAt"] = json!(now_ms());
            return Ok(json!({"ok": true, "updated": true, "id": acc["id"].clone(), "name": name, "hash": hash}));
        }
    }
    let id = format!("zcode:{hash}");
    accounts.push(json!({
        "id": id,
        "name": name,
        "hash": hash,
        "credentials": creds,
        "virtualDeviceMid": Value::Null,
        "addedAt": now_ms(),
        "updatedAt": now_ms(),
    }));
    save_pool(&path, &pool)?;
    Ok(json!({"ok": true, "updated": false, "id": id, "name": name, "hash": hash}))
}

pub fn remove_account(id: &str) -> Result<(), String> {
    let path = pool_file();
    let mut pool = read_pool(&path)?;
    let before = pool
        .get("accounts")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    if let Some(obj) = pool.as_object_mut() {
        if let Some(arr) = obj.get_mut("accounts").and_then(Value::as_array_mut) {
            arr.retain(|acc| acc.get("id").and_then(Value::as_str) != Some(id));
        }
    }
    let after = pool
        .get("accounts")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    if before == after {
        return Err(format!("账号不存在: {id}"));
    }
    save_pool(&path, &pool)?;
    // 额度缓存条目随账号移除（避免孤儿缓存混入 status）。
    let mut cache = load_quota_cache();
    let removed = cache
        .as_object_mut()
        .map(|m| m.remove(id).is_some())
        .unwrap_or(false);
    if removed {
        let _ = atomic_write(&quota_cache_path(), &serde_json::to_string_pretty(&cache).unwrap_or_default());
    }
    Ok(())
}

/// 切换：ZCode 运行中且 !force → 拒绝；否则换写 credentials.json + deviceMid +
/// 删套餐缓存（+ 可选重启）。写入前自动把当前 live 保全入库（防丢号）。
pub fn switch_account(id: &str, force: bool, restart: bool, hot: bool) -> Result<Value, String> {
    let running = desktop_running();
    if running && hot {
        // ── 热切换（丝滑路径，逆向自 zcode-switch hot_swap_verified）：
        // 不杀进程，直接换写 credentials.json，双读校验确认 ZCode 没有回写，
        // 失败退避重试 3 次。项目/会话/设置全程不动，ZCode 下一次鉴权即用新号。
        let target = list_accounts()
            .into_iter()
            .find(|acc| acc.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| format!("账号不存在: {id}"))?;
        let target_creds = read_pool(&pool_file())?
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .find(|acc| acc.get("id").and_then(Value::as_str) == Some(id))
            .and_then(|acc| acc.get("credentials").cloned())
            .ok_or_else(|| format!("账号凭据缺失: {id}"))?;
        let target_hash = target.get("hash").and_then(Value::as_str).unwrap_or("").to_string();

        // 防丢号：当前 live 与目标不同时先保全入库。
        if let Some(live) = live_credentials() {
            if canonical_hash(&live) != target_hash {
                let _ = capture_current(None);
            }
        }
        // 中继密钥保活。
        let mut creds = target_creds.clone();
        if let Some(live) = live_credentials() {
            if let Some(relay) = live.get(RELAY_PASS_KEY) {
                if creds.get(RELAY_PASS_KEY).is_none() {
                    if let Some(obj) = creds.as_object_mut() {
                        obj.insert(RELAY_PASS_KEY.to_string(), relay.clone());
                    }
                }
            }
        }
        let payload = serde_json::to_string_pretty(&creds).unwrap_or_default();
        let written_hash = canonical_hash(&creds);
        // 校验基准对齐 zcode-switch：身份匹配优先（ZCode 回写时可能更新无关键，
        // 哈希必变但登录身份不变——只比哈希会假阴性）；身份无信号时哈希兜底。
        let want_identity = identity_of(&target_creds);
        let verify = || -> bool {
            let Some(v) = live_credentials() else { return false };
            let li = identity_of(&v);
            if identity_has_signal(&want_identity) {
                identity_has_signal(&li) && identity_matches(&li, &want_identity)
            } else {
                canonical_hash(&v) == written_hash
            }
        };
        let mut last_err = String::new();
        for attempt in 0..3u32 {
            atomic_write(&credentials_file(), &payload)
                .map_err(|e| format!("写 credentials.json 失败: {e}"))?;
            if verify() {
                std::thread::sleep(Duration::from_millis(150));
                if verify() {
                    let _ = std::fs::remove_file(plan_cache_file());
                    let mid = ensure_virtual_device_mid(id)?;
                    write_live_device_mid(&mid)?;
                    return Ok(json!({
                        "ok": true, "switched": true, "hot": true,
                        "killed": false, "launched": false, "deviceMid": mid,
                        "message": format!("已热切换到「{}」，ZCode 无需重启（下一次鉴权生效）",
                            target.get("name").and_then(Value::as_str).unwrap_or(id)),
                    }));
                }
            }
            last_err = "热切换校验失败：ZCode 进程回写了登录态".to_string();
            std::thread::sleep(Duration::from_millis(250 + u64::from(attempt) * 250));
        }
        return Err(format!(
            "{last_err}。请改用「强制冷切换」（会关闭 ZCode）后重试"
        ));
    }
    if running && !force {
        return Err(
            "ZCode 正在运行，直接覆盖登录态可能被进程回写覆盖。请使用「热切换」（不关闭 ZCode）或「强制冷切换」（将自动关闭 ZCode）"
                .to_string(),
        );
    }
    let target = list_accounts()
        .into_iter()
        .find(|acc| acc.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("账号不存在: {id}"))?;
    let target_creds = read_pool(&pool_file())?
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .find(|acc| acc.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|acc| acc.get("credentials").cloned())
        .ok_or_else(|| format!("账号凭据缺失: {id}"))?;

    let mut killed = false;
    if running {
        if !kill_zcode()? {
            return Err("ZCode 进程关闭超时（8s），请手动关闭后重试".to_string());
        }
        killed = true;
    }

    // 防丢号：当前 live（若有且与目标不同）先保全入库。
    if let Some(live) = live_credentials() {
        let live_hash = canonical_hash(&live);
        if live_hash != target.get("hash").and_then(Value::as_str).unwrap_or("") {
            let _ = capture_current(None);
        }
    }

    // 中继密钥保活：external-relay pass_hash 从当前 live 透传到目标。
    let mut creds = target_creds.clone();
    if let Some(live) = live_credentials() {
        if let Some(relay) = live.get(RELAY_PASS_KEY) {
            if creds.get(RELAY_PASS_KEY).is_none() {
                if let Some(obj) = creds.as_object_mut() {
                    obj.insert(RELAY_PASS_KEY.to_string(), relay.clone());
                }
            }
        }
    }
    atomic_write(&credentials_file(), &serde_json::to_string_pretty(&creds).unwrap_or_default())
        .map_err(|e| format!("写 credentials.json 失败: {e}"))?;

    // 套餐缓存重置 + 设备身份虚拟化。
    let _ = std::fs::remove_file(plan_cache_file());
    let mid = ensure_virtual_device_mid(id)?;
    write_live_device_mid(&mid)?;

    let mut launched = false;
    let mut launched_exe: Option<String> = None;
    let mut launch_error: Option<String> = None;
    if restart {
        match launch_zcode_detailed() {
            Ok(exe) => {
                launched = true;
                launched_exe = Some(exe);
            }
            Err(e) => launch_error = Some(e),
        }
    }

    Ok(json!({
        "ok": true,
        "switched": true,
        "killed": killed,
        "launched": launched,
        "launchedExe": launched_exe,
        "launchError": launch_error,
        "deviceMid": mid,
        "newGen": new_gen(),
        "message": if launched {
            format!("已切换到「{}」并自动拉起 ZCode（以该账号登录）", target.get("name").and_then(Value::as_str).unwrap_or(id))
        } else if let Some(le) = &launch_error {
            format!("凭据已切换到「{}」，但自动拉起 ZCode 失败：{}；请手动启动 ZCode 即以该账号登录", target.get("name").and_then(Value::as_str).unwrap_or(id), le)
        } else {
            format!("已切换到「{}」；打开 ZCode 即以新账号登录", target.get("name").and_then(Value::as_str).unwrap_or(id))
        }
    }))
}

// ---------------------------------------------------------------------------
// 额度（三通道中的两条；Monitor 按 plan api-key，ZaiBilling 按 JWT/start-plan key）
// ---------------------------------------------------------------------------

fn plan_api_keys(creds: &Value) -> Vec<String> {
    creds
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| k.ends_with(PLAN_KEY_SUFFIX))
                .filter_map(|(_, v)| decrypt_opt(v.as_str()))
                .filter(|k| !k.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn zai_billing_token(creds: &Value) -> Option<String> {
    // 实测（2026-10-02）：bigmodel 账号（start-plan）的 ZaiBilling 通道同样走
    // zcodejwttoken——zcode-switch 的 start-plan 分支在 active=bigmodel 时也取 JWT。
    creds
        .get("zcodejwttoken")
        .and_then(Value::as_str)
        .and_then(|s| decrypt_opt(Some(s)))
        .filter(|s| !s.is_empty())
}

fn zai_headers(token: &str, mid: Option<&str>) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = vec![
        ("User-Agent".into(), format!("ZCode/{CLIENT_VERSION}")),
        ("HTTP-Referer".into(), "https://zcode.z.ai".into()),
        ("X-Title".into(), "Z Code@electron".into()),
        ("X-ZCode-App-Version".into(), CLIENT_VERSION.into()),
        ("X-Platform".into(), "win32-x64".into()),
        ("X-Release-Channel".into(), "stable".into()),
        ("X-Client-Language".into(), "zh-CN".into()),
        ("X-Client-Timezone".into(), "Asia/Shanghai".into()),
        ("X-Os-Category".into(), "windows".into()),
        (
            "Authorization".into(),
            format!("Bearer {token}"),
        ),
        ("x-request-id".into(), uuid::Uuid::new_v4().to_string()),
    ];
    if let Some(mid) = mid {
        h.push(("X-Device-Mid".into(), mid.to_string()));
    }
    h
}

async fn http_get_json(
    url: &str,
    headers: &[(String, String)],
    proxy: Option<&str>,
) -> Result<Value, String> {
    // 代理是 ClientBuilder 级设置（reqwest 无 per-request proxy）；
    // 额度/领取调用频率低（日频），按调用构建客户端开销可忽略。
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(20));
    if let Some(px) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
        builder = builder.proxy(
            reqwest::Proxy::all(px).map_err(|e| format!("代理配置无效 ({px}): {e}"))?,
        );
    }
    let client = builder.build().unwrap_or_default();
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req.send().await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => Ok(v),
                Err(_) => Err(format!("HTTP {status}: 响应非 JSON（{} 字符）", text.len())),
            }
        }
        Err(e) => Err(format!("请求失败: {e}")),
    }
}

/// 单账号额度（契约 §0 两条通道合并）。
pub async fn quota_for(creds: &Value) -> Value {
    let mut channels: Vec<Value> = Vec::new();
    // X-Device-Mid 必带（实测缺失→上游 code 3001 parameter error）；
    // 来源=telemetry-state.json 的 deviceMid（ZCode 运行时身份）。
    let mid = std::fs::read_to_string(telemetry_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("deviceMid").and_then(Value::as_str).map(String::from));
    let keys = plan_api_keys(creds);
    let plan_key_count = keys.len();

    // 两通道并发（实测串行时 Monitor 无套餐也白等一次，总额 10s+；
    // 并发后整体耗时≈单通道 3-5s）。Monitor 对无套餐账号的业务拒绝
    // （code 500「不存在coding plan」）静默跳过，不占 error。
    let monitor_futs = keys.iter().map(|key| async move {
        match http_get_json(MONITOR_URL, &bigmodel_headers(key), None).await {
            Ok(v) => match normalize_monitor(&v) {
                Some(items) => Some(json!({
                    "name": "BigModel Coding Plan",
                    "available": sum_items_remaining(&items),
                    "items": items,
                })),
                None => {
                    // 业务层无套餐/结构不符：静默（avoid 空通道噪音）。
                    None
                }
            },
            Err(e) => Some(json!({"name": "BigModel Coding Plan", "error": e})),
        }
    });
    let zai_fut = async {
        match zai_billing_token(creds) {
            Some(token) => {
                match http_get_json(ZAI_BILLING_URL, &zai_headers(&token, mid.as_deref()), None).await {
                    Ok(v) => match normalize_zai_billing(&v) {
                        Some(items) => Some(json!({
                            "name": "Z.ai Coding Plan",
                            "available": sum_items_remaining(&items),
                            "items": items,
                        })),
                        None => None,
                    },
                    Err(e) => Some(json!({"name": "Z.ai Coding Plan", "error": e})),
                }
            }
            None => None,
        }
    };
    let (monitor_results, zai_result) = futures_util::future::join(
        futures_util::future::join_all(monitor_futs),
        zai_fut,
    )
    .await;
    for ch in monitor_results.into_iter().flatten() {
        channels.push(ch);
    }
    if let Some(ch) = zai_result {
        channels.push(ch);
    }

    json!({
        "ok": !channels.is_empty() && channels.iter().any(|c| c.get("items").is_some()),
        "channels": channels,
        "diag": {
            "planKeyCount": plan_key_count,
            "activeProvider": creds.get("oauth:active_provider").and_then(Value::as_str).map(|s| s.chars().take(6).collect::<String>()),
            "hasJwt": creds.get("zcodejwttoken").is_some(),
            "secretPrefix": secret().chars().take(40).collect::<String>(),
            "zaiChannels": channels.iter().filter(|c| c.get("name").and_then(Value::as_str) == Some("Z.ai Coding Plan")).count(),
        },
    })
}

fn bigmodel_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("User-Agent".into(), format!("ZCode/{CLIENT_VERSION}")),
        ("x-request-id".into(), uuid::Uuid::new_v4().to_string()),
    ]
}

/// monitor 接口 → [{name,total,used,remaining,percentUsed,periodEnd}]（结构容错）。
fn normalize_monitor(v: &Value) -> Option<Vec<Value>> {
    let arr = v
        .get("data")
        .and_then(|d| d.get("items").or_else(|| d.get("list")).or_else(|| d.get("usage")))
        .and_then(Value::as_array)
        .or_else(|| v.get("items").and_then(Value::as_array))?;
    let items: Vec<Value> = arr
        .iter()
        .map(|it| {
            json!({
                "name": it.get("name").or_else(|| it.get("model")).cloned().unwrap_or(json!("套餐")),
                "total": it.get("total").cloned().unwrap_or(Value::Null),
                "used": it.get("used").cloned().unwrap_or(Value::Null),
                "remaining": it.get("remaining").cloned().unwrap_or(Value::Null),
                "percentUsed": it.get("percentUsed").or_else(|| it.get("percentage")).cloned().unwrap_or(Value::Null),
                "periodEnd": it.get("periodEnd").or_else(|| it.get("resetTime")).cloned().unwrap_or(Value::Null),
                "unit": it.get("unit").cloned().unwrap_or(json!("tokens")),
            })
        })
        .collect();
    (!items.is_empty()).then_some(items)
}

/// 数值 → 展示串（整数去尾零，其余两位小数）。
fn fmt_num(f: f64) -> String {
    if (f - f.round()).abs() < f64::EPSILON {
        format!("{}", f.round() as i64)
    } else {
        format!("{f:.2}")
    }
}

/// 通道 items 的 remaining 求和 → 通道级 available（前端大数字来源）。
fn sum_items_remaining(items: &[Value]) -> String {
    let sum: f64 = items
        .iter()
        .filter_map(|it| json_num(it.get("remaining")))
        .sum();
    fmt_num(sum)
}

/// Value 数字/字符串 → f64（容错）。
fn json_num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|x| {
        x.as_f64()
            .or_else(|| x.as_str().and_then(|s| s.replace(',', "").trim().parse::<f64>().ok()))
    })
}

/// zai billing balance → [{name,remaining,total,unit,expireAt}]（结构容错）。
fn normalize_zai_billing(v: &Value) -> Option<Vec<Value>> {
    // 2026-10-02 实测结构：data.plans[]（plan_id→name）+ data.balances[]
    // （show_name/total_units/used_units/remaining_units/expires_at，单位 tokens）。
    let data = v.get("data").unwrap_or(v);
    let plan_names: std::collections::HashMap<String, String> = data
        .get("plans")
        .and_then(Value::as_array)
        .map(|plans| {
            plans
                .iter()
                .filter_map(|p| {
                    let id = p.get("plan_id").and_then(Value::as_str)?.to_string();
                    let name = p
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("Coding Plan")
                        .to_string();
                    Some((id, name))
                })
                .collect()
        })
        .unwrap_or_default();
    let buckets = data.get("balances").and_then(Value::as_array)?;
    let items: Vec<Value> = buckets
        .iter()
        .map(|b| {
            let plan_id = b.get("plan_id").and_then(Value::as_str).unwrap_or("");
            let plan_name = plan_names.get(plan_id).cloned().unwrap_or_else(|| "Coding Plan".into());
            let model = b.get("show_name").and_then(Value::as_str).unwrap_or("");
            let total = json_num(b.get("total_units")).unwrap_or(0.0);
            let used = json_num(b.get("used_units")).unwrap_or(0.0);
            let remaining = json_num(b.get("remaining_units")).unwrap_or((total - used).max(0.0));
            json!({
                "name": if model.is_empty() { plan_name.clone() } else { format!("{plan_name} · {model}") },
                "remaining": fmt_num(remaining),
                "total": fmt_num(total),
                "used": fmt_num(used),
                "unit": "tokens",
                "expireAt": b.get("expires_at").and_then(Value::as_i64),
                "periodEnd": b.get("period_end").and_then(Value::as_i64),
            })
        })
        .collect();
    (!items.is_empty()).then_some(items)
}

/// 状态聚合（GET /api/zcode/status 的数据层）。
pub fn status() -> Value {
    let live = live_credentials();
    let identity = live.as_ref().map(identity_of);
    let logged_in = live.as_ref().map(is_logged_in).unwrap_or(false);
    let mut accounts = list_accounts();
    // 附额度缓存（无网络调用——缓存由 /api/zcode/quota 查询时落盘）。
    let cache = load_quota_cache();
    for acc in accounts.iter_mut() {
        if let Some(id) = acc.get("id").and_then(Value::as_str) {
            if let Some(entry) = cache.get(id) {
                acc["quota"] = entry.get("quota").cloned().unwrap_or(Value::Null);
                acc["quotaFetchedAt"] = entry.get("fetchedAt").cloned().unwrap_or(Value::Null);
            }
        }
    }
    json!({
        "ok": true,
        "liveExists": live.is_some(),
        "loggedIn": logged_in,
        "autoSwitch": auto_switch_state(),
        "desktopRunning": desktop_running(),
        "newGen": new_gen(),
        "credentialsFile": credentials_file().display().to_string(),
        "identity": match &identity {
            Some(i) => json!({
                "provider": i.provider,
                "label": i.label(),
                "displayName": i.display_name,
                "username": i.username,
                "email": if i.email.as_deref().map(str::is_empty).unwrap_or(true) { Value::Null } else { json!(i.email) },
                "userId": i.user_id,
                "jwtExpired": i.jwt_expired,
            }),
            None => Value::Null,
        },
        "accounts": accounts,
        "activeHash": live.as_ref().map(canonical_hash),
        "liveQuotaCache": quota_cache_entry("zcode:live"),
    })
}

/// 账号出站代理（一号一 IP：覆盖 IP 维度的限领/风控；空=直连）。
pub fn account_proxy(id: &str) -> Option<String> {
    let pool = read_pool(&pool_file()).ok()?;
    pool.get("accounts")
        .and_then(Value::as_array)?
        .iter()
        .find(|acc| acc.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|acc| {
            acc.get("proxy")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
}

/// 更新池账号（name/proxy 可选；不存在报错）。
pub fn update_account(id: &str, name: Option<&str>, proxy: Option<&str>) -> Result<(), String> {
    let path = pool_file();
    let mut pool = read_pool(&path)?;
    let mut found = false;
    if let Some(arr) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for acc in arr.iter_mut() {
            if acc.get("id").and_then(Value::as_str) == Some(id) {
                if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
                    acc["name"] = json!(n);
                }
                if let Some(px) = proxy {
                    acc["proxy"] = json!(px.trim());
                }
                acc["updatedAt"] = json!(now_ms());
                found = true;
                break;
            }
        }
    }
    if !found {
        return Err(format!("账号不存在: {id}"));
    }
    save_pool(&path, &pool)
}

// ---------------------------------------------------------------------------
// 额度缓存（~/.wb-switch/zcode_quota_cache.json）：
// 每次查询落盘，status 直接回缓存——前端跨路由/重进组件不丢、不重打上游。
// 条目：{ id: { quota: <通道payload>, fetchedAt: ms } }；live 用 "zcode:live"。
// ---------------------------------------------------------------------------

const QUOTA_CACHE_FILE: &str = "zcode_quota_cache.json";

fn quota_cache_path() -> PathBuf {
    store_dir().join(QUOTA_CACHE_FILE)
}

pub fn load_quota_cache() -> Value {
    std::fs::read_to_string(quota_cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

pub fn quota_cache_entry(id: &str) -> Option<Value> {
    load_quota_cache().get(id).cloned()
}

pub fn save_quota_cache_entry(id: &str, quota: &Value) {
    let mut cache = load_quota_cache();
    if !cache.is_object() {
        cache = json!({});
    }
    cache[id] = json!({ "quota": quota, "fetchedAt": now_ms() });
    let _ = atomic_write(&quota_cache_path(), &serde_json::to_string_pretty(&cache).unwrap_or_default());
}

/// 账号自身的虚拟设备身份（上游按 X-Device-Mid 识别设备并限领——每账号必须
/// 用自己的 virtualDeviceMid，全用 telemetry 的 mid 会被判定同设备限领 2 号）。
pub fn account_mid(id: &str) -> Option<String> {
    let pool = read_pool(&pool_file()).ok()?;
    pool.get("accounts")
        .and_then(Value::as_array)?
        .iter()
        .find(|acc| acc.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|acc| {
            acc.get("virtualDeviceMid")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
}

/// 池账号原始凭据（api 层实时额度用；list 只回摘要不泄露 token）。
pub fn pool_credentials(id: &str) -> Option<Value> {
    let pool = read_pool(&pool_file()).ok()?;
    pool.get("accounts")
        .and_then(Value::as_array)?
        .iter()
        .find(|acc| acc.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|acc| acc.get("credentials").cloned())
}

// ---------------------------------------------------------------------------
// 单测（纯函数；不依赖网络/真实文件）
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
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
        map.retain(|_, until| until.as_i64().unwrap_or(0) > now);
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
            .filter(|e| now.saturating_sub(e.get("fetchedAt").and_then(Value::as_i64).unwrap_or(0)) < 30 * 60 * 1000)
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

mod tests {
    use super::*;

    #[test]
    fn enc_roundtrip_and_tamper() {
        let sec = "zcode-credential-fallback:win32:C:\\Users\\test:tester";
        let plain = "fb_301d4868-7b14-434d-b07f-4cfca6e028fa";
        let enc = encrypt_with_secret(plain, sec).unwrap();
        assert!(enc.starts_with(ENC_PREFIX));
        assert_eq!(decrypt_with_secret(&enc, sec).unwrap(), plain);
        // 错密钥必须失败。
        assert!(decrypt_with_secret(&enc, "wrong-secret").is_err());
    }

    #[test]
    fn secret_derivation_matches_verified_shape() {
        // 2026-10-01 本机实测：zcode-credential-fallback:win32:{home}:{username}
        // 可解密真实 credentials.json（zcodefeedbackclientid → fb_ UUID）。
        let s = secret();
        assert!(s.starts_with("zcode-credential-fallback:win32:"));
        assert!(s.contains(':'));
    }

    #[test]
    fn identity_parses_provider_userinfo_and_jwt() {
        let creds = json!({
            "oauth:active_provider": "bigmodel",
            "oauth:bigmodel:user_info": serde_json::to_string(&json!({
                "username": "tester", "displayName": "测 试", "email": "t@x.com", "id": "u1"
            })).unwrap(),
            "oauth:bigmodel:access_token": "eyJhbGciOiJub25lIn0.eyJleHAiOjkNDY1NDAwMH0.",
        });
        let id = identity_of(&creds);
        assert_eq!(id.provider, "bigmodel");
        assert_eq!(id.username.as_deref(), Some("tester"));
        assert_eq!(id.email.as_deref(), Some("t@x.com"));
    }

    #[test]
    fn canonical_hash_ignores_device_keys() {
        let a = json!({"oauth:bigmodel:access_token": "x", "deviceName": "pc1"});
        let b = json!({"oauth:bigmodel:access_token": "x", "deviceName": "pc2"});
        assert_eq!(canonical_hash(&a), canonical_hash(&b), "device* 键不参与指纹");
        let c = json!({"oauth:bigmodel:access_token": "y"});
        assert_ne!(canonical_hash(&a), canonical_hash(&c));
    }

    #[test]
    fn normalize_helpers_are_tolerant() {
        assert!(normalize_monitor(&json!({"data": {"items": []}})).is_none());
        assert!(normalize_zai_billing(&json!({"data": {"plans": [{"plan_id": "p1", "name": "Start"}], "balances": [{"plan_id": "p1", "show_name": "GLM-5.3-Flash", "total_units": 100, "used_units": 20, "remaining_units": 80}]}})).is_some());
        assert!(normalize_zai_billing(&json!({"data": {"plans": [], "balances": []}})).is_none());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// 本机真实凭据诊断（非 CI 用）：确认 plan_api_keys/identity 在真实文件上工作。
    #[test]
    fn live_plan_keys_diagnostic() {
        let Some(creds) = live_credentials() else {
            eprintln!("[diag] live credentials 不存在");
            return;
        };
        let keys = plan_api_keys(&creds);
        eprintln!("[diag] plan_api_keys 数量: {}", keys.len());
        for k in &keys {
            eprintln!("[diag] key prefix: {}", &k[..k.len().min(12)]);
        }
        eprintln!("[diag] zai_token: {:?}", zai_billing_token(&creds).map(|t| t.len()));
        eprintln!("[diag] identity label: {:?}", identity_of(&creds).label());
        assert!(!keys.is_empty(), "plan_api_keys 不应为空");
    }
}

#[cfg(test)]
mod live_tests2 {
    use super::*;

    /// 定位 zcodejwttoken 解密失败的真实错误（本机诊断）。
    #[test]
    fn live_jwt_decrypt_diagnostic() {
        let creds = live_credentials().expect("live");
        let v = creds.get("zcodejwttoken").and_then(Value::as_str).expect("键存在");
        eprintln!("[diag] is_encrypted: {}", is_encrypted(v));
        eprintln!("[diag] secret 长度: {}", secret().len());
        match decrypt_with_secret(v, &secret()) {
            Ok(p) => eprintln!("[diag] 解密成功，明文长度: {}", p.len()),
            Err(e) => eprintln!("[diag] 解密失败: {e}"),
        }
        // 对照：user_info（键随 active_provider 变化，缺键跳过）
        let provider = creds
            .get("oauth:active_provider")
            .and_then(Value::as_str)
            .and_then(|s| decrypt_opt(Some(s)))
            .unwrap_or_else(|| "bigmodel".into());
        let ui_key = format!("oauth:{provider}:user_info");
        match creds.get(ui_key.as_str()).and_then(Value::as_str) {
            Some(ui) => match decrypt_with_secret(ui, &secret()) {
                Ok(p) => eprintln!("[diag] user_info({provider}) 解密成功，长度: {}", p.len()),
                Err(e) => eprintln!("[diag] user_info 解密失败: {e}"),
            },
            None => eprintln!("[diag] 无 {ui_key} 键（跳过对照）"),
        }
    }
}

// ---------------------------------------------------------------------------
// 活动领取（billing/preview + billing/claim；协议逆向自 pjpv/zcode-switch claim.rs）
// 每日可领套餐（如 ZCode Trust Build · GLM-5.3-Flash 1 亿 tokens/日）。
// claim 必须带 X-Aliyun-Captcha-Verify-Param（阿里云验证码，前端无痕验证/滑块产出）。
// ---------------------------------------------------------------------------

const BILLING_PREVIEW_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/preview";
const BILLING_CLAIM_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/claim";
const CLIENT_CONFIGS_URL: &str = "https://zcode.z.ai/api/v1/client/configs";

/// 领取 token：优先 zcodejwttoken（>20 字符），兜底 start-plan key。
fn claim_token(creds: &Value) -> Result<String, String> {
    if let Some(jwt) = creds
        .get("zcodejwttoken")
        .and_then(Value::as_str)
        .and_then(|s| decrypt_opt(Some(s)))
        .filter(|t| t.trim().len() > 20)
    {
        return Ok(jwt);
    }
    if let Some(t) = zai_billing_token(creds) {
        return Ok(t);
    }
    Err("该账号没有可用的领取凭证（zcodejwttoken 缺失）。请先在 ZCode 中登录".to_string())
}

/// 验证码配置（client/configs → data.configs.captcha）。
pub async fn claim_captcha_config() -> Result<Value, String> {
    let creds = live_credentials().ok_or("未找到 ZCode 登录态")?;
    let token = claim_token(&creds)?;
    let v = http_get_json(CLIENT_CONFIGS_URL, &zai_headers(&token, None), None).await?;
    if v.get("code").and_then(Value::as_i64) != Some(0) {
        return Err("验证码配置不可用（client/configs 返回非 0）".to_string());
    }
    let c = v
        .pointer("/data/configs/captcha")
        .cloned()
        .unwrap_or(Value::Null);
    let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(json!({
        "enabled": c.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        "region": s("region"),
        "prefix": s("prefix"),
        "sceneId": s("sceneId"),
    }))
}

fn claim_failure_message(code: i64, body: &Value) -> String {
    let server = body
        .get("msg")
        .or_else(|| body.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    // 错误码表校准（2026-10-05 对齐 thelabcorner/zcode-claim-bot zcode-api.mjs——
    // 旧表 1002/1003/1004/1005 全错，导致「已领取」被报成「频繁」等状态误判）：
    //   1001=plan not found  1002=offer ended/unavailable  1003=already claimed
    //   1004=not eligible    1005=daily quota exhausted(波次回补)  3001=参数
    //   3007=captcha failed  401=登录失效
    let base = match code {
        1001 => "活动不存在",
        1002 => "名额已结束或下线",
        1003 => "该账号已领取过",
        1004 => "账号/客户端资格不符",
        1005 => "本波名额已抢空（等回补）",
        3001 => "参数错误",
        3007 => "验证码校验失败",
        401 => "登录态失效，请重新登录 ZCode",
        _ => "领取失败",
    };
    if server.is_empty() {
        base.to_string()
    } else {
        format!("{base}：{server}")
    }
}

/// 从上游响应体提取业务码（领取引擎状态分类用）。
fn claim_code_of(body: &Value) -> i64 {
    body.get("code").and_then(Value::as_i64).unwrap_or(-1)
}

/// 可领套餐解析（entitlements 过滤 meter=model_usage & unit_type=token）。
fn parse_claim_plan(p: &Value) -> Option<Value> {
    let plan_id = p
        .get("plan_id")
        .or_else(|| p.get("planId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let grants: Vec<Value> = p
        .get("entitlements")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|e| {
                    e.get("meter").and_then(Value::as_str) == Some("model_usage")
                        && e.get("unit_type").and_then(Value::as_str) == Some("token")
                        && e.get("show_name")
                            .and_then(Value::as_str)
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                })
                .map(|e| {
                    let units = json_num(e.get("grant_units")).unwrap_or(0.0);
                    json!({
                        "name": e.get("show_name").and_then(Value::as_str).unwrap_or(""),
                        "units": units,
                        "period": e.get("period").and_then(Value::as_str).unwrap_or("one_time"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(json!({
        "planId": plan_id,
        "name": p.get("name").and_then(Value::as_str).unwrap_or("").trim(),
        "description": p.get("description").and_then(Value::as_str).unwrap_or("").trim(),
        "priority": p.get("priority").and_then(Value::as_i64).unwrap_or(0),
        "grants": grants,
    }))
}

/// 可领套餐列表（billing/preview；无需验证码）。
/// mid=账号自身虚拟设备身份（设备维度）；proxy=账号出站代理（IP 维度）。
pub async fn claim_preview_for(
    creds: &Value,
    mid: Option<String>,
    proxy: Option<&str>,
) -> Result<Vec<Value>, String> {
    let token = claim_token(creds)?;
    let url = format!("{BILLING_PREVIEW_URL}?app_version={CLIENT_VERSION}&platform=win32-x64");
    let v = http_get_json(&url, &zai_headers(&token, mid.as_deref()), proxy).await?;
    let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        return Err(claim_failure_message(code, &v));
    }
    let plans = v
        .pointer("/data/plans")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(parse_claim_plan)
        .collect::<Vec<_>>();
    Ok(plans)
}

/// 提交领取（billing/claim；必须带阿里云验证码参数）。
/// 错误 = (业务码, 人话)；code 供领取引擎做状态分类（终态/退避/成功）。
pub async fn claim_submit_for(
    creds: &Value,
    plan_id: &str,
    captcha_param: &str,
    captcha_region: Option<&str>,
    mid: Option<String>,
    proxy: Option<&str>,
) -> Result<Value, (i64, String)> {
    if captcha_param.trim().is_empty() {
        return Err((-1, "缺少验证码参数（请先完成验证）".to_string()));
    }
    let token = claim_token(creds).map_err(|e| (-1, e))?;
    let mut headers = zai_headers(&token, mid.as_deref());
    headers.push(("X-Aliyun-Captcha-Verify-Param".into(), captcha_param.trim().to_string()));
    if let Some(r) = captcha_region.map(str::trim).filter(|s| !s.is_empty()) {
        headers.push(("X-Aliyun-Captcha-Verify-Region".into(), r.to_string()));
    }
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(25));
    if let Some(px) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
        builder = builder.proxy(
            reqwest::Proxy::all(px).map_err(|e| (-1, format!("代理配置无效 ({px}): {e}")))?,
        );
    }
    let client = builder.build().unwrap_or_default();
    let mut req = client.post(BILLING_CLAIM_URL);
    for (k, v) in &headers {
        req = req.header(k, v);
    }
    let resp = req
        .json(&json!({ "plan_id": plan_id }))
        .send()
        .await
        .map_err(|e| (-1, format!("领取请求失败: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text)
        .unwrap_or_else(|_| json!({"code": -1, "msg": format!("HTTP {status}: 非 JSON 响应")}));
    let code = claim_code_of(&v);
    if code != 0 {
        return Err((code, claim_failure_message(code, &v)));
    }
    Ok(v)
}

pub fn telemetry_device_mid() -> Option<String> {
    std::fs::read_to_string(telemetry_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("deviceMid").and_then(Value::as_str).map(String::from))
}

/// 验证码铸造自检（不消耗任何资格）：拉配置 → ensure_cdp_ready（按需自动重启
/// ZCode 开调试口）→ CDP 注入阿里云 SDK 无痕验证 → 返回参数长度与耗时。
/// 用于在不依赖每日名额的情况下验证「CDP mint → claim」的险路全通。
pub async fn captcha_mint_selftest() -> Result<Value, String> {
    let cfg = claim_captcha_config().await?;
    let t0 = Instant::now();
    ensure_cdp_ready(true).await?;
    let cdp_ready_ms = (Instant::now() - t0).as_millis() as u64;
    let param = mint_captcha_via_cdp(&cfg).await?;
    let total_ms = (Instant::now() - t0).as_millis() as u64;
    Ok(json!({
        "ok": true,
        "paramLen": param.chars().count(),
        "cdpReadyMs": cdp_ready_ms,
        "totalMs": total_ms,
        "sceneId": cfg.get("sceneId").cloned().unwrap_or(Value::Null),
    }))
}

/// 全量扫描：live + 池账号逐一 preview，返回可领清单（供定时探测/通知）。
pub async fn claim_scan_all() -> Value {
    let mut items: Vec<Value> = Vec::new();
    let mut sources: Vec<(String, Value)> = Vec::new();
    if let Some(live) = live_credentials() {
        if is_logged_in(&live) {
            sources.push(("zcode:live".to_string(), live));
        }
    }
    for acc in list_accounts() {
        if let Some(id) = acc.get("id").and_then(Value::as_str) {
            if let Some(creds) = pool_credentials(id) {
                sources.push((id.to_string(), creds));
            }
        }
    }
    // 展示去重：live 与池内同身份账号只扫一个（页面上不出现重复行）。
    let claim_state = load_claim_state();
    let mut seen_identity: Vec<ZcodeIdentity> = Vec::new();
    for (id, creds) in sources {
        let identity = identity_of(&creds);
        if identity_has_signal(&identity)
            && seen_identity
                .iter()
                .any(|s| identity_matches(s, &identity))
        {
            continue;
        }
        seen_identity.push(identity.clone());
        let mid = if id == "zcode:live" {
            telemetry_device_mid()
        } else {
            account_mid(&id).or_else(telemetry_device_mid)
        };
        let px = account_proxy(&id);
        // 余额快照（领取状态的事实源：手动/app 内领取同样可见）。
        let balance = billing_balance_raw(&creds, mid.clone(), px.as_deref())
            .await
            .unwrap_or_else(|e| json!({"error": e}));
        // 已领历史（v2 状态机 plans.accounts[id].claimed）。
        let mut claimed_plans: Vec<String> = Vec::new();
        if let Some(plans_map) = claim_state.get("plans").and_then(Value::as_object) {
            for (pid, ps) in plans_map {
                if ps
                    .pointer(&format!("/accounts/{id}/claimed"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    claimed_plans.push(pid.clone());
                }
            }
        }
        match claim_preview_for(&creds, mid, px.as_deref()).await {
            Ok(plans) if !plans.is_empty() => {
                items.push(json!({
                    "id": id,
                    "name": identity.label().unwrap_or_else(|| id.clone()),
                    "claimable": true,
                    "plans": plans,
                    "balance": balance,
                    "claimedPlans": claimed_plans,
                }));
            }
            Ok(_) => {
                items.push(json!({
                    "id": id,
                    "name": identity.label().unwrap_or_else(|| id.clone()),
                    "claimable": false,
                    "balance": balance,
                    "claimedPlans": claimed_plans,
                }));
            }
            Err(e) => {
                items.push(json!({
                    "id": id,
                    "name": identity.label().unwrap_or_else(|| id.clone()),
                    "claimable": false,
                    "error": e,
                }));
            }
        }
    }
    let claimable = items
        .iter()
        .filter(|i| i.get("claimable").and_then(Value::as_bool) == Some(true))
        .count();
    json!({"ok": true, "accounts": items, "claimableCount": claimable})
}

// ---------------------------------------------------------------------------
// 领取引擎 v2（2026-10-05 重构；参照 thelabcorner/zcode-claim-bot run-core.mjs
// 状态机并多账号化）：
//   plan 键 = plan_id；contentHash 抓同 id 换波重开；
//   终态 = 1003 已领(成功) / 1002 下线 / 1004 无资格（per 账号）；
//   可重试 = 1005 抢空(波次回补,指数退避) / 3007 换码重试 / 429 全局退避；
//   识别 = billing/balance 快照作为「已领取/额度」事实源（手动、app 内领取同样可见）。
// ---------------------------------------------------------------------------

/// 波次内容指纹：name + 规范化 entitlements（同 plan_id 换波→hash 变→重置退避重试）。
fn claim_content_hash(plan: &Value) -> String {
    let mut ents: Vec<String> = plan
        .get("grants")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|e| {
            format!(
                "{}|{}|{}",
                e.get("name").and_then(Value::as_str).unwrap_or(""),
                json_num(e.get("units")).unwrap_or(0.0),
                e.get("period").and_then(Value::as_str).unwrap_or("")
            )
        })
        .collect();
    ents.sort();
    let norm = json!({
        "name": plan.get("name").and_then(Value::as_str).unwrap_or(""),
        "ents": ents,
    });
    let bytes = serde_json::to_vec(&norm).unwrap_or_default();
    let d = Sha256::digest(&bytes);
    d.iter().map(|b| format!("{b:02x}")).collect::<String>().chars().take(12).collect()
}

/// ZaiBilling 余额原始 data{plans,balances}（领取状态识别的事实源）。
async fn billing_balance_raw(
    creds: &Value,
    mid: Option<String>,
    proxy: Option<&str>,
) -> Result<Value, String> {
    let token = zai_billing_token(creds).ok_or("无 ZaiBilling token")?;
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(20));
    if let Some(px) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
        builder = builder.proxy(reqwest::Proxy::all(px).map_err(|e| e.to_string())?);
    }
    let client = builder.build().unwrap_or_default();
    let mut req = client.get(ZAI_BILLING_URL);
    for (k, v) in zai_headers(&token, mid.as_deref()) {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    if v.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(format!("code {}", v.get("code").and_then(Value::as_i64).unwrap_or(-1)));
    }
    Ok(v.get("data").cloned().unwrap_or_else(|| json!({})))
}

/// events 环形缓冲（最新 60 条；kind: scan/offer/claim-ok/claim-already/...）。
fn push_claim_event(state: &mut Value, kind: &str, msg: &str) {
    let arr = state
        .as_object_mut()
        .unwrap()
        .entry("events")
        .or_insert(json!([]));
    if let Some(a) = arr.as_array_mut() {
        a.push(json!({"at": now_ms(), "kind": kind, "msg": msg}));
        let n = a.len();
        if n > 60 {
            a.drain(0..n - 60);
        }
    }
}

fn day_key_now() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn bump_daily(v: &serde_json::Map<String, Value>) -> Value {
    let day = day_key_now();
    let (d, c) = match v.get("daily") {
        Some(x) if x.get("date").and_then(Value::as_str) == Some(day.as_str()) => (
            day,
            x.get("count").and_then(Value::as_u64).unwrap_or(0) + 1,
        ),
        _ => (day, 1),
    };
    json!({"date": d, "count": c})
}

fn daily_count(v: &serde_json::Map<String, Value>) -> u64 {
    match v.get("daily") {
        Some(x) if x.get("date").and_then(Value::as_str) == Some(day_key_now().as_str()) => {
            x.get("count").and_then(Value::as_u64).unwrap_or(0)
        }
        _ => 0,
    }
}

/// 领取引擎 v2 主循环体（每扫描 tick 执行一次）。
/// auto_claim=false 时为观察模式：只 preview+balance+记录，不铸码不提交。
pub async fn claim_cycle(auto_restart: bool) -> Value {
    let mut state = load_claim_state();
    let observe = !state
        .get("autoClaim")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    state["lastScanAt"] = json!(now_ms());
    state["lastScanDay"] = json!(day_key_now());

    let meta = state.get("meta").cloned().unwrap_or_else(|| json!({}));
    let global_day = if meta.get("day").and_then(Value::as_str) == Some(day_key_now().as_str()) {
        meta.get("globalAttempts").and_then(Value::as_u64).unwrap_or(0)
    } else {
        0
    };
    let backoff_until = meta.get("backoffUntil").and_then(Value::as_u64).unwrap_or(0);
    let rate_limited = now_ms() < backoff_until as i64;

    let mut events: Vec<Value> = Vec::new();
    let mut plans_state = state.get("plans").cloned().unwrap_or_else(|| json!({}));
    let mut rollup: serde_json::Map<String, Value> = serde_json::Map::new();

    // 收集账号（live + 池；身份去重）。
    let mut sources: Vec<(String, Value)> = Vec::new();
    if let Some(live) = live_credentials() {
        if is_logged_in(&live) {
            sources.push(("zcode:live".to_string(), live));
        }
    }
    for acc in list_accounts() {
        if let Some(id) = acc.get("id").and_then(Value::as_str) {
            if let Some(creds) = pool_credentials(id) {
                sources.push((id.to_string(), creds));
            }
        }
    }

    if observe {
        events.push(json!({"at": now_ms(), "kind": "scan", "msg": "观察模式（autoClaim 关闭）：只记录不领取"}));
    } else if rate_limited {
        events.push(json!({"at": now_ms(), "kind": "backoff", "msg": "429 全局退避中，本轮只观察"}));
    }

    // CDP 就绪一次性确认（领取模式才需要；失败不阻塞观察记录）。
    let mut cdp_ready = false;
    if !observe && !rate_limited {
        cdp_ready = ensure_cdp_ready(auto_restart).await.is_ok();
        if !cdp_ready {
            events.push(json!({"at": now_ms(), "kind": "warn", "msg": "CDP 未就绪（ZCode 未运行或未开调试口），本轮无法铸验证码"}));
        }
    }
    let cfg = if cdp_ready {
        claim_captcha_config().await.unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    let captcha_ok = cdp_ready
        && cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false)
        && cfg
            .get("sceneId")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
    if cdp_ready && !captcha_ok {
        events.push(json!({"at": now_ms(), "kind": "warn", "msg": "上游验证码配置缺失，本轮无法领取"}));
    }

    let mut seen_identity: Vec<ZcodeIdentity> = Vec::new();
    let mut global_attempts = global_day;
    let mut total_claimed = 0usize;
    let mut offers_seen = 0usize;

    for (id, creds) in sources {
        let identity = identity_of(&creds);
        if identity_has_signal(&identity)
            && seen_identity.iter().any(|s| identity_matches(s, &identity))
        {
            continue;
        }
        seen_identity.push(identity.clone());
        let label = identity.label().unwrap_or_else(|| id.clone());
        let mid = if id == "zcode:live" {
            telemetry_device_mid()
        } else {
            account_mid(&id).or_else(telemetry_device_mid)
        };
        let px = account_proxy(&id);

        // ── 余额快照（每账号每轮；「已领取/额度」的事实源）──
        let balance = billing_balance_raw(&creds, mid.clone(), px.as_deref())
            .await
            .unwrap_or_else(|e| json!({"error": e}));

        // ── preview（按优先级降序）──
        let mut plans = claim_preview_for(&creds, mid.clone(), px.as_deref())
            .await
            .unwrap_or_default();
        plans.sort_by_key(|p| -p.get("priority").and_then(Value::as_i64).unwrap_or(0));
        offers_seen += plans.len();

        let mut claimed_today: Vec<String> = Vec::new();
        let mut last_error = String::new();

        if !observe && !rate_limited && captcha_ok {
            'plans: for plan in &plans {
                let Some(plan_id) = plan.get("planId").and_then(Value::as_str).map(String::from)
                else {
                    continue;
                };
                let hash = claim_content_hash(plan);
                let ps_entry = plans_state
                    .as_object_mut()
                    .unwrap()
                    .entry(plan_id.clone())
                    .or_insert(json!({
                        "firstSeen": now_ms(),
                        "contentHash": hash,
                    }));
                let ps_obj = ps_entry.as_object_mut().unwrap();
                ps_obj["lastSeen"] = json!(now_ms());
                ps_obj["planSnapshot"] = json!(plan);
                // 换波重开：同 id 内容变化 → 重置退避与终态。
                if ps_obj.get("contentHash").and_then(Value::as_str) != Some(hash.as_str()) {
                    ps_obj["contentHash"] = json!(hash);
                    ps_obj["exhaustedStreak"] = json!(0);
                    if let Some(accts) = ps_obj.get_mut("accounts").and_then(Value::as_object_mut) {
                        for (_, av) in accts.iter_mut() {
                            if let Some(o) = av.as_object_mut() {
                                o.remove("terminal");
                                o["exhaustedStreak"] = json!(0);
                            }
                        }
                    }
                    events.push(json!({"at": now_ms(), "kind": "wave", "msg": format!("{plan_id} 波次内容变化，重置重试")}));
                }
                let accounts = ps_obj.entry("accounts").or_insert(json!({}));
                let acc_entry = accounts
                    .as_object_mut()
                    .unwrap()
                    .entry(id.clone())
                    .or_insert(json!({}));
                let acc_obj = acc_entry.as_object_mut().unwrap();

                // 已领 / 终态 / 退避 / 日上限 → 跳过。
                if acc_obj.get("claimed").and_then(Value::as_bool) == Some(true) {
                    claimed_today.push(plan_id.clone());
                    continue;
                }
                if acc_obj.get("terminal").is_some() {
                    continue;
                }
                if let Some(b) = acc_obj.get("backoffUntil").and_then(Value::as_u64) {
                    if now_ms() < b as i64 {
                        continue;
                    }
                }
                if daily_count(acc_obj) >= 6 {
                    continue;
                }
                if global_attempts >= 60 {
                    events.push(json!({"at": now_ms(), "kind": "cap", "msg": "今日全局尝试已达 60 次，停止领取"}));
                    break 'plans;
                }

                // ── 铸码 + 提交 ──
                acc_obj["lastTried"] = json!(now_ms());
                let d2 = bump_daily(acc_obj);
                acc_obj["daily"] = d2;
                global_attempts += 1;
                match mint_captcha_via_cdp(&cfg).await {
                    Ok(param) => {
                        match claim_submit_for(
                            &creds,
                            &plan_id,
                            &param,
                            cfg.get("region").and_then(Value::as_str),
                            mid.clone(),
                            px.as_deref(),
                        )
                        .await
                        {
                            Ok(_) => {
                                acc_obj["claimed"] = json!(true);
                                acc_obj["claimedAt"] = json!(now_ms());
                                acc_obj.remove("lastCode");
                                acc_obj.remove("lastMsg");
                                total_claimed += 1;
                                claimed_today.push(plan_id.clone());
                                let name = plan
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or(&plan_id)
                                    .to_string();
                                events.push(json!({"at": now_ms(), "kind": "claim-ok", "msg": format!("{label} ← {name}：领取成功")}));
                            }
                            Err((code, msg)) => {
                                acc_obj["lastCode"] = json!(code);
                                acc_obj["lastMsg"] = json!(msg.clone());
                                last_error = msg.clone();
                                match code {
                                    1003 => {
                                        acc_obj["claimed"] = json!(true);
                                        acc_obj["claimedAt"] = json!(now_ms());
                                        claimed_today.push(plan_id.clone());
                                        events.push(json!({"at": now_ms(), "kind": "claim-already", "msg": format!("{label} ← {plan_id}：此前已领取过")}));
                                    }
                                    1002 | 1004 => {
                                        acc_obj["terminal"] = json!(format!("code {code}"));
                                        events.push(json!({"at": now_ms(), "kind": "terminal", "msg": format!("{label} ← {plan_id}: {msg}")}));
                                    }
                                    1005 => {
                                        let streak = acc_obj
                                            .get("exhaustedStreak")
                                            .and_then(Value::as_u64)
                                            .unwrap_or(0)
                                            + 1;
                                        acc_obj["exhaustedStreak"] = json!(streak);
                                        let hot = in_hot_window(chrono::Local::now());
                                        let base_ms: u64 = if hot { 4 * 60_000 } else { 12 * 60_000 };
                                        let cap_ms: u64 = if hot { 25 * 60_000 } else { 45 * 60_000 };
                                        let wait = (base_ms as f64
                                            * 1.6f64.powi(streak.min(6) as i32))
                                            as u64;
                                        acc_obj["backoffUntil"] =
                                            json!(now_ms() + wait.min(cap_ms) as i64);
                                        events.push(json!({"at": now_ms(), "kind": "exhausted", "msg": format!("{label} ← {plan_id}: 名额抢空(第 {streak} 次)，退避 {}s", wait / 1000)}));
                                    }
                                    3007 => {
                                        events.push(json!({"at": now_ms(), "kind": "captcha", "msg": format!("{label} 验证码被拒，下轮换新码")}));
                                    }
                                    429 => {
                                        state["meta"]["backoffUntil"] =
                                            json!(now_ms() + 15 * 60_000);
                                        events.push(json!({"at": now_ms(), "kind": "rate-limit", "msg": "429：全局退避 15 分钟"}));
                                    }
                                    _ => {
                                        events.push(json!({"at": now_ms(), "kind": "claim-err", "msg": format!("{label} ← {plan_id}: {msg}")}));
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        last_error = format!("验证码: {e}");
                        events.push(json!({"at": now_ms(), "kind": "mint-err", "msg": format!("{label}: {e}")}));
                        break 'plans;
                    }
                }
                // 人味间隔（300-1200ms）。
                tokio::time::sleep(Duration::from_millis(300 + (now_ms() % 900) as u64)).await;
            }
        }

        rollup.insert(
            id.clone(),
            json!({
                "name": label,
                "lastCheck": now_ms(),
                "planCount": plans.len(),
                "plans": plans,
                "balance": balance,
                "claimedToday": claimed_today,
                "lastError": last_error,
            }),
        );
    }

    state["plans"] = plans_state;
    state["rollup"] = Value::Object(rollup);
    state["meta"]["day"] = json!(day_key_now());
    state["meta"]["globalAttempts"] = json!(global_attempts);
    if total_claimed > 0 {
        state["lastClaimAt"] = json!(now_ms());
    }
    if offers_seen > 0 {
        events.push(json!({"at": now_ms(), "kind": "offer", "msg": format!("本轮可见可领套餐 {offers_seen} 个")}));
    }
    // 事件缓冲并入 state（环形 60）。
    {
        let arr = state
            .as_object_mut()
            .unwrap()
            .entry("events")
            .or_insert(json!([]));
        if let Some(a) = arr.as_array_mut() {
            a.extend(events);
            let n = a.len();
            if n > 60 {
                a.drain(0..n - 60);
            }
        }
    }
    if total_claimed > 0 {
        crate::modules::notify::notify(
            "zcode-claim",
            "ZCode 自动领取成功",
            &format!("本轮领取 {total_claimed} 个套餐"),
        );
    }
    let _ = save_claim_state(&state);
    json!({
        "ok": true, "observe": observe, "offers": offers_seen,
        "claimed": total_claimed, "cdpReady": cdp_ready,
    })
}

// ---------------------------------------------------------------------------
// 全自动领取：CDP 驱动 ZCode 内嵌渲染进程 mint 阿里云验证码（无痕验证）。
// 方案逆向自 thelabcorner/zcode-claim-bot：ZCode 是 Electron，带
// --remote-debugging-port 启动后，Runtime.evaluate 注入阿里云 SDK 并跑
// startTracelessVerification()，产出与 Claim 按钮同源的真验证码参数（~2s）。
// CDP 轮子复用 grok_sso.rs 的 tokio-tungstenite 模式。
// ---------------------------------------------------------------------------

/// CDP 调试端口（可被 ZCODE_CLAIM_CDP_PORT 覆盖；默认 9334 避开常用口）。
pub fn claim_cdp_port() -> u16 {
    std::env::var("ZCODE_CLAIM_CDP_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(9334)
}

async fn cdp_alive_on(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/json/version");
    matches!(
        tokio::time::timeout(Duration::from_secs(2), reqwest::get(&url)).await,
        Ok(Ok(resp)) if resp.status().is_success()
    )
}

async fn pick_page_ws_url_on(port: u16) -> Result<String, String> {
    pick_page_ws_urls_on(port)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| "CDP 无可用页面 target".to_string())
}

/// 全部可用页面 target（排序：排除 devtools/extension；URL 含 zcode 的主窗口优先
/// ——铸码必须落在有用户活动痕迹的主渲染进程，后台工具页会被风控拒绝）。
async fn pick_page_ws_urls_on(port: u16) -> Result<Vec<String>, String> {
    let url = format!("http://127.0.0.1:{port}/json");
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("CDP /json 请求失败: {e}"))?;
    let targets: Value = resp.json().await.map_err(|e| format!("CDP /json 解析失败: {e}"))?;
    let mut pages: Vec<(String, String)> = targets
        .as_array()
        .ok_or("CDP targets 格式异常")?
        .into_iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .filter_map(|t| {
            let ws = t.get("webSocketDebuggerUrl").and_then(Value::as_str)?.to_string();
            let u = t.get("url").and_then(Value::as_str).unwrap_or("").to_string();
            Some((ws, u))
        })
        .filter(|(_, u)| !u.starts_with("devtools://") && !u.starts_with("chrome-extension://"))
        .collect();
    pages.sort_by_key(|(_, u)| {
        let lower = u.to_ascii_lowercase();
        (if lower.contains("zcode") { 0 } else { 1 }, u.len())
    });
    Ok(pages.into_iter().map(|(ws, _)| ws).collect())
}

async fn cdp_eval_on(port: u16, expression: &str, timeout_secs: u64) -> Result<Value, String> {
    let ws_url = pick_page_ws_url_on(port).await?;
    cdp_eval_on_ws(&ws_url, expression, timeout_secs).await
}

async fn cdp_eval_on_ws(ws_url: &str, expression: &str, timeout_secs: u64) -> Result<Value, String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| format!("CDP WebSocket 连接失败: {e}"))?;
    let request = json!({
        "id": 1,
        "method": "Runtime.evaluate",
        "params": {"expression": expression, "awaitPromise": true, "returnByValue": true},
    });
    ws.send(Message::Text(request.to_string()))
        .await
        .map_err(|e| format!("CDP 发送失败: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        let msg = tokio::time::timeout(Duration::from_secs(timeout_secs), ws.next())
            .await
            .map_err(|_| "CDP 响应超时".to_string())?
            .ok_or_else(|| "CDP 连接关闭".to_string())?
            .map_err(|e| format!("CDP 读错误: {e}"))?;
        if let Message::Text(text) = msg {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if value.get("id").and_then(Value::as_u64) == Some(1) {
                    if let Some(err) = value.get("error") {
                        return Err(format!("CDP 错误: {err}"));
                    }
                    if let Some(exc) = value.pointer("/result/exceptionDetails") {
                        return Err(format!(
                            "页面异常: {}",
                            exc.pointer("/exception/description")
                                .or_else(|| exc.get("text"))
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                        ));
                    }
                    return Ok(value.pointer("/result/result/value").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }
    Err("CDP 响应超时".to_string())
}

/// 在 ZCode 渲染进程里 mint 验证码参数（注入阿里云 SDK + 无痕验证，~2-20s）。
async fn mint_captcha_via_cdp(cfg: &Value) -> Result<String, String> {
    let port = claim_cdp_port();
    let region = cfg.get("region").and_then(Value::as_str).unwrap_or("");
    let prefix = cfg.get("prefix").and_then(Value::as_str).unwrap_or("");
    let scene_id = cfg.get("sceneId").and_then(Value::as_str).unwrap_or("");
    if scene_id.is_empty() {
        return Err("验证码配置缺少 sceneId".to_string());
    }
    let sdk = "https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js";
    let js = format!(
        r#"(async () => {{
    const log = [];
    const t0 = Date.now();
    document.querySelectorAll('[id^="__zsw_captcha_"]').forEach(e => e.remove());
    const CTR = '__zsw_captcha_' + Math.random().toString(36).slice(2, 8);
    const BTN = CTR + '_btn', EL = CTR + '_el';
    const ctr = document.createElement('div');
    ctr.id = CTR;
    ctr.style.cssText = 'position:fixed;left:0;bottom:0;width:2px;height:2px;opacity:0.01;overflow:hidden;z-index:-1;';
    ctr.innerHTML = '<div id="' + EL + '"></div><button id="' + BTN + '" type="button">v</button>';
    document.body.appendChild(ctr);
    let inst = null, param = null, failInfo = null;
    try {{
        if (typeof window.initAliyunCaptcha !== 'function') {{
            await new Promise((res, rej) => {{
                const s = document.createElement('script');
                s.src = '{sdk}';
                s.onload = res;
                s.onerror = () => rej(new Error('aliyun sdk load failed'));
                document.head.appendChild(s);
            }});
            log.push('sdk loaded');
        }}
        window.AliyunCaptchaConfig = {{ region: '{region}', prefix: '{prefix}' }};
        window.initAliyunCaptcha({{
            SceneId: '{scene_id}',
            mode: 'popup',
            element: '#' + EL,
            button: '#' + BTN,
            showErrorTip: false,
            getInstance(i) {{ inst = i; log.push('instance ready'); }},
            success(p) {{ param = p; log.push('success len=' + p.length); }},
            fail(e) {{ failInfo = (e === undefined || e === null) ? 'fail' : (typeof e === 'string' ? e : JSON.stringify(e)); log.push('fail'); }},
            onError(e) {{ failInfo = (e === undefined || e === null) ? 'onerror' : (typeof e === 'string' ? e : JSON.stringify(e)); log.push('onerror'); }},
        }});
        for (let i = 0; i < 100 && !inst; i++) await new Promise(r => setTimeout(r, 100));
        if (!inst) throw new Error('no instance');
        if (typeof inst.startTracelessVerification === 'function') inst.startTracelessVerification();
        else if (typeof inst.show === 'function') inst.show();
        for (let i = 0; i < 400 && !param && !failInfo; i++) await new Promise(r => setTimeout(r, 100));
        try {{ ctr.remove(); }} catch (_e) {{}}
        if (param) return {{ ok: true, param, ms: Date.now() - t0 }};
        return {{ ok: false, reason: 'verify failed', failInfo: (failInfo === undefined || failInfo === null) ? 'fail(null)' : (typeof failInfo === 'string' ? failInfo : JSON.stringify(failInfo)), log }};
    }} catch (e) {{
        ctr.remove();
        return {{ ok: false, reason: String(e && e.message || e) }};
    }}
}})()"#
    );
    let urls = pick_page_ws_urls_on(port).await?;
    if urls.is_empty() {
        return Err("CDP 无可用页面 target".to_string());
    }
    let mut last: Option<String> = None;
    for ws_url in &urls {
        let out = match cdp_eval_on_ws(ws_url, &js, 90).await {
            Ok(v) => v,
            Err(e) => {
                last = Some(format!("target 失败: {e}"));
                continue;
            }
        };
        let ok = out.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if ok {
            return out
                .get("param")
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| "验证码参数缺失".to_string());
        }
        let log = out
            .get("log")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" → "))
            .unwrap_or_default();
        let base = out
            .get("reason")
            .or_else(|| out.get("failInfo"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        last = Some(if log.is_empty() { base.to_string() } else { format!("{base} [{log}]") });
    }
    Err(format!(
        "验证码未通过（无痕验证被风控拦截，需人工）：{}（尝试了 {} 个页面）",
        last.unwrap_or_else(|| "unknown".into()),
        urls.len()
    ))
}

/// 确保 CDP 可用：已开→Ok；ZCode 没跑→带 CDP 拉起并等待（≤40s）；
/// ZCode 在跑但没开 CDP→Err（需要用户确认后重启，前端有对应入口）。
/// 查询运行中 ZCode.exe 的真实可执行路径（wmic；输出为 GBK——中文安装目录
/// 必须 GBK 解码，UTF-8 lossy 会产生替换符导致拉起失败。2026-10-03 实锤：
/// 用户装在 E:\Zcode国外版本\，候选路径全猜错 → 杀掉后拉不回来）。
fn query_running_zcode_path() -> Option<String> {
    let out = crate::modules::process::run_cmd_timeout(
        "wmic",
        &["process", "where", "name='ZCode.exe'", "get", "ExecutablePath"],
        8,
    )?;
    let raw = out.stdout;
    // wmic 输出是控制台代码页（CN Windows = GBK）；ASCII 部分两者一致。
    let text = match std::str::from_utf8(&raw) {
        Ok(s) if !s.contains('\u{FFFD}') => s.to_string(),
        _ => encoding_rs::GBK.decode(&raw).0.into_owned(),
    };
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.eq_ignore_ascii_case("ExecutablePath") {
            continue;
        }
        if l.to_ascii_lowercase().ends_with("zcode.exe") {
            return Some(l.to_string());
        }
    }
    None
}

/// 解析 ZCode 可执行路径：运行中进程 > state 持久化 > env > 常见候选。
/// 找不到返回 None（调用方**绝不**在此情况下杀进程）。
fn resolve_zcode_exe() -> Option<String> {
    if let Some(p) = std::env::var("ZCODE_EXE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    let state = load_claim_state();
    if let Some(p) = state.get("zcodeExePath").and_then(Value::as_str) {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    if let Some(p) = query_running_zcode_path() {
        if std::path::Path::new(&p).exists() {
            // 持久化：ZCode 关闭后仍可拉起。
            let mut s2 = load_claim_state();
            s2["zcodeExePath"] = json!(p);
            let _ = save_claim_state(&s2);
            return Some(p);
        }
    }
    zcode_launch_candidates()
        .into_iter()
        .find(|p| p.exists())
        .map(|p| p.display().to_string())
}

async fn ensure_cdp_ready(auto_restart: bool) -> Result<(), String> {
    let port = claim_cdp_port();
    if cdp_alive_on(port).await {
        return Ok(());
    }
    // 先解析真实安装路径（运行中进程 > 持久化 > env > 候选）；**解析不到绝不杀**，
    // 否则会把用户的 ZCode 杀掉还拉不回来（2026-10-03 mint 自检事故）。
    let exe = if desktop_running() {
        query_running_zcode_path()
    } else {
        None
    }
    .or_else(resolve_zcode_exe);
    let Some(exe) = exe else {
        return Err("未找到 ZCode.exe（请在 /zcode 页确认安装路径或设置 ZCODE_EXE 环境变量）".to_string());
    };
    if desktop_running() {
        if !auto_restart {
            return Err(
                "ZCode 正在运行但未开启调试端口，无法自动完成验证码。请到 /zcode 页开启「自动重启 ZCode」"
                    .to_string(),
            );
        }
        // 自动重启：热切换语义下换写凭据不影响登录，重启仅数秒——领取窗口期一次性成本。
        // 预告通知：用户不该看到 ZCode「无故」退出。
        crate::modules::notify::notify(
            "zcode-claim",
            "ZCode 即将自动重启",
            "为领取每日福利需开启调试通道，ZCode 将在数秒内重启（登录不受影响）",
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = kill_zcode()?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new(&exe)
            .arg(format!("--remote-debugging-port={}", claim_cdp_port()))
            .creation_flags(0x0000_0008 | 0x0000_0200)
            .spawn()
            .map_err(|e| format!("拉起 ZCode 失败: {e}"))?;
    }
    #[cfg(not(windows))]
    {
        let _ = &exe;
    }
    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if cdp_alive_on(port).await {
            return Ok(());
        }
    }
    Err("ZCode 已拉起但调试端口 40s 内未就绪".to_string())
}

/// 单账号自动领取：preview → 有可领则 mint 验证码 → 逐套餐 claim。
/// 返回人话结果 JSON。CDP 不可用时返回明确错误。
pub async fn auto_claim_account(id: &str, auto_restart: bool) -> Result<Value, String> {
    let creds = pool_credentials(id)
        .or_else(|| if id == "zcode:live" { live_credentials() } else { None })
        .ok_or_else(|| format!("账号凭据缺失: {id}"))?;
    let identity = identity_of(&creds);
    let label = identity.label().unwrap_or_else(|| id.to_string());

    let mid = account_mid(id).or_else(telemetry_device_mid);
    let px = account_proxy(id);
    let plans = claim_preview_for(&creds, mid.clone(), px.as_deref()).await?;
    if plans.is_empty() {
        return Ok(json!({
            "id": id, "name": label, "claimed": 0,
            "message": "暂无可领套餐（可能今日已领）",
        }));
    }
    ensure_cdp_ready(auto_restart).await?;
    let cfg = claim_captcha_config().await?;
    if !cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
        return Err("上游未启用验证码配置，无法自动领取".to_string());
    }
    let mut claimed = 0usize;
    let mut details: Vec<String> = Vec::new();
    let mut last_err = String::new();
    let mut state = load_claim_state();
    for plan in &plans {
        let plan_id = plan.get("planId").and_then(Value::as_str).unwrap_or("");
        if plan_id.is_empty() {
            continue;
        }
        // 每次领取都要新验证码参数（单次有效）。
        match mint_captcha_via_cdp(&cfg).await {
            Ok(param) => match claim_submit_for(&creds, plan_id, &param, None, mid.clone(), px.as_deref()).await {
                Ok(_) => {
                    claimed += 1;
                    let name = plan.get("name").and_then(Value::as_str).unwrap_or(plan_id);
                    details.push(format!("{name}：领取成功"));
                    // 记入 v2 状态机（手动按钮与循环共用同一份事实）。
                    if let Some(pm) = state
                        .as_object_mut()
                        .unwrap()
                        .entry("plans")
                        .or_insert(json!({}))
                        .as_object_mut()
                    {
                        let e = pm.entry(plan_id.to_string()).or_insert(json!({}));
                        let accts = e.as_object_mut().unwrap().entry("accounts").or_insert(json!({}));
                        if let Some(o) = accts.as_object_mut().unwrap().get_mut(id).and_then(Value::as_object_mut) {
                            o["claimed"] = json!(true);
                            o["claimedAt"] = json!(now_ms());
                        } else {
                            accts.as_object_mut().unwrap().insert(
                                id.to_string(),
                                json!({"claimed": true, "claimedAt": now_ms()}),
                            );
                        }
                    }
                    push_claim_event(&mut state, "claim-ok", &format!("{label} ← {name}（手动）"));
                }
                Err((code, msg)) => {
                    let name = plan.get("name").and_then(Value::as_str).unwrap_or(plan_id);
                    details.push(format!("{name}：{msg}"));
                    last_err = msg;
                    push_claim_event(&mut state, "claim-err", &format!("{label} ← {plan_id}: code {code} {last_err}"));
                    // 1003=已领过也视为成功语义。
                    if code == 1003 {
                        claimed += 1;
                    }
                }
            },
            Err(e) => {
                details.push(format!("验证码：{e}"));
                last_err = e;
                break;
            }
        }
    }
    let _ = save_claim_state(&state);
    Ok(json!({
        "id": id, "name": label,
        "claimable": plans.len(),
        "claimed": claimed,
        "details": details,
        "ok": claimed > 0,
        "lastError": if claimed > 0 { Value::Null } else { json!(last_err) },
    }))
}

/// 领取状态落盘（~/.wb-switch/zcode_claim_state.json）：循环与前端共用。
const CLAIM_STATE_FILE: &str = "zcode_claim_state.json";

fn claim_state_path() -> PathBuf {
    store_dir().join(CLAIM_STATE_FILE)
}

pub fn load_claim_state() -> Value {
    std::fs::read_to_string(claim_state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({"lastScanAt": null, "lastClaimAt": null, "accounts": {}, "autoClaim": false}))
}

pub fn save_claim_state(state: &Value) -> Result<(), String> {
    atomic_write(&claim_state_path(), &serde_json::to_string_pretty(state).unwrap_or_default())
        .map_err(|e| format!("写领取状态失败: {e}"))
}

/// 抢购热窗（北京时间，源自 @zcode_ai drop 历史校准，参照 thelabcorner/zcode-claim-bot
/// schedule.mjs）：surprise drop 21:25-次日 01:45、夜间回补+烧尾 04:30-10:30。
/// 热窗内高频轮询（默认 2 分钟），闲时低频（默认 30 分钟）。
fn in_hot_window(now: chrono::DateTime<chrono::Local>) -> bool {
    use chrono::Timelike;
    let mins = now.hour() * 60 + now.minute();
    // 21:25-次日 01:45（跨午夜）
    if mins >= 21 * 60 + 25 || mins <= 1 * 60 + 45 {
        return true;
    }
    // 04:30-10:30
    mins >= 4 * 60 + 30 && mins <= 10 * 60 + 30
}

/// 定时自动领取循环（main.rs 注册）——v2 引擎驱动：
/// 每 tick 调 claim_cycle（preview+balance+per-plan×per-账号状态机+CDP 铸码领取），
/// 节奏：热窗内 pollHotMin（默认 2 分钟）、闲时 pollIdleMin（默认 30 分钟），
/// 均可在 zcode_claim_state.json 调整；启动即扫一轮。
pub fn spawn_claim_loop() {
    tokio::spawn(async move {
        let mut last_scan: Option<Instant> = None;
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            let state = load_claim_state();
            let idle_min = state
                .get("pollIdleMin")
                .and_then(Value::as_u64)
                .filter(|v| *v >= 2)
                .unwrap_or(30);
            let hot_min = state
                .get("pollHotMin")
                .and_then(Value::as_u64)
                .filter(|v| *v >= 1)
                .unwrap_or(2);
            let auto_restart = state
                .get("autoRestartZcode")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let interval = if in_hot_window(chrono::Local::now()) {
                Duration::from_secs(hot_min * 60)
            } else {
                Duration::from_secs(idle_min * 60)
            };
            let due = match last_scan {
                None => true,
                Some(t0) => t0.elapsed() >= interval,
            };
            if !due {
                continue;
            }
            last_scan = Some(Instant::now());
            let _ = claim_cycle(auto_restart).await;
        }
    });
}
