//! OpenCode 网关（独立网关 · 2026-09-22 三网关拆分之一）。
//!
//! 定位与边界：
//! - 与 **WorkBuddy 网关同级且相互独立**：独立路由 `/opencode/v1`、独立密钥与配置文件；
//! - 但**共用同一个本机 OpenCode server 进程**（端口自动发现，看门狗由 `workbuddy_proxy`
//!   统一管理，避免两个网关各自拉起一个 server 互相抢端口）；
//! - 因此本模块只做三件事：**① 免费模型收敛 ② 独立密钥 ③ 对话委托**，
//!   不重复实现 server 发现/健康检查/消息提交。
//!
//! 「仅免费」判定与前端 `src/lib/model-taxonomy.ts::isFreeModelId` **保持同口径**
//! （id 含 `free` / 以 `:0` 或 `/0` 结尾 / 含 `zero`，忽略大小写），
//! 保证「页面显示的免费清单」与「网关实际放行的模型」不会漂移。

use bytes::Bytes;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::glm_proxy;
use crate::modules::workbuddy_proxy::{self, WorkbuddyUpstreamStream};

const CONFIG_FILE: &str = "opencode_proxy_config.json";

// ---------------------------------------------------------------------------
// 配置（~/.wb-switch/opencode_proxy_config.json）—— 与 WorkBuddy / TRAE 密钥彻底分开
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct OpenCodeProxyConfig {
    pub enabled: bool,
    pub api_key: String,
}

fn config_path() -> PathBuf {
    store_dir().join(CONFIG_FILE)
}

pub fn load_config() -> OpenCodeProxyConfig {
    let mut cfg = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| OpenCodeProxyConfig {
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            api_key: v
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .unwrap_or(OpenCodeProxyConfig {
            enabled: true,
            api_key: String::new(),
        });
    // 首次使用自动生成密钥并落盘（用户无需手填即可接入）
    if cfg.api_key.is_empty() {
        cfg.api_key = glm_proxy::generate_api_key();
        let _ = save_config(&cfg);
    }
    cfg
}

pub fn save_config(cfg: &OpenCodeProxyConfig) -> Result<(), String> {
    let body = json!({
        "enabled": cfg.enabled,
        "api_key": cfg.api_key,
        "updatedAt": now_ms(),
    });
    atomic_write(&config_path(), &body.to_string()).map_err(|e| format!("写入 OpenCode 配置失败: {e}"))
}

// ---------------------------------------------------------------------------
// 免费口径（与前端 model-taxonomy.ts::isFreeModelId 同口径）
// ---------------------------------------------------------------------------

pub fn is_free_model(id: &str) -> bool {
    let l = id.to_ascii_lowercase();
    l.contains("free") || l.ends_with(":0") || l.ends_with("/0") || l.contains("zero")
}

// ---------------------------------------------------------------------------
// 模型目录（仅免费）
// ---------------------------------------------------------------------------

/// OpenAI `/v1/models` 形态，**只含免费模型**。
pub async fn models_payload() -> Result<Value, String> {
    let payload = workbuddy_proxy::models_payload().await?;
    let data: Vec<Value> = payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            m.get("id")
                .and_then(Value::as_str)
                .map(is_free_model)
                .unwrap_or(false)
        })
        .collect();
    Ok(json!({ "object": "list", "data": data }))
}

/// 统计免费 / 总数 / 渠道数（一次 catalog 调用算完，避免重复探测上游）。
async fn counts() -> (usize, usize, usize) {
    let Ok(catalog) = workbuddy_proxy::models_catalog().await else {
        return (0, 0, 0);
    };
    let mut total = 0usize;
    let mut free = 0usize;
    let mut channels = 0usize;
    if let Some(provs) = catalog.get("providers").and_then(Value::as_array) {
        for p in provs {
            channels += 1;
            let pid = p.get("id").and_then(Value::as_str).unwrap_or("");
            if let Some(ms) = p.get("models").and_then(Value::as_array) {
                for m in ms {
                    total += 1;
                    let mid = m.get("id").and_then(Value::as_str).unwrap_or("");
                    if is_free_model(&format!("{pid}/{mid}")) {
                        free += 1;
                    }
                }
            }
        }
    }
    (free, total, channels)
}

// ---------------------------------------------------------------------------
// 对话（委托 workbuddy_proxy 的 server 通道，但只放行免费模型）
// ---------------------------------------------------------------------------

/// 非流式对话。**非免费模型一律拒绝**（本网关的契约就是「只提供免费模型」）。
pub async fn chat_completions_json(mut body: Value) -> Result<Value, String> {
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    if !requested.is_empty() && !is_free_model(&requested) {
        return Err(format!(
            "OpenCode 网关仅提供免费模型，`{requested}` 不在免费清单内。\
             请先 GET /opencode/v1/models 选择可用模型。"
        ));
    }

    if requested.is_empty() {
        // 未指定模型 → 自动选第一个免费模型（降低接入门槛）
        let list = models_payload().await?;
        match list.pointer("/data/0/id").and_then(Value::as_str) {
            Some(first) => {
                body["model"] = json!(first);
            }
            None => {
                return Err(
                    "未指定模型，且未发现任何免费模型（请确认本机 OpenCode server 在运行）".to_string(),
                )
            }
        }
    }

    workbuddy_proxy::chat_completions_json(body).await
}

/// 流式对话：OpenCode 的 message 提交是 fire-and-poll（无逐 token 流），
/// 故与 WorkBuddy 网关同策略——聚合完成后合成 SSE 帧（含末尾 usage 帧）。
pub async fn chat_completions_stream(body: Value) -> Result<WorkbuddyUpstreamStream, String> {
    let completion = chat_completions_json(body).await?;
    let model = completion
        .get("model")
        .cloned()
        .unwrap_or(json!("opencode"));
    let content = completion
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = completion.get("usage").cloned().unwrap_or(Value::Null);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    tokio::spawn(async move {
        let id = format!("chatcmpl-opencode-{}", now_ms());
        let frame = |delta: Value, finish: Option<&str>| {
            json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": now_ms() / 1000,
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
            .to_string()
        };
        let _ = tx
            .send(Ok(Bytes::from(format!(
                "data: {}\n\n",
                frame(json!({"role": "assistant", "content": ""}), None)
            ))))
            .await;
        if !content.is_empty() {
            let _ = tx
                .send(Ok(Bytes::from(format!(
                    "data: {}\n\n",
                    frame(json!({"content": content}), None)
                ))))
                .await;
        }
        let mut usage_json = usage;
        if let Some(obj) = usage_json.as_object_mut() {
            obj.insert("stream".to_string(), json!(true));
        }
        let _ = tx
            .send(Ok(Bytes::from(format!(
                "data: {}\n\n",
                json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": now_ms() / 1000,
                    "model": model,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": usage_json,
                })
                .to_string()
            ))))
            .await;
        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
    });
    Ok(WorkbuddyUpstreamStream { rx })
}

// ---------------------------------------------------------------------------
// 状态与接入信息
// ---------------------------------------------------------------------------

pub async fn proxy_status() -> Value {
    let cfg = load_config();
    let server = match workbuddy_proxy::discover_server().await {
        Ok(port) => json!({"ok": true, "port": port}),
        Err(error) => json!({"ok": false, "error": error}),
    };
    let (free, total, channels) = counts().await;
    json!({
        "enabled": cfg.enabled,
        "server": server,
        "channelCount": channels,
        "modelCount": total,
        "freeModelCount": free,
        // 该网关只暴露免费模型，页面可直接展示这个数
        "exposedModelCount": free,
    })
}

/// 接入信息（只读、不触发服务发现，保证页面秒开）。
pub fn config_info(bound_port: u16) -> Value {
    let cfg = load_config();
    json!({
        "enabled": cfg.enabled,
        "endpoint": format!("http://127.0.0.1:{bound_port}/opencode/v1"),
        "apiKey": cfg.api_key,
        "note": "仅回环；独立密钥，与 WorkBuddy / TRAE 网关不共用",
    })
}
