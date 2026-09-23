//! # 注册机（Grok 账号注册专用 Chrome 实例）
//!
//! **注册机**是 Grok 融合网关所驱动的那个**专用 Chrome 实例**的称呼（术语已由用户记入
//! CHANGELOG，不得改名）：独立 profile `~/.wb-switch/grok-chrome-profile`（与日常用的
//! Chrome 完全隔离）、固定 CDP 调试端口 `19233`、非 headless 常驻窗口、打开的页面是
//! `https://grok.com`（未登录时该页即注册页）。浏览器生命周期与 CDP 客户端复用
//! `grok_sso` 模块（同一实例，多个功能共用）。
//!
//! ## 工作原理
//!
//! 1. **手动启动**：用户点击「打开注册机」—— 这是注册机唯一的启动方式，网关不会在
//!    开机、后台任务或任何计划任务中自动拉起它。
//! 2. **打开注册页**：注册机以调试模式启动 Chrome 并打开 `https://grok.com`；未登录时
//!    该页面就是注册页。
//! 3. **人工注册**：在注册机窗口里按 grok.com 的流程完成新账号注册（填邮箱、验证码等
//!    由用户本人操作；注册机不代填、不自动提交）。
//! 4. **抓取成果**：注册完成后点「抓取注册成果」，网关经 CDP 的 `Network.getCookies`
//!    读取该实例的 sso / sso-rw 等 cookie（含 httpOnly），拼成账号凭证写入 Grok
//!    账号池（固定账号 ID `grok_register`）。
//! 5. **入池即用**：该账号随即参与 web 通道的轮询调度；之后每次同步/保活都会经同一
//!    实例续期，无需重复注册。
//!
//! ## 设计纪律
//!
//! - **零自动触发**：本模块不注册任何后台任务（无定时同步、无保活、无体检会启动它）；
//!   `status()` 是只读探测 —— 不启动浏览器、不拉起进程，只做 CDP 端口探测 + 读 token
//!   文件 + 读账号池。由本模块 `mod tests` 的守卫测试保证这一点。
//! - **会话隔离**：专用 profile 不影响日常 Chrome；多个功能共用同一实例，手动「立即
//!   同步」会把当前登录态写回 autosync 账号。
//! - **进程精准**：「关闭注册机」只结束命令行带该 profile 目录的 Chrome 进程，绝不按
//!   进程名无差别杀 chrome（见 `kill_register_browser`）。
//! - **入池去重**：以固定 ID `grok_register` upsert 写账号池，刻意不用
//!   `grok::import_account` —— 它每次生成随机 ID `grok_web_<uuid8>` 且不去重，反复
//!   抓取会往池里堆重复账号。
//!
//! 面向用户的同款说明见 `workbuddy-switch/docs/grok-register-machine.md` 与 GrokPage
//! 的「工作原理」折叠面板。
//!
//! 调试日志用 `eprintln!("[grok-register] ...")`；注意 launcher 会丢弃后端 stderr，
//! 因此面向用户的信息必须体现在 `status()` / `open()` / `capture()` / `close()`
//! 返回体的 `message` 字段里。

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::grok_sso;

/// 注册页入口：grok.com 未登录时该页即注册页。
pub const REGISTER_URL: &str = "https://grok.com";
/// 抓取后写入账号池的固定账号 ID（upsert 语义，反复抓取不产生重复账号）。
pub const POOL_ACCOUNT_ID: &str = "grok_register";
/// 注册机账号在池里的显示名。
const POOL_ACCOUNT_NAME: &str = "注册机注册账号";
/// 专用 Chrome 的 CDP 调试端口（与 `grok_sso` 共用同一实例）。
const CDP_PORT: u16 = 19233;
/// 专用 profile 目录名（与 `grok_sso` 默认值一致；进程匹配也用它）。
const PROFILE_DIR_NAME: &str = "grok-chrome-profile";
/// 恒 false 的说明文案（写进 status 响应，UI 原样展示）。
const AUTO_TRIGGER_NOTE: &str =
    "注册机无任何启动/后台自动触发逻辑；仅在你手动点击「打开注册机」后才会启动专用浏览器";

// ---------------------------------------------------------------------------
// 状态文件（~/.wb-switch/grok_register_state.json）
// ---------------------------------------------------------------------------

/// 注册机状态文件路径。
fn state_path() -> PathBuf {
    store_dir().join("grok_register_state.json")
}

/// 全新默认状态：`idle` / 计数 0 / 时间戳 null。
fn default_state() -> Value {
    json!({
        "phase": "idle",
        "manualOpenCount": 0,
        "lastOpenedAt": Value::Null,
        "lastCapturedAt": Value::Null,
        "poolAccountId": Value::Null,
        "updatedAt": 0,
    })
}

/// 读状态文件；文件不存在/损坏时返回默认态，已有字段逐个覆盖（缺字段自动补齐）。
fn load_state_from(path: &Path) -> Value {
    let mut state = default_state();
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
            if let Some(obj) = parsed.as_object() {
                for (key, value) in obj {
                    state[key] = value.clone();
                }
            }
        }
    }
    state
}

fn load_state() -> Value {
    load_state_from(&state_path())
}

/// 读-改-写状态文件（`atomic_write` 原子落盘，字段 camelCase）。
fn update_state(fields: &[(&str, Value)]) -> Result<(), String> {
    let mut state = load_state();
    if let Some(obj) = state.as_object_mut() {
        for (key, value) in fields {
            obj.insert((*key).to_string(), value.clone());
        }
    }
    save_state(&state)
}

fn save_state(state: &Value) -> Result<(), String> {
    let mut next = state.clone();
    if let Some(obj) = next.as_object_mut() {
        obj.insert("updatedAt".to_string(), json!(now_ms()));
    }
    let content = serde_json::to_string_pretty(&next).map_err(|e| e.to_string())?;
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

fn phase_of(state: &Value) -> String {
    state
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or("idle")
        .to_string()
}

fn count_of(state: &Value) -> u64 {
    state
        .get("manualOpenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn ts_of(state: &Value, key: &str) -> Option<i64> {
    state.get(key).and_then(Value::as_i64)
}

// ---------------------------------------------------------------------------
// 只读探测辅助（status() 用；不启动任何进程）
// ---------------------------------------------------------------------------

/// 直连本机 CDP 的同步探测：读 `GET /json/version`，有 `Browser` 字段即存活。
///
/// 刻意不用 reqwest：① `status()` 是同步函数，而 reqwest 只有 async 形态；② 127.0.0.1
/// 请求必须绕过系统代理（否则被 Clash 吃掉的坑见 `grok_sso.rs:106-117`
/// `cdp_http_client` 的注释）。手写最小 HTTP/1.1 请求既同步又天然无代理。
fn cdp_alive(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(600)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(900)));
    let request = format!(
        "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    // 健康特征与 grok_sso 的 cdp_alive 一致：/json/version 里有 Browser 字段。
    let mut buf = [0u8; 4096];
    let mut filled = 0usize;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                if String::from_utf8_lossy(&buf[..filled]).contains("\"Browser\"") {
                    return true;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf[..filled]).contains("\"Browser\"")
}

/// 注册机 profile 目录：读 `grok_sso` 配置里的 `profile_dir`，为空时用
/// `store_dir()/grok-chrome-profile`（与 `grok_sso.rs:148-153` 的默认值一致）。
///
/// 刻意不走 `grok_sso::load_config()`：那个函数在配置文件缺失时会**写盘**，而
/// `status()` 必须严格只读。
fn profile_dir() -> PathBuf {
    let configured = std::fs::read_to_string(store_dir().join("grok_sso_config.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| {
            v.get("profile_dir")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        store_dir().join(PROFILE_DIR_NAME)
    } else {
        PathBuf::from(trimmed)
    }
}

/// 从 `grok_sso` 的 token 文件推登录态：
/// `ssoPresent || ssoRwPresent` → `logged_in`；文件存在但无 sso → `not_logged_in`；
/// 无文件 → `unknown`。同时给出 sso 预览（前 8 位 + "…"）与 cookie 数。
fn login_from_token(token: Option<&Value>) -> (&'static str, Option<String>, Option<u64>) {
    let Some(v) = token else {
        return ("unknown", None, None);
    };
    let sso_present = v
        .get("ssoPresent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sso_rw_present = v
        .get("ssoRwPresent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let preview = v
        .get("sso")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!("{}…", s.chars().take(8).collect::<String>()));
    let cookie_count = v.get("cookieCount").and_then(Value::as_u64);
    let login_state = if sso_present || sso_rw_present {
        "logged_in"
    } else {
        "not_logged_in"
    };
    (login_state, preview, cookie_count)
}

/// 账号池里指定 ID 的当前 status（未入池返回 None）。
fn pool_account_status(account_id: &str) -> Option<String> {
    crate::modules::grok::load_accounts()
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(account_id))
        .and_then(|a| a.get("status").and_then(Value::as_str).map(String::from))
}

/// 人话 message：按 phase × loginState × browserRunning 组合给中文提示。
fn message_for(phase: &str, login_state: &str, browser_running: bool) -> String {
    if !browser_running {
        return match phase {
            "captured" => {
                "注册机已关闭（成果已入池）。再次点击「打开注册机」可复用同一 profile 继续注册"
                    .to_string()
            }
            _ => {
                "注册机待启动：点击「打开注册机」将在专用 Chrome 中打开 grok.com 注册页".to_string()
            }
        };
    }
    match login_state {
        "logged_in" => {
            "注册机运行中且已登录 grok.com：点「抓取注册成果」即可把当前登录态写入账号池"
                .to_string()
        }
        "not_logged_in" => {
            "注册机运行中、尚未登录：在窗口内完成 grok.com 注册（邮箱验证等由你本人操作），完成后点「抓取注册成果」"
                .to_string()
        }
        _ => "注册机运行中：登录态探测中（token 文件可能尚未生成）".to_string(),
    }
}

/// 把状态字段并入返回值：open/capture/close 都返回「状态体 + ok/message」，
/// 前端按 `GrokRegisterStatus` 解析（见 `src/lib/types.ts`）。
fn with_status(extra: Value) -> Value {
    let mut base = status();
    if let (Some(dst), Some(src)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in src {
            dst.insert(key.clone(), value.clone());
        }
    }
    base
}

// ---------------------------------------------------------------------------
// CDP target 列表（异步；open() 用）
// ---------------------------------------------------------------------------

/// 直连本机 CDP 的异步 client：强制绕过系统代理（reqwest 默认会读 Windows IE 代理
/// 设置，127.0.0.1 请求被送进 Clash 后 /json 无法返回；写法对齐
/// `grok_sso.rs:106-117`）。
fn cdp_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build cdp http client")
    })
}

/// 读 CDP target 列表（`GET /json`）。
async fn cdp_targets() -> Result<Vec<Value>, String> {
    cdp_http_client()
        .get(format!("http://127.0.0.1:{CDP_PORT}/json"))
        .send()
        .await
        .map_err(|e| format!("CDP /json 请求失败: {e}"))?
        .json::<Vec<Value>>()
        .await
        .map_err(|e| format!("CDP /json 解析失败: {e}"))
}

/// target 列表里是否已有 grok.com 页面（复用同一实例时不必重复开 tab）。
fn has_grok_page(list: &[Value]) -> bool {
    list.iter().any(|t| {
        t.get("type").and_then(Value::as_str) == Some("page")
            && t.get("url")
                .and_then(Value::as_str)
                .map(|u| u.starts_with(REGISTER_URL))
                .unwrap_or(false)
    })
}

/// 新开一个 grok.com tab（Chrome 111+ 必须是 `PUT /json/new`，老版本回退 `GET`；
/// 写法对齐 `grok_sso.rs:354-369`）。
async fn open_register_tab() {
    let url = format!("http://127.0.0.1:{CDP_PORT}/json/new?{REGISTER_URL}");
    if let Ok(resp) = cdp_http_client()
        .put(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        if resp.status().is_success() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }
    }
    let _ = cdp_http_client()
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
}

// ---------------------------------------------------------------------------
// 公开接口（API 层薄封装调用）
// ---------------------------------------------------------------------------

/// 只读状态探测（`GET /api/grok/register/status`）。
///
/// 恒 `autoTrigger=false`；不启动浏览器、不拉起进程、不抓 cookie，只探测 CDP 端口 +
/// 读 token 文件 + 读账号池。字段缺失时给 null/0/false，不省略键。
pub fn status() -> Value {
    let state = load_state();
    let phase = phase_of(&state);
    let browser_running = cdp_alive(CDP_PORT);
    let (login_state, sso_preview, cookie_count) =
        login_from_token(grok_sso::read_token_raw().as_ref());
    let pool_status = pool_account_status(POOL_ACCOUNT_ID);
    let pool_account_id = match &pool_status {
        Some(_) => Some(POOL_ACCOUNT_ID.to_string()),
        None => state
            .get("poolAccountId")
            .and_then(Value::as_str)
            .map(String::from),
    };
    json!({
        "phase": phase,
        "browserRunning": browser_running,
        "cdpPort": CDP_PORT,
        "profileDir": profile_dir().display().to_string(),
        "registerUrl": REGISTER_URL,
        "manualOpenCount": count_of(&state),
        "lastOpenedAt": ts_of(&state, "lastOpenedAt"),
        "lastCapturedAt": ts_of(&state, "lastCapturedAt"),
        "loginState": login_state,
        "ssoPreview": sso_preview,
        "cookieCount": cookie_count,
        "poolAccountId": pool_account_id,
        "poolAccountStatus": pool_status,
        "autoTrigger": false,
        "autoTriggerNote": AUTO_TRIGGER_NOTE,
        "message": message_for(&phase, login_state, browser_running),
    })
}

/// 手动打开注册机（`POST /api/grok/register/open`）：UI 入口按钮唯一的启动方式。
///
/// 复用 `grok_sso` 的专用 Chrome 实例（`ensure_browser`，已运行时直接复用），无
/// grok.com tab 时新开一个；状态落 `phase=open` + `manualOpenCount+=1` +
/// `lastOpenedAt`。
pub async fn open() -> Value {
    let cfg = grok_sso::load_config();
    let port = match grok_sso::ensure_browser(&cfg).await {
        Ok(port) => port,
        Err(error) => {
            eprintln!("[grok-register] 打开失败: {error}");
            return with_status(json!({
                "ok": false,
                "status": "error",
                "error": error,
                "message": "打开注册机失败：请检查配置里的 chrome_path，或改用「打开登录窗口」",
            }));
        }
    };
    // 已有 grok.com tab 就不重复开（重复手动打开幂等）。
    let need_tab = match cdp_targets().await {
        Ok(list) => !has_grok_page(&list),
        Err(_) => true,
    };
    if need_tab {
        open_register_tab().await;
    }
    let open_count = count_of(&load_state()) + 1;
    let saved = update_state(&[
        ("phase", json!("open")),
        ("manualOpenCount", json!(open_count)),
        ("lastOpenedAt", json!(now_ms())),
    ]);
    if let Err(error) = &saved {
        eprintln!("[grok-register] 状态落盘失败: {error}");
    }
    let suffix = match &saved {
        Ok(()) => String::new(),
        Err(error) => format!("（状态落盘失败：{error}）"),
    };
    with_status(json!({
        "ok": true,
        "port": port,
        "openedTab": need_tab,
        "message": format!("注册机已打开：在专用 Chrome 窗口里按 grok.com 流程完成新账号注册，完成后点「抓取注册成果」{suffix}"),
    }))
}

/// 抓取注册成果并入池（`POST /api/grok/register/capture`）。
///
/// 经 CDP `Network.getCookies` 读该实例全部 grok/x.ai cookie，无 sso 且无 sso-rw 时
/// 干净返回 `not_registered`；否则以固定 ID `grok_register` upsert 写账号池。
/// 刻意不调 `ensure_browser` —— 抓取是纯读取，注册机没开时就老实失败。
pub async fn capture() -> Value {
    let cookies = match grok_sso::fetch_grok_cookies().await {
        Ok(cookies) => cookies,
        Err(error) => {
            eprintln!("[grok-register] 抓取失败: {error}");
            return with_status(json!({
                "ok": false,
                "status": "error",
                "error": error,
                "message": "抓取失败：注册机未启动或 CDP 无响应，请先点「打开注册机」",
            }));
        }
    };
    let (sso_value, cookie_string) = collect_grok_cookies(&cookies);
    if sso_value.is_empty() {
        return with_status(json!({
            "ok": false,
            "status": "not_registered",
            "cookieCount": cookies.len(),
            "message": "未抓到 sso / sso-rw cookie：注册机里还没有完成注册，请先在窗口内完成 grok.com 注册",
        }));
    }
    if let Err(error) = feed_pool(&sso_value, &cookie_string) {
        eprintln!("[grok-register] 入池失败: {error}");
        return with_status(json!({
            "ok": false,
            "status": "error",
            "error": error,
            "cookieCount": cookies.len(),
            "message": "抓取成功但写入账号池失败，请重试",
        }));
    }
    if let Err(error) = update_state(&[
        ("phase", json!("captured")),
        ("lastCapturedAt", json!(now_ms())),
        ("poolAccountId", json!(POOL_ACCOUNT_ID)),
    ]) {
        eprintln!("[grok-register] 状态落盘失败: {error}");
    }
    with_status(json!({
        "ok": true,
        "status": "ok",
        "accountId": POOL_ACCOUNT_ID,
        "accountName": POOL_ACCOUNT_NAME,
        "cookieCount": cookies.len(),
        "message": "注册成果已抓取并入池：账号参与 web 通道轮询调度，无需重复注册",
    }))
}

/// 关闭注册机（`POST /api/grok/register/close`）：只结束本实例的 Chrome 进程。
///
/// 杀完后 `phase` 置回 `idle`（保留 `manualOpenCount` 历史）。
pub async fn close() -> Value {
    // 探测结果只用于回报文案；杀进程本身无条件执行（无命中时幂等无害）。
    let was_running = cdp_alive(CDP_PORT);
    let killed = kill_register_browser();
    if let Err(error) = update_state(&[("phase", json!("idle"))]) {
        eprintln!("[grok-register] 状态落盘失败: {error}");
        return with_status(json!({
            "ok": true,
            "killed": killed,
            "error": error,
            "message": "注册机进程已处理，但状态落盘失败",
        }));
    }
    with_status(json!({
        "ok": true,
        "killed": killed,
        "message": match (was_running, killed) {
            (true, true) => "注册机已关闭：只结束了带 grok-chrome-profile 的 Chrome 进程，日常浏览器不受影响",
            (true, false) => "关闭注册机失败：没能结束带 grok-chrome-profile 的 Chrome 进程，可手动关掉该窗口",
            _ => "注册机当前未在运行（没有带 grok-chrome-profile 的 Chrome 进程）",
        },
    }))
}

// ---------------------------------------------------------------------------
// 抓取与入池细节
// ---------------------------------------------------------------------------

/// 从 CDP cookie 列表里取 sso 值（无 sso 时用 sso-rw 兜底）并拼 cookie 串。
/// 只收 grok/x.ai 域的 cookie（口径与 `grok_sso.rs:455-497` 一致）。
fn collect_grok_cookies(cookies: &[Value]) -> (String, String) {
    let mut sso_value = String::new();
    let mut pairs: Vec<String> = Vec::new();
    for cookie in cookies {
        let name = cookie.get("name").and_then(Value::as_str).unwrap_or("");
        let value = cookie.get("value").and_then(Value::as_str).unwrap_or("");
        let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or("");
        if !domain.contains("grok") && !domain.contains("x.ai") {
            continue;
        }
        if !name.is_empty() && !value.is_empty() {
            pairs.push(format!("{name}={value}"));
        }
        if (name == "sso" || name == "sso-rw") && sso_value.is_empty() && !value.is_empty() {
            sso_value = value.to_string();
        }
    }
    (sso_value, pairs.join("; "))
}

/// 以固定账号 ID `grok_register` upsert 写账号池。
///
/// 存在则更新 token/status，不存在则 push（写法照 `grok_sso.rs:657-694`
/// `feed_gateway_pool`）。**不要**改用 `grok::import_account`：它每次生成随机 ID
/// `grok_web_<uuid8>` 且不去重，反复抓取会产生重复账号。
fn feed_pool(sso_value: &str, cookie_string: &str) -> Result<(), String> {
    let mut accounts = crate::modules::grok::load_accounts();
    let existing = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(POOL_ACCOUNT_ID));
    match existing {
        Some(entry) => {
            entry["token"] = json!(cookie_string);
            entry["updatedAt"] = json!(now_ms());
            if !sso_value.is_empty() {
                entry["status"] = json!("ok");
                entry["error"] = Value::Null;
                entry["enabled"] = json!(true);
            }
        }
        None => {
            if sso_value.is_empty() {
                return Ok(()); // 从未登录过：不入池
            }
            accounts.push(json!({
                "id": POOL_ACCOUNT_ID,
                "name": POOL_ACCOUNT_NAME,
                "provider": "web",
                "token": cookie_string,
                "tier": "free",
                "enabled": true,
                "status": "ok",
                "error": null,
                "cooldownUntil": 0,
                "lastUsedAt": null,
                "addedAt": now_ms(),
                "updatedAt": now_ms(),
            }));
        }
    }
    crate::modules::grok::save_accounts(&accounts)
}

/// 只结束「命令行带 grok-chrome-profile」的 Chrome 进程。
///
/// 🔴 严禁按进程名无差别杀 chrome —— 那会连用户日常浏览器一起杀掉。因此必须按
/// CommandLine 匹配本注册机实例的 profile 目录。
fn kill_register_browser() -> bool {
    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='chrome.exe'\" | Where-Object CommandLine -like '*{PROFILE_DIR_NAME}*' | Stop-Process -Force"
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-Command", &script]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW：不闪控制台窗口
        match cmd.status() {
            Ok(status) => status.success(),
            Err(error) => {
                eprintln!("[grok-register] 关闭进程失败: {error}");
                false
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        match Command::new("pkill")
            .args(["-f", PROFILE_DIR_NAME])
            .status()
        {
            Ok(status) => status.success(),
            Err(error) => {
                eprintln!("[grok-register] 关闭进程失败: {error}");
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 守卫测试（契约 §3.4：证明「默认关闭 + 零自动触发 + 只读 + 干净失败」）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 守卫 1：全新环境（状态文件不存在）时状态必须是 idle / 0 / null。
    #[test]
    fn default_state_is_idle_and_zero() {
        // 用一个必然不存在的临时路径，避免受本机真实状态文件影响。
        let missing = std::env::temp_dir().join(format!(
            "wb-grok-register-absent-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let state = load_state_from(&missing);
        assert_eq!(state.get("phase").and_then(Value::as_str), Some("idle"));
        assert_eq!(
            state.get("manualOpenCount").and_then(Value::as_u64),
            Some(0)
        );
        assert!(state
            .get("lastOpenedAt")
            .map(|v| v.is_null())
            .unwrap_or(false));
        assert!(state
            .get("lastCapturedAt")
            .map(|v| v.is_null())
            .unwrap_or(false));
        assert!(state
            .get("poolAccountId")
            .map(|v| v.is_null())
            .unwrap_or(false));
        // 持久化形状也要是 camelCase（契约 §3.1）。
        for key in [
            "phase",
            "manualOpenCount",
            "lastOpenedAt",
            "lastCapturedAt",
            "poolAccountId",
            "updatedAt",
        ] {
            assert!(default_state().get(key).is_some(), "状态文件缺字段 {key}");
        }
    }

    /// 守卫 2：本模块源码不得含常驻循环/后台任务入口（零自动触发的静态证明）。
    ///
    /// 模式串用 `concat!` 拼装：否则测试自身源码里就含这些字面量，断言必失败。
    #[test]
    fn no_background_loop_registered() {
        let src = include_str!("grok_register.rs");
        let tokio_spawn = concat!("tokio", "::", "spawn");
        let fn_spawn = concat!("fn ", "spawn_");
        let loop_brace = concat!("loop", " {");
        assert!(!src.contains(tokio_spawn), "注册机不得在运行时里起后台任务");
        assert!(!src.contains(fn_spawn), "注册机不得提供后台任务入口函数");
        assert!(!src.contains(loop_brace), "注册机不得有常驻循环");
    }

    /// 守卫 3：`status()` 只读 —— 恒 `autoTrigger == false`，且键不省略。
    #[test]
    fn status_is_read_only() {
        // 变量名避开 status 函数本身（同名会遮蔽调用，编译不过）。
        let first = status();
        assert_eq!(
            first.get("autoTrigger").and_then(Value::as_bool),
            Some(false)
        );
        // 不强制 browserRunning 真假（CI 无浏览器）；只要求它是 bool。
        assert!(first
            .get("browserRunning")
            .map(|v| v.is_boolean())
            .unwrap_or(false));
        assert_eq!(
            first.get("cdpPort").and_then(Value::as_u64),
            Some(u64::from(CDP_PORT))
        );
        assert_eq!(
            first.get("registerUrl").and_then(Value::as_str),
            Some(REGISTER_URL)
        );
        // 池里已有注册机账号时，poolAccountId 必为固定 ID；未入池时两个池字段都为 null。
        if first
            .get("poolAccountStatus")
            .and_then(Value::as_str)
            .is_some()
        {
            assert_eq!(
                first.get("poolAccountId").and_then(Value::as_str),
                Some(POOL_ACCOUNT_ID)
            );
        }
        // 契约 §3.3：字段缺失时给 null/0/false，不得省略键。
        for key in [
            "phase",
            "browserRunning",
            "cdpPort",
            "profileDir",
            "registerUrl",
            "manualOpenCount",
            "lastOpenedAt",
            "lastCapturedAt",
            "loginState",
            "ssoPreview",
            "cookieCount",
            "poolAccountId",
            "poolAccountStatus",
            "autoTrigger",
            "autoTriggerNote",
            "message",
        ] {
            assert!(first.get(key).is_some(), "status() 不得省略键 {key}");
        }
        // 只读证明：连续探测不改变手动打开计数（status 不含任何启动动作）。
        let again = status();
        assert_eq!(
            first.get("manualOpenCount").and_then(Value::as_u64),
            again.get("manualOpenCount").and_then(Value::as_u64)
        );
    }

    /// 守卫 4：无浏览器环境下 `capture()` 干净返回 `ok == false`，不 panic。
    #[tokio::test]
    async fn capture_without_browser_fails_cleanly() {
        if cdp_alive(CDP_PORT) {
            // 本机正好跑着注册机：跳过真调（capture 会真实写入账号池，测试不应有副作用）。
            // CI 无浏览器时会走到下面的断言。
            return;
        }
        let out = capture().await;
        assert_eq!(out.get("ok").and_then(Value::as_bool), Some(false));
        let status = out.get("status").and_then(Value::as_str).unwrap_or("");
        assert!(
            status == "not_registered" || status == "error",
            "无浏览器时应干净失败，实际 status = {status}"
        );
        assert!(out.get("message").and_then(Value::as_str).is_some());
    }
}
