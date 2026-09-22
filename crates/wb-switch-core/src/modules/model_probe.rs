//! 模型可用性自测：在本机直接对「网关 + 模型」发起一次**真实调用**，给出可用性结论与失败原因，
//! 免去用户再到别的客户端里配一遍模型才知道能不能用。
//!
//! 设计要点（都是刻意的，改动前先读）：
//! - **回环自调用**：请求打到本服务**自己的对外端点** `127.0.0.1:{port}{base}/chat/completions`，
//!   因此覆盖「鉴权 → 路由 → 账号选择 → 上游 → 协议转换」完整链路，
//!   失败原因与真实客户端所见一致（而不是绕过网关的内部直连，那种测不出鉴权/路由问题）。
//! - **鉴权用 GLM 主密钥**：`gateway_admin::verify_request_key` 允许「网关专用 Key」或
//!   「GLM 代理主密钥」二选一；主密钥命中时**提前返回、不计入 Key 的 requestCount**，
//!   所以自测不会污染管理台里网关密钥的使用统计（用专用 Key 会每次都 +1）。
//! - **非流式 + 小 `max_tokens`**：一次请求即得确定结论，耗时短，且不存在 SSE 分块解码歧义
//!   （流式路径历史上踩过「UTF-8 半字符 chunk 整块丢弃」的坑，见 CHANGELOG 0.1.43）。
//! - **必须 `no_proxy()`**：本机回环不得继承系统代理 —— 本机 `HTTP_PROXY` 残留会让
//!   `127.0.0.1` 也被送去代理，直接表现成"连不上网关"。

use serde_json::{json, Value};
use std::error::Error as _;
use std::time::{Duration, Instant};

use crate::modules::config::now_ms;
use crate::modules::{gateway_overview, glm_proxy};

const DEFAULT_PROMPT: &str = "只回复两个字：可用";
const DEFAULT_MAX_TOKENS: u64 = 64;
const MAX_MAX_TOKENS: u64 = 8192;
const MAX_PROMPT_CHARS: usize = 4000;
const CONNECT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_TIMEOUT_SECS: u64 = 90;
const MIN_TIMEOUT_SECS: u64 = 5;
const MAX_TIMEOUT_SECS: u64 = 600;
const RAW_SNIPPET_CHARS: usize = 600;

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 执行一次模型可用性探测。
///
/// `bound_port` 由服务端传入（`api::BOUND_PORT`）——core 层不持有监听端口，
/// 与 `opencode::config_info(bound_port)` 的既有做法一致。
/// 返回结构见本文件末尾的 `result_shape` 单测。
pub async fn run(bound_port: u16, body: &Value) -> Value {
    let gateway = str_field(body, "gateway");
    let model = str_field(body, "model");
    let prompt = {
        let raw = str_field(body, "prompt");
        if raw.is_empty() {
            DEFAULT_PROMPT.to_string()
        } else {
            raw.chars().take(MAX_PROMPT_CHARS).collect()
        }
    };
    let max_tokens = body
        .get("maxTokens")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0 && *v <= MAX_MAX_TOKENS)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let timeout_secs = body
        .get("timeoutSecs")
        .and_then(Value::as_u64)
        .filter(|v| *v >= MIN_TIMEOUT_SECS && *v <= MAX_TIMEOUT_SECS)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    let started = Instant::now();
    let mut base = json!({
        "at": now_ms(),
        "gateway": gateway,
        "gatewayName": Value::Null,
        "model": model,
        "protocol": "openai-chat",
        "prompt": prompt,
        "maxTokens": max_tokens,
        "endpoint": Value::Null,
        "status": Value::Null,
        "reply": Value::Null,
        "reasoning": Value::Null,
        "tokens": Value::Null,
    });

    // ① 解析网关 → 基址
    let Some((gw_name, gw_base)) = gateway_overview::gateway_base(&gateway) else {
        let shown = if gateway.is_empty() {
            "(空)".to_string()
        } else {
            gateway.clone()
        };
        return fail(
            base,
            started,
            "unknown_gateway",
            format!("未知网关「{shown}」——请从网关下拉列表中选择"),
            Value::Null,
        );
    };
    base["gatewayName"] = json!(gw_name);
    if model.is_empty() {
        return fail(
            base,
            started,
            "missing_model",
            "未指定模型 ID——请从模型下拉列表中选择或手动填写".to_string(),
            Value::Null,
        );
    }
    let endpoint = format!("http://127.0.0.1:{bound_port}{gw_base}/chat/completions");
    base["endpoint"] = json!(endpoint);

    // ② 鉴权：主密钥全网关通用，且不计入网关 Key 使用计数
    let master = glm_proxy::load_config().api_key;
    if master.trim().is_empty() {
        return fail(
            base,
            started,
            "no_master_key",
            "GLM 主密钥为空，无法自测：请先在「GLM 代理」页完成初始化".to_string(),
            Value::Null,
        );
    }

    // ③ 建客户端（回环必须绕开系统代理）
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(timeout_secs))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return fail(
                base,
                started,
                "client_build_failed",
                format!("HTTP 客户端创建失败：{e}"),
                Value::Null,
            )
        }
    };

    let payload = json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": max_tokens,
        "stream": false,
    });

    // ④ 发送
    let resp = match client
        .post(&endpoint)
        .bearer_auth(&master)
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let (code, reason) = classify_transport_error(&e, timeout_secs);
            return fail(base, started, code, reason, Value::Null);
        }
    };

    let status = resp.status().as_u16();
    base["status"] = json!(status);
    let text = resp.text().await.unwrap_or_default();
    let snippet = snippet_of(&text);

    // ⑤ HTTP 层失败 → 直接透出上游原因
    if !(200..300).contains(&status) {
        let message = upstream_error_message(&text).unwrap_or_else(|| {
            if snippet.is_empty() {
                format!("上游返回 HTTP {status}（响应体为空）")
            } else {
                format!("上游返回 HTTP {status}：{snippet}")
            }
        });
        let code = match status {
            401 | 403 => "auth_failed",
            404 => "model_not_found",
            429 => "rate_limited",
            _ => "upstream_error",
        };
        return fail(base, started, code, message, json!(snippet));
    }

    // ⑥ 200 → 解析响应体
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            let reason = if snippet.is_empty() {
                format!("上游返回 200 但响应体为空（无法解析：{e}）")
            } else {
                format!("上游返回 200 但响应体不是合法 JSON（{e}）：{snippet}")
            };
            return fail(base, started, "bad_response", reason, json!(snippet));
        }
    };

    // 200 里带 error 字段：部分网关用 200 承载业务错误
    if let Some(err) = parsed.get("error").filter(|v| !v.is_null()) {
        let message = upstream_error_message(&text)
            .unwrap_or_else(|| format!("上游返回 200 但内含错误：{}", snippet_of(&err.to_string())));
        return fail(base, started, "upstream_error", message, json!(snippet));
    }

    let (reply, reasoning) = extract_reply(&parsed);
    base["tokens"] = extract_usage(&parsed);

    if reply.is_empty() && reasoning.is_empty() {
        return fail(
            base,
            started,
            "empty_reply",
            format!("上游返回 200 但没有任何内容（收到 {} 字节，无正文无思维链）——多为账号额度受限、被风控或上游抖动", text.len()),
            json!(snippet),
        );
    }

    // ⑦ 成功
    let mut out = base;
    out["ok"] = json!(true);
    out["code"] = json!("ok");
    out["latencyMs"] = json!(started.elapsed().as_millis() as u64);
    out["raw"] = Value::Null;
    if reply.is_empty() {
        // 只有思维链：模型确实可用，但没产出可见正文，如实标注
        out["reply"] = Value::Null;
        out["reasoning"] = json!(truncate(&reasoning, 400));
        out["reason"] = json!("模型可用（本次只返回了思维链，无可见正文）");
    } else {
        out["reason"] = json!("调通成功");
        out["reply"] = json!(truncate(&reply, 600));
        out["reasoning"] = if reasoning.is_empty() {
            Value::Null
        } else {
            json!(truncate(&reasoning, 400))
        };
    }
    out
}

// ---------------------------------------------------------------------------
// 结果构造
// ---------------------------------------------------------------------------

fn fail(mut base: Value, started: Instant, code: &str, reason: String, raw: Value) -> Value {
    base["ok"] = json!(false);
    base["code"] = json!(code);
    base["reason"] = json!(reason);
    base["raw"] = raw;
    base["latencyMs"] = json!(started.elapsed().as_millis() as u64);
    base
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// 压平成单行并截断，避免把多行原始报文塞进 UI。
fn snippet_of(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    truncate(flat.trim(), RAW_SNIPPET_CHARS)
}

/// 完整 source 链：reqwest 的 Display 会掩盖真因（"error sending request"）。
fn error_chain(e: &(dyn std::error::Error)) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(c) = cur {
        parts.push(c.to_string());
        cur = c.source();
    }
    parts.join(" ← ")
}

fn classify_transport_error(e: &reqwest::Error, timeout_secs: u64) -> (&'static str, String) {
    let raw = error_chain(e);
    if e.is_timeout() {
        (
            "timeout",
            format!("超时：{timeout_secs}s 内未收到完整响应。上游可能过载或模型响应过慢，可重试或调大超时。原文：{raw}"),
        )
    } else if e.is_connect() {
        (
            "connect_failed",
            format!("无法连接本机网关（Sylvia Switch 自身）。若服务正在重启/重载，请稍后重试。原文：{raw}"),
        )
    } else if e.is_request() {
        ("network_error", format!("请求发送失败：{raw}"))
    } else {
        ("network_error", format!("网络层错误：{raw}"))
    }
}

/// 从上游错误体里抽可读原因：优先 JSON 的 message 类字段，其次原始文本。
fn upstream_error_message(text: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        for pointer in [
            "/error/message",
            "/message",
            "/error/error/message",
            "/msg",
            "/detail",
            "/error",
        ] {
            if let Some(s) = v.pointer(pointer).and_then(Value::as_str) {
                if !s.trim().is_empty() {
                    return Some(format!("上游拒绝：{}", truncate(s.trim(), 400)));
                }
            }
        }
    }
    let snippet = snippet_of(text);
    if snippet.is_empty() {
        None
    } else {
        Some(format!("上游返回错误：{snippet}"))
    }
}

/// 抽取正文与思维链，同时兼容 OpenAI 与 Anthropic 两种响应形状。
fn extract_reply(v: &Value) -> (String, String) {
    let mut text = String::new();
    let mut reasoning = String::new();

    if let Some(first) = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        let msg = first.get("message").unwrap_or(&Value::Null);
        if let Some(s) = msg.get("content").and_then(Value::as_str) {
            text.push_str(s);
        } else if let Some(arr) = msg.get("content").and_then(Value::as_array) {
            // 少数实现把 content 拆成 [{type,text}]
            for c in arr {
                if let Some(s) = c.get("text").and_then(Value::as_str) {
                    text.push_str(s);
                }
            }
        }
        for key in ["reasoning_content", "reasoning"] {
            if let Some(s) = msg.get(key).and_then(Value::as_str) {
                reasoning.push_str(s);
                break;
            }
        }
    } else if let Some(arr) = v.get("content").and_then(Value::as_array) {
        // Anthropic Messages 形状
        for c in arr {
            match c.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(c.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("thinking") => {
                    reasoning.push_str(c.get("thinking").and_then(Value::as_str).unwrap_or(""))
                }
                _ => {}
            }
        }
    }

    (text.trim().to_string(), reasoning.trim().to_string())
}

/// usage 归一：OpenAI 用 prompt/completion，Anthropic 用 input/output。
fn extract_usage(v: &Value) -> Value {
    let Some(u) = v.get("usage").filter(|u| u.is_object()) else {
        return Value::Null;
    };
    let pick = |keys: &[&str]| -> Option<u64> {
        keys.iter()
            .find_map(|k| u.get(*k).and_then(Value::as_u64))
    };
    let prompt = pick(&["prompt_tokens", "input_tokens"]);
    let completion = pick(&["completion_tokens", "output_tokens"]);
    let total = pick(&["total_tokens"])
        .or_else(|| match (prompt, completion) {
            (Some(p), Some(c)) => Some(p + c),
            _ => None,
        });
    if prompt.is_none() && completion.is_none() && total.is_none() {
        return Value::Null;
    }
    json!({ "prompt": prompt, "completion": completion, "total": total })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_reply_handles_openai_shape() {
        let v = json!({
            "choices": [{ "message": { "content": "可用", "reasoning_content": "在想" } }]
        });
        let (t, r) = extract_reply(&v);
        assert_eq!(t, "可用");
        assert_eq!(r, "在想");
    }

    #[test]
    fn extract_reply_handles_anthropic_shape() {
        let v = json!({
            "content": [
                { "type": "thinking", "thinking": "思考中" },
                { "type": "text", "text": "可用" }
            ]
        });
        let (t, r) = extract_reply(&v);
        assert_eq!(t, "可用");
        assert_eq!(r, "思考中");
    }

    #[test]
    fn extract_reply_only_thinking_is_not_text() {
        let v = json!({ "choices": [{ "message": { "reasoning_content": "只有思维链" } }] });
        let (t, r) = extract_reply(&v);
        assert!(t.is_empty());
        assert_eq!(r, "只有思维链");
    }

    #[test]
    fn extract_usage_normalizes_both_protocols() {
        let o = extract_usage(&json!({ "usage": { "prompt_tokens": 3, "completion_tokens": 5 } }));
        assert_eq!(o["prompt"], json!(3));
        assert_eq!(o["total"], json!(8));
        let a = extract_usage(&json!({ "usage": { "input_tokens": 2, "output_tokens": 7 } }));
        assert_eq!(a["completion"], json!(7));
        assert_eq!(extract_usage(&json!({})), Value::Null);
    }

    #[test]
    fn upstream_error_message_prefers_json_message() {
        let got = upstream_error_message(r#"{"error":{"message":"额度不足"}}"#).unwrap();
        assert!(got.contains("额度不足"), "got={got}");
        let raw = upstream_error_message("plain text failure").unwrap();
        assert!(raw.contains("plain text failure"));
        assert!(upstream_error_message("   ").is_none());
    }

    #[test]
    fn snippet_flattens_and_truncates() {
        assert_eq!(snippet_of("a\nb\r\nc"), "a b  c");
        let long = "x".repeat(RAW_SNIPPET_CHARS + 50);
        assert_eq!(snippet_of(&long).chars().count(), RAW_SNIPPET_CHARS + 1); // 含省略号
    }

    #[test]
    fn truncate_keeps_multibyte_intact() {
        assert_eq!(truncate("中文三字", 2), "中文…");
        assert_eq!(truncate("中文", 5), "中文");
    }

    /// 结果结构契约：前端按这些字段渲染，改动需同步 `model-test-types.ts`。
    #[tokio::test]
    async fn unknown_gateway_fails_with_actionable_reason() {
        let out = run(57890, &json!({ "gateway": "no-such-gw", "model": "m" })).await;
        assert_eq!(out["ok"], json!(false));
        assert_eq!(out["code"], json!("unknown_gateway"));
        assert!(out["reason"].as_str().unwrap().contains("未知网关"));
        assert!(out["latencyMs"].is_number());
    }

    #[tokio::test]
    async fn missing_model_is_reported_before_any_network_call() {
        let out = run(57890, &json!({ "gateway": "glm" })).await;
        assert_eq!(out["code"], json!("missing_model"));
        assert_eq!(out["gatewayName"], json!("GLM 代理"));
    }
}
