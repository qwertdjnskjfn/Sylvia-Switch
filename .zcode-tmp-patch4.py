import re

p = r"crates/wb-switch-core/src/modules/zcode.rs"
t = open(p, encoding="utf-8").read()

# 1) pick_page_ws_url_on → 返回全部页面 target（按优先级排序：zcode 主页面 > 其它 page > 排除 devtools/extension）
old = """async fn pick_page_ws_url_on(port: u16) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{port}/json");
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("CDP /json 请求失败: {e}"))?;
    let targets: Value = resp.json().await.map_err(|e| format!("CDP /json 解析失败: {e}"))?;
    let pages = targets
        .as_array()
        .ok_or("CDP targets 格式异常")?
        .into_iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .collect::<Vec<_>>();
    let ws = pages
        .first()
        .and_then(|t| t.get("webSocketDebuggerUrl").and_then(Value::as_str))
        .ok_or("CDP 无可用页面 target")?;
    Ok(ws.to_string())
}"""
new = """async fn pick_page_ws_url_on(port: u16) -> Result<String, String> {
    pick_page_ws_urls_on(port)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| "CDP 无可用页面 target".to_string())
}

/// 全部可用页面 target 的 WS URL（排序：排除 devtools/extension；URL 含 zcode
/// 的主窗口优先——铸码必须落在有用户活动痕迹的主渲染进程，后台工具页会被
/// 阿里云风控拒绝（2026-10-03 实测首个 target 铸码 unknown 失败））。
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
        (
            if lower.contains("zcode") { 0 } else { 1 },
            u.len(),
        )
    });
    Ok(pages.into_iter().map(|(ws, _)| ws).collect())
}"""
assert old in t
t = t.replace(old, new, 1)

# 2) mint：遍历全部 target，带回每页失败原因
old2 = """    let out = cdp_eval_on(port, &js, 40).await?;
    let ok = out.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if ok {
        out.get("param")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "验证码参数缺失".to_string())
    } else {
        Err(format!(
            "验证码未通过（无痕验证被风控拦截，需人工）：{}",
            out.get("reason")
                .or_else(|| out.get("failInfo"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ))
    }
}"""
new2 = """    let urls = pick_page_ws_urls_on(port).await?;
    if urls.is_empty() {
        return Err("CDP 无可用页面 target".to_string());
    }
    let mut last: Option<String> = None;
    for ws_url in &urls {
        let out = match cdp_eval_on_ws(ws_url, &js, 40).await {
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
        last = Some(
            out.get("reason")
                .or_else(|| out.get("failInfo"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        );
    }
    Err(format!(
        "验证码未通过（无痕验证被风控拦截，需人工）：{}（尝试了 {} 个页面）",
        last.unwrap_or_else(|| "unknown".into()),
        urls.len()
    ))
}"""
assert old2 in t
t = t.replace(old2, new2, 1)

# 3) cdp_eval_on 变体：直接连指定 ws url
old3 = """async fn cdp_eval_on(port: u16, expression: &str, timeout_secs: u64) -> Result<Value, String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let ws_url = pick_page_ws_url_on(port).await?;
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)"""
new3 = """async fn cdp_eval_on(port: u16, expression: &str, timeout_secs: u64) -> Result<Value, String> {
    let ws_url = pick_page_ws_url_on(port).await?;
    cdp_eval_on_ws(&ws_url, expression, timeout_secs).await
}

async fn cdp_eval_on_ws(ws_url: &str, expression: &str, timeout_secs: u64) -> Result<Value, String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)"""
assert old3 in t
t = t.replace(old3, new3, 1)

# 4) JS failInfo 安全序列化（undefined 时 JSON.stringify 返回 undefined 导致 .slice 抛错）
old4 = """            fail(e) { failInfo = e; log.push('fail'); }},
            onError(e) { failInfo = e; log.push('onerror'); }},"""
new4 = """            fail(e) { failInfo = (e === undefined || e === null) ? 'fail' : (typeof e === 'string' ? e : JSON.stringify(e)); log.push('fail'); }},
            onError(e) { failInfo = (e === undefined || e === null) ? 'onerror' : (typeof e === 'string' ? e : JSON.stringify(e)); log.push('onerror'); }},"""
assert old4 in t
t = t.replace(old4, new4, 1)

# 5) 重启前预告通知（解决「莫名其妙被退出」）
old5 = """        // 自动重启：热切换语义下换写凭据不影响登录，重启仅数秒——领取窗口期一次性成本。
        let _ = kill_zcode()?;
        tokio::time::sleep(Duration::from_millis(1500)).await;"""
new5 = """        // 自动重启：热切换语义下换写凭据不影响登录，重启仅数秒——领取窗口期一次性成本。
        // 预告通知：用户不该看到 ZCode「无故」退出。
        crate::modules::notify::notify(
            "zcode-claim",
            "ZCode 即将自动重启",
            "为领取每日福利需开启调试通道，ZCode 将在数秒内重启（登录不受影响）",
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = kill_zcode()?;
        tokio::time::sleep(Duration::from_millis(1500)).await;"""
assert old5 in t
t = t.replace(old5, new5, 1)
open(p, "w", encoding="utf-8", newline="").write(t)
print("mint multi-target + notify done")
