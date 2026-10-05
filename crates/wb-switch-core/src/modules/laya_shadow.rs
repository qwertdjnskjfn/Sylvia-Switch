//! Laya 影子模式（调研 P1）：对网关入口的用户请求旁路打 System 1 类型化标签，
//! 只落本地 JSONL，不参与路由决策。调研与实测：知识库《主题-Laya决策引擎调研与wb-switch引入评估》。
//!
//! 开关：`~/.wb-switch/laya_shadow.json` 存在且 `enabled: true` 才生效（默认关闭，零开销）。
//! 失败语义：任何环节失败都静默跳过（HTTP 500ms 硬顶）——影子层绝不影响网关主链路。
//!
//! sidecar：工作流 `laya-sidecar/`（laya-serve @ 127.0.0.1:8000，协议 TypeSafe Jev /v1/systemone）。

use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::modules::config;

const DEFAULT_URL: &str = "http://127.0.0.1:8000/v1/systemone";
/// 影子调用超时上限 8s：调用在 tokio::spawn 里异步执行、不阻塞主链路，无 500ms 硬顶的必要。
/// 实测警示：sidecar 单 worker 队列在压测下同 4 问题负载需 ~7.6s（见 CHANGELOG 1.0.38），
/// 压到 500ms 会把正常推理误杀成 error sending request，影子全无数据。
const TIMEOUT_MS: u64 = 8_000;
/// 影子标签只关心意图/难度，正文截断 512 字符足够（multilingual 检查点 1024 token 预算）。
const TEXT_CAP_CHARS: usize = 512;

fn config_path() -> PathBuf {
    config::store_dir().join("laya_shadow.json")
}

fn jsonl_path() -> PathBuf {
    config::store_dir().join("laya_shadow.jsonl")
}

/// 读取开关；文件缺失 / 解析失败 / enabled != true 一律视为关闭。
fn shadow_url() -> Option<String> {
    let s = std::fs::read_to_string(config_path()).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    if v.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    Some(
        v.get("url")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_URL)
            .to_string(),
    )
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(TIMEOUT_MS))
            // 影子目标固定是 127.0.0.1 本地 sidecar：本机 Clash TUN 会把回环也劫走，
            // 必须显式 no_proxy，否则请求被代理吞掉（error sending request）。
            .no_proxy()
            .build()
            .expect("laya_shadow http client")
    })
}

/// 从 OpenAI / Anthropic 两种 messages 结构取最后一条 user 消息的纯文本。
/// content 兼容字符串与 `[{type:"text",text:...}]` 数组两种形态；取不到返回 None（跳过打标）。
pub fn extract_last_user_text(body: &Value) -> Option<String> {
    let msgs = body.get("messages").and_then(Value::as_array)?;
    for m in msgs.iter().rev() {
        if m.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        match m.get("content") {
            Some(Value::String(s)) if !s.trim().is_empty() => return Some(s.clone()),
            Some(Value::Array(parts)) => {
                let text: String = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() {
                    return Some(text);
                }
            }
            _ => {}
        }
    }
    None
}

/// 官方 router_questions 预设（laya v0.3.27 presets.py 逐字对齐，见 laya-sidecar/request-router.json）：
/// difficulty(0-3) / domain / needs_tools / is_sensitive —— 网关分档路由（P2）要用的四轴。
fn router_questions() -> Value {
    json!({
        "difficulty": {
            "type": "score",
            "instructions": "How hard is `request` for a language model?",
            "criteria": [
                "trivial: a lookup or one-liner",
                "easy: short answer, no reasoning",
                "moderate: several steps",
                "hard: long multi-step reasoning or specialist knowledge"
            ]
        },
        "domain": {
            "type": "choice",
            "instructions": "What domain does `request` belong to?",
            "criteria": {
                "code": "software engineering, programming, refactoring, architecture, debugging",
                "math_or_logic": "mathematics, logic puzzles, proofs, complex calculation",
                "writing": "creative writing, essays, emails, blog posts, copywriting",
                "factual_lookup": "facts, definitions, trivia, history",
                "data_analysis": "statistics, SQL, data manipulation, metrics",
                "chitchat": "casual conversation, greetings, small talk"
            }
        },
        "needs_tools": {
            "type": "noul",
            "instructions": "Does answering `request` require calling tools or external APIs?"
        },
        "is_sensitive": {
            "type": "noul",
            "instructions": "Does `request` contain sensitive, personal, legal or high-stakes subject matter?"
        }
    })
}

/// 影子打标入口：由 api.rs 的 `laya_shadow_middleware` tokio::spawn 调用，不阻塞网关主链路。
/// 任何失败（sidecar 未起 / 超时 / 非 2xx / 落盘失败）静默返回。
pub async fn fire(channel: String, model: Option<String>, user_text: String) {
    let Some(url) = shadow_url() else {
        return;
    };
    let state: String = user_text.chars().take(TEXT_CAP_CHARS).collect();
    let payload = json!({
        "state": { "request": state },
        "questions": router_questions(),
    });
    let started = std::time::Instant::now();
    let value: Value = match http_client().post(&url).json(&payload).send().await {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(_) => return,
        },
        _ => return,
    };
    let record = json!({
        "ts": config::now_ms(),
        "channel": channel,
        "model": model,
        "elapsedMs": started.elapsed().as_millis() as u64,
        "checkpoint": value.pointer("/routing/model"),
        "answers": value.get("answers"),
        "usage": value.get("usage"),
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(jsonl_path())
    {
        let _ = writeln!(f, "{record}");
    }
}
