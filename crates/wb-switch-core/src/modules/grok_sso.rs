//! Grok SSO Cookie 同步工具（v1.0.0）。
//!
//! 解决 Grok 网页版（grok.com）的 `sso` / `sso-rw` 会话 cookie 无法长期
//! 自动获取的问题：维护一个专用 Chrome 实例（独立 profile，调试端口
//! `--remote-debugging-port`），用户在该实例里登录 grok.com 一次，之后
//! 工具通过 CDP（Chrome DevTools Protocol）定时抓取最新 cookie 值与
//! 有效期并落盘到 `~/.wb-switch/grok_sso_token.json`，供其它脚本/代理
//! 直接读取。
//!
//! 需求决策（2026-09-15 与用户确认）：
//! - 接入方式：专用浏览器实例（不影响日常 Chrome）；
//! - 产出形态：Sylvia 集成（后端循环 + 侧边栏页）；
//! - 同步频率：30 分钟；剩余有效期 < 24h 视为「即将过期」提前提醒，
//!   值变化即写盘；cookie 消失/过期视为登录态失效。
//!
//! 关键实现：
//! - CDP 通过 `GET /json` 拿 page target 的 WebSocket URL，
//!   `Network.getCookies {urls:["https://grok.com"]}` 取 cookie；
//! - 续期语义：sso cookie 由 Grok 服务端在浏览器访问时自动续签，
//!   工具定时打开 `https://grok.com` 页面触发服务端续签，再抓取新值；
//! - 失败重试：单轮内指数退避 ×3，轮次失败不写盘不覆盖旧值；
//! - 浏览器生命周期：常驻（非 headless，便于用户随时补登录），崩溃
//!   自动拉起；`open_login_window` 供前端触发登录窗口。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::modules::config::{atomic_write, now_ms, store_dir};

pub const VERSION: &str = "1.1.0";
const CDP_PORT: u16 = 19233;
const GROK_URL: &str = "https://grok.com";
/// sso cookie 的服务端有效期实测约 30 天；< 24h 视为即将过期。
const EXPIRE_SOON_MS: i64 = 24 * 3600 * 1000;

// ---------------------------------------------------------------------------
// 配置与状态
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct GrokSsoConfig {
    pub enabled: bool,
    /// Chrome 可执行文件路径（空 = 自动探测）。
    pub chrome_path: String,
    /// 专用 profile 目录（空 = ~/.wb-switch/grok-chrome-profile）。
    pub profile_dir: String,
}

fn config_path() -> PathBuf {
    store_dir().join("grok_sso_config.json")
}

pub fn load_config() -> GrokSsoConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| GrokSsoConfig {
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            chrome_path: v
                .get("chrome_path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            profile_dir: v
                .get("profile_dir")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .unwrap_or_else(|| {
            let cfg = GrokSsoConfig {
                enabled: true,
                chrome_path: String::new(),
                profile_dir: String::new(),
            };
            let _ = save_config(&cfg);
            cfg
        })
}

pub fn save_config(cfg: &GrokSsoConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(&json!({
        "enabled": cfg.enabled,
        "chrome_path": cfg.chrome_path,
        "profile_dir": cfg.profile_dir,
        "updatedAt": now_ms(),
    }))
    .map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())
}

/// token 落盘路径（其它脚本读这个）。
pub fn token_path() -> PathBuf {
    store_dir().join("grok_sso_token.json")
}

static LAST_SYNC: Mutex<Option<Value>> = Mutex::new(None);

/// 直连本机 CDP 的 client：强制绕过系统代理（reqwest 默认会读 Windows
/// IE 代理设置，127.0.0.1 请求被送进 Clash 后 /json 无法返回）。
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
static BROWSER_PORT: Mutex<Option<u16>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Chrome 生命周期
// ---------------------------------------------------------------------------

/// 探测 Chrome 可执行文件：配置优先，其次注册表 App Paths 与常见安装位置。
fn resolve_chrome_path(cfg_chrome: &str) -> Option<PathBuf> {
    if !cfg_chrome.trim().is_empty() {
        let p = PathBuf::from(cfg_chrome.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    // 用户自己装在非标准位置（本机实测）。
    let known = [
        r"E:\驱动精灵搬家目录(勿删改)\软件\Google Chrome\安装目录\541\chrome.exe",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    ];
    for k in known {
        let p = PathBuf::from(k);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn profile_dir(cfg_profile: &str) -> PathBuf {
    if !cfg_profile.trim().is_empty() {
        return PathBuf::from(cfg_profile.trim());
    }
    store_dir().join("grok-chrome-profile")
}

/// CDP 端口上是否有我们的 Chrome（health 特征）。
async fn cdp_alive(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/json/version");
    match cdp_http_client().get(&url).send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(v) => v.get("Browser").is_some(),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// 确保专用 Chrome 在跑：已存活则复用；否则拉起（分离进程）。
async fn ensure_browser(cfg: &GrokSsoConfig) -> Result<u16, String> {
    if cdp_alive(CDP_PORT).await {
        return Ok(CDP_PORT);
    }
    let chrome = resolve_chrome_path(&cfg.chrome_path)
        .ok_or_else(|| "未找到 Chrome/Edge 可执行文件（请在配置里指定 chrome_path）".to_string())?;
    let profile = profile_dir(&cfg.profile_dir);
    std::fs::create_dir_all(&profile).map_err(|e| format!("创建 profile 目录失败: {e}"))?;
    let mut cmd = std::process::Command::new(&chrome);
    cmd.args([
        &format!("--remote-debugging-port={CDP_PORT}"),
        &format!("--user-data-dir={}", profile.display()),
        "--no-first-run",
        "--no-default-browser-check",
        // 打开 grok.com 首页：触发服务端续签，也方便用户看到登录状态。
        GROK_URL,
    ]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008); // DETACHED_PROCESS
    }
    cmd.spawn()
        .map_err(|e| format!("启动 Chrome 失败: {e}"))?;
    // 等调试端口就绪（最多 20s）。
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if cdp_alive(CDP_PORT).await {
            let mut guard = BROWSER_PORT.lock().unwrap();
            *guard = Some(CDP_PORT);
            return Ok(CDP_PORT);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("Chrome 已启动但调试端口未就绪".to_string())
}

// ---------------------------------------------------------------------------
// CDP 客户端（WebSocket）
// ---------------------------------------------------------------------------

/// 取一个 page target 的 WebSocket URL（无 page 则新开一个）。
async fn pick_page_ws_url() -> Result<String, String> {
    let list: Vec<Value> = cdp_http_client().get(format!("http://127.0.0.1:{CDP_PORT}/json"))
        .send()
        .await
        .map_err(|e| format!("CDP /json 请求失败: {e}"))?
        .json()
        .await
        .map_err(|e| format!("CDP /json 解析失败: {e}"))?;
    let page = list
        .iter()
        .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .ok_or_else(|| "CDP 无可用 page target".to_string())?;
    page.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| "CDP target 缺少 webSocketDebuggerUrl".to_string())
}

/// CDP 单命令：连 WS → 发命令 → 收到对应 id 的响应即返回。
async fn cdp_call(method: &str, params: Value) -> Result<Value, String> {
    let ws_url = pick_page_ws_url().await?;
    cdp_call_on(&ws_url, method, params).await
}

/// 同 cdp_call，但指定目标 WS URL（Page.reload 等需要精确 target）。
async fn cdp_call_on(ws_url: &str, method: &str, params: Value) -> Result<Value, String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| format!("CDP WebSocket 连接失败: {e}"))?;
    let request = json!({"id": 1, "method": method, "params": params});
    ws.send(Message::Text(request.to_string().into()))
        .await
        .map_err(|e| format!("CDP 发送失败: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
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
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }
    Err("CDP 响应超时".to_string())
}

/// 版本号：v1.1.0 起 web 反代借道 CDP fetch（修 Cloudflare 403）。
pub const GATEWAY_BRIDGE_VERSION: &str = "1.1.0";

/// JS 字符串转义（塞进 JS 源码字面量用）。反斜杠用 unicode 转义规避字面量歧义。
fn js_escape(s: &str) -> String {
    let backslash: char = '\u{005C}';
    let quote: char = '\"';
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c == quote {
            out.push(backslash);
            out.push(quote);
        } else if c == backslash {
            out.push(backslash);
            out.push(backslash);
        } else {
            out.push(c);
        }
    }
    out
}

/// CDP fetch 桥是否可用（浏览器在跑且至少有一个页面）。
pub async fn cdp_fetch_json_ready() -> bool {
    cdp_alive(CDP_PORT).await
}

pub async fn cdp_fetch_json(
    url: &str,
    body: &Value,
    extra_headers: &[(String, String)],
) -> Result<(u16, Value), String> {
    let headers_js: Vec<String> = extra_headers
        .iter()
        .map(|(k, v)| format!("{}: {}", js_escape(k), js_escape(v)))
        .collect();
    // 注：headers_js 拼进 JS 对象字面量；body 由 JSON.stringify 注入避免转义问题。
    let body_json = serde_json::to_string(body).map_err(|e| e.to_string())?;
    let js = format!(
        "(async () => {{             const resp = await fetch({url}, {{                 method: 'POST',                 headers: Object.assign({{ 'Content-Type': 'application/json' }}, {headers}),                 body: JSON.stringify({body}),                 credentials: 'include',             }});             const text = await resp.text();             return JSON.stringify({{ __status: resp.status, __body: text }});         }})()",
        url = serde_json::to_string(url).map_err(|e| e.to_string())?,
        headers = if headers_js.is_empty() {
            "{}".to_string()
        } else {
            format!("{{ {} }}", headers_js.join(", "))
        },
        body = body_json,
    );
    let result = cdp_call(
        "Runtime.evaluate",
        json!({"expression": js, "awaitPromise": true, "returnByValue": true}),
    )
    .await?;
    let value_str = result
        .pointer("/result/value")
        .and_then(Value::as_str)
        .ok_or_else(|| "CDP fetch 返回缺少 value".to_string())?;
    let wrapped: Value =
        serde_json::from_str(value_str).map_err(|e| format!("CDP fetch 结果解析失败: {e}"))?;
    let status = wrapped.get("__status").and_then(Value::as_u64).unwrap_or(0) as u16;
    let body_text = wrapped.get("__body").and_then(Value::as_str).unwrap_or("");
    let parsed: Value = serde_json::from_str(body_text)
        .map_err(|e| format!("上游响应不是 JSON: {e}；前 200 字: {}", body_text.chars().take(200).collect::<String>()))?;
    Ok((status, parsed))
}

/// 确保专用浏览器里有 grok.com 页面：没有则新开一个 tab（用于 CDP fetch 上下文）。
pub async fn ensure_grok_page() -> Result<(), String> {
    let cfg = load_config();
    ensure_browser(&cfg).await?;
    let list: Vec<Value> = cdp_http_client().get(format!("http://127.0.0.1:{CDP_PORT}/json"))
        .send()
        .await
        .map_err(|e| format!("CDP /json 请求失败: {e}"))?
        .json()
        .await
        .map_err(|e| format!("CDP /json 解析失败: {e}"))?;
    let has_grok = list.iter().any(|t| {
        t.get("type").and_then(Value::as_str) == Some("page")
            && t.get("url")
                .and_then(Value::as_str)
                .map(|u| u.starts_with(GROK_URL))
                .unwrap_or(false)
    });
    if has_grok {
        return Ok(());
    }
    // 用 PUT /json/new 打开 grok.com（Chrome 111+ 需要 PUT）。
    let client = reqwest::Client::new();
    let resp = client
        .put(format!("http://127.0.0.1:{CDP_PORT}/json/new?{GROK_URL}"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("打开 grok.com tab 失败: {e}"))?;
    if !resp.status().is_success() {
        // 老版本 Chrome 用 GET。
        let _ = client
            .get(format!("http://127.0.0.1:{CDP_PORT}/json/new?{GROK_URL}"))
            .timeout(Duration::from_secs(10))
            .send()
            .await;
    }
    // 等页面就绪。
    tokio::time::sleep(Duration::from_secs(2)).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// 抓取与落盘
// ---------------------------------------------------------------------------

/// 从 CDP 抓 grok.com 的全部 cookie（含 httpOnly 的 sso / sso-rw）。
async fn fetch_grok_cookies() -> Result<Vec<Value>, String> {
    let result = cdp_call(
        "Network.getCookies",
        json!({"urls": [GROK_URL, "https://accounts.x.ai"]}),
    )
    .await?;
    Ok(result
        .get("cookies")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn cookie_ms(expires: f64) -> i64 {
    // CDP expires 是秒（epoch）；-1 = session cookie。
    if expires <= 0.0 {
        0
    } else {
        (expires * 1000.0) as i64
    }
}

/// 让 grok.com 页面 reload 一次：Grok 服务端在页面加载时续签 sso cookie
/// （实测 reload 后 expiresAt 前移 30 分钟；不做此步 cookie 会自然到期）。
async fn trigger_renewal() -> Result<(), String> {
    let list: Vec<Value> = cdp_http_client().get(format!("http://127.0.0.1:{CDP_PORT}/json"))
        .send()
        .await
        .map_err(|e| format!("CDP /json 请求失败: {e}"))?
        .json()
        .await
        .map_err(|e| format!("CDP /json 解析失败: {e}"))?;
    let grok = list
        .iter()
        .find(|t| {
            t.get("type").and_then(Value::as_str) == Some("page")
                && t.get("url")
                    .and_then(Value::as_str)
                    .map(|u| u.starts_with(GROK_URL))
                    .unwrap_or(false)
        })
        .ok_or_else(|| "无 grok.com 页面".to_string())?;
    let ws_url = grok
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "grok 页面缺少 WS URL".to_string())?;
    // Page.reload 在 SPA 上可能吃缓存不触发续签，改用 Page.navigate 到
    // 强制刷新 URL（grok.com 对每个页面加载都会下发续签后的 sso cookie）。
    let _ = cdp_call_on(
        ws_url,
        "Page.navigate",
        json!({"url": "https://grok.com/discover", "transitionType": "link"}),
    )
    .await;
    // 等页面加载完成（服务端 Set-Cookie 在响应头里，通常 2-5 秒内完成）。
    tokio::time::sleep(Duration::from_secs(6)).await;
    Ok(())
}

/// 一轮同步：触发续签 → 抓 cookie → 组装 token 文件 → 值变化时写盘。
/// 返回 (写盘与否, 摘要)。
async fn sync_once(cfg: &GrokSsoConfig) -> Result<(bool, Value), String> {
    let _port = ensure_browser(cfg).await?;
    // 已登录过才做续签（未登录时页面 reload 无意义）。
    if read_token_raw().is_some() {
        let _ = trigger_renewal().await;
    }
    let cookies = fetch_grok_cookies().await?;
    if cookies.is_empty() {
        return Ok((false, json!({"status": "no_cookies", "message": "未抓到 grok.com 的任何 cookie（未登录或域名不符）"})));
    }
    let mut sso: Option<&Value> = None;
    let mut sso_rw: Option<&Value> = None;
    let mut cookie_pairs: Vec<String> = Vec::new();
    let mut earliest_expire: i64 = i64::MAX;
    for cookie in &cookies {
        let name = cookie.get("name").and_then(Value::as_str).unwrap_or("");
        let value = cookie.get("value").and_then(Value::as_str).unwrap_or("");
        let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or("");
        let expires = cookie.get("expires").and_then(Value::as_f64).unwrap_or(-1.0);
        if !domain.contains("grok") && !domain.contains("x.ai") {
            continue;
        }
        if !name.is_empty() && !value.is_empty() {
            cookie_pairs.push(format!("{name}={value}"));
        }
        match name {
            // v1.1.0：expiresAt 只看 sso/sso-rw 自己的有效期（180 天）；
            // 其它 cookie（会话级，20 分钟）不再拉低最早过期时间造成误报。
            "sso" => {
                sso = Some(cookie);
                let ms = cookie_ms(expires);
                if ms > 0 && ms < earliest_expire {
                    earliest_expire = ms;
                }
            }
            "sso-rw" => {
                sso_rw = Some(cookie);
                let ms = cookie_ms(expires);
                if ms > 0 && ms < earliest_expire {
                    earliest_expire = ms;
                }
            }
            _ => {}
        }
    }
    if sso.is_none() && sso_rw.is_none() {
        return Ok((false, json!({
            "status": "not_logged_in",
            "message": "grok.com cookie 里没有 sso / sso-rw（请在专用浏览器窗口登录 grok.com）",
        })));
    }
    let sso_value = sso.or(sso_rw)
        .and_then(|c| c.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let cookie_string = cookie_pairs.join("; ");
    let expires_at = if earliest_expire == i64::MAX { 0 } else { earliest_expire };
    let expires_soon = expires_at > 0 && expires_at - now_ms() < EXPIRE_SOON_MS;

    // 与上次对比，值变化才写盘。
    let mut last = LAST_SYNC.lock().unwrap();
    let fingerprint_changed = match last.as_ref() {
        Some(prev) => {
            prev.get("sso").and_then(Value::as_str) != Some(sso_value.as_str())
                || prev
                    .get("cookieString")
                    .and_then(Value::as_str)
                    .map(|c| c.len())
                    != Some(cookie_string.len())
        }
        None => true,
    };
    let token_file = json!({
        "version": VERSION,
        "updatedAt": now_ms(),
        "sso": sso_value,
        "ssoPresent": sso.is_some(),
        "ssoRwPresent": sso_rw.is_some(),
        "cookieString": cookie_string,
        "expiresAt": expires_at,
        "expiresSoon": expires_soon,
        "cookieCount": cookies.len(),
    });
    // v1.5.0：浏览器若已被程序切号接管，则当前抓到的 cookie 属于被轮询账号，
    // 不可写回 autosync（否则会把用户原始登录态覆盖掉，令账号池去重坍缩）。
    if is_browser_taken_over() {
        if fingerprint_changed {
            atomic_write(&token_path(), &serde_json::to_string_pretty(&token_file).map_err(|e| e.to_string())?)
                .map_err(|e| format!("写 token 文件失败: {e}"))?;
        }
    } else if fingerprint_changed {
        atomic_write(&token_path(), &serde_json::to_string_pretty(&token_file).map_err(|e| e.to_string())?)
            .map_err(|e| format!("写 token 文件失败: {e}"))?;
        feed_gateway_pool(&sso_value, &cookie_string)?;
    } else {
        // 首轮或值未变时也确保池里存在（幂等）。
        feed_gateway_pool(&sso_value, &cookie_string)?;
    }
    *last = Some(token_file.clone());
    Ok((
        fingerprint_changed,
        json!({
            "status": "ok",
            "ssoPresent": sso.is_some(),
            "ssoRwPresent": sso_rw.is_some(),
            "expiresAt": expires_at,
            "expiresSoon": expires_soon,
            "cookieCount": cookies.len(),
            "written": fingerprint_changed,
        }),
    ))
}

// ---------------------------------------------------------------------------
// 公开接口（API/循环用）
// ---------------------------------------------------------------------------

/// 手动触发同步（用户在 UI 点「立即同步」）：视为确认当前浏览器登录态，
/// 重置「已被接管」标记，让同步把当前登录态写回 autosync 账号。
///
/// 刻意**不受** `enabled` 约束：`enabled=false` 的语义是「不要在后台无人值守地
/// 自动打开浏览器」，用户主动点击时仍应始终可同步。
pub async fn run_sync_manual() -> Value {
    reset_browser_taken_over();
    clear_resident();
    run_sync_with(&load_config()).await
}

/// 定时触发一轮同步（后台循环用；不重置接管标记）。
pub async fn run_sync() -> Value {
    let cfg = load_config();
    if !cfg.enabled {
        return json!({"status": "disabled"});
    }
    run_sync_with(&cfg).await
}

/// 实际同步逻辑（含单轮指数退避 ×3）；是否受 `enabled` 约束由调用方决定。
async fn run_sync_with(cfg: &GrokSsoConfig) -> Value {
    // 单轮内重试：指数退避 ×3（网络抖动/CDP 瞬断）。
    let mut last_error = String::new();
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(1500 * (1 << attempt))).await;
        }
        match sync_once(&cfg).await {
            Ok((written, summary)) => {
                let mut v = json!({"attempt": attempt + 1, "written": written});
                if let Some(obj) = v.as_object_mut() {
                    if let Some(s) = summary.as_object() {
                        for (k, val) in s {
                            obj.insert(k.clone(), val.clone());
                        }
                    }
                }
                return v;
            }
            Err(error) => {
                last_error = error;
            }
        }
    }
    // 全部失败：不覆盖旧 token 文件。
    json!({"status": "error", "error": last_error, "note": "旧 token 文件未被改动"})
}

/// 打开登录窗口（浏览器已带 grok.com 首页；重复调用幂等）。
pub async fn open_login_window() -> Value {
    let cfg = load_config();
    match ensure_browser(&cfg).await {
        Ok(_) => json!({"ok": true, "port": CDP_PORT, "message": "专用浏览器已启动，请在窗口内登录 grok.com"}),
        Err(error) => json!({"ok": false, "error": error}),
    }
}

/// 读取当前 token 文件（供 API 展示；敏感值打码）。
pub fn token_status() -> Value {
    match std::fs::read_to_string(token_path()) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(mut v) => {
                let expires_at = v.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
                let remain = expires_at - now_ms();
                let sso_preview: String = v
                    .get("sso")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(8)
                    .collect();
                if let Some(obj) = v.as_object_mut() {
                    let logged_in = obj.get("ssoPresent").and_then(Value::as_bool).unwrap_or(false)
                        || obj.get("ssoRwPresent").and_then(Value::as_bool).unwrap_or(false);
                    obj.insert("status".to_string(), json!(if logged_in { "ok" } else { "not_logged_in" }));
                    obj.insert("expiresInDays".to_string(), json!((remain.max(0) / 86_400_000) as i64));
                    obj.insert("sso".to_string(), json!(format!("{sso_preview}…")));
                    obj.insert("cookieString".to_string(), Value::Null);
                }
                v
            }
            Err(error) => json!({"status": "corrupt", "error": error.to_string()}),
        },
        Err(_) => json!({"status": "empty", "message": "尚未同步过 token（先点「打开登录窗口」登录 grok.com）"}),
    }
}

/// 读取完整 token 文件（内部使用；不对外暴露）。
pub fn read_token_raw() -> Option<Value> {
    std::fs::read_to_string(token_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// 把最新 sso cookie 幂等喂给 grok.rs 融合网关的账号池：
/// 固定账号 ID `grok_web_autosync`（单账号语义——专用浏览器只有一个登录态），
/// 每次同步覆盖其 token；浏览器侧未登录则禁用该账号而不是删除。
fn feed_gateway_pool(sso_value: &str, cookie_string: &str) -> Result<(), String> {
    const AUTO_ID: &str = "grok_web_autosync";
    let mut accounts = crate::modules::grok::load_accounts();
    let existing = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(AUTO_ID));
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
                "id": AUTO_ID,
                "name": "Grok Web（SSO 自动同步）",
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

/// 周期同步循环（30 分钟一次；由 server 启动时拉起）。
pub fn spawn_sync_loop() {
    tokio::spawn(async move {
        // 启动后 2 分钟做首轮（避免与签到循环抢 I/O）。
        tokio::time::sleep(Duration::from_secs(120)).await;
        loop {
            let _ = run_sync().await;
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        }
    });
}

/// v1.5.0：冷账号保活续签循环（每 6 小时一次）。
///
/// 仅对"超过 6 小时未被请求激活"的 web 账号做一次注入+导航，触发服务端 sso
/// 续签，避免冷账号静默过期。热账号由真实请求驱动，不重复打扰（降低机器行为
/// 特征，规避风控）。
pub fn spawn_keepalive_loop() {
    tokio::spawn(async move {
        // 启动后 10 分钟做首轮（避开同步循环与签到循环的 I/O 高峰）。
        tokio::time::sleep(Duration::from_secs(600)).await;
        loop {
            let _ = keepalive_cold_accounts().await;
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
        }
    });
}

/// 对冷账号执行一次保活续签（注入 → 导航 → 抓回新 cookie 写回池）。
async fn keepalive_cold_accounts() -> Result<(), String> {
    const COLD_THRESHOLD_MS: i64 = 6 * 60 * 60 * 1000;
    let cfg = load_config();
    if !cfg.enabled {
        return Ok(());
    }
    ensure_browser(&cfg).await?;
    ensure_grok_page().await?;

    let accounts = crate::modules::grok::load_accounts();
    let now = now_ms();
    let mut touched = 0usize;
    for acc in &accounts {
        if acc.get("provider").and_then(Value::as_str) != Some("web") {
            continue;
        }
        if acc.get("status").and_then(Value::as_str) == Some("error") {
            continue; // 失效账号不保活（避免刷错误）
        }
        let last_used = acc.get("lastUsedAt").and_then(Value::as_i64).unwrap_or(0);
        if last_used > 0 && now - last_used < COLD_THRESHOLD_MS {
            continue;
        }
        let account_id = acc.get("id").and_then(Value::as_str).unwrap_or_default();
        let token = acc.get("token").and_then(Value::as_str).unwrap_or_default();
        if account_id.is_empty() || token.is_empty() {
            continue;
        }
        // 注入 → 导航触发续签 → 抓回新 cookie。
        if switch_account_to(token).await.is_err() {
            continue;
        }
        let _ = trigger_renewal().await;
        let cookies = fetch_grok_cookies().await.unwrap_or_default();
        // 续签可能下发了新 sso，回写目标账号（而非 autosync），保持冷账号常青。
        let mut new_pairs: Vec<String> = Vec::new();
        let mut new_sso = String::new();
        for c in &cookies {
            let name = c.get("name").and_then(Value::as_str).unwrap_or("");
            let value = c.get("value").and_then(Value::as_str).unwrap_or("");
            let domain = c.get("domain").and_then(Value::as_str).unwrap_or("");
            if value.is_empty() || (!domain.contains("grok") && !domain.contains("x.ai")) {
                continue;
            }
            if name == "sso" && new_sso.is_empty() {
                new_sso = value.to_string();
            }
            if !name.is_empty() {
                new_pairs.push(format!("{name}={value}"));
            }
        }
        if !new_sso.is_empty() {
            update_account_token(account_id, &new_pairs.join("; "));
        }
        {
            let mut guard = RESIDENT.lock().unwrap();
            *guard = Some((account_id.to_string(), now_ms() + RESIDENT_TTL_MS));
        }
        mark_browser_taken_over();
        touched += 1;
    }
    if touched > 0 {
        eprintln!("[grok-keepalive] 已保活 {touched} 个冷账号");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// v1.5.0：多账号轮询（A2 单实例 + CDP 注入切号）
// ---------------------------------------------------------------------------

/// 切号时需要清理的"身份类" cookie：决定服务端认谁 + 前端显示谁。
/// 保留 cf_clearance / __cf_bm 等 Cloudflare 通行 cookie（绑 IP+UA，跨账号共享）。
const IDENTITY_COOKIES: &[&str] = &["sso", "sso-rw", "x-userid"];

/// 账号驻留期（毫秒）：同一账号在该时段内不切号，避免高频身份横跳触发风控。
const RESIDENT_TTL_MS: i64 = 30 * 60 * 1000;

/// 当前驻留账号 (account_id, expire_ms)
static RESIDENT: Mutex<Option<(String, i64)>> = Mutex::new(None);

/// 浏览器是否已被程序切号接管（v1.5.0）。
/// 置位期间自动同步不回写 autosync 账号 —— 否则会把用户原始登录态覆盖成被
/// 轮询账号的身份，令账号池去重坍缩。用户手动「立即同步」会重置此标记。
static BROWSER_TAKEN_OVER: AtomicBool = AtomicBool::new(false);

/// 当前驻留账号 ID（未过期时返回）；供选号层优先复用，避免每请求都切号。
pub fn resident_account_id() -> Option<String> {
    let guard = RESIDENT.lock().unwrap();
    match guard.as_ref() {
        Some((id, expire)) if now_ms() < *expire => Some(id.clone()),
        _ => None,
    }
}

/// 清除驻留（切号失败/账号失效时调用，下轮重新选号）。
pub fn clear_resident() {
    let mut guard = RESIDENT.lock().unwrap();
    *guard = None;
}

/// 标记浏览器已被程序接管（供 ui_chat_with_account 调用）。
pub fn mark_browser_taken_over() {
    BROWSER_TAKEN_OVER.store(true, Ordering::Relaxed);
}

/// 重置「已被接管」标记（用户手动同步时调用，表示确认当前登录态）。
pub fn reset_browser_taken_over() {
    BROWSER_TAKEN_OVER.store(false, Ordering::Relaxed);
}

/// 浏览器当前是否处于「被程序接管」状态。
pub fn is_browser_taken_over() -> bool {
    BROWSER_TAKEN_OVER.load(Ordering::Relaxed)
}

/// 更新指定账号的 token（保活续签后回写新 cookie）。
fn update_account_token(account_id: &str, cookie_string: &str) {
    let mut accounts = crate::modules::grok::load_accounts();
    if let Some(acc) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(account_id))
    {
        acc["token"] = json!(cookie_string);
        acc["updatedAt"] = json!(now_ms());
        acc["status"] = json!("ok");
        acc["error"] = Value::Null;
        let _ = crate::modules::grok::save_accounts(&accounts);
    }
}

/// 解析 "name=value; name2=value2" 形式的 cookie 串。
fn parse_cookie_pairs(cookie_string: &str) -> Vec<(String, String)> {
    cookie_string
        .split(';')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let mut kv = part.splitn(2, '=');
            let name = kv.next()?.trim();
            let value = kv.next()?.trim();
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

/// 从 cookie 串提取 sso 值作为账号指纹（无 sso 时用整串兜底）。
fn extract_sso_fingerprint(cookie_string: &str) -> String {
    for (name, value) in parse_cookie_pairs(cookie_string) {
        if name == "sso" {
            return value;
        }
    }
    cookie_string.trim().to_string()
}

/// 读浏览器当前 grok.com 的 sso 值（判断"浏览器里现在是哪个账号"）。
async fn current_browser_sso_fingerprint() -> Result<String, String> {
    let cookies = fetch_grok_cookies().await?;
    for cookie in &cookies {
        let name = cookie.get("name").and_then(Value::as_str).unwrap_or("");
        if name == "sso" {
            return Ok(cookie
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string());
        }
    }
    Ok(String::new())
}

/// 找到 grok.com 页面的 WS URL（切号注入目标）。
async fn grok_page_ws_url() -> Result<String, String> {
    let list: Vec<Value> = cdp_http_client()
        .get(format!("http://127.0.0.1:{CDP_PORT}/json"))
        .send()
        .await
        .map_err(|e| format!("CDP /json 请求失败: {e}"))?
        .json()
        .await
        .map_err(|e| format!("CDP /json 解析失败: {e}"))?;
    let grok = list
        .iter()
        .find(|t| {
            t.get("type").and_then(Value::as_str) == Some("page")
                && t.get("url")
                    .and_then(Value::as_str)
                    .map(|u| u.starts_with(GROK_URL))
                    .unwrap_or(false)
        })
        .ok_or_else(|| "无 grok.com 页面".to_string())?;
    grok.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| "grok 页面缺少 WS URL".to_string())
}

/// 智能清理 + 注入目标账号 cookie（不导航；调用方随后 navigate 刷新）。
///
/// 关键坑（2026-09-17 实测）：grok 系站点会跨多个 domain 下发同名 sso cookie
/// （.grok.com / grok.com / .grokusercontent.com / .grokipedia.com），且同名
/// cookie 可同时存在于带点与不带点 domain。只按 url 删除会漏删，残留的旧值会
/// 被浏览器优先采用 → 注入看似成功实则无效。必须 Storage.getCookies 全量扫描
/// + 按 domain+path 精确删除，再注入到 .grok.com 与 grok.com 两个域。
async fn switch_account_to(cookie_string: &str) -> Result<(), String> {
    let ws_url = grok_page_ws_url().await?;
    let _ = cdp_call_on(&ws_url, "Network.enable", json!({})).await;

    // 1) 全量扫描身份类 cookie，按 domain+path 精确删除（覆盖所有 grok 系域名）。
    if let Ok(result) = cdp_call_on(&ws_url, "Storage.getCookies", json!({})).await {
        if let Some(cookies) = result.get("cookies").and_then(Value::as_array) {
            for cookie in cookies {
                let name = cookie.get("name").and_then(Value::as_str).unwrap_or("");
                let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or("");
                let path = cookie.get("path").and_then(Value::as_str).unwrap_or("/");
                if !IDENTITY_COOKIES.contains(&name) || !domain.contains("grok") {
                    continue;
                }
                let _ = cdp_call_on(
                    &ws_url,
                    "Network.deleteCookies",
                    json!({"name": name, "domain": domain, "path": path}),
                )
                .await;
            }
        }
    }

    // 2) 注入目标 cookie 到 .grok.com 与 grok.com 两个域（跳过 CF 通行 cookie）。
    let pairs: Vec<(String, String)> = parse_cookie_pairs(cookie_string)
        .into_iter()
        .filter(|(name, _)| !name.starts_with("cf_") && !name.starts_with("__cf"))
        .collect();
    if pairs.is_empty() {
        return Err("目标账号 cookie 串为空或全部被跳过".to_string());
    }
    for domain in [".grok.com", "grok.com"] {
        for (name, value) in &pairs {
            let _ = cdp_call_on(
                &ws_url,
                "Network.setCookie",
                json!({
                    "name": name,
                    "value": value,
                    "domain": domain,
                    "path": "/",
                    "secure": true,
                }),
            )
            .await;
        }
    }
    Ok(())
}

/// UI 对话失败的分类（v1.5.0）。
///
/// 关键：必须区分「临时问题」与「账号失效」。限流/超时/页面抖动若被误判为
/// 账号失效，会让好账号被永久跳过 —— 账号池会随着几次限流逐渐枯竭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatErrorKind {
    /// 免费额度限流 / 使用人数过多 —— 分钟级，冷却即可恢复，账号本身有效。
    RateLimited,
    /// 等待回复超时 —— 可能只是慢，也可能是账号被静默限速。短冷却。
    Timeout,
    /// 基础设施问题：页面未加载、输入框缺失、CDP 抖动 —— 与账号无关。
    Infra,
    /// 账号登录态失效：SSO 过期、未登录、被拦截 —— 需重新登录。
    NotLoggedIn,
    /// 配置/注入问题：cookie 为空、注入失败。
    Invalid,
}

#[derive(Debug, Clone)]
pub struct ChatError {
    pub kind: ChatErrorKind,
    pub message: String,
}

impl ChatError {
    pub fn new(kind: ChatErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    /// 是否应把账号标记为 error（永久失效）。
    pub fn is_account_fatal(&self) -> bool {
        matches!(self.kind, ChatErrorKind::NotLoggedIn | ChatErrorKind::Invalid)
    }

    /// 非致命错误建议的冷却秒数（0 = 不冷却，仅重试）。
    pub fn cooldown_seconds(&self) -> u64 {
        match self.kind {
            ChatErrorKind::RateLimited => 60,
            ChatErrorKind::Timeout => 30,
            ChatErrorKind::Infra => 15,
            _ => 0,
        }
    }
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// UI 对话全局互斥锁（v1.5.0）。
///
/// 单浏览器实例同一时刻只能进行一个对话：并发调用会互相抢 CDP、
/// 互相导航/插入文字/点提交，导致所有请求全部失败（实测 2026-09-17）。
/// 锁覆盖「切号 + 对话」全过程，串行化请求。
static UI_CHAT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 带账号路由的 UI 驱动对话（v1.5.0）：
/// 驻留期内复用当前账号；需要切号时按 cookie 指纹短路，命中则跳过注入。
pub async fn ui_chat_with_account(
    account_id: &str,
    cookie_string: &str,
    prompt: &str,
) -> Result<String, ChatError> {
    // 串行化：同一时刻只允许一个对话操作浏览器。
    let _chat_guard = UI_CHAT_LOCK.lock().await;
    let cfg = load_config();
    ensure_browser(&cfg)
        .await
        .map_err(|e| ChatError::new(ChatErrorKind::Infra, e))?;
    ensure_grok_page()
        .await
        .map_err(|e| ChatError::new(ChatErrorKind::Infra, e))?;

    let need_switch = {
        let guard = RESIDENT.lock().unwrap();
        match guard.as_ref() {
            Some((id, expire)) if id == account_id && now_ms() < *expire => false,
            _ => true,
        }
    };

    if need_switch {
        let target_fp = extract_sso_fingerprint(cookie_string);
        let current_fp = current_browser_sso_fingerprint().await.unwrap_or_default();
        // 指纹命中（浏览器里已是目标账号）→ 跳过注入。
        if current_fp.is_empty() || current_fp != target_fp {
            switch_account_to(cookie_string)
                .await
                .map_err(|e| ChatError::new(ChatErrorKind::Invalid, e))?;
        }
        let mut guard = RESIDENT.lock().unwrap();
        *guard = Some((account_id.to_string(), now_ms() + RESIDENT_TTL_MS));
        // 标记浏览器已被程序接管（同步循环据此跳过回写 autosync）。
        mark_browser_taken_over();
    }

    match ui_chat(prompt).await {
        Ok(reply) => Ok(reply),
        Err(e) => {
            // 本轮失败：清驻留，让下一轮重新选号（可能是账号失效而非平台问题）。
            clear_resident();
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// v1.5.0：账号池健康体检（供 grok_pool 调用）
// ---------------------------------------------------------------------------

/// 若当前页面是 ToS 确认门槛（/tos-gate），点击「知道了」通过。
///
/// 背景（2026-09-17）：新登录的账号首次访问会停在服务条款确认页，
/// 不确认则永远等不到输入框 —— 会被误判为"页面未就绪"。批量注册的账号
/// 从未走过此流程，因此长期卡死。
async fn dismiss_tos_if_present() -> Result<bool, String> {
    let result = eval_js(
        r#"(() => {
            if (!location.href.includes('tos-gate')) return 'no-gate';
            const b = [...document.querySelectorAll('button')].find(
                x => (x.textContent || '').trim() === '知道了');
            if (!b) return 'no-btn';
            b.click();
            return 'clicked';
        })()"#,
    )
    .await?;
    Ok(result.as_str() == Some("clicked"))
}

/// 账号体检结果。
pub struct ProbeOutcome {
    /// 是否处于已登录状态（页面出现输入框 = 登录态有效）。
    pub logged_in: bool,
}

/// 对指定账号做一次体检：注入 cookie → 导航 grok.com → 检查是否登录。
///
/// 复用 UI_CHAT_LOCK 串行化，避免与真实对话/保活抢浏览器（实测并发会互相踩）。
/// 注入成功但页面无输入框 → 视为登录态失效（sso 过期）。
pub async fn probe_account(cookie_string: &str) -> Result<ProbeOutcome, String> {
    let _guard = UI_CHAT_LOCK.lock().await;
    let cfg = load_config();
    ensure_browser(&cfg).await?;
    ensure_grok_page().await?;

    let target_fp = extract_sso_fingerprint(cookie_string);
    if target_fp.is_empty() {
        return Err("账号 cookie 为空".to_string());
    }
    let current_fp = current_browser_sso_fingerprint().await.unwrap_or_default();
    if current_fp.is_empty() || current_fp != target_fp {
        switch_account_to(cookie_string).await?;
        mark_browser_taken_over();
    }

    let ws_url = grok_page_ws_url().await?;
    let _ = cdp_call_on(&ws_url, "Page.bringToFront", json!({})).await;
    let _ = cdp_call_on(
        &ws_url,
        "Page.navigate",
        json!({"url": format!("{GROK_URL}/")}),
    )
    .await;
    // 轮询等输入框出现（SPA 渲染耗时不定，实测可达 10s+）。
    // 固定 sleep 会误判好账号失效 —— 必须轮询 + 超时才判负。
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let _ = dismiss_tos_if_present().await;
        let state = eval_js(
            r#"(() => {
                const hasPM = !!document.querySelector('.tiptap.ProseMirror');
                return JSON.stringify({ hasPM });
            })()"#,
        )
        .await;
        if let Ok(v) = state {
            let parsed: Value =
                serde_json::from_str(v.as_str().unwrap_or("{}")).unwrap_or(json!({}));
            if parsed.get("hasPM").and_then(Value::as_bool).unwrap_or(false) {
                return Ok(ProbeOutcome { logged_in: true });
            }
        }
        if Instant::now() >= deadline {
            return Ok(ProbeOutcome { logged_in: false });
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

// ---------------------------------------------------------------------------
// v1.2.0：UI 驱动 chat（绕 Cloudflare 的唯一可靠路径）
// ---------------------------------------------------------------------------

/// 在页面上下文执行 JS 并返回 value（包装 cdp_call_on）。
async fn eval_js(js: &str) -> Result<Value, String> {
    let list: Vec<Value> = cdp_http_client().get(format!("http://127.0.0.1:{CDP_PORT}/json"))
        .send()
        .await
        .map_err(|e| format!("CDP /json 请求失败: {e}"))?
        .json()
        .await
        .map_err(|e| format!("CDP /json 解析失败: {e}"))?;
    let grok = list
        .iter()
        .find(|t| {
            t.get("type").and_then(Value::as_str) == Some("page")
                && t.get("url")
                    .and_then(Value::as_str)
                    .map(|u| u.starts_with(GROK_URL))
                    .unwrap_or(false)
        })
        .ok_or_else(|| "无 grok.com 页面".to_string())?;
    let ws_url = grok
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "grok 页面缺少 WS URL".to_string())?;
    let result = cdp_call_on(ws_url, "Runtime.evaluate", json!({"expression": js, "returnByValue": true})).await?;
    Ok(result.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

/// UI 驱动对话：新建会话页 → tiptap 输入 → 点提交 → 轮询 DOM 取 assistant 回复。
/// 这是唯一能穿 Cloudflare 的 web 通道（真实浏览器事件链路）。
pub async fn ui_chat(prompt: &str) -> Result<String, ChatError> {
    let cfg = load_config();
    ensure_browser(&cfg)
        .await
        .map_err(|e| ChatError::new(ChatErrorKind::Infra, e))?;
    // 回新会话页（主页），保证从干净状态开始。
    let list: Vec<Value> = cdp_http_client().get(format!("http://127.0.0.1:{CDP_PORT}/json"))
        .send()
        .await
        .map_err(|e| ChatError::new(ChatErrorKind::Infra, format!("CDP /json 请求失败: {e}")))?
        .json()
        .await
        .map_err(|e| ChatError::new(ChatErrorKind::Infra, format!("CDP /json 解析失败: {e}")))?;
    let grok = list
        .iter()
        .find(|t| {
            t.get("type").and_then(Value::as_str) == Some("page")
                && t.get("url")
                    .and_then(Value::as_str)
                    .map(|u| u.starts_with(GROK_URL))
                    .unwrap_or(false)
        })
        .ok_or_else(|| {
            let dump: Vec<String> = list
                .iter()
                .map(|t| {
                    format!(
                        "{}/{}",
                        t.get("type").and_then(Value::as_str).unwrap_or("?"),
                        t.get("url").and_then(Value::as_str).unwrap_or("?")
                    )
                })
                .collect();
            let _ = std::fs::write(
                store_dir().join("grok_sso_debug.log"),
                format!("{} ui_chat targets: {}", now_ms(), dump.join(" | ")),
            );
            "无 grok.com 页面".to_string()
        })
        .map_err(|e| ChatError::new(ChatErrorKind::Infra, e))?;
    let ws_url = grok
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| ChatError::new(ChatErrorKind::Infra, "grok 页面缺少 WS URL"))
        .map(String::from)?;
    // 页面在后台时 Chrome 节流输入/定时器，tiptap 对 Input.insertText 无响应
    // （实测 visibilityState=hidden 时提交按钮点击完全不产生网络请求）。
    // 必须先 bringToFront。
    let _ = cdp_call_on(&ws_url, "Page.bringToFront", json!({})).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = cdp_call_on(&ws_url, "Page.navigate", json!({"url": format!("{GROK_URL}/")})).await;
    // 轮询等输入框出现（页面加载/SPA 切换耗时不可控，固定 sleep 不够）。
    // 期间处理 ToS 门槛（新账号首次登录必经）。
    let pm_deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let _ = dismiss_tos_if_present().await;
        // 导航过渡期 eval_js 会报「无 grok.com 页面」，容错跳过。
        let pm = eval_js("document.querySelectorAll('.tiptap.ProseMirror').length")
            .await
            .ok()
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if pm > 0 {
            break;
        }
        if Instant::now() >= pm_deadline {
            return Err(ChatError::new(
                ChatErrorKind::Infra,
                "导航到 Grok 主页后 45 秒内未出现输入框（可能卡在 ToS 门槛或页面异常）",
            ));
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

    // 1) 关 OneTrust 弹窗（首次访问会出现，挡住输入区）。
    let _ = eval_js(
        "(() => { const b = document.getElementById('onetrust-accept-btn-handler'); if (b) { b.click(); return 1; } const c = document.querySelector('.onetrust-close-btn-handler'); if (c) { c.click(); return 1; } return 0; })()",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 2) tiptap focus + 真实键盘输入。
    let focused = eval_js(
        "(() => { const pm = document.querySelector('.tiptap.ProseMirror'); if (!pm) return 0; pm.focus(); return 1; })()",
    )
    .await
    .map_err(|e| ChatError::new(ChatErrorKind::Infra, e))?;
    if focused.as_i64() != Some(1) {
        return Err(ChatError::new(
            ChatErrorKind::Infra,
            "未找到 Grok 输入框（页面可能改版或未加载完成）",
        ));
    }
    let _ = cdp_call_on(&ws_url, "Input.insertText", json!({"text": prompt}))
        .await
        .map_err(|e| ChatError::new(ChatErrorKind::Infra, e))?;
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // 3) 写入 prompt 锚（截 80 字，供回复轮询定位 user 气泡），点「提交」。
    let anchor_short: String = prompt.chars().take(80).collect();
    let _ = eval_js(&format!(
        "window.__uiPromptAnchor = {};",
        serde_json::to_string(&anchor_short)
            .map_err(|e| ChatError::new(ChatErrorKind::Infra, e.to_string()))?
    ))
    .await;
    let _ = eval_js(
        "(() => { const b = [...document.querySelectorAll('button')].find(x => x.getAttribute('aria-label') === '提交'); if (!b) return 0; b.click(); return 1; })()",
    )
    .await
    .map_err(|e| ChatError::new(ChatErrorKind::Infra, e))?;

    // 4) 轮询：以「生成中」按钮状态为主信号判断回复完成（v1.5.0）。
    //
    //    实测（2026-09-17）：生成中会出现 aria-label='停止模型响应' 的按钮，
    //    生成完成后该按钮消失、'提交' 按钮恢复。仅靠"文本连续两轮相同"会在
    //    思考占位文案（'工作...'/'Analyzing...'）阶段误判完成 —— 这是旧版
    //    回复被截断的根因。现在要求：不再生成 && 有回复文本 && 非占位。
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut last_stable: Option<String> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(3000)).await;
        // 导航过渡期 target URL 会短暂离开 grok.com，eval_js 可能报
        // 「无 grok.com 页面」——跳过本轮继续轮询，而不是中断整个 chat。
        let state = match eval_js(
            r#"(() => {
                const bubbles = [...document.querySelectorAll('.message-bubble')];
                // v1.6.0：正文只取 .response-content-markdown。
                // 气泡 textContent 会把思考面板（"工作了 Ns"/"执行了 N 次搜索"/
                // "N sources"）拼进来，实测这些 UI 文案会泄漏为正文（2026-09-24 实证）。
                // 该容器不存在时（页面改版）回退气泡全文，保证不比旧版更差。
                const body = (b) => {
                    const md = b.querySelector('.response-content-markdown');
                    const t = md ? (md.textContent || '').trim() : '';
                    return t || (b.textContent || '').trim();
                };
                const texts = bubbles.map(body);
                const raw = bubbles.map(b => (b.textContent || '').trim());
                // 生成中：存在"停止"按钮（实测 aria-label='停止模型响应'）。
                const generating = [...document.querySelectorAll('button')].some(
                    x => /停止/.test(x.getAttribute('aria-label') || ''));
                // 占位文案：仅在「干净正文尚未产出」时才算占位。
                // 旧逻辑拿气泡全文判定，正文短（如"收到"）而思考面板残留时
                // 会被永久判为占位 → 240s 超时；现在以正文是否为空为准。
                const lastText = (raw[raw.length - 1] || '').trim();
                const lastBody = (texts[texts.length - 1] || '').trim();
                const placeholder = lastBody.length === 0
                    && /^(工作|思考|Analyzing|Thinking|Working)/.test(lastText)
                    && lastText.length <= 48;
                // v1.6.1：定位本次回复 —— 优先用语义 data-testid。
                // 不能用纯文本锚点做主手段：页面会把缩进空格渲染成 \xa0（NBSP），
                // 而注入的 [AVAILABLE_TOOLS] 里有 JSON 缩进 → 原样字符串匹配失配
                // → anchor=-1 → 完成判定永不触发 → 240s 超时（0.1.86 实证）。
                let anchor = -1;
                let reply_idx = -1;
                const role = (i) => bubbles[i].getAttribute('data-testid') || '';
                for (let i = bubbles.length - 1; i >= 0; i--) {
                    if (role(i) === 'user-message') { anchor = i; break; }
                }
                if (anchor >= 0) {
                    for (let i = anchor + 1; i < bubbles.length; i++) {
                        if (role(i) === 'assistant-message') { reply_idx = i; break; }
                    }
                }
                // 兜底：data-testid 改版时退回文本锚点，但先归一化空白（含 NBSP）
                if (reply_idx < 0) {
                    const norm = (s) => (s || '').replace(/\s+/g, ' ');
                    const key = norm(window.__uiPromptAnchor || '');
                    if (key) {
                        const ai = raw.findIndex(t => norm(t).includes(key));
                        if (ai >= 0 && ai + 1 < texts.length) { anchor = ai; reply_idx = ai + 1; }
                    }
                }
                const reply = reply_idx >= 0 ? texts[reply_idx] : '';
                // 限流错误文案出现在气泡外（body 全文），出现且 user 气泡已存在 =
                // 本条消息被限流，不会再有 assistant 回复。
                const limited = /使用人数较多|请稍后再试|升级你的订阅/.test(document.body.innerText);
                return JSON.stringify({ anchor, reply: reply.slice(-4000), limited, generating, placeholder });
            })()"#,
        )
        .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        let parsed: Value = serde_json::from_str(state.as_str().unwrap_or("{}")).unwrap_or(json!({}));
        let anchor_idx = parsed.get("anchor").and_then(Value::as_i64).unwrap_or(-1);
        let last = parsed.get("reply").and_then(Value::as_str).unwrap_or("");
        // 限流检测：user 气泡已出现但无 assistant 回复，且页面有限流文案。
        if anchor_idx >= 0 && last.is_empty()
            && parsed.get("limited").and_then(Value::as_bool) == Some(true)
        {
            return Err(ChatError::new(
                ChatErrorKind::RateLimited,
                "Grok 免费额度限流（使用人数较多）。SSO cookie 本身有效，稍后自动恢复",
            ));
        }
        // 完成判定（v1.5.0，以按钮状态为主信号）：
        // 找到 user 锚点 + 有回复文本 + 非占位 + 不在生成中 + 文本稳定。
        let generating = parsed.get("generating").and_then(Value::as_bool).unwrap_or(false);
        let placeholder = parsed.get("placeholder").and_then(Value::as_bool).unwrap_or(false);
        if anchor_idx >= 0 && !last.is_empty() {
            // Grok 免费额度限流：把服务端文案作为错误透出，而不是当作回复。
            if last.contains("请稍后再试") || last.contains("使用人数较多") || last.contains("升级你的订阅") {
                return Err(ChatError::new(
                    ChatErrorKind::RateLimited,
                    format!("Grok 免费额度限流：{}", last.chars().take(80).collect::<String>()),
                ));
            }
            // 关键：只要仍在生成中，或末尾是思考占位文案，就继续等 —— 否则会
            // 把 '工作... 1s' 之类的中间态当成最终回复返回（旧版截断 bug）。
            if !generating && !placeholder && last_stable.as_deref() == Some(last) {
                return Ok(last.to_string());
            }
            last_stable = Some(last.to_string());
        }
        if Instant::now() >= deadline {
            // 超时：把当前已有内容带回去（可能是部分回复），避免完全丢上下文。
            if !last.is_empty() {
                return Ok(format!("{last}

[注意：回复可能未生成完毕]"));
            }
            return Err(ChatError::new(
                ChatErrorKind::Timeout,
                "等待 Grok 回复超时（240 秒）",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_ms_handles_epoch_and_session() {
        assert_eq!(cookie_ms(-1.0), 0);
        assert_eq!(cookie_ms(0.0), 0);
        assert_eq!(cookie_ms(1789457858.0), 1_789_457_858_000);
    }

    #[test]
    fn resolve_chrome_prefers_config_and_known_paths() {
        // 配置路径无效时回退到已知路径探测（本机装有浏览器 → Some）。
        let resolved = resolve_chrome_path(r"C:\nonexistent\chrome.exe");
        let any_known = [
            r"E:\驱动精灵搬家目录(勿删改)\软件\Google Chrome\安装目录\541\chrome.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        ]
        .iter()
        .any(|k| PathBuf::from(k).is_file());
        assert_eq!(resolved.is_some(), any_known);
    }
}
