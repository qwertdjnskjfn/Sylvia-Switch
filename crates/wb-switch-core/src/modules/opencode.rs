//! OpenCode 网关（独立网关 · 2026-09-22 三网关拆分之一）。
//!
//! 定位与边界：
//! - 与 **WorkBuddy 网关同级且相互独立**：独立路由 `/opencode/v1`、独立密钥与配置文件；
//! - 但**共用同一个本机 OpenCode server 进程**（端口自动发现，看门狗由 `workbuddy_proxy`
//!   统一管理，避免两个网关各自拉起一个 server 互相抢端口）；
//! - 因此本模块只做四件事：**① 免费模型收敛 ② 独立密钥 ③ 对话委托 ④ 自动 fallback**，
//!   不重复实现 server 发现/健康检查/消息提交。
//! - ④ 自动 fallback（0.1.85）：上游 5xx / 连接断开时，按「用户指定模型 →
//!   已验证可用链 → 当前免费目录其余」自动换下一个模型重试；全部失败返回带
//!   尝试清单的明确错误。成功响应的 `model` 字段回显**实际使用**的模型。
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

/// 已验证可用的免费模型 fallback 链（顺序即优先级）。
/// 来源：CHANGELOG 0.1.84 逐个实调（2026-09-23，200 且正文非空），
/// 与「给用户的口径」一致：先 `opencode/mimo-v2.6-flash-free`，再
/// `openrouter/nex-agi/nex-n2.5-pro:free`，其余按实测稳定性排后。
/// 可被 `opencode_proxy_config.json` 的 `fallback_chain` 覆盖（仍过滤仅免费）。
const VERIFIED_FALLBACK_CHAIN: &[&str] = &[
    "opencode/mimo-v2.6-flash-free",
    "openrouter/nex-agi/nex-n2.5-pro:free",
    "openrouter/inclusionai/ling-3.0-flash-fin:free",
    "openrouter/cohere/north-mini-code:free",
    "openrouter/inclusionai/ling-3.0-flash-vl:free",
    "openrouter/nvidia/nemotron-3-super-120b-a12b:free",
    "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free",
    "openrouter/liquid/lfm-2.5-2.6b:free",
];

// ---------------------------------------------------------------------------
// 配置（~/.wb-switch/opencode_proxy_config.json）—— 与 WorkBuddy / TRAE 密钥彻底分开
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct OpenCodeProxyConfig {
    pub enabled: bool,
    pub api_key: String,
    /// 上游 502 / 连接断开时，是否按 fallback 链自动换模型重试（默认 true，0.1.85 新增）。
    pub fallback_enabled: bool,
    /// 自定义 fallback 链；空 = 用内置 `VERIFIED_FALLBACK_CHAIN`。仅接受免费模型 id。
    pub fallback_chain: Vec<String>,
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
            // 旧配置文件缺省 → 开 fallback + 内置链（行为与 0.1.85 默认一致）
            fallback_enabled: v
                .get("fallback_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            fallback_chain: v
                .get("fallback_chain")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
        })
        .unwrap_or(OpenCodeProxyConfig {
            enabled: true,
            api_key: String::new(),
            fallback_enabled: true,
            fallback_chain: Vec::new(),
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
        "fallback_enabled": cfg.fallback_enabled,
        "fallback_chain": cfg.fallback_chain,
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
// 自动 fallback（0.1.85）：上游 502 / 连接断开 → 按已验证模型链换下一个重试
// ---------------------------------------------------------------------------

/// fallback 链解析：配置自定义链（过滤仅免费）优先，为空则用内置已验证链。
fn resolve_fallback_chain(cfg: &OpenCodeProxyConfig) -> Vec<String> {
    if cfg.fallback_chain.is_empty() {
        VERIFIED_FALLBACK_CHAIN
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        cfg.fallback_chain
            .iter()
            .filter(|id| is_free_model(id))
            .cloned()
            .collect()
    }
}

/// 当前免费目录 id 清单（fallback 兜底批用）；抓取失败返回空
/// （只影响兜底，不影响主链，请求仍可完成）。
async fn free_catalog_ids() -> Vec<String> {
    models_payload()
        .await
        .ok()
        .and_then(|v| v.get("data").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
        .collect()
}

/// 追加到候选链（去重、保序、跳过空 id）。
fn push_unique(out: &mut Vec<String>, id: &str) {
    if id.is_empty() || out.iter().any(|x| x == id) {
        return;
    }
    out.push(id.to_string());
}

/// 本次请求的模型尝试顺序：① 用户显式指定（免费校验已过）→ ② fallback 链 → ③ 免费目录其余。
fn build_attempt_chain(requested: &str, chain: &[String], free_catalog: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    push_unique(&mut out, requested);
    for id in chain {
        push_unique(&mut out, id);
    }
    for id in free_catalog {
        push_unique(&mut out, id);
    }
    out
}

/// 是否「换一个模型可能有用」的上游失败：提交消息失败 + (5xx | 连接类签名)。
/// - `提交消息失败 5xx …`：opencode server 转述上游/provider 故障（502 为实测最常见形态，
///   500/503 等同理——都是服务端侧问题，换模型可能有用）；
/// - `提交消息失败: error sending request …`：无状态码 = 连接被断 —— 实证多为上游免费池
///   限流 → opencode server FK bug → 断连（CHANGELOG 0.1.84），换模型确实有用。
///   4xx（如 400 unknown model）是请求本身的问题，换模型无意义 → 不重试。
///   其余（messages 为空 / 服务发现失败 / 等待回复超时 / …）同样不重试。
fn is_retryable_upstream_error(msg: &str) -> bool {
    let Some(rest) = msg.split("提交消息失败").nth(1) else {
        return false;
    };
    let token = rest.trim_start().split_whitespace().next().unwrap_or("");
    if token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        // 形如 "502 Bad Gateway"：只认状态码位，5xx 才换（正文里出现 502 字样不误伤）
        return token.starts_with('5');
    }
    // 无状态码 → 连接类失败（FK bug 链路的最终表现）
    workbuddy_proxy::is_connect_error(msg)
}

/// 判断一次 completion 是否**真有正文**。
///
/// 免费池常见「HTTP 200 但 `choices[0].message.content` 为空」——
/// 实测：`unorouter/gpt-5.5:free` 84.8s 返回空、部分模型 6.8s 即返回空且
/// `usage.prompt_tokens == 0`（说明上游根本没处理请求）。
/// 对调用方而言这等于没回答，故应视作可降级错误、继续换模型。
/// 只认 `message.content`（不含 reasoning_content）：正文为空时推理块对用户无意义。
fn completion_has_text(value: &Value) -> bool {
    value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// 0.1.84 的可操作建议原样保留：提交失败多为上游免费额度/限流，别让用户误以为网络断了。
fn decorate_upstream_error(e: String) -> String {
    if e.contains("提交消息失败") {
        format!(
            "{e}\\n\\n【多为上游免费额度/限流】这些模型走各 provider 的共享免费池，会随时段波动。\\n\
             建议：换个免费模型（先 GET /opencode/v1/models 取当前可用清单），或稍后重试。"
        )
    } else {
        e
    }
}

/// 全部候选失败时的汇总错误：带**尝试清单**（按序）+ 最后一次错误原文 + 建议。
/// 单测覆盖（线上无法构造「所有免费模型同时失败」的灾难场景）。
fn all_failed_error(tried: &[String], last_err: String) -> String {
    format!(
        "OpenCode 网关 fallback 全部失败（已按序尝试 {} 个模型：{}）。\\n\\
         最后一次错误：{}\\n\\n\
         【多为上游免费额度/限流】这些模型走各 provider 的共享免费池，会随时段波动。\\n\
         建议：稍后重试，或 GET /opencode/v1/models 取当前可用清单后手动指定。",
        tried.len(),
        tried.join(" → "),
        last_err
    )
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
///
/// 0.1.85 起带**自动 fallback**：上游 5xx / 连接断开时，按「用户指定模型 →
/// 已验证可用链 → 当前免费目录其余」的顺序自动换下一个模型重试；全部失败时
/// 返回带尝试清单的明确错误。成功响应的 `model` 字段天然回显**实际使用**的模型
/// （workbuddy_proxy 按 body 里的 model 回显），调用侧无需猜。
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

    let cfg = load_config();
    let chain = if cfg.fallback_enabled {
        resolve_fallback_chain(&cfg)
    } else {
        Vec::new()
    };
    // 第一批候选：用户指定 + fallback 链（纯本地，零网络开销）。
    // 未指定模型时由链首顶替旧的「取目录第一个免费模型」——server 不可达也能选模型。
    let mut candidates = build_attempt_chain(&requested, &chain, &[]);
    if candidates.is_empty() {
        return Err(
            "未指定模型，且未发现任何免费模型（请确认本机 OpenCode server 在运行，\
             或在 opencode_proxy_config.json 配置 fallback_chain）"
                .to_string(),
        );
    }

    let mut tried: Vec<String> = Vec::new();
    let mut last_err = String::new();
    let mut catalog_loaded = false;
    // 2026-10-02：免费池常出现「HTTP 200 但正文为空」（实测 gpt-5.5 84.8s 返回空、
    // 部分模型 6.8s 即返回空且 prompt_tokens=0）。对用户等于没回答，应触发 fallback；
    // 但不能无上限重试（14 个候选 × 最坏 60s 会拖到十几分钟），故给一个额度。
    // 额度耗尽即返回最后一次结果（哪怕是空的），保证有界。
    const EMPTY_RETRY_BUDGET: usize = 3;
    let mut empty_budget = EMPTY_RETRY_BUDGET;
    let mut last_value: Option<Value> = None;
    let mut idx = 0usize;
    loop {
        if idx >= candidates.len() {
            // 当前批次走完且最后一个错误可降级 → 拉一次当前免费目录兜底（其余免费模型）。
            // 只拉一次；拉不到或没有新模型就收尾（防止无 catalog 时反复探测）。
            if cfg.fallback_enabled && !catalog_loaded && is_retryable_upstream_error(&last_err) {
                catalog_loaded = true;
                for id in free_catalog_ids().await {
                    if !tried.contains(&id) {
                        candidates.push(id);
                    }
                }
                if idx >= candidates.len() {
                    break;
                }
                continue;
            }
            break;
        }
        let model = candidates[idx].clone();
        idx += 1;
        body["model"] = json!(model.as_str());
        tried.push(model);
        match workbuddy_proxy::chat_completions_json(body.clone()).await {
            Ok(value) => {
                // 「200 但正文为空」= 免费池常见的伪成功 → 当可降级错误处理，继续换模型。
                // 记住最后一次结果：额度耗尽或链走完时把它返回，避免把空回答变成硬错误。
                if completion_has_text(&value) {
                    // 成功：响应 model 字段 = 本次实际使用的模型（回显由 workbuddy_proxy 按 body 完成）
                    return Ok(value);
                }
                if cfg.fallback_enabled && empty_budget > 0 && idx < candidates.len() {
                    empty_budget -= 1;
                    last_err = "上游返回 200 但正文为空".to_string();
                    last_value = Some(value);
                    continue;
                }
                return Ok(value);
            }
            Err(e) => {
                last_err = e;
                if !is_retryable_upstream_error(&last_err) {
                    // 不可降级错误（输入错误 / 服务发现失败等）原样直出，不烧 fallback 链
                    return Err(decorate_upstream_error(last_err));
                }
                // 可降级：继续下一个候选
            }
        }
    }

    // 试过多个候选但都只是「空正文」→ 把最后一次的空结果返回给调用方，
    // 比抛错误更贴近旧行为（调用方能拿到 model 字段做展示）。
    if let Some(v) = last_value {
        return Ok(v);
    }
    if tried.len() <= 1 {
        // 只试了一个模型（fallback 关闭等场景）→ 保持旧行为：原始错误直出
        return Err(decorate_upstream_error(last_err));
    }
    Err(all_failed_error(&tried, last_err))
}

/// 流式对话：OpenCode 的 message 提交是 fire-and-poll（无逐 token 流），
/// 故与 WorkBuddy 网关同策略——聚合完成后合成 SSE 帧（含末尾 usage 帧）。
///
/// 0.1.85：返回 `(流, 实际模型名)` —— 内部可能已 fallback 到别的模型，
/// SSE 帧与网关日志都以**实际使用**的模型为准。
pub async fn chat_completions_stream(
    body: Value,
) -> Result<(WorkbuddyUpstreamStream, String), String> {
    let completion = chat_completions_json(body).await?;
    let model = completion
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("opencode")
        .to_string();
    let content = completion
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = completion.get("usage").cloned().unwrap_or(Value::Null);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    // model 会被 spawn 任务（SSE 帧）移走，返回给调用方（网关日志）用克隆件
    let model_out = model.clone();
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
    Ok((WorkbuddyUpstreamStream { rx }, model_out))
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
        // 0.1.85：fallback 诊断（只读）。关闭时为 []，开启时 = 生效链（内置或自定义）。
        "fallbackEnabled": cfg.fallback_enabled,
        "fallbackChain": if cfg.fallback_enabled { resolve_fallback_chain(&cfg) } else { Vec::new() },
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

// ---------------------------------------------------------------------------
// 单测（fallback 链构造与触发判定 —— 纯函数，无网络）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn attempt_chain_orders_requested_then_fallback_then_catalog_no_dupes() {
        let got = build_attempt_chain(
            "opencode/mimo-v2.6-flash-free", // 与链首重复 → 只出现一次
            &chain(&[
                "opencode/mimo-v2.6-flash-free",
                "openrouter/nex-agi/nex-n2.5-pro:free",
            ]),
            &chain(&[
                "openrouter/liquid/lfm-2.5-2.6b:free",
                "unorouter/gpt-5.5:free",
                "openrouter/nex-agi/nex-n2.5-pro:free", // 目录里重复 → 去重
            ]),
        );
        assert_eq!(
            got,
            vec![
                "opencode/mimo-v2.6-flash-free".to_string(),
                "openrouter/nex-agi/nex-n2.5-pro:free".to_string(),
                "openrouter/liquid/lfm-2.5-2.6b:free".to_string(),
                "unorouter/gpt-5.5:free".to_string(),
            ]
        );
    }

    #[test]
    fn attempt_chain_empty_requested_starts_with_chain_head() {
        let got = build_attempt_chain("", &chain(&["opencode/mimo-v2.6-flash-free"]), &[]);
        assert_eq!(got, vec!["opencode/mimo-v2.6-flash-free".to_string()]);
    }

    #[test]
    fn attempt_chain_all_empty_is_empty() {
        assert!(build_attempt_chain("", &[], &[]).is_empty());
    }

    #[test]
    fn retryable_error_matches_5xx_and_connect_drop_only() {
        // 触发：5xx（502 为实测最常见，500/503 同理）+ 连接被断（FK bug 链路的最终表现）
        assert!(is_retryable_upstream_error(
            "提交消息失败 502 Bad Gateway: AI_APICallError: Your balance is empty"
        ));
        assert!(is_retryable_upstream_error(
            "提交消息失败 500 Internal Server Error: Unexpected server error"
        ));
        assert!(is_retryable_upstream_error(
            "提交消息失败: error sending request for url (http://127.0.0.1:4097/session/s1/message)"
        ));
        // 不触发：4xx（请求本身有问题）/ 输入错误 / 服务发现失败 / 等待超时 / 正文含 502 字样
        assert!(!is_retryable_upstream_error(
            "提交消息失败 400 Bad Request: unknown model"
        ));
        assert!(!is_retryable_upstream_error(
            "提交消息失败 400: model 502 not found in catalog"
        ));
        assert!(!is_retryable_upstream_error("messages 不能为空"));
        assert!(!is_retryable_upstream_error(
            "未发现 OpenCode server 进程（未在监听端口）"
        ));
        assert!(!is_retryable_upstream_error("等待回复超时（模型可能排队中或不可用）"));
    }

    #[test]
    #[test]
    fn completion_has_text_detects_empty_body() {
        // 有正文 → true
        assert!(completion_has_text(&serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "收到"}}]
        })));
        // 空串 / 纯空白 → false（免费池最常见的伪成功）
        assert!(!completion_has_text(&serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": ""}}]
        })));
        assert!(!completion_has_text(&serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "   \n "}}]
        })));
        // 只有 reasoning_content、无 content → false（正文为空对用户无意义）
        assert!(!completion_has_text(&serde_json::json!({
            "choices": [{"message": {"reasoning_content": "思考中…"}}]
        })));
        // 结构缺失 → false，不 panic
        assert!(!completion_has_text(&serde_json::json!({"choices": []})));
        assert!(!completion_has_text(&serde_json::json!({})));
        assert!(!completion_has_text(&serde_json::json!(null)));
    }

    #[test]
    fn resolve_chain_prefers_custom_free_only_and_falls_back_to_builtin() {
        // 自定义链仅留免费 id（付费条目被过滤，与网关「仅免费」契约一致）
        let cfg = OpenCodeProxyConfig {
            enabled: true,
            api_key: String::new(),
            fallback_enabled: true,
            fallback_chain: chain(&["openrouter/zzz:free", "opencode/paid-pro-max"]),
        };
        assert_eq!(resolve_fallback_chain(&cfg), chain(&["openrouter/zzz:free"]));
        // 空自定义链 → 内置已验证链（mimo 为首）
        let cfg2 = OpenCodeProxyConfig {
            fallback_chain: Vec::new(),
            ..cfg.clone()
        };
        let builtin = resolve_fallback_chain(&cfg2);
        assert_eq!(builtin.first().unwrap(), "opencode/mimo-v2.6-flash-free");
        assert_eq!(builtin.len(), VERIFIED_FALLBACK_CHAIN.len());
    }

    #[test]
    fn decorate_only_appends_hint_to_submit_failures() {
        assert!(!decorate_upstream_error("messages 不能为空".into()).contains("多为上游免费额度"));
        assert!(decorate_upstream_error("提交消息失败 502: x".into()).contains("多为上游免费额度"));
    }

    #[test]
    fn all_failed_error_lists_chain_and_last_error() {
        let msg = all_failed_error(
            &chain(&["opencode/aaa-free", "opencode/bbb-free"]),
            decorate_upstream_error("提交消息失败 502: x".into()),
        );
        assert!(msg.contains("fallback 全部失败"));
        assert!(msg.contains("已按序尝试 2 个模型"));
        assert!(msg.contains("opencode/aaa-free → opencode/bbb-free"));
        assert!(msg.contains("最后一次错误：提交消息失败 502"));
        assert!(msg.contains("多为上游免费额度/限流"));
    }
}
