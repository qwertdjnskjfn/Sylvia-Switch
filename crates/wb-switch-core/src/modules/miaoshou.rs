//! 妙手（美团智能体 catpaw-moon，Electron 桌面端）账号接入。
//!
//! 协议情报（2026-09-21 逆向实测确认）：
//! - 登录态文件：`%APPDATA%\catpaw-moon\catx-credential.json`，内容
//!   `{"ssoTokenEnc": "<base64 字符串>"}`；
//! - 解密算法（Node 复刻已验证）：
//!   1. MachineGuid：`reg query "HKLM\SOFTWARE\Microsoft\Cryptography" /v MachineGuid`，
//!      取 REG_SZ 后的值，去所有空白字符并转小写；
//!   2. key = SHA256(MachineGuid + ":catpaw-desk-token-v2")；
//!   3. blob = base64_decode(ssoTokenEnc)，布局 iv(12) ‖ authTag(16) ‖ ciphertext；
//!   4. AES-256-GCM 解出 `{"access_token": "...", "modified_at": ...}`；
//! - API 头：`X-Auth-Token: <token>` + `client-id: CATPAW_PC`
//!   （⚠️ 2026-09-21 本机实测：对外版网关是 `https://catx.nocode.cn`
//!   （SDK TENANT_BASE_URL_MAP external.prod），且**只认 `X-Auth-Token`**——
//!   单发 `access-token` 会返回 code 4010 "Authentication token is required"；
//!   桌面端 doFetch 同时下发 `X-Auth-Token` 与 `access-token`，本模块照做）；
//! - whoami：`GET /api/gateway/passport/current-user`
//!   → `{"code":0,"data":{"userId":3022607961,"userName":"...","mobile":"..."}}`；
//! - 积分：`GET /api/gateway/credit/balance`
//!   → `{"code":0,"data":{"availableCredits":"1200.00","userPlan":{...}}}`。
//!
//! 重要结论：**妙手没有每日签到功能**（客户端全量扫描确认），积分按月刷新，
//! 因此本模块的「状态」查询只做登录态探测 + 积分余额，`checkinSupported`
//! 恒为 false。
//!
//! ⚠️ AES-GCM auth tag 布局坑：妙手 blob 把 authTag 单独放在 iv 之后
//! （iv ‖ tag ‖ ct），而 aes-gcm 0.10 的 `Aead::decrypt` 约定输入为
//! `ct ‖ tag`（tag 附在密文尾部，`Payload` 也没有独立 tag 字段），因此必须
//! 先把 tag 拼到 ct 尾部再送进 decrypt，否则永远解密失败。

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::engine::general_purpose;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::modules::config::{atomic_write, now_ms, store_dir};

/// 平台标识（platform_checkin / gateway 集成用）。
pub const PLATFORM_MIAOSHOU: &str = "miaoshou";

/// 妙手网关根地址（对外版 = SDK TENANT_BASE_URL_MAP.external.prod，
/// 2026-09-21 本机实测确认；早期情报里的 catpaw.meituan.com 是对内版地址，
/// 对外版 token 在该 host 上永远 4010）。
const API_BASE: &str = "https://catx.nocode.cn/api/gateway";
/// 客户端标识（桌面端固定值）。
const CLIENT_ID: &str = "CATPAW_PC";
/// 账号池文件（~/.wb-switch/miaoshou_accounts.json）。
const ACCOUNTS_FILE: &str = "miaoshou_accounts.json";
/// 妙手 userData 目录名（%APPDATA% 下）。
const APPDATA_DIR: &str = "catpaw-moon";
/// 登录态文件名。
const CREDENTIAL_FILE: &str = "catx-credential.json";
/// token 加密密钥派生后缀（Node 端固定值）。
const KEY_SUFFIX: &str = ":catpaw-desk-token-v2";
/// 该平台无每日签到的说明文案。
const NO_CHECKIN_NOTE: &str = "该平台无每日签到，积分按月刷新";

static WRITE_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// HTTP 客户端（妙手网关）
// ---------------------------------------------------------------------------

/// 妙手网关客户端：connect_timeout 15s、no_proxy（美团域国内直连，不继承
/// 系统代理，与 kimi / grok_sso 等国内上游模块一致），总超时 30s。
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build miaoshou client")
    })
}

/// 读 Windows 系统代理（Clash/Surge 等，注册表 Internet Settings）。
/// 返回形如 `http://127.0.0.1:7897` 的代理地址；未启用或读取失败返回 None。
///
/// 为什么需要：本机 Clash 处于 fake-ip 模式，`*.nocode.cn` / `*.meituan.com`
/// 全部解析到 198.18.x.x 保留段——只有把流量送给本地代理才能到达真实服务端。
/// reqwest 默认只读环境变量代理、不读 Windows 注册表，必须自己读。
fn system_proxy_url() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
        let enabled = Command::new("reg")
            .args(["query", key, "/v", "ProxyEnable"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("0x1"))
            .unwrap_or(false);
        if !enabled {
            return None;
        }
        let out = Command::new("reg")
            .args(["query", key, "/v", "ProxyServer"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let server = text.split("REG_SZ").nth(1)?.trim().to_string();
        if server.is_empty() {
            return None;
        }
        if server.starts_with("http") {
            Some(server)
        } else {
            Some(format!("http://{server}"))
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// 走系统代理的客户端（妙手域名在 fake-ip 环境下必须走代理）。
fn proxy_client() -> Option<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let proxy = system_proxy_url()?;
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(&proxy).ok()?)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .ok()
    })
    .as_ref()
}

/// 走系统代理的长读客户端（SSE 用：无总超时，只限连续无数据 300s）。
fn proxy_stream_client() -> Option<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let proxy = system_proxy_url()?;
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(&proxy).ok()?)
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(300))
            .build()
            .ok()
    })
    .as_ref()
}

/// 是否连接类错误（值得 fallback 到直连重试）。
fn is_connect_error(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_request()
}

/// 带「代理优先、直连兜底」的 GET。
async fn get_with_fallback(
    url: &str,
    headers: &[(&str, &str)],
    client: &reqwest::Client,
) -> Result<reqwest::Response, String> {
    let build = |c: &reqwest::Client| {
        let mut req = c.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req
    };
    if let Some(pc) = proxy_client() {
        match build(pc).send().await {
            Ok(resp) => return Ok(resp),
            Err(e) if is_connect_error(&e) => { /* 落到直连 */ }
            Err(e) => return Err(format!("请求失败: {e}")),
        }
    }
    build(client).send().await.map_err(|e| format!("请求失败: {e}"))
}

/// 带「代理优先、直连兜底」的 POST（JSON body）。
async fn post_json_with_fallback(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    client: &reqwest::Client,
) -> Result<reqwest::Response, String> {
    let build = |c: &reqwest::Client| {
        let mut req = c.post(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.json(body)
    };
    if let Some(pc) = proxy_client() {
        match build(pc).send().await {
            Ok(resp) => return Ok(resp),
            Err(e) if is_connect_error(&e) => { /* 落到直连 */ }
            Err(e) => return Err(format!("请求失败: {e}")),
        }
    }
    build(client)
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))
}

/// 妙手 API 请求头。⚠️ 鉴权头是 `X-Auth-Token`（实测必须；单发
/// `access-token` 会被网关以 4010 拒绝），与桌面端 doFetch 一致同时下发
/// `X-Auth-Token` 与 `access-token`，另带 `client-id`。
fn api_headers(token: &str) -> [(&str, &str); 3] {
    [
        ("X-Auth-Token", token),
        ("access-token", token),
        ("client-id", CLIENT_ID),
    ]
}

/// GET 妙手网关，返回解析后的原始 JSON；传输层失败返回
/// `{"code": -1, "message": ...}`（与 config::http_request 的约定一致）。
async fn gateway_get(token: &str, path: &str) -> Value {
    let headers: Vec<(&str, &str)> = api_headers(token.trim()).to_vec();
    match get_with_fallback(&format!("{API_BASE}{path}"), &headers, http_client()).await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            serde_json::from_str(&body).unwrap_or_else(|_| {
                json!({
                    "code": status.as_u16(),
                    "message": body.chars().take(200).collect::<String>(),
                })
            })
        }
        Err(e) => json!({"code": -1, "message": e}),
    }
}

/// 业务码（0 为成功；-1 或 HTTP 状态码表示传输/网关错误）。
fn business_code(resp: &Value) -> i64 {
    resp.get("code").and_then(Value::as_i64).unwrap_or(-1)
}

/// 错误信息提取（message 优先，兼容 msg）。
fn error_message(resp: &Value) -> String {
    resp.get("message")
        .or_else(|| resp.get("msg"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| "请求失败".to_string())
}

// ---------------------------------------------------------------------------
// whoami / 积分 / 状态（token 有效性探测）
// ---------------------------------------------------------------------------

/// whoami：校验 access token 有效性并取用户信息。
/// 成功时返回响应 data 对象：`{userId, userName, mobile, avatarUrl}`。
pub async fn whoami(token: &str) -> Result<Value, String> {
    if token.trim().is_empty() {
        return Err("access token 为空".to_string());
    }
    let resp = gateway_get(token, "/passport/current-user").await;
    if business_code(&resp) != 0 {
        return Err(format!("妙手 whoami 失败: {}", error_message(&resp)));
    }
    resp.get("data")
        .filter(|d| !d.is_null())
        .cloned()
        .ok_or_else(|| "whoami 响应缺少 data".to_string())
}

/// 积分查询。
/// 成功：`{ok:true, balance:f64, planName:String, nextRefreshTime:i64, platform}`；
/// 网关业务错误（含传输失败、登录态失效）：`{ok:false, error, platform}`；
/// 本地参数错误（账号无 accessToken）：Err。
pub async fn get_credits(account: &Value) -> Result<Value, String> {
    let token = account
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "账号缺少 accessToken（请重新导入妙手登录态）".to_string())?;
    let resp = gateway_get(token, "/credit/balance").await;
    if business_code(&resp) != 0 {
        return Ok(json!({
            "ok": false,
            "error": format!("妙手积分查询失败: {}", error_message(&resp)),
            "platform": PLATFORM_MIAOSHOU,
        }));
    }
    let data = resp
        .get("data")
        .filter(|d| !d.is_null())
        .ok_or_else(|| "积分响应缺少 data".to_string())?;
    // availableCredits 是字符串数字（如 "1200.00"），兼容数值形式。
    let balance = data
        .get("availableCredits")
        .and_then(Value::as_f64)
        .or_else(|| {
            data.get("availableCredits")
                .and_then(Value::as_str)
                .and_then(|s| s.trim().parse::<f64>().ok())
        })
        .unwrap_or(0.0);
    let plan = data.get("userPlan").cloned().unwrap_or(Value::Null);
    let plan_name = plan
        .get("planName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let next_refresh = plan
        .get("nextRefreshTime")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Ok(json!({
        "ok": true,
        "balance": balance,
        "planName": plan_name,
        "nextRefreshTime": next_refresh,
        "platform": PLATFORM_MIAOSHOU,
    }))
}

/// 妙手账号状态。
/// 妙手无每日签到，状态 = 登录态/积分探测：
/// `{ok, platform:"miaoshou", checkinSupported:false, balance, planName,
/// nextRefreshTime, note:"该平台无每日签到，积分按月刷新"}`，
/// 失败时附带 `error` 字段（balance/planName 为 null）。
pub async fn get_status(account: &Value) -> Value {
    let mut base = json!({
        "platform": PLATFORM_MIAOSHOU,
        "checkinSupported": false,
        "note": NO_CHECKIN_NOTE,
    });
    let map = base.as_object_mut().expect("base 是 JSON 对象");
    match get_credits(account).await {
        Ok(credits) if credits.get("ok").and_then(Value::as_bool) == Some(true) => {
            map.insert("ok".to_string(), json!(true));
            map.insert(
                "balance".to_string(),
                credits.get("balance").cloned().unwrap_or(json!(0.0)),
            );
            map.insert(
                "planName".to_string(),
                credits.get("planName").cloned().unwrap_or(json!("")),
            );
            map.insert(
                "nextRefreshTime".to_string(),
                credits.get("nextRefreshTime").cloned().unwrap_or(json!(0)),
            );
        }
        Ok(credits) => {
            map.insert("ok".to_string(), json!(false));
            map.insert("balance".to_string(), Value::Null);
            map.insert("planName".to_string(), Value::Null);
            map.insert("nextRefreshTime".to_string(), Value::Null);
            map.insert(
                "error".to_string(),
                credits
                    .get("error")
                    .cloned()
                    .unwrap_or_else(|| json!("积分查询失败")),
            );
        }
        Err(e) => {
            map.insert("ok".to_string(), json!(false));
            map.insert("balance".to_string(), Value::Null);
            map.insert("planName".to_string(), Value::Null);
            map.insert("nextRefreshTime".to_string(), Value::Null);
            map.insert("error".to_string(), json!(e));
        }
    }
    base
}

// ---------------------------------------------------------------------------
// 本机登录态解密（catx-credential.json）
// ---------------------------------------------------------------------------

/// 本机妙手登录态文件路径（`%APPDATA%\catpaw-moon\catx-credential.json`）。
fn credential_path() -> PathBuf {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| {
        crate::modules::config::home_dir()
            .join("AppData")
            .join("Roaming")
    });
    appdata.join(APPDATA_DIR).join(CREDENTIAL_FILE)
}

/// 从 `reg query` 输出解析 MachineGuid：取含 MachineGuid 行中 REG_SZ 之后的
/// 值，去掉所有空白字符并转小写（注册表值大小写不保证）。
fn parse_machine_guid(reg_output: &str) -> Option<String> {
    for line in reg_output.lines() {
        if !line.contains("MachineGuid") {
            continue;
        }
        let upper = line.to_ascii_uppercase();
        let pos = upper.find("REG_SZ")?;
        let value = line
            .get(pos + "REG_SZ".len()..)?
            .split_whitespace()
            .collect::<String>();
        if !value.is_empty() {
            return Some(value.to_lowercase());
        }
    }
    None
}

/// 读取本机 MachineGuid（reg query HKLM\SOFTWARE\Microsoft\Cryptography）。
/// 仅 Windows 有效；解析失败返回 Err（调用方据此中止解密）。
fn read_machine_guid() -> Result<String, String> {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .map_err(|e| format!("执行 reg query 失败（仅支持 Windows）: {e}"))?;
    if !output.status.success() {
        return Err("reg query MachineGuid 执行失败（仅支持 Windows）".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_machine_guid(&text).ok_or_else(|| "无法解析 reg query 输出中的 MachineGuid".to_string())
}

/// token 加密 key：SHA256(MachineGuid + ":catpaw-desk-token-v2")。
fn derive_key(machine_guid: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(machine_guid.as_bytes());
    hasher.update(KEY_SUFFIX.as_bytes());
    let digest = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    key
}

/// ssoTokenEnc blob 布局切分：iv(12) ‖ authTag(16) ‖ ciphertext。
fn split_token_blob(blob: &[u8]) -> Result<(&[u8], &[u8], &[u8]), String> {
    if blob.len() < 12 + 16 {
        return Err(format!(
            "ssoTokenEnc 解密数据过短（{} 字节，至少需要 28）",
            blob.len()
        ));
    }
    Ok((&blob[..12], &blob[12..28], &blob[28..]))
}

/// AES-256-GCM 解密妙手 token blob，返回明文里的 access_token。
///
/// ⚠️ 坑：妙手布局是 iv ‖ tag ‖ ct，而 aes-gcm 0.10 的 `Aead::decrypt` 约定
/// 输入为 ct ‖ tag（tag 附在密文尾部），因此必须先把 tag 拼到 ct 尾部再解密。
fn decrypt_token_blob(key: &[u8; 32], blob: &[u8]) -> Result<String, String> {
    let (iv, tag, ciphertext) = split_token_blob(blob)?;
    let mut ct_and_tag = Vec::with_capacity(ciphertext.len() + tag.len());
    ct_and_tag.extend_from_slice(ciphertext);
    ct_and_tag.extend_from_slice(tag);
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Nonce::from_slice(iv);
    let plaintext = cipher
        .decrypt(nonce, ct_and_tag.as_slice())
        .map_err(|e| format!("AES-256-GCM 解密失败（登录态损坏或非本机加密）: {e}"))?;
    let text = String::from_utf8(plaintext).map_err(|_| "解密结果不是合法 UTF-8".to_string())?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("解密结果不是合法 JSON: {e}"))?;
    value
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| "解密结果缺少 access_token".to_string())
}

/// base64 解码（兼容标准 / 无填充 / URL 安全三种字母表）。
fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    general_purpose::STANDARD
        .decode(text)
        .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(text))
        .or_else(|_| general_purpose::URL_SAFE.decode(text))
        .or_else(|_| general_purpose::URL_SAFE_NO_PAD.decode(text))
        .map_err(|e| format!("ssoTokenEnc base64 解码失败: {e}"))
}

/// whoami data 里的 userId 统一转字符串（兼容数字与字符串两种序列化）。
fn uid_from_whoami(data: &Value) -> Option<String> {
    data.get("userId").and_then(|v| {
        v.as_str()
            .map(String::from)
            .or_else(|| v.as_i64().map(|n| n.to_string()))
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    })
}

/// 解密本机登录态并调 whoami 补全身份（read_local_credential 的 async 内核；
/// import_local 直接复用，避免同步入口再起运行时）。
async fn read_local_credential_async() -> Result<Value, String> {
    let path = credential_path();
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "读取妙手登录态失败（{}）: {e}；请安装并登录妙手桌面端",
            path.display()
        )
    })?;
    let stored: Value = serde_json::from_str(&text)
        .map_err(|e| format!("解析 {} 失败: {e}", path.display()))?;
    let enc = stored
        .get("ssoTokenEnc")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{} 缺少 ssoTokenEnc（妙手尚未登录？）", path.display()))?;
    let blob = decode_base64(enc)?;
    let guid = read_machine_guid()?;
    let access_token = decrypt_token_blob(&derive_key(&guid), &blob)?;

    // whoami 校验 token 并补全 uid/name/mobile。妙手 token 非 JWT，离线
    // 拿不到身份，whoami 失败即整体失败（token 不可用）。
    let data = whoami(&access_token).await?;
    let uid = uid_from_whoami(&data).ok_or_else(|| "whoami 响应缺少 userId".to_string())?;
    let name = data
        .get("userName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mobile = data
        .get("mobile")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(json!({
        "uid": uid,
        "name": name,
        "mobile": mobile,
        "accessToken": access_token,
        "source": "miaoshou-local",
    }))
}

/// 解密本机妙手登录态，返回
/// `{uid, name, mobile, accessToken, source:"miaoshou-local"}`。
///
/// 同步公共入口：内部在专用线程上跑 current-thread 运行时执行 whoami
/// （避免在 async 上下文中直接 block_on 触发 panic）；调用方本线程会阻塞到
/// whoami 返回，这是同步 API 的固有语义。async 调用方请优先用 import_local。
pub fn read_local_credential() -> Result<Value, String> {
    let handle = std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("创建 Tokio 运行时失败: {e}"))?;
        rt.block_on(read_local_credential_async())
    });
    handle
        .join()
        .map_err(|_| "妙手登录态读取线程异常退出".to_string())?
}

// ---------------------------------------------------------------------------
// 账号池（~/.wb-switch/miaoshou_accounts.json）
// ---------------------------------------------------------------------------

/// 账号池文件路径。
pub fn accounts_path() -> PathBuf {
    store_dir().join(ACCOUNTS_FILE)
}

/// 账号池全部账号（JSON 数组）。
pub fn load_accounts() -> Vec<Value> {
    std::fs::read_to_string(accounts_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("accounts").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn save_accounts(accounts: &[Value]) -> Result<(), String> {
    let path = accounts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "accounts": accounts,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

/// 按 id 幂等写入账号池（保留原 addedAt，其余字段以新条目为准）。
fn upsert_account(entry: &Value) -> Result<Value, String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "账号条目缺少 id".to_string())?;
    if let Some(existing) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    {
        if let Some(map) = entry.as_object() {
            for (key, value) in map {
                if key != "addedAt" {
                    existing[key] = value.clone();
                }
            }
        }
        let snapshot = existing.clone();
        save_accounts(&accounts)?;
        Ok(snapshot)
    } else {
        let mut entry = entry.clone();
        if entry.get("addedAt").is_none() {
            entry["addedAt"] = json!(now_ms());
        }
        accounts.push(entry.clone());
        save_accounts(&accounts)?;
        Ok(entry)
    }
}

/// 手机号脱敏（138****5678）。
fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() >= 7 {
        format!(
            "{}****{}",
            chars[..3].iter().collect::<String>(),
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    } else {
        phone.to_string()
    }
}

/// 账号展示名：昵称 > 脱敏手机号 > uid，统一带「妙手 · 」前缀。
fn display_name(account: &Value) -> String {
    let name = account
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let mobile = account
        .get("mobile")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let uid = account
        .get("miaoshouUid")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !name.is_empty() {
        format!("妙手 · {name}")
    } else if !mobile.is_empty() {
        format!("妙手 · {}", mask_phone(mobile))
    } else {
        format!("妙手 · {uid}")
    }
}

/// 按 uid 移除账号池条目（同步，供管理路由调用）。
pub fn remove_account(uid: &str) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let before = accounts.len();
    accounts.retain(|a| {
        a.get("uid").and_then(Value::as_str) != Some(uid)
            && a.get("id").and_then(Value::as_str) != Some(&format!("miaoshou:{uid}"))
    });
    if accounts.len() == before {
        return false;
    }
    save_accounts(&accounts).is_ok()
}

/// 导入本机登录态到账号池（幂等，落盘 ~/.wb-switch/miaoshou_accounts.json）。
pub async fn import_local() -> Result<Value, String> {
    let credential = read_local_credential_async().await?;
    let uid = credential
        .get("uid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "妙手登录态缺少 uid".to_string())?
        .to_string();
    let access_token = credential
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "妙手登录态缺少 accessToken".to_string())?
        .to_string();
    let now = now_ms();
    let mut entry = json!({
        "id": format!("{PLATFORM_MIAOSHOU}:{uid}"),
        "miaoshouUid": uid,
        "accessToken": access_token,
        "name": credential.get("name").cloned().unwrap_or(json!("")),
        "mobile": credential.get("mobile").cloned().unwrap_or(json!("")),
        "source": "local_snapshot",
        "status": "ok",
        "addedAt": now,
        "updatedAt": now,
    });
    entry["label"] = json!(display_name(&entry));
    upsert_account(&entry)
}

/// 账号池全部账号（platform_checkin 集成形状）：
/// `[{id:"miaoshou:<uid>", email/nickname: 展示名, platform:"miaoshou",
/// miaoshouUid, accessToken}]`。
pub fn discover_accounts() -> Vec<Value> {
    let prefix = format!("{PLATFORM_MIAOSHOU}:");
    load_accounts()
        .into_iter()
        .filter_map(|account| {
            let uid = account
                .get("miaoshouUid")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    account
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| id.strip_prefix(&prefix))
                })?;
            let id = account
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| format!("{prefix}{uid}"));
            let display = account
                .get("label")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| display_name(&account));
            Some(json!({
                "id": id,
                "email": display,
                "nickname": display,
                "platform": PLATFORM_MIAOSHOU,
                "miaoshouUid": uid,
                "accessToken": account
                    .get("accessToken")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            }))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 反代：OpenAI 兼容对话（create → agent-stream → SSE/history）
// 协议 2026-09-21 实测确认（agent-team D 队员突破：流域名 ai.catpaw.meituan.com）
// ---------------------------------------------------------------------------

/// nocode 流域（建会话/发起生成）。
const NOCODE_BASE: &str = "https://nocode.cn";
/// agent 流域（SSE 实时流 + 会话历史）。⚠️ catpaw-agent.meituan.com 是 SDK 旧
/// 默认值，对外版（妙手桌面端）实际在 ai.catpaw.meituan.com（catDeskStore 配置）。
const AGENT_BASE: &str = "https://ai.catpaw.meituan.com";
/// 默认模型（App 内置默认）。
pub const DEFAULT_MODEL: &str = "claude-sonnet-4.6";
/// 非流式轮询 history 的上限（妙手单轮生成实测约 80s，留足余量）。
const HISTORY_POLL_TIMEOUT_SECS: u64 = 240;
/// SSE 主路已有正文但持续无新增的静默收尾阈值——正常工具轮（搜索等）的
/// 思考间隔远小于此值，超过基本可判定上游卡死。
const SSE_QUIET_CLOSE_SECS: u64 = 90;

/// SSE/长请求专用客户端（无总超时，只限连续无数据 300s）。
fn stream_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("failed to build miaoshou stream client")
    })
}

/// 模型目录缓存（10 分钟；与 qwen.rs 的桌面模型缓存策略一致）。
static MODEL_TYPES_CACHE: Mutex<Option<(Instant, Vec<Value>)>> = Mutex::new(None);
const MODEL_TYPES_TTL: Duration = Duration::from_secs(600);

/// 桌面端模型目录静态兜底（2026-09-21 从 `/api/agent/maas/model-types`
/// 实测拉取，9 个；上游不可达时用它，避免 /v1/models 空列表）。
fn fallback_model_types() -> Vec<Value> {
    [
        ("LongCat-2.0", "美团 LongCat-2.0（限时免费）", true),
        ("glm-5.3-flashx", "GLM-5.3-FlashX", true),
        ("glm-5.3-flash", "GLM-5.3-Flash", true),
        ("glm-5.3", "GLM-5.3", false),
        ("deepseek-v4-flash", "DeepSeek-V4-Flash", false),
        ("deepseek-v4-pro", "DeepSeek-V4-Pro", false),
        ("MiniMax-M3", "MiniMax-M3", true),
        ("kimi-k3", "Kimi-K3", true),
    ]
    .iter()
    .map(|(id, name, img)| {
        json!({
            "id": id,
            "object": "model",
            "created": 0,
            "owned_by": "miaoshou",
            "description": name,
            "supportsImages": img,
            "contextWindow": 1024000,
        })
    })
    .collect()
}

/// 拉取桌面端可选模型类型（`/api/agent/maas/model-types`）。
///
/// 该端点在 ai 流域（`AGENT_BASE`），**POST**，鉴权用 `X-Passport-Token`
/// （+ 同名 Cookie），body `{tenant:"CatDesk", scene:"CATX_APP", env:"EXTERNAL"}`。
/// 返回 `data[]`（`SelectableModelTypeVO`：modelTypeName/modelTypeId/
/// rateMultiplier/supportImage/parameterDefinitions）。
async fn fetch_model_types(token: &str) -> Result<Vec<Value>, String> {
    let body = json!({
        "tenant": "CatDesk",
        "scene": "CATX_APP",
        "env": "EXTERNAL",
    });
    let build = |c: &reqwest::Client| {
        c.post(format!("{AGENT_BASE}/api/agent/maas/model-types"))
            .header("X-Passport-Token", token.trim())
            .header("Cookie", format!("X-Passport-Token={}", token.trim()))
            .header("client-id", CLIENT_ID)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&body)
    };
    // 代理优先、直连兜底：本机 Clash fake-ip 模式下 *.meituan.com 解析到
    // 198.18.x.x，纯直连必失败（2026-09-21：动态拉取一度全部落到静态兜底）。
    let resp = match proxy_stream_client() {
        Some(pc) => match build(pc).send().await {
            Ok(r) => r,
            Err(e) if is_connect_error(&e) => build(stream_client())
                .send()
                .await
                .map_err(|e2| format!("请求失败: {e2}"))?,
            Err(e) => return Err(format!("请求失败: {e}")),
        },
        None => build(stream_client())
            .send()
            .await
            .map_err(|e| format!("请求失败: {e}"))?,
    };
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let value: Value = resp
        .json()
        .await
        .map_err(|e| format!("响应解析失败: {e}"))?;
    let arr = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "响应缺少 data 数组".to_string())?;
    let mut out = Vec::new();
    for m in arr {
        let Some(name) = m.get("modelTypeName").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() || name == "auto" {
            continue; // auto 是"自动选模型"占位，不是具体模型
        }
        let ei = m.get("extendedInfo").cloned().unwrap_or(Value::Null);
        // 上下文档位：取 context 参数的最大值（妙手按 token 数给档位）
        let mut ctx_max: Option<i64> = None;
        for pd in m.get("parameterDefinitions").and_then(Value::as_array).into_iter().flatten() {
            if pd.get("id").and_then(Value::as_str) != Some("context") {
                continue;
            }
            for v in pd.get("values").and_then(Value::as_array).into_iter().flatten() {
                if let Some(n) = v.get("value").and_then(Value::as_str).and_then(|s| s.parse::<i64>().ok()) {
                    ctx_max = Some(ctx_max.map_or(n, |c| c.max(n)));
                }
            }
        }
        out.push(json!({
            "id": name,
            "object": "model",
            "created": 0,
            "owned_by": "miaoshou",
            "description": m.get("description").and_then(Value::as_str).unwrap_or(name),
            "supportsImages": m.get("supportImage").and_then(Value::as_bool).unwrap_or(false),
            "contextWindow": ctx_max.unwrap_or(204800),
            "modelTypeId": m.get("modelTypeId").cloned().unwrap_or(Value::Null),
            "rateMultiplier": ei.get("rateMultiplier").cloned().unwrap_or(Value::Null),
            "marketingCopy": ei.get("marketingCopy").cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(out)
}

/// 模型目录（OpenAI /v1/models 形状）——动态拉取桌面端真实目录。
///
/// 取账号池第一个可用账号的 token 拉 `/api/agent/maas/model-types`；
/// 失败（无账号/网络/上游）时回退静态兜底表。缓存 10 分钟。
pub async fn models_payload_async() -> Value {
    let models = fetch_model_types_cached().await;
    json!({
        "object": "list",
        "data": models,
    })
}

/// 带缓存的模型目录拉取。
async fn fetch_model_types_cached() -> Vec<Value> {
    {
        let cache = MODEL_TYPES_CACHE.lock().unwrap();
        if let Some((at, list)) = cache.as_ref() {
            if at.elapsed() < MODEL_TYPES_TTL && !list.is_empty() {
                return list.clone();
            }
        }
    }
    // 取第一个有 token 的账号
    let token = load_accounts()
        .into_iter()
        .find_map(|a| {
            a.get("accessToken")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        });
    let Some(token) = token else {
        return fallback_model_types();
    };
    match fetch_model_types(&token).await {
        Ok(list) if !list.is_empty() => {
            let mut cache = MODEL_TYPES_CACHE.lock().unwrap();
            *cache = Some((Instant::now(), list.clone()));
            list
        }
        _ => fallback_model_types(),
    }
}

/// 绕过缓存直拉桌面端模型目录（webui「刷新」用）。
/// 成功返回 OpenAI /v1/models 形状；无账号或上游失败返回 None（由调用方报错）。
pub async fn fetch_models_payload_fresh() -> Option<Value> {
    let token = load_accounts().into_iter().find_map(|a| {
        a.get("accessToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    })?;
    let list = fetch_model_types(&token).await.ok()?;
    if list.is_empty() {
        return None;
    }
    // 成功直拉后顺带刷新缓存，让后续常规请求也拿到新数据
    let mut cache = MODEL_TYPES_CACHE.lock().unwrap();
    *cache = Some((Instant::now(), list.clone()));
    drop(cache);
    Some(json!({
        "object": "list",
        "data": list,
    }))
}

/// 模型目录（同步兼容入口：内部阻塞拉取，供非 async 调用方）。
pub fn models_payload() -> Value {
    let handle = std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        Some(rt.block_on(models_payload_async()))
    });
    handle
        .join()
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({
            "object": "list",
            "data": fallback_model_types(),
        }))
}

/// 建会话 → chatId。
pub async fn create_chat(token: &str, prompt: &str) -> Result<String, String> {
    let chat_id = uuid::Uuid::new_v4().to_string();
    let body = json!({
        "chatId": chat_id,
        "source": "catdesk",
        "prompt": prompt,
        "images": [],
        "files": [],
        "type": "COMMON",
        "techStackTemplateId": "default",
        "isAgent": true,
    });
    let headers: Vec<(&str, &str)> = api_headers(token.trim()).to_vec();
    let resp = post_json_with_fallback(
        &format!("{NOCODE_BASE}/api/chat/create"),
        &headers,
        &body,
        http_client(),
    )
    .await
    .map_err(|e| format!("妙手建会话失败: {e}"))?;
    let value: Value = resp
        .json()
        .await
        .map_err(|e| format!("妙手建会话响应解析失败: {e}"))?;
    if value.get("code").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(format!(
            "妙手建会话被拒: {}",
            value.get("msg").and_then(Value::as_str).unwrap_or("未知错误")
        ));
    }
    Ok(chat_id)
}

/// 发起生成 → conversationId（生成在服务端异步进行，约 80s）。
pub async fn kick_agent_stream(
    token: &str,
    chat_id: &str,
    prompt: &str,
    model: &str,
) -> Result<String, String> {
    let body = json!({
        "chatId": chat_id,
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": prompt}],
            "createTime": chrono_like_iso_now(),
        }],
        "files": [],
        "urls": [],
        "model": model,
        "manualToolName": "",
        "toolMessageId": "",
        "isAgent": true,
        "currentPath": "/",
        "techStackTemplateId": "default",
        "sourcePlatform": "web",
        "isImportProject": false,
        "frontCreateTime": now_ms(),
    });
    let headers: Vec<(&str, &str)> = api_headers(token.trim()).to_vec();
    let resp = post_json_with_fallback(
        &format!("{NOCODE_BASE}/api/chat/agent-stream"),
        &headers,
        &body,
        http_client(),
    )
    .await
    .map_err(|e| format!("妙手发起生成失败: {e}"))?;
    let value: Value = resp
        .json()
        .await
        .map_err(|e| format!("妙手发起生成响应解析失败: {e}"))?;
    let conv = value
        .pointer("/data/conversationId")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| {
            format!(
                "妙手发起生成被拒: {}",
                value
                    .pointer("/data/errorMessage")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("msg").and_then(Value::as_str))
                    .unwrap_or("未知错误")
            )
        })?;
    Ok(conv)
}

/// SSE 实时流连接（ai 流域，X-Passport-Token 头 + 同名 Cookie）。
/// 返回上游响应供 server 层泵取转换；帧格式见 convert_sse_frames 的测试样本。
pub async fn stream_connect(token: &str, conversation_id: &str) -> Result<reqwest::Response, String> {
    let body = json!({
        "timestamp": now_ms(),
        "conversationId": conversation_id,
        "messageIndex": 0,
        "incrementalOnly": true,
    });
    let build = |c: &reqwest::Client| {
        c.post(format!("{AGENT_BASE}/api/agent/stream/connect"))
            .header("X-Passport-Token", token.trim())
            .header("Cookie", format!("X-Passport-Token={}", token.trim()))
            .header("client-id", CLIENT_ID)
            .header("Accept", "text/event-stream")
            .header("Content-Type", "application/json")
            .header("Cache-Control", "no-cache")
            .json(&body)
    };
    // 代理优先（fake-ip 域名必须走代理），连接类失败回落直连
    let resp = if let Some(pc) = proxy_stream_client() {
        match build(pc).send().await {
            Ok(r) => r,
            Err(e) if is_connect_error(&e) => build(stream_client())
                .send()
                .await
                .map_err(|e2| format!("妙手 SSE 连接失败: {e2}"))?,
            Err(e) => return Err(format!("妙手 SSE 连接失败: {e}")),
        }
    } else {
        build(stream_client())
            .send()
            .await
            .map_err(|e| format!("妙手 SSE 连接失败: {e}"))?
    };
    if !resp.status().is_success() {
        return Err(format!("妙手 SSE 连接返回 HTTP {}", resp.status()));
    }
    Ok(resp)
}

/// 会话历史（ai 流域）：返回 data.items（user + assistant 全文）。
pub async fn conversation_history(token: &str, conversation_id: &str) -> Result<Value, String> {
    let url = format!(
        "{AGENT_BASE}/api/agent/conversation/history?page=1&pageSize=500&conversationId={conversation_id}&size=500"
    );
    let headers: [(&str, &str); 2] = [("X-Passport-Token", token.trim()), ("Accept", "application/json")];
    let resp = get_with_fallback(&url, &headers, stream_client())
        .await
        .map_err(|e| format!("妙手会话历史请求失败: {e}"))?;
    let value: Value = resp
        .json()
        .await
        .map_err(|e| format!("妙手会话历史解析失败: {e}"))?;
    Ok(value)
}

/// 从 history 响应聚合 assistant 全文（content[].text 拼接）。
/// ⚠️ 条目用 `type` 字段（"user"/"assistant"）区分角色，不是 `role`——
/// 2026-09-21 实测踩坑：按 role 取会永远拿到空串。
pub fn extract_history_text(history: &Value) -> String {
    let mut out = String::new();
    if let Some(items) = history.pointer("/data/items").and_then(Value::as_array) {
        for item in items {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if kind != "assistant" {
                continue;
            }
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        out.push_str(text);
                    }
                }
            }
        }
    }
    out
}

/// 从 history 提取最后一条非空 assistant 的正文与 finished 标志。
/// history 兜底路径专用：finished 只表示「该条消息写完」，不代表会话结束，
/// 所以兜底还要靠稳定窗口近似完成判定（见 chat_completions）。
fn extract_last_assistant(history: &Value) -> Option<(String, bool)> {
    let items = history.pointer("/data/items")?.as_array()?;
    let mut last: Option<(String, bool)> = None;
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let mut t = String::new();
        if let Some(parts) = item.get("content").and_then(Value::as_array) {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    t.push_str(text);
                }
            }
        }
        if t.trim().is_empty() {
            continue;
        }
        let finished = item.get("finished").and_then(Value::as_bool).unwrap_or(false);
        last = Some((t, finished));
    }
    last
}

/// 从 history 提取 usage（assistant 条目的 totalUsage，无则零值）。
fn extract_history_usage(history: &Value) -> Value {
    if let Some(items) = history.pointer("/data/items").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            if let Some(u) = item.get("totalUsage") {
                let pick = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
                return json!({
                    "prompt_tokens": pick("inputTokens").max(pick("promptTokens")),
                    "completion_tokens": pick("outputTokens").max(pick("completionTokens")),
                    "total_tokens": pick("totalTokens"),
                });
            }
        }
    }
    json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})
}

/// OpenAI 兼容非流式对话：create → kick → 轮询 history 聚合全文。
pub async fn chat_completions(account: &Value, body: &Value) -> Result<Value, String> {
    let token = account
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "妙手账号缺少 accessToken（请重新导入登录态）".to_string())?
        .to_string();
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "请求缺少 messages".to_string())?;
    let prompt = flatten_messages(messages);
    if prompt.trim().is_empty() {
        return Err("请求 messages 为空".to_string());
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_MODEL);

    let chat_id = create_chat(&token, &prompt).await?;
    let conversation_id = kick_agent_stream(&token, &chat_id, &prompt, model).await?;

    let deadline = Instant::now() + Duration::from_secs(HISTORY_POLL_TIMEOUT_SECS);
    let mut text = String::new();
    let mut usage = json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0});

    // ── 主路：SSE 聚合到真正的结束信号 ──
    // 🔴 2026-09-25 修复「中间态被当最终答案」：此前轮询 history 一出现非空正文
    // 就返回，而 history 条目的 finished 只表示「该条消息写完」——多轮 agent
    // （先搜索再作答）第一轮的过程叙述（实测如「搜索结果比较零散，我再获取
    // 更权威的…：」）会被当成最终答案，真正的结论被丢弃。会话级的结束信号
    // 只有 SSE 的 statusUpdate 终止帧，因此非流式也必须按流聚合到收尾。
    let mut sse_done = false;
    match stream_connect(&token, &conversation_id).await {
        Ok(mut resp) => {
            let mut line_buf = String::new();
            let mut carry: Vec<u8> = Vec::new();
            let mut last_text_at = Instant::now();
            while Instant::now() < deadline && !sse_done {
                match tokio::time::timeout(Duration::from_secs(6), resp.chunk()).await {
                    Ok(Ok(Some(chunk))) => {
                        let decoded =
                            crate::modules::glm_anthropic::decode_stream_chunk(&mut carry, &chunk);
                        line_buf.push_str(&decoded);
                        while let Some(pos) = line_buf.find('\n') {
                            let line: String = line_buf.drain(..=pos).collect();
                            // 帧前缀实测是 data:（无空格），trim 兼容带空格变体
                            let Some(payload) = line
                                .trim_end_matches(['\r', '\n'])
                                .strip_prefix("data:")
                                .map(str::trim)
                                .filter(|p| !p.is_empty())
                            else {
                                continue;
                            };
                            if payload == "[DONE]" {
                                sse_done = true;
                                break;
                            }
                            let Ok(frame) = serde_json::from_str::<Value>(payload) else {
                                continue;
                            };
                            if frame.get("statusUpdate").is_some() {
                                sse_done = true;
                                break;
                            }
                            let Some(agent) = frame.pointer("/headlessRemoteAgentResp") else {
                                continue;
                            };
                            // 历史回放是本轮之前的内容，非流式只聚合实时增量
                            if agent.get("isHistoryMessage").and_then(Value::as_bool) == Some(true)
                            {
                                continue;
                            }
                            if let Some(delta) = agent
                                .pointer("/message/content")
                                .and_then(Value::as_array)
                                .map(|parts| {
                                    parts
                                        .iter()
                                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                                        .collect::<String>()
                                })
                                .filter(|t| !t.is_empty())
                            {
                                text.push_str(&delta);
                                last_text_at = Instant::now();
                            }
                        }
                    }
                    Ok(Ok(None)) => {
                        // 连接正常关闭 = 上游认为生成结束
                        sse_done = !text.trim().is_empty();
                        break;
                    }
                    Ok(Err(_)) => break, // 流中断 → 落 history 兜底
                    Err(_) => {
                        // 分片读超时：已有正文但持续 90s 无新增，视为上游卡死，
                        // 按已有内容收尾（正常工具轮的思考间隔远小于此值）
                        if !text.trim().is_empty()
                            && last_text_at.elapsed() >= Duration::from_secs(SSE_QUIET_CLOSE_SECS)
                        {
                            sse_done = true;
                            break;
                        }
                    }
                }
            }
        }
        Err(_) => {} // SSE 不可用 → history 兜底
    }

    // ── 兜底：history 轮询（SSE 中断/不可用/零正文时）──
    // 只求可用、不保证多轮完整性：取最后一条 assistant，连续 4 轮（8s）无增长
    // 视为完成。多轮 agent 的中间叙述在此路径下仍可能被提前取走——已知妥协。
    if text.trim().is_empty() {
        let mut prev = String::new();
        let mut stable = 0u32;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(2)).await;
            match conversation_history(&token, &conversation_id).await {
                Ok(history) => {
                    if let Some((t, _finished)) = extract_last_assistant(&history) {
                        if !t.is_empty() && t == prev {
                            stable += 1;
                        } else {
                            stable = 0;
                        }
                        prev = t;
                        if stable >= 4 {
                            text = prev.clone();
                            usage = extract_history_usage(&history);
                            break;
                        }
                    }
                }
                Err(_) => {}
            }
        }
        if text.trim().is_empty() && !prev.trim().is_empty() {
            // 到期仍不稳定：带上最后一次看到的非空内容（好过空手报错）
            text = prev;
        }
    }

    if text.trim().is_empty() {
        return Err("妙手生成超时未返回内容（SSE 聚合与 history 兜底均无正文）".to_string());
    }
    // SSE 主路拿不到计量：结束后补拉一次 history 取 usage（best-effort）
    if usage.get("total_tokens").and_then(Value::as_i64) == Some(0) {
        if let Ok(history) = conversation_history(&token, &conversation_id).await {
            usage = extract_history_usage(&history);
        }
    }
    Ok(json!({
        "id": format!("chatcmpl-miaoshou-{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": now_ms() / 1000,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop",
        }],
        "usage": usage,
        "miaoshou_channel": "agent-history",
    }))
}

/// 多轮 messages 拍平为单条 prompt（妙手 agent-stream 是单消息形态）。
/// 多轮 messages 拍平为单条 prompt（妙手 agent-stream 是单消息形态）。
/// 单条 user 消息原样透传（不加角色前缀——上游会包 #user-query 信封，
/// 前缀反而污染模型输入）；多轮才加 [role] 分隔。
pub fn flatten_messages(messages: &[Value]) -> String {
    let mut parts: Vec<(String, String)> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user").to_string();
        let content = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if content.trim().is_empty() {
            continue;
        }
        parts.push((role, content));
    }
    if parts.len() == 1 && parts[0].0 == "user" {
        return parts.into_iter().next().map(|(_, c)| c).unwrap_or_default();
    }
    let mut out = String::new();
    for (role, content) in parts {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("[{role}]\n{content}"));
    }
    out
}

/// ISO8601 当前时间（秒精度 + Z，妙手 createTime 格式）。
fn chrono_like_iso_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 手写 UTC 格式化（避免引入 chrono 依赖）
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 粗略日期（够用即可，服务端不严格校验）
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000Z")
}

/// Unix 天数 → (年, 月, 日)（Howard Hinnant 算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_machine_guid_from_reg_output() {
        // reg query 真实输出样例（Windows；值取自注册表 GUID 格式）。
        let sample = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography\r\n    MachineGuid    REG_SZ    4C4C4544-0036-5910-8054-B7C04F463833\r\n";
        assert_eq!(
            parse_machine_guid(sample).as_deref(),
            Some("4c4c4544-0036-5910-8054-b7c04f463833")
        );
        // 无 MachineGuid 行 / REG_SZ 后为空 → None。
        assert_eq!(parse_machine_guid("HKEY_LOCAL_MACHINE\\X\r\n"), None);
        assert_eq!(parse_machine_guid("    MachineGuid    REG_SZ    \r\n"), None);
    }

    #[test]
    fn token_blob_layout_split() {
        // iv(12) ‖ tag(16) ‖ ct，喂假数据验证切分与长度边界。
        let mut blob = Vec::new();
        blob.extend_from_slice(&[0xAAu8; 12]); // iv
        blob.extend_from_slice(&[0xBBu8; 16]); // authTag
        blob.extend_from_slice(&[0xCCu8; 40]); // ciphertext
        let (iv, tag, ct) = split_token_blob(&blob).expect("合法 blob 应切分成功");
        assert_eq!(iv.len(), 12);
        assert_eq!(tag.len(), 16);
        assert_eq!(ct.len(), 40);
        assert!(iv.iter().all(|b| *b == 0xAA));
        assert!(tag.iter().all(|b| *b == 0xBB));
        assert!(ct.iter().all(|b| *b == 0xCC));
        // 过短（iv + tag 都放不下）→ Err。
        assert!(split_token_blob(&[0u8; 27]).is_err());
    }

    #[test]
    fn gcm_decrypt_roundtrip_with_key_derivation() {
        // 用 aes-gcm 自带加密构造 iv‖tag‖ct blob，离线验证 derive_key +
        // decrypt_token_blob 完整链路（不依赖本机注册表）。
        use aes_gcm::aead::{AeadCore, OsRng};
        let guid = "4c4c4544-0036-5910-8054-b7c04f463833";
        let key = derive_key(guid);
        // key 必须与 SHA256(guid + ":catpaw-desk-token-v2") 一致。
        let mut hasher = Sha256::new();
        hasher.update(format!("{guid}{KEY_SUFFIX}").as_bytes());
        assert_eq!(key.as_slice(), hasher.finalize().as_slice());

        let plaintext = br#"{"access_token":"tk-abc123","modified_at":1790000000000}"#;
        let cipher = Aes256Gcm::new(GenericArray::from_slice(&key));
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        // aes-gcm 输出即 ct‖tag，按妙手布局重组为 iv‖tag‖ct。
        let ct_and_tag = cipher.encrypt(&nonce, &plaintext[..]).expect("encrypt");
        let (ct, tag) = ct_and_tag.split_at(ct_and_tag.len() - 16);
        let mut blob = Vec::new();
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(tag);
        blob.extend_from_slice(ct);

        assert_eq!(decrypt_token_blob(&key, &blob).unwrap(), "tk-abc123");
        // 篡改一个密文字节 → GCM 认证失败。
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        assert!(decrypt_token_blob(&key, &blob).is_err());
    }

    #[test]
    fn discover_accounts_empty_without_accounts_file() {
        // 本机未导入过妙手（无账号池文件）时返回空列表。
        if accounts_path().exists() {
            return; // 环境已有导入数据，跳过以保持测试稳定。
        }
        assert!(discover_accounts().is_empty());
    }

    #[test]
    fn extract_last_assistant_takes_last_nonempty_and_finished_flag() {
        // 多轮场景：中间叙述（finished=true）之后还有未写完的最终答案，
        // 必须取「最后一条非空 assistant」，而不是第一条或全文拼接。
        let history = json!({
            "data": { "items": [
                { "type": "user", "content": [{ "type": "text", "text": "问天气" }] },
                { "type": "assistant", "finished": true,
                  "content": [{ "type": "text", "text": "搜索结果比较零散，我再获取更权威的：" }] },
                { "type": "assistant", "finished": false,
                  "content": [{ "type": "text", "text": "长沙晴 25℃" }] }
            ] }
        });
        let (text, finished) = extract_last_assistant(&history).unwrap();
        assert_eq!(text, "长沙晴 25℃");
        assert!(!finished);
    }

    #[test]
    fn extract_last_assistant_none_when_only_user_or_empty() {
        assert_eq!(extract_last_assistant(&json!({ "data": { "items": [] } })), None);
        assert_eq!(
            extract_last_assistant(&json!({ "data": { "items": [
                { "type": "user", "content": [{ "type": "text", "text": "hi" }] }
            ] } })),
            None
        );
    }
}
