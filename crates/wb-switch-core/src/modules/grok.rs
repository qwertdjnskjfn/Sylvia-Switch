//! Grok 融合网关（移植并汲取 chenyme/grok2api 开源项目核心协议设计）。
//!
//! 把多个 Grok 账号（Grok Web SSO Cookie、Grok Build OAuth/API Key、Grok Console 凭证）
//! 汇聚为统一的 OpenAI 兼容接口（`/grok/v1/chat/completions` + `/grok/v1/models`），
//! 支持多账号轮询调度、自动故障转移与 429 冷却降级。
//!
//! 核心支持能力：
//! - Grok Web SSO 网页端会话反代，自动伪造现代 Chrome 浏览器指纹与同源请求头；
//! - Grok Build 官方开发者接口高速直连；
//! - 完整支持流式 SSE（`stream: true`）与非流式响应；
//! - 智能识别并实时转译深度思考过程（Reasoning / Thinking）与联网搜索（Web Search）；
//! - 账号池落盘至 `~/.wb-switch/grok_accounts.json`，支持去重、启用/禁用、在线探活与一键测试。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::modules::config::{atomic_write, now_ms, store_dir};

const GROK_WEB_BASE: &str = "https://grok.com";
const GROK_BUILD_BASE: &str = "https://api.x.ai";

/// 账号写入全局互斥锁
static WRITE_LOCK: Mutex<()> = Mutex::new(());
/// 轮询调度游标
static ROUND_ROBIN_INDEX: AtomicUsize = AtomicUsize::new(0);

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .build()
            .expect("failed to build grok client")
    })
}

// ---------------------------------------------------------------------------
// grok2api 桥接（Build 通道新旗舰：凭据与 OAuth 续期由 grok2api 管理，本网关纯转发）
// ---------------------------------------------------------------------------

/// grok2api 桥接配置文件（~/.wb-switch/grok_g2a_proxy.json）。
fn g2a_proxy_config_path() -> PathBuf {
    store_dir().join("grok_g2a_proxy.json")
}

/// grok2api 桥接配置。
#[derive(Debug, Clone)]
pub struct G2aProxyConfig {
    /// grok2api 基址（如 http://127.0.0.1:8002，无尾斜杠）
    pub base_url: String,
    /// 客户端密钥（g2a_…）
    pub api_key: String,
    /// 由 grok2api 桥接承接的模型清单
    pub models: Vec<String>,
    /// Anthropic `/v1/messages` 缺省思考预算（tokens）。**0 = 不注入**。
    ///
    /// 为什么需要它：Anthropic 规范把 thinking 定义为**显式 opt-in** —— 请求不带
    /// `thinking:{type:"enabled",budget_tokens:N}` 时上游**合法地**不产出思考块，
    /// 而 OpenAI 路径默认就带 `reasoning_content`。实测（2026-09-28）同一问题：
    /// OpenAI 80 字思考 vs Anthropic 0 字，带上该参数后 Anthropic 立得 73/246 字。
    /// 故此处为 Anthropic 协议客户端补一个默认值，客户端零改动；客户端显式传入
    /// 的 `thinking` 永远优先（不覆盖）。
    pub default_thinking_budget_tokens: u64,
    /// 工具调用可见思考链（1.1.0 一期）：tools 非空时向 system 追加「强制工具
    /// 规划协议」+「已完成调用链」摘要。**false = 关闭**（请求体原样转发）。
    ///
    /// 为什么需要它：grok-4.7 的内部思考是服务端加密的（客户端永远看不到），
    /// ZCode 侧只能看到 tool_use 而看不到"为什么调它"。该开关把规划外置为
    /// 可见文本（每步调用前输出【规划】、最终答复前输出【收尾】），并让模型
    /// 每轮都"看得见"完整调用链。`x-grok-tool-protocol: off` 头可按请求逃生。
    pub tool_protocol: bool,
    /// 1.2.0 二期：工具交换最终回答的**收尾复盘尾块**（响应侧）——历史含工具
    /// 调用且响应不再发起 tool_use 时，在流式 message_stop 前 / 非流式 content
    /// 数组末尾追加【工具链路复盘】。**false = 关闭**。
    pub wrap_up: bool,
    /// 1.2.0 二期：复盘内容升级——回答过薄或存在断链时，追加一次预算封顶
    /// （max_tokens=1000、无思考注入、30s 超时）的模型复盘调用，失败静默
    /// 回退到确定性复盘。**false = 只用确定性复盘**。
    pub wrap_up_model_review: bool,
    /// 1.2.1：调用链摘要注入 system 的独立开关（默认开）。它只进请求输入侧、
    /// 不进会话历史；想彻底省掉这部分输入 token 时置 false。
    pub chain_summary: bool,
}

/// 缺省思考预算（tokens）。Anthropic 要求 `budget_tokens >= 1024` 且 `< max_tokens`，
/// 故仅当 max_tokens 放得下时才注入（见 `default_thinking_budget`）。
pub const DEFAULT_THINKING_BUDGET_TOKENS: u64 = 4000;

/// 从 JSON 解析桥接配置（纯函数，便于单测）：enabled + 非空 base_url/api_key/models
/// 缺一不可，否则返回 None（桥接关闭，完全走原账号池路径）。
fn g2a_proxy_config_from_value(v: &Value) -> Option<G2aProxyConfig> {
    let enabled = v.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    if !enabled {
        return None;
    }
    let base_url = v
        .get("base_url")
        .and_then(Value::as_str)
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .unwrap_or_default();
    let api_key = v
        .get("api_key")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let models = v
        .get("models")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if base_url.is_empty() || api_key.is_empty() || models.is_empty() {
        return None;
    }
    // 缺省思考预算：未配置时用默认值（4000）；显式 0 表示关闭注入（逃生用）。
    let default_thinking_budget_tokens = v
        .get("default_thinking_budget_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_THINKING_BUDGET_TOKENS);
    // 工具协议：未配置时默认开启；显式 false 关闭（逃生用）。
    let tool_protocol = v.get("tool_protocol").and_then(Value::as_bool).unwrap_or(true);
    // 二期收尾复盘：均默认开启；显式 false 关闭。
    let wrap_up = v.get("wrapUp").and_then(Value::as_bool).unwrap_or(true);
    let wrap_up_model_review = v
        .get("wrapUpModelReview")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let chain_summary = v.get("chainSummary").and_then(Value::as_bool).unwrap_or(true);
    Some(G2aProxyConfig {
        base_url,
        api_key,
        models,
        default_thinking_budget_tokens,
        tool_protocol,
        wrap_up,
        wrap_up_model_review,
        chain_summary,
    })
}

/// 读桥接配置（供 api 层 Anthropic Messages 桥接复用）。
pub fn g2a_proxy_config() -> Option<G2aProxyConfig> {
    let text = std::fs::read_to_string(g2a_proxy_config_path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    g2a_proxy_config_from_value(&v)
}

/// Anthropic Messages 桥接（/grok/v1/messages → grok2api /v1/messages）：
/// cli-chat-proxy 原生支持 Messages 协议与工具调用（tool_use），且 Build 池
/// 自动续期。原样转发请求体（ZCode 等 Anthropic 协议客户端的 tools/system
/// 均由上游原生处理），返回上游 Response 供 api 层按 Content-Type 透传。
/// `anthropic_version`：上游要求该头（客户端未带时补官方默认 2023-06-01）。
/// 请求体里的 model 会做归一化（Grok4.7 → grok-4.7）——客户端钉住的写法
/// 不一，不归一化会被上游判「模型不存在」404。
pub async fn g2a_messages_forward(
    cfg: &G2aProxyConfig,
    body: &[u8],
    anthropic_version: Option<&str>,
    thinking_disabled: bool,
    tool_protocol_disabled: bool,
) -> Result<reqwest::Response, String> {
    let url = format!("{}/v1/messages", cfg.base_url);
    let forwarded = prepare_messages_body(cfg, body, thinking_disabled);
    let forwarded = augment_messages_tools(cfg, &forwarded, tool_protocol_disabled);
    http_client()
        .post(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .header("Content-Type", "application/json")
        .header(
            "anthropic-version",
            anthropic_version.unwrap_or("2023-06-01"),
        )
        .body(forwarded)
        .send()
        .await
        .map_err(|e| format!("grok2api Messages 桥接连接失败: {e}"))
}

/// Messages 请求体的 model 归一化：JSON 解析成功且带 model 字段时，用
/// `canonical_g2a_model` 映射为上游规范 ID 后重序列化；非 JSON/无 model 原样返回。
/// 请求头 `x-grok-thinking` 是否要求**关闭**缺省思考注入（逃生开关）。
/// 接受 off / disable / disabled / none / 0（大小写与空白不敏感）。
pub fn thinking_disabled_from_header(value: Option<&str>) -> bool {
    matches!(
        value.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("off") | Some("disable") | Some("disabled") | Some("none") | Some("0")
    )
}

/// 缺省思考预算：`None` = 不注入。
/// 约束来自 Anthropic 规范：`budget_tokens >= 1024` 且**严格小于** `max_tokens`
/// （否则上游 400），故按 max_tokens 自适应收缩，并给正文留 512 tokens 余量。
fn default_thinking_budget(max_tokens: u64, configured: u64) -> Option<u64> {
    if configured == 0 || max_tokens <= 1024 {
        return None;
    }
    let with_room_for_text = max_tokens.saturating_sub(512).max(1024);
    let budget = configured.min(with_room_for_text).max(1024);
    (budget < max_tokens).then_some(budget)
}

/// Messages 请求体预处理：① model 归一化（既有行为，不能丢）；
/// ② 缺省注入 `thinking`（1.0.5）。非 JSON / 无 model 时原样返回，不做猜测性改造。
///
/// thinking 仅在三个条件同时成立时注入：客户端**未显式传入**（显式永远优先）、
/// 未触发逃生开关、且预算算得出（见 `default_thinking_budget`）。
fn prepare_messages_body(cfg: &G2aProxyConfig, body: &[u8], thinking_disabled: bool) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(model) = v.get("model").and_then(Value::as_str).map(String::from) else {
        return body.to_vec();
    };
    v["model"] = json!(canonical_g2a_model(cfg, &model));
    normalize_effort_fields(&mut v);
    if !thinking_disabled && v.get("thinking").is_none() {
        let max_tokens = v.get("max_tokens").and_then(Value::as_u64).unwrap_or(0);
        if let Some(budget) =
            default_thinking_budget(max_tokens, cfg.default_thinking_budget_tokens)
        {
            v["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
}

/// effort 档位归一化为上游接受的英文名。
///
/// 上游（grok2api → Grok Build）只接受 `minimal | low | medium | high | xhigh | max`
/// 六档，其它取值一律 `400 不支持 output_config.effort=%q`
/// （见 `conversation/messages_request.go` 的 switch）。而客户端（实测 ZCode）
/// 会把 UI 上的**中文档位名**（如「最高」）原样塞进字段 ⇒ 请求直接 400。
fn canonical_effort(raw: &str) -> Option<&'static str> {
    let key: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    match key.as_str() {
        "max" | "maximum" | "ultra" | "extreme" | "最高" | "极大" | "顶格" => Some("max"),
        "xhigh" | "extrahigh" | "extra-high" | "超高" | "特高" => Some("xhigh"),
        "high" | "高" | "较高" => Some("high"),
        "medium" | "mid" | "中" | "中等" | "中档" => Some("medium"),
        "low" | "低" | "较低" => Some("low"),
        "minimal" | "min" | "minimum" | "最低" | "极小" => Some("minimal"),
        _ => None,
    }
}

/// 归一化请求体里的 effort：`output_config.effort` 与 `thinking.effort`。
///
/// 上游两处最终汇入**同一个** switch 校验（grok2api 先取 thinking.effort，
/// 再被 output_config.effort 覆盖，最后统一校验）⇒ 任一非法都会 400，故必须
/// 同时处理。无法识别的取值**剥离该字段**（退回由 budget_tokens 推断），
/// 而不是让整通请求失败；`output_config` 因剥离而变空时整块摘掉。
fn normalize_effort_fields(v: &mut Value) {
    for path in [["output_config", "effort"], ["thinking", "effort"]] {
        let Some(raw) = v
            .get(path[0])
            .and_then(|o| o.get(path[1]))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        match canonical_effort(&raw) {
            Some(eff) => v[path[0]][path[1]] = json!(eff),
            None => {
                if let Some(obj) = v[path[0]].as_object_mut() {
                    obj.remove(path[1]);
                }
            }
        }
    }
    let empty_oc = v
        .get("output_config")
        .and_then(Value::as_object)
        .is_some_and(|o| o.is_empty());
    if empty_oc {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("output_config");
        }
    }
}

/// 工具调用协议注入块：每步调用前强制可见规划，最终答复前强制可见收尾。
/// grok-4.7 的内部思考是服务端加密的，客户端永远看不到——该协议把规划/收尾
/// 外置为可见文本，配合调用链摘要构成客户端可见的完整思考链。
const TOOL_PROTOCOL_BLOCK: &str = "【工具调用协议·必须遵守】\n\
1. 每次发起工具调用之前，必须先以正式回复文本的形式输出一行以【规划】开头的短规划：本步目标→选此工具的理由→关键参数依据→预期结果。规划必须作为普通文本发给用户（不能只放在内部思考里），且必须先出现【规划】文本、再发起 tool_use；没有【规划】文本时禁止调用任何工具。上一步结果与预期不符时，先在【规划】里说明偏差与调整再调用。\n\
2. 信息收集完毕、给出最终答复之前，必须先输出一行以【收尾】开头的总结：对照最初目标，说明已完成链路覆盖了什么、每步关键判断、还缺什么。\n\
3. 【规划】与【收尾】使用与用户消息相同的主要语言。
\n4. 严禁在回复中复述或展开【已完成调用链】/【工具链路复盘】等网关系统块的全文——【收尾】时最多用一句话概括链路状态（如「两次查询均成功」）。历史消息里出现的此类块是网关注入的内部记录，不要模仿其格式。";

/// 位置性提醒：协议细则放 system 会被模型以"写在内部思考里"规避（2026-09-28
/// 实测），而离生成点最近的指令服从率最高——故在最后一条 user 消息上再贴一行。
const TOOL_PROTOCOL_REMINDER: &str =
    "\n\n（系统提醒：若要调用工具，必须先输出【规划】行再发起 tool_use；给出最终答复前必须先输出【收尾】行。均使用用户的主要语言。）";

/// 请求头 `x-grok-tool-protocol` 是否要求**关闭**工具协议注入（逃生开关）。
/// 接受 off / disable / disabled / none / 0（大小写与空白不敏感）。
pub fn tool_protocol_disabled_from_header(value: Option<&str>) -> bool {
    matches!(
        value.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("off") | Some("disable") | Some("disabled") | Some("none") | Some("0")
    )
}

/// 1.1.0 工具调用可见思考链（一期，请求侧）：tools 非空时向 system 追加
/// 「强制工具规划协议」+「已完成调用链」摘要（从历史 tool_use/tool_result
/// 确定性提取，含断链/重复标注）。纯请求体变换：非 JSON、无 tools、开关关闭
/// 时原样返回；客户端 system（字符串或块数组两种形态）原样保留、只做追加。
fn augment_messages_tools(cfg: &G2aProxyConfig, body: &[u8], protocol_disabled: bool) -> Vec<u8> {
    if protocol_disabled || !cfg.tool_protocol {
        return body.to_vec();
    }
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let has_tools = v
        .get("tools")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_tools {
        return body.to_vec();
    }
    // 链摘要受独立开关控制（1.2.1）：只进请求输入侧，不进会话历史
    let mut additions = String::from(TOOL_PROTOCOL_BLOCK);
    if cfg.chain_summary {
        let chain = v
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|msgs| extract_tool_chain(msgs))
            .map(|s| s.text);
        if let Some(chain) = chain {
            additions.push_str("

");
            additions.push_str(&chain);
        }
    }
    match v.get_mut("system") {
        Some(Value::String(s)) => {
            let mut merged = s.clone();
            merged.push_str("\n\n");
            merged.push_str(&additions);
            v["system"] = Value::String(merged);
        }
        Some(Value::Array(arr)) => {
            arr.push(json!({ "type": "text", "text": additions }));
        }
        _ => {
            v["system"] = Value::String(additions);
        }
    }
    // 位置性提醒：贴在最后一条 user 消息上（内容为字符串则拼接，为块数组则追加文本块）。
    if let Some(last) = v
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|a| a.last_mut())
    {
        if last.get("role").and_then(Value::as_str) == Some("user") {
            match last.get_mut("content") {
                Some(Value::String(s)) => s.push_str(TOOL_PROTOCOL_REMINDER),
                Some(Value::Array(arr)) => arr.push(json!({
                    "type": "text",
                    "text": TOOL_PROTOCOL_REMINDER.trim_start_matches('\n')
                })),
                _ => {}
            }
        }
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
}

/// 从 messages 历史确定性提取工具调用链：assistant.tool_use 与 user.tool_result
/// 按 id 配对，产出带状态与 ⚠ 标注的摘要；历史无 tool_use 时返回 None。
fn extract_tool_chain(messages: &[Value]) -> Option<ToolChainSummary> {
    struct Call {
        id: String,
        name: String,
        input: String,
        /// None=尚未收到结果；Some((摘要, 是否错误))
        result: Option<(String, bool)>,
    }
    let mut calls: Vec<Call> = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
            if role == "assistant" && btype == "tool_use" {
                calls.push(Call {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                    input: summarize_compact(block.get("input").unwrap_or(&Value::Null), 40),
                    result: None,
                });
            } else if role == "user" && btype == "tool_result" {
                let tid = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let is_error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let summary = summarize_compact(block.get("content").unwrap_or(&Value::Null), 30);
                if let Some(call) = calls.iter_mut().rev().find(|c| c.id == tid && c.result.is_none())
                {
                    call.result = Some((summary, is_error));
                }
            }
        }
    }
    if calls.is_empty() {
        return None;
    }
    let mut lines = vec!["【已完成调用链】(网关自动生成，规划下一步前先对照)".to_string()];
    // 只显示最近 6 步：链摘要每轮都会进 system，步数上限必须克制（上下文成本）
    let skip = calls.len().saturating_sub(6);
    if skip > 0 {
        lines.push(format!("(前 {skip} 步省略)"));
    }
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (idx, call) in calls.iter().enumerate().skip(skip) {
        let status = match &call.result {
            Some((s, false)) => format!("→ ok({s})"),
            Some((s, true)) => format!("→ 错误({s})"),
            None => "→ ⚠无结果".to_string(),
        };
        let dup_flag = if !seen.insert(format!("{}|{}", call.name, call.input)) {
            " ⚠重复调用"
        } else {
            ""
        };
        lines.push(format!("{}. {}({}){}{}", idx + 1, call.name, call.input, status, dup_flag));
    }
    let dangling = calls.iter().filter(|c| c.result.is_none()).count();
    if dangling > 0 {
        lines.push(format!("⚠ {dangling} 个调用未收到结果：规划时先补齐再继续，勿重复发起。"));
    }
    Some(ToolChainSummary {
        text: lines.join("\n"),
        calls: calls.len(),
        dangling,
    })
}

/// 调用链摘要与统计（一期注入 + 二期复盘共用）。
#[derive(Clone)]
pub struct ToolChainSummary {
    /// 注入 system / 复盘块用的多行文本
    pub text: String,
    /// 工具调用总步数
    pub calls: usize,
    /// 未收到结果的调用数（断链）
    pub dangling: usize,
}

/// 压缩为单行摘要并按字符截断（避免多字节截半）。
fn summarize_compact(value: &Value, max_chars: usize) -> String {
    let raw = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut flat: String = raw
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let count = flat.chars().count();
    if count > max_chars {
        flat = flat.chars().take(max_chars).collect::<String>() + "…";
    }
    flat
}

/// 1.2.0 二期：收尾复盘上下文（请求侧一次性提取）。
#[derive(Clone)]
pub struct WrapUpContext {
    pub chain: ToolChainSummary,
    /// 首条 user 消息文本（前 120 字）——复盘"对照最初目标"的基准
    pub goal: String,
}

/// 从请求体提取收尾复盘上下文：tools 非空且历史含工具调用时返回 Some，
/// 否则 None（首轮请求 / 纯对话请求都不需要收尾复盘）。
pub fn wrap_up_context(cfg: &G2aProxyConfig, body: &[u8]) -> Option<WrapUpContext> {
    if !cfg.wrap_up {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let has_tools = v
        .get("tools")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_tools {
        return None;
    }
    let messages = v.get("messages")?.as_array()?;
    let chain = extract_tool_chain(messages)?;
    let goal = messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|m| first_text(m.get("content")))
        .map(|s| summarize_compact(&Value::String(s), 120))
        .unwrap_or_default();
    Some(WrapUpContext { chain, goal })
}

/// 取 content（字符串或块数组）里的首段文本。
fn first_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => arr.iter().find_map(|b| {
            if b.get("type").and_then(Value::as_str) == Some("text") {
                b.get("text").and_then(Value::as_str).map(String::from)
            } else {
                None
            }
        }),
        _ => None,
    }
}

/// 3A 确定性复盘尾块：链路回放 + 悬空检查，零模型调用。
pub fn wrap_up_tail(ctx: &WrapUpContext) -> String {
    let check = if ctx.chain.dangling > 0 {
        format!("共 {} 次工具调用，其中 {} 个未收到结果", ctx.chain.calls, ctx.chain.dangling)
    } else {
        format!("共 {} 次工具调用，全部有结果", ctx.chain.calls)
    };
    format!(
        "【工具链路复盘·网关生成】\n目标：{}\n{}\n检查：{}。\n（本块由网关在工具链路收尾时自动生成，用于补全可见思考链。）",
        ctx.goal, ctx.chain.text, check
    )
}

/// 3B 触发条件：存在断链调用，或最终回答过薄（<200 字）。
pub fn wrap_up_review_needed(ctx: &WrapUpContext, draft_text: &str) -> bool {
    ctx.chain.dangling > 0 || draft_text.trim().chars().count() < 200
}

/// 3B 条件性模型复盘：预算封顶（max_tokens=1000、无思考注入、30s 超时），
/// 失败返回 None（调用方回退到 3A 确定性复盘）。
pub async fn g2a_wrap_up_review(
    cfg: &G2aProxyConfig,
    ctx: &WrapUpContext,
    draft_text: &str,
) -> Option<String> {
    let model = cfg.models.first()?.clone();
    let prompt = format!(
        "以下是本次任务的目标、工具调用链与助手最终回答。请以第三方视角输出不超过 200 字的复盘：目标→实际链路→每步关键判断→遗漏与下一步建议。直接输出正文，不要客套。\n目标：{}\n调用链：\n{}\n最终回答：{}",
        ctx.goal,
        ctx.chain.text,
        summarize_compact(&Value::String(draft_text.to_string()), 500)
    );
    let body = json!({
        "model": model,
        "max_tokens": 1000,
        "messages": [{ "role": "user", "content": prompt }]
    });
    let resp = http_client()
        .post(format!("{}/v1/messages", cfg.base_url))
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .timeout(std::time::Duration::from_secs(30))
        .json(&body)
        .send()
        .await
        .ok()?;
    let v: Value = resp.json().await.ok()?;
    let text: String = v
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 解析 anthropic JSON 响应：返回 (正文文本, 是否发起 tool_use)。非 JSON 返回 None。
pub fn parse_response_content(body_text: &str) -> Option<(String, bool)> {
    let v: Value = serde_json::from_str(body_text).ok()?;
    let content = v.get("content")?.as_array()?;
    let mut text = String::new();
    let mut has_tool_use = false;
    for b in content {
        match b.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            "tool_use" => has_tool_use = true,
            _ => {}
        }
    }
    Some((text, has_tool_use))
}

/// 非流式收尾：向 content 数组末尾追加复盘文本块；非 JSON 原样返回。
pub fn wrap_up_append_to_json(body_text: &str, tail: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(body_text) else {
        return body_text.to_string();
    };
    let Some(content) = v.get_mut("content").and_then(Value::as_array_mut) else {
        return body_text.to_string();
    };
    content.push(json!({ "type": "text", "text": tail }));
    serde_json::to_string(&v).unwrap_or_else(|_| body_text.to_string())
}

/// 1.2.0 二期：SSE 流式复盘尾块注入器（字节保真版）。
/// 上游原始字节原样透传（UTF-8 多字节字符跨 chunk 也不会被截坏——解码分析只
/// 发生在完整事件的副本上）；收到 `message_stop` 事件时扣住原始字节，由调用方
/// 经 `complete_with_tail` 注入尾块事件（index = 已见最大 index + 1）后再放行。
/// 响应里出现过 tool_use 时调用方应传 tail=None（调用链未结束）。
pub struct WrapUpStreamInjector {
    buf: Vec<u8>,
    tail_enabled: bool,
    seen_tool_use: bool,
    max_index: i64,
    draft: String,
    holding_stop: bool,
    injected: bool,
    held_event: Vec<u8>,
}

impl WrapUpStreamInjector {
    pub fn new(tail_enabled: bool) -> Self {
        Self {
            buf: Vec::new(),
            tail_enabled,
            seen_tool_use: false,
            max_index: -1,
            draft: String::new(),
            holding_stop: false,
            injected: false,
            held_event: Vec::new(),
        }
    }

    /// 模型已输出的可见正文（供"回答是否过薄"的复盘判定）。
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// 本轮响应是否发起过 tool_use（发起过 = 调用链未结束，不注入复盘）。
    pub fn seen_tool_use(&self) -> bool {
        self.seen_tool_use
    }

    /// 是否扣住了 message_stop（等待 complete_with_tail）。
    pub fn is_holding_stop(&self) -> bool {
        self.holding_stop
    }

    /// 喂入上游字节，返回应下发给客户端的字节（字节保真）。
    /// 完成态（已注入/已放行）后原样透传。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.holding_stop || self.injected {
            return chunk.to_vec();
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = find_event_end(&self.buf) {
            let event: Vec<u8> = self.buf.drain(..pos).collect();
            out.extend_from_slice(&self.process_event(&event));
        }
        out
    }

    /// 放行收尾：tail=Some 时先注入尾块事件（content_block_start/delta/stop，
    /// index = max_index + 1）再放行扣住的 message_stop 原始字节；None 时直接
    /// 放行。调用后进入完成态。
    pub fn complete_with_tail(&mut self, tail: Option<String>) -> Vec<u8> {
        let mut out = Vec::new();
        if let (Some(tail), false) = (&tail, self.injected) {
            let idx = self.max_index + 1;
            let start = json!({"type":"content_block_start","index":idx,"content_block":{"type":"text","text":""}});
            let delta = json!({"type":"content_block_delta","index":idx,"delta":{"type":"text_delta","text":tail}});
            let stop = json!({"type":"content_block_stop","index":idx});
            for (name, payload) in [
                ("content_block_start", start),
                ("content_block_delta", delta),
                ("content_block_stop", stop),
            ] {
                out.extend_from_slice(format!("event: {name}\ndata: {payload}\n\n").as_bytes());
            }
        }
        if !self.held_event.is_empty() {
            out.extend_from_slice(&self.held_event);
            self.held_event = Vec::new();
        }
        // 复位扣留态：否则每次 poll 都会重入补尾分支、发出 0 字节死循环
        // （实测 2026-09-28：24 万+ 次空 poll，流永不结束，客户端超时）。
        self.holding_stop = false;
        self.injected = true;
        out
    }

    /// 上游流结束时调用：冲刷残留缓冲（不足一个完整事件的部分原样放行）。
    pub fn finish(&mut self) -> Vec<u8> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        std::mem::take(&mut self.buf)
    }

    fn process_event(&mut self, event: &[u8]) -> Vec<u8> {
        let analysis = String::from_utf8_lossy(event);
        let mut is_message_stop = false;
        for line in analysis.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str).unwrap_or("") {
                "content_block_start" => {
                    if let Some(idx) = v.get("index").and_then(Value::as_i64) {
                        if idx > self.max_index {
                            self.max_index = idx;
                        }
                    }
                    if v.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use") {
                        self.seen_tool_use = true;
                    }
                }
                "content_block_delta" => {
                    if let Some(t) = v.pointer("/delta/text").and_then(Value::as_str) {
                        self.draft.push_str(t);
                    }
                }
                "message_stop" => is_message_stop = true,
                _ => {}
            }
        }
        if is_message_stop && self.tail_enabled && !self.injected {
            self.holding_stop = true;
            self.held_event = event.to_vec();
            return Vec::new();
        }
        event.to_vec()
    }
}

/// 找 SSE 事件边界（\n\n 或 \r\n\r\n），返回"边界之后"的字节偏移；未找到返回 None。
fn find_event_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i + 2);
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some(i + 4);
        }
        i += 1;
    }
    None
}

/// 该模型是否由 grok2api 桥接承接。纯函数。
/// 模型 ID 归一化匹配（大小写/连字符不敏感）：Grok4.7 / grok 4.7 / GROK-4.7
/// 都映射到 grok-4.7——客户端（ZCode 会话钉住的旧 ID 等）写法不一时也能路由。
fn g2a_handles(cfg: &G2aProxyConfig, model: &str) -> bool {
    let key = normalize_model_key(model);
    cfg.models.iter().any(|m| normalize_model_key(m) == key)
}

/// 模型 ID 归一化：小写 + 仅保留字母数字（大小写/连字符/空格不敏感）。
fn normalize_model_key(model: &str) -> String {
    model
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 把客户端请求的模型 ID 解析为 grok2api 上游的规范 ID（如 Grok4.7 → grok-4.7）。
/// 无精确/归一化命中时原样返回（交给上游报未知模型）。
fn canonical_g2a_model(cfg: &G2aProxyConfig, model: &str) -> String {
    let key = normalize_model_key(model);
    cfg.models
        .iter()
        .find(|m| normalize_model_key(m) == key)
        .cloned()
        .unwrap_or_else(|| model.to_string())
}

/// Build 专属模型经 grok2api 桥接转发：OpenAI 兼容端点，强制流式（统一解析器按
/// delta.content 提取）。凭据由 grok2api 管理（含 OAuth 自动续期），本网关零凭据负担；
/// 不经账号池，不做账号级标记。
async fn send_g2a_request(
    cfg: &G2aProxyConfig,
    model: &str,
    payload: &Value,
) -> Result<WebReply, crate::modules::grok_sso::ChatError> {
    use crate::modules::grok_sso::{ChatError, ChatErrorKind};
    let url = format!("{}/v1/chat/completions", cfg.base_url);
    // 归一化命中后用上游规范 ID 发送（Grok4.7 → grok-4.7）
    let canonical = canonical_g2a_model(cfg, model);
    let mut upstream_payload = payload.clone();
    upstream_payload["model"] = json!(canonical);
    upstream_payload["stream"] = json!(true);

    let resp = http_client()
        .post(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .header("Content-Type", "application/json")
        .json(&upstream_payload)
        .send()
        .await
        .map_err(|e| {
            ChatError::new(ChatErrorKind::Infra, format!("grok2api 桥接连接失败: {e}"))
        })?;

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(ChatError::new(
            ChatErrorKind::Infra,
            format!(
                "grok2api 上游错误 ({}): {}",
                status,
                body.chars().take(200).collect::<String>()
            ),
        ));
    }
    Ok(WebReply { status, body })
}

// ---------------------------------------------------------------------------
// 账号池存储（~/.wb-switch/grok_accounts.json）
// ---------------------------------------------------------------------------

fn accounts_path() -> PathBuf {
    store_dir().join("grok_accounts.json")
}

pub fn load_accounts() -> Vec<Value> {
    std::fs::read_to_string(accounts_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("accounts").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

pub fn save_accounts(accounts: &[Value]) -> Result<(), String> {
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

/// 脱敏凭证信息，用于安全展示
fn mask_secret(secret: &str) -> String {
    let s = secret.trim();
    if s.len() <= 12 {
        return "******".to_string();
    }
    format!("{}...{}", &s[..6], &s[s.len() - 4..])
}

/// 写入/更新一个账号（按 id 去重）
pub fn upsert_account(entry: Value) -> Result<Value, String> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let now = now_ms();
    let mut stored = entry.clone();

    if let Some(existing) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
    {
        for field in [
            "name",
            "provider",
            "token",
            "refreshToken",
            "tier",
            "status",
            "enabled",
            "error",
            "cooldownUntil",
        ] {
            if let Some(value) = entry.get(field) {
                if !value.is_null() {
                    existing[field] = value.clone();
                }
            }
        }
        existing["updatedAt"] = json!(now);
        stored = existing.clone();
    } else {
        stored["addedAt"] = json!(now);
        stored["updatedAt"] = json!(now);
        if stored.get("enabled").is_none() {
            stored["enabled"] = json!(true);
        }
        if stored.get("status").is_none() {
            stored["status"] = json!("ok");
        }
        accounts.push(stored.clone());
    }

    save_accounts(&accounts)?;
    Ok(stored)
}

/// 删除账号
pub fn remove_account(id: &str) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    let before = accounts.len();
    accounts.retain(|a| a.get("id").and_then(Value::as_str) != Some(id));
    if accounts.len() == before {
        return false;
    }
    save_accounts(&accounts).is_ok()
}

/// 启用/禁用账号
pub fn set_account_enabled(id: &str, enabled: bool) -> bool {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(item) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    {
        item["enabled"] = json!(enabled);
        item["updatedAt"] = json!(now_ms());
        return save_accounts(&accounts).is_ok();
    }
    false
}

/// 导入账号（支持 SSO Cookie、xAI API Key 等）
pub fn import_account(
    name: &str,
    provider: &str,
    token: &str,
    tier: Option<&str>,
) -> Result<Value, String> {
    let clean_token = token.trim();
    if clean_token.is_empty() {
        return Err("凭证内容不能为空".to_string());
    }

    let clean_provider = match provider.to_lowercase().as_str() {
        "build" | "api" | "oauth" => "build",
        "console" => "console",
        _ => "web",
    };

    let id = format!("grok_{}_{}", clean_provider, &uuid::Uuid::new_v4().to_string()[..8]);
    let display_name = if name.trim().is_empty() {
        match clean_provider {
            "build" => format!("Grok Build ({})", &clean_token[..clean_token.len().min(6)]),
            "console" => "Grok Console 账号".to_string(),
            _ => "Grok Web 账号".to_string(),
        }
    } else {
        name.trim().to_string()
    };

    let entry = json!({
        "id": id,
        "name": display_name,
        "provider": clean_provider,
        "token": clean_token,
        "tier": tier.unwrap_or("free"),
        "enabled": true,
        "status": "ok",
        "error": null,
        "cooldownUntil": 0,
        "lastUsedAt": null,
        "addedAt": now_ms(),
        "updatedAt": now_ms(),
    });

    upsert_account(entry)
}

/// 获取面向前端列表展示的脱敏数据
pub fn list_for_display() -> Vec<Value> {
    let accounts = load_accounts();
    let now = now_ms();
    accounts
        .into_iter()
        .map(|mut acc| {
            if let Some(token) = acc.get("token").and_then(Value::as_str) {
                acc["tokenMasked"] = json!(mask_secret(token));
            }
            let cooldown_until = acc
                .get("cooldownUntil")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            acc["isCooling"] = json!(cooldown_until > now);
            acc
        })
        .collect()
}

/// 标记账号冷却（如遇到 429 / 限流 / 超时）。
///
/// v1.5.0：同时更新健康分（限流/超时扣分）与连续失败计数，
/// 交由 grok_pool 的策略阈值推导风险等级。
fn mark_account_cooldown(id: &str, cooldown_seconds: u64, error_msg: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let policy = crate::modules::grok_pool::load_policy();
    let mut accounts = load_accounts();
    if let Some(item) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    {
        let until = now_ms() + (cooldown_seconds * 1000) as i64;
        item["cooldownUntil"] = json!(until);
        item["status"] = json!("cooldown");
        item["error"] = json!(error_msg);
        crate::modules::grok_pool::ensure_health_fields(item);
        // 限流与超时按不同权重扣分。
        let is_rate_limit = error_msg.contains("429") || error_msg.contains("限流") || error_msg.contains("使用人数");
        let penalty = if is_rate_limit {
            policy.rate_limit_penalty
        } else {
            policy.timeout_penalty
        };
        let cur = item.get("healthScore").and_then(Value::as_i64).unwrap_or(100);
        let h = crate::modules::grok_pool::next_health(cur, -penalty);
        item["healthScore"] = json!(h);
        item["riskLevel"] = json!(crate::modules::grok_pool::risk_level(h, &policy));
        let cf = item.get("consecutiveFailures").and_then(Value::as_i64).unwrap_or(0) + 1;
        item["consecutiveFailures"] = json!(cf);
        if is_rate_limit {
            let rc = item.get("rateLimitCount").and_then(Value::as_i64).unwrap_or(0) + 1;
            item["rateLimitCount"] = json!(rc);
            item["lastErrorKind"] = json!("rate_limit");
        } else {
            item["lastErrorKind"] = json!("timeout");
        }
        item["updatedAt"] = json!(now_ms());
        let acc_name = item.get("name").and_then(Value::as_str).unwrap_or("账号").to_string();
        let _ = save_accounts(&accounts);

        // C2：限流是风控升温的早期信号，值得单独告警（notify 内部 10 分钟节流防刷屏）。
        if is_rate_limit {
            crate::modules::notify::notify(
                "grok-pool",
                "Grok 账号触发限流",
                &format!("账号「{acc_name}」遇到限流，已自动冷却降级；系统将避让该账号直至恢复。"),
            );
        }
    }
}

/// 标记账号错误（如 401 凭证失效）。
///
/// v1.5.0：带指数退避 —— 首次 30 分钟，之后 2h / 6h / 24h（上限）。
/// 过了 errorRetryAt 账号会重新进入候选池尝试，成功即自动恢复；
/// 这样既不会永久跳过"被误判"的账号，也不会让真失效账号每轮都白试。
fn mark_account_error(id: &str, error_msg: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(item) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    {
        let level = item.get("errorRetryLevel").and_then(Value::as_i64).unwrap_or(0);
        let new_level = (level + 1).min(4);
        let backoff_ms: i64 = match new_level {
            1 => 30 * 60 * 1000,
            2 => 2 * 3600 * 1000,
            3 => 6 * 3600 * 1000,
            _ => 24 * 3600 * 1000,
        };
        item["status"] = json!("error");
        item["error"] = json!(error_msg);
        item["errorRetryLevel"] = json!(new_level);
        item["errorRetryAt"] = json!(now_ms() + backoff_ms);
        // v1.5.0：致命失效重扣健康分。
        let policy = crate::modules::grok_pool::load_policy();
        crate::modules::grok_pool::ensure_health_fields(item);
        let cur = item.get("healthScore").and_then(Value::as_i64).unwrap_or(100);
        let h = crate::modules::grok_pool::next_health(cur, -policy.fatal_penalty);
        item["healthScore"] = json!(h);
        item["riskLevel"] = json!(crate::modules::grok_pool::risk_level(h, &policy));
        let cf = item.get("consecutiveFailures").and_then(Value::as_i64).unwrap_or(0) + 1;
        item["consecutiveFailures"] = json!(cf);
        item["lastErrorKind"] = json!("fatal");
        item["updatedAt"] = json!(now_ms());
        let _ = save_accounts(&accounts);
    }
}

/// 记录账号调用成功
fn mark_account_success(id: &str) {
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut accounts = load_accounts();
    if let Some(item) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
    {
        item["status"] = json!("ok");
        item["error"] = Value::Null;
        // v1.5.0：成功后清除 error 退避状态（账号已恢复健康）。
        item["errorRetryLevel"] = Value::Null;
        item["errorRetryAt"] = Value::Null;
        item["lastUsedAt"] = json!(now_ms());
        // v1.5.0：成功回补健康分、清零连续失败。
        let policy = crate::modules::grok_pool::load_policy();
        crate::modules::grok_pool::ensure_health_fields(item);
        let cur = item.get("healthScore").and_then(Value::as_i64).unwrap_or(100);
        let h = crate::modules::grok_pool::next_health(cur, policy.success_delta);
        item["healthScore"] = json!(h);
        item["riskLevel"] = json!(crate::modules::grok_pool::risk_level(h, &policy));
        item["consecutiveFailures"] = json!(0);
        item["lastSuccessAt"] = json!(now_ms());
        item["updatedAt"] = json!(now_ms());
        let _ = save_accounts(&accounts);
    }
}

// ---------------------------------------------------------------------------
// 模型目录与映射
// ---------------------------------------------------------------------------

pub fn models_payload() -> Value {
    let mut models = vec![
        json!({
            "id": "grok-4.7",
            "object": "model",
            "created": 1790400000,
            "owned_by": "xai",
            "description": "Grok 4.7 最新旗舰（Build 通道 · 支持原生工具调用；免费档无内置联网，实时信息请用 grok-4）"
        }),
        json!({
            "id": "grok-composer-2.5-fast",
            "object": "model",
            "created": 1790300000,
            "owned_by": "xai",
            "description": "Grok Composer 2.5 Fast（Build 通道 · 极速生成）"
        }),
        json!({
            "id": "grok-4.6",
            "object": "model",
            "created": 1780000000,
            "owned_by": "xai",
            "description": "Grok 4.6（已被上游模型目录下线，请求将回落 web 通道 Grok 4）"
        }),
        json!({
            "id": "grok-4",
            "object": "model",
            "created": 1745000000,
            "owned_by": "xai",
            "description": "Grok 4 旗舰基座模型，推理能力全面升级"
        }),
        json!({
            "id": "grok-4-fast",
            "object": "model",
            "created": 1745000000,
            "owned_by": "xai",
            "description": "Grok 4 高速版，速度与智能平衡"
        }),
        json!({
            "id": "grok-4-thinking",
            "object": "model",
            "created": 1745000000,
            "owned_by": "xai",
            "description": "Grok 4 深度思考模式，完整思维链推演"
        }),
        json!({
            "id": "grok-beta",
            "object": "model",
            "created": 1710000000,
            "owned_by": "xai",
            "description": "Grok 官方通用主力模型"
        }),
        json!({
            "id": "grok-2",
            "object": "model",
            "created": 1720000000,
            "owned_by": "xai",
            "description": "Grok 2 标准大模型，具备高智能推理与编程表现"
        }),
        json!({
            "id": "grok-2-mini",
            "object": "model",
            "created": 1720000000,
            "owned_by": "xai",
            "description": "Grok 2 轻量版，兼具极速首字延迟与轻快响应"
        }),
        json!({
            "id": "grok-2-vision",
            "object": "model",
            "created": 1720000000,
            "owned_by": "xai",
            "description": "Grok 2 多模态视觉模型，支持图文理解"
        }),
        json!({
            "id": "grok-3",
            "object": "model",
            "created": 1735000000,
            "owned_by": "xai",
            "description": "Grok 3 新一代旗舰基座模型"
        }),
        json!({
            "id": "grok-3-mini",
            "object": "model",
            "created": 1735000000,
            "owned_by": "xai",
            "description": "Grok 3 轻量极速推理模型"
        }),
        json!({
            "id": "grok-3-thinking",
            "object": "model",
            "created": 1735000000,
            "owned_by": "xai",
            "description": "Grok 3 深度思考模式（输出完整思维链与详细推演）"
        }),
        json!({
            "id": "grok-chat-fast",
            "object": "model",
            "created": 1715000000,
            "owned_by": "xai",
            "description": "Grok 网页端极速通道（Fast Mode）"
        }),
        json!({
            "id": "grok-chat-auto",
            "object": "model",
            "created": 1715000000,
            "owned_by": "xai",
            "description": "Grok 网页端自适应通道（Auto Mode）"
        }),
        json!({
            "id": "grok-chat-expert",
            "object": "model",
            "created": 1715000000,
            "owned_by": "xai",
            "description": "Grok 网页端专家深度推理通道（Expert Mode）"
        }),
    ];

    // model-sync 注册表合并尾步（契约 model-sync-contract.md §3.3）：同步引擎检出的
    // 新模型 / 页面手动补录追加在静态表之后。去重：已存在 id 跳过。
    // 注册表为空时本循环零迭代，输出与既有实现逐字节一致。
    for entry in crate::modules::model_sync::registry_entries_for("grok") {
        if models
            .iter()
            .any(|m| m.get("id").and_then(Value::as_str) == Some(entry.id.as_str()))
        {
            continue;
        }
        models.push(json!({
            "id": entry.id,
            "object": "model",
            "created": entry.added_ts / 1000,
            "owned_by": "xai",
            "description": entry.name.clone().unwrap_or_else(|| "（模型同步注册）".to_string()),
        }));
    }

    json!({
        "object": "list",
        "data": models
    })
}

// ---------------------------------------------------------------------------
// 核心协议转译与通信引擎
// ---------------------------------------------------------------------------

pub enum GrokChatOutput {
    Json(Value),
    Stream(reqwest::Response),
}

/// 账号当前是否可被调度（选号层与容量守卫的同一事实来源）。
///
/// 判定规则（v1.5.0 起）：
/// - `enabled` 未被手动禁用；
/// - 不在冷却期（`cooldownUntil <= now`）；
/// - `status == "error"` 的账号过了 `errorRetryAt` 指数退避期后才重新纳入
///   （历史无该字段 = retry_at=0 → 立即纳入，实现"假 error 自愈"）。
pub(crate) fn is_account_available(a: &Value, now: i64) -> bool {
    let enabled = a.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let cooldown = a.get("cooldownUntil").and_then(Value::as_i64).unwrap_or(0);
    let errored = a.get("status").and_then(Value::as_str) == Some("error");
    let retry_at = a.get("errorRetryAt").and_then(Value::as_i64).unwrap_or(0);
    let blocked_by_error = errored && retry_at > now;
    enabled && cooldown <= now && !blocked_by_error
}

/// 调度选出一个可用的 Grok 账号
fn pick_available_account() -> Result<Value, String> {
    let accounts = load_accounts();
    if accounts.is_empty() {
        return Err("当前 Grok 网关尚未配置任何可用账号，请先进入「Grok 网关」页面添加账号凭证".to_string());
    }

    let now = now_ms();
    let candidates: Vec<&Value> = accounts
        .iter()
        .filter(|a| is_account_available(a, now))
        .collect();

    // C2 容量守卫：可用数低于策略下限时暂停调度（体检循环不受影响——池越浅
    // 越需要体检找回死账号）。默认 min=1 时仅「无可用账号」触发，与旧语义一致。
    let cap = crate::modules::grok_pool::capacity_guard_state();
    if cap.get("belowMin").and_then(Value::as_bool) == Some(true) {
        let floor = cap.get("minPoolSize").and_then(Value::as_i64).unwrap_or(1);
        let available = cap.get("available").and_then(Value::as_i64).unwrap_or(0);
        return Err(format!(
            "账号池可用数（{available}）低于容量下限（{floor}），已按策略暂停调度；请补充账号或在「策略」中调整下限"
        ));
    }

    if candidates.is_empty() {
        return Err("所有 Grok 账号当前均处于冷却或禁用状态，请稍后重试或补充新账号".to_string());
    }

    // v1.5.0：驻留优先 —— 若当前有未过期的驻留账号且可用，直接复用它，
    // 避免每条请求都 round-robin 切号（高频身份横跳会触发风控）。
    if let Some(resident_id) = crate::modules::grok_sso::resident_account_id() {
        if let Some(acc) = candidates
            .iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(resident_id.as_str()))
        {
            return Ok((*acc).clone());
        }
    }

    let idx = ROUND_ROBIN_INDEX.fetch_add(1, Ordering::Relaxed) % candidates.len();
    Ok(candidates[idx].clone())
}

/// 换号重试的退避时间（毫秒）；`None` = 不重试。
///
/// 最多 1 次换号重试（attempt>=1 一律 None）。分级依据：
/// - `Invalid`（cookie 为空等数据问题）重试无意义；
/// - `NotLoggedIn`/`RateLimited` 退避 300ms —— 账号侧冷却/errorRetryAt 已由
///   mark_account_* 负责，这里只控换号节奏；
/// - `Timeout`/`Infra` 退避 600ms —— 瞬时抖动，略长的退避避免连续撞击。
fn retry_backoff_ms(kind: crate::modules::grok_sso::ChatErrorKind, attempt: u32) -> Option<u64> {
    if attempt >= 1 {
        return None;
    }
    match kind {
        crate::modules::grok_sso::ChatErrorKind::Invalid => None,
        crate::modules::grok_sso::ChatErrorKind::NotLoggedIn => Some(300),
        crate::modules::grok_sso::ChatErrorKind::RateLimited => Some(300),
        crate::modules::grok_sso::ChatErrorKind::Timeout => Some(600),
        crate::modules::grok_sso::ChatErrorKind::Infra => Some(600),
    }
}

/// 从 OpenAI messages 列表解析出最新用户提示词和历史上下文
/// 从网页抓取的回复里嗅探工具调用，返回 (工具调用, 该 JSON 块在原文中的字节区间)。
///
/// 为什么要包一层：`glm_web::try_extract_tool_calls` 依赖 `{"tool_calls"` 字面量
/// （花括号紧贴引号）或 ```json 围栏。而 Grok 网页端把回复渲染成代码块后，
/// textContent 是**美化过的多行 JSON**，且前面带代码块头部 UI 文案
/// （实测前缀 `JSON复制已复制`），两种策略都会失配 —— 0.1.87 实证：模型确实
/// 按约定回了 JSON，但识别成 `null`。
/// 这里先「剥前缀 + 括号配平截取」得到一个干净 JSON 对象，再交给共享嗅探器。
fn sniff_tool_calls(text: &str) -> (Option<Vec<Value>>, Option<(usize, usize)>) {
    if let Some(calls) = crate::modules::glm_web::try_extract_tool_calls(text) {
        return (Some(calls), None);
    }
    let start = match text.find('{') {
        Some(i) => i,
        None => return (None, None),
    };
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut esc = false;
    let mut end: Option<usize> = None;
    for (off, ch) in text[start..].char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match ch {
            '\\' if in_str => esc = true,
            '"' => in_str = !in_str,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth <= 0 {
                    end = Some(start + off);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = match end {
        Some(e) => e,
        None => return (None, None),
    };
    match crate::modules::glm_web::try_extract_tool_calls(&text[start..=end]) {
        Some(calls) => (Some(calls), Some((start, end))),
        None => (None, None),
    }
}

/// 去掉代码块头部的 UI 文案（实测 `JSON复制已复制`、`PythonCopy` 之类：
/// 语言标签 + 复制按钮文案）。只剥**开头/结尾那一串**，保留真正的正文。
///
/// 注意：不硬编码整条文案（换个语言/版本就失效），而是反复剥离
/// 「复制类词」再剥「语言标签」，对中英文 UI 都成立。
fn strip_code_block_chrome(s: &str) -> String {
    const COPY_WORDS: [&str; 6] = ["已复制", "复制", "已拷贝", "拷贝", "Copied", "Copy"];
    let mut out = s.trim_end().to_string();
    let mut removed_any = false;
    loop {
        let mut changed = false;
        for w in COPY_WORDS {
            if out.ends_with(w) {
                out.truncate(out.len() - w.len());
                out = out.trim_end().to_string();
                changed = true;
                removed_any = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }
    // 没有复制类文案 → 不是代码块头部，原样返回。
    // 这一步很关键：否则会误伤以英文单词结尾的正文（如 "Let me check"）。
    if !removed_any {
        return out.trim().to_string();
    }
    // 语言标签：剥离复制词后末尾残留的短标识（JSON / Python / Bash / C++）
    let tail: String = out
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '.' | '_'))
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if !tail.is_empty() && tail.len() <= 12 && tail.len() <= out.len() {
        out.truncate(out.len() - tail.len());
        out = out.trim_end().to_string();
    }
    out.trim().to_string()
}

fn push_turn(buf: &mut String, label: &str, text: &str) {
    if !buf.is_empty() {
        buf.push_str("\n\n");
    }
    buf.push_str(&format!("{}: {}", label, text));
}

fn extract_prompt_from_messages(messages: &[Value], tools: Option<&[Value]>) -> (String, String) {
    let mut system_prompt = String::new();
    let mut conversation_text = String::new();

    // v1.6.0：工具定义注入（伪工具调用，协议与 glm_web.rs:469 完全一致）。
    // web 通道没有 tools 参数入口：只能把工具定义写进 prompt，
    // 再从回复文本里嗅探回标准 tool_calls（见下方 try_extract_tool_calls）。
    if let Some(list) = tools {
        if !list.is_empty() {
            let tools_json =
                serde_json::to_string_pretty(list).unwrap_or_else(|_| "[]".to_string());
            system_prompt.push_str("[AVAILABLE_TOOLS]\nYou have access to the following tools:\n");
            system_prompt.push_str(&tools_json);
            system_prompt.push_str(
                "\n\nWhen you need to call one or more tools, you MUST respond ONLY with a JSON block in the following format:\n```json\n{\n  \"tool_calls\": [\n    {\n      \"name\": \"<tool_name>\",\n      \"arguments\": { /* arguments object */ }\n    }\n  ]\n}\n```\nIf no tool call is needed, respond directly with plain text.\n[/AVAILABLE_TOOLS]",
            );
        }
    }

    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = if let Some(text) = m.get("content").and_then(Value::as_str) {
            text.to_string()
        } else if let Some(arr) = m.get("content").and_then(Value::as_array) {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t);
                }
            }
            parts.join("\n")
        } else {
            String::new()
        };

        // 兼容 assistant 历史携带 tool_calls（伪工具调用的多轮回环）
        if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
            if !tcs.is_empty() {
                let summary = serde_json::to_string(tcs).unwrap_or_default();
                push_turn(
                    &mut conversation_text,
                    "Assistant",
                    &format!("[TOOL_CALLS: {summary}]"),
                );
                continue;
            }
        }

        // 兼容工具结果回灌（role: "tool"）
        if role == "tool" {
            let tid = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
            push_turn(
                &mut conversation_text,
                "User",
                &format!("[TOOL_RESULT id={tid}]: {content}"),
            );
            continue;
        }

        if role == "system" {
            if !system_prompt.is_empty() {
                system_prompt.push('\n');
            }
            system_prompt.push_str(&content);
        } else {
            let role_label = match role {
                "assistant" => "Assistant",
                _ => "User",
            };
            push_turn(&mut conversation_text, role_label, &content);
        }
    }

    // v1.6.2：格式约定**再追加到末尾**。
    // 实测只放在开头时遵守率不稳定（同一 payload 时而回 JSON、时而直接作答），
    // 模型对靠近输出位置的指令更敏感，所以首尾各放一次。
    if let Some(list) = tools {
        if !list.is_empty() {
            if !conversation_text.is_empty() {
                conversation_text.push_str("\n\n");
            }
            conversation_text.push_str(
                "[REMINDER] If you need to call a tool, reply ONLY with the JSON block described in [AVAILABLE_TOOLS]. Do not output anything else.",
            );
        }
    }

    if conversation_text.is_empty() {
        conversation_text = "你好".to_string();
    }

    (system_prompt, conversation_text)
}

/// 合并一条 OpenAI 流式 tool_calls 分片进累加器（v1.7：grok2api/Build 通道原生
/// 工具调用透传）。delta.tool_calls[] = {index, id?, type?, function?: {name?,
/// arguments?}}——arguments 跨多 chunk 分片到达需按 index 拼接，id/name 只在
/// 首片出现。纯函数，便于单测。
fn merge_tool_call_fragment(acc: &mut Vec<Value>, frag: &Value) {
    let idx = frag
        .get("index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    while acc.len() <= idx {
        acc.push(json!({
            "id": Value::Null,
            "type": "function",
            "function": {"name": String::new(), "arguments": String::new()},
        }));
    }
    let slot = &mut acc[idx];
    if let Some(id) = frag.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        slot["id"] = json!(id);
    }
    if let Some(t) = frag.get("type").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        slot["type"] = json!(t);
    }
    if let Some(name) = frag
        .pointer("/function/name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        slot["function"]["name"] = json!(name);
    }
    if let Some(args) = frag
        .pointer("/function/arguments")
        .and_then(Value::as_str)
    {
        let current = slot["function"]["arguments"].as_str().unwrap_or_default();
        slot["function"]["arguments"] = json!(format!("{current}{args}"));
    }
}

/// 执行统一的 chat_completions 请求
pub async fn chat_completions_unified(payload: Value) -> Result<GrokChatOutput, String> {
    // v1.1.0：流式请求也走 WebReply 统一路径，最后按 payload.stream 合成输出。
    let want_stream = payload.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("grok-2")
        .to_string();

    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // v1.7：Build 通道新旗舰（grok-4.7 等）经本地 grok2api 桥接——上游模型目录轮换后
    // （grok-4.6 下线、grok-4.7 上线），web 页面与 api.x.ai 直连都跑不了这些模型；
    // grok2api 的 Build 池（OAuth 自动续期）是唯一稳定入口。桥接关闭/配置缺失时
    // 完全走原有账号池路径，行为不变。
    let g2a = g2a_proxy_config();
    // 回落守卫：g2a 空内容回落 web 的递归请求带 _g2a_fallback 标记，二跳不再走桥接。
    let g2a_skip = payload
        .get("_g2a_fallback")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let g2a_serve =
        !g2a_skip && g2a.as_ref().map(|c| g2a_handles(c, &model)).unwrap_or(false);
    let mut g2a_empty_fallback = false;

    // v1.6.0：tools 参数在此进入 —— web 通道没有原生入口，只能注入 prompt。
    let tools = payload.get("tools").and_then(Value::as_array).cloned();
    let (system_prompt, prompt) = extract_prompt_from_messages(&messages, tools.as_deref());

    // 账号挑选与换号重试（最多 1 次换号重试；退避按错误类型分级，
    // 见 retry_backoff_ms）。build 通道的错误此前被 `?` 直接吞掉不重试，
    // 现在与 web 通道统一走 ChatError 分类。
    let mut last_err = String::new();
    for attempt in 0..2u32 {
        // grok2api 桥接优先：Build 专属模型不经账号池（凭据由 grok2api 管理）。
        let (req_res, served_by_g2a, account_id, channel_label) = if g2a_serve {
            let cfg = g2a.as_ref().expect("g2a_serve implies config present");
            (
                send_g2a_request(cfg, &model, &payload).await,
                true,
                String::new(),
                "grok2api 桥".to_string(),
            )
        } else {
            let account = match pick_available_account() {
                Ok(acc) => acc,
                Err(e) => return Err(e),
            };

            let account_id = account.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let provider = account.get("provider").and_then(Value::as_str).unwrap_or("web");
            let token = account.get("token").and_then(Value::as_str).unwrap_or_default();
            let channel_label = if provider == "build" { "Build" } else { "Web" }.to_string();

            let req_res: Result<WebReply, crate::modules::grok_sso::ChatError> = match provider {
                "build" => match send_build_request(&account_id, token, &model, &payload).await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let body = resp.text().await.unwrap_or_default();
                        Ok(crate::modules::grok::WebReply { status, body })
                    }
                    Err(e) => Err(e),
                },
                _ => send_web_request(&account_id, token, &model, &system_prompt, &prompt).await,
            };
            (req_res, false, account_id, channel_label)
        };

        match req_res {
            Ok(reply) => {
                if !served_by_g2a {
                    mark_account_success(&account_id);
                }
                let mut req_res_body = Some(reply.body);
                let upstream_status = reply.status;

                // v1.1.0：WebReply 无逐 token 流（CDP fetch 一次性返回），流式请求
                // 走下方统一解析后合成 OpenAI chunk 帧（与 glm_proxy / loomy_proxy 同策略）。
                let _ = upstream_status;

                // 非流式：聚合上游流/数据为完整 chat.completion JSON
                let mut content = String::new();
                let mut reasoning = String::new();
                // v1.7：原生 tool_calls 累加（grok2api/Build 通道按 OpenAI 流式规范
                // 分片下发；此前被解析器丢弃——网页反代模型无法调用工具的根因）。
                let mut native_tool_calls: Vec<Value> = Vec::new();
                let mut upstream_finish_reason: Option<String> = None;
                let completion_id = format!("chatcmpl-{}", &uuid::Uuid::new_v4().to_string()[..12]);

                // v1.1.0：WebReply.body（CDP fetch 为完整 JSON / 直连为 SSE 文本）统一按行解析。
                let resp_text = req_res_body.take().unwrap_or_default();
                {
                    let text: &str = &resp_text;
                    for line in text.lines().chain(std::iter::once("")) {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let data = if let Some(d) = trimmed.strip_prefix("data:") {
                            d.trim()
                        } else {
                            trimmed
                        };
                        if data == "[DONE]" || data.is_empty() {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            // Grok Web 或 OpenAI SSE 格式提取
                            if let Some(delta) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                                content.push_str(delta);
                            } else if let Some(msg) = v.pointer("/result/response/modelResponse/message").and_then(Value::as_str) {
                                content.push_str(msg);
                            } else if let Some(t) = v.pointer("/result/response/token").and_then(Value::as_str) {
                                content.push_str(t);
                            }
                            // 思考链提取
                            if let Some(r) = v.pointer("/choices/0/delta/reasoning_content").and_then(Value::as_str) {
                                reasoning.push_str(r);
                            } else if let Some(r_str) = v.pointer("/result/response/reasoningToken").and_then(Value::as_str) {
                                reasoning.push_str(r_str);
                            }
                            // v1.7：原生 tool_calls 分片累积（OpenAI 流式规范）
                            if let Some(fragments) = v.pointer("/choices/0/delta/tool_calls").and_then(Value::as_array) {
                                for frag in fragments {
                                    merge_tool_call_fragment(&mut native_tool_calls, frag);
                                }
                            }
                            // finish_reason 透传（tool_calls 判定的上游权威来源）
                            if let Some(fr) = v.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
                                if !fr.is_empty() {
                                    upstream_finish_reason = Some(fr.to_string());
                                }
                            }
                        }
                    }
                }

                // v1.7：原生 tool_calls 优先——有则组装标准 message.tool_calls 且
                // content 保持纯净（推理只进 reasoning_content，不裹 <think>，
                // 落实「工具调用不混思考」的用户要求）；无则走既有伪嗅探路径。
                let native_tools: Option<Vec<Value>> = if native_tool_calls.is_empty() {
                    None
                } else {
                    Some(
                        native_tool_calls
                            .iter()
                            .map(|tc| {
                                json!({
                                    "id": tc["id"].clone(),
                                    "type": "function",
                                    "function": {
                                        "name": tc["function"]["name"].clone(),
                                        "arguments": tc["function"]["arguments"].clone(),
                                    },
                                })
                            })
                            .collect(),
                    )
                };

                // v1.7 空内容回落：g2a（grok-4.7 等 Build 免费档模型）模型内部联网
                // 尝试未获执行时正文为空——自动回落 web 通道（页面自带搜索），让
                // 实时问题拿到真实答案而不是「无文本返回内容」。
                if served_by_g2a && content.trim().is_empty() && native_tools.is_none() {
                    g2a_empty_fallback = true;
                    last_err = "grok-4.7 返回空正文（模型内部联网尝试未获执行），回落 web 通道".to_string();
                    break;
                }

                let final_message_content = if native_tools.is_some() {
                    // 原生工具调用：正文保持纯净（推理只进 reasoning_content 字段）
                    content
                } else if served_by_g2a {
                    // g2a 纯文本路径：推理进独立字段，不裹 <think>（输出干净）
                    if content.is_empty() {
                        "Grok 服务已响应（无文本返回内容）".to_string()
                    } else {
                        content
                    }
                } else if !reasoning.is_empty() {
                    format!("<think>\n{}\n</think>\n\n{}", reasoning, content)
                } else if content.is_empty() {
                    "Grok 服务已响应（无文本返回内容）".to_string()
                } else {
                    content
                };

                // v1.6.0：伪工具调用嗅探 —— **仅在请求带 tools 且无原生 tool_calls 时**
                // 兜底启用（web 通道 prompt 注入路径）；原生优先，避免双重工具调用。
                let (tool_calls, span) = if native_tools.is_some() {
                    (native_tools.clone(), None)
                } else if tools.is_some() {
                    sniff_tool_calls(&final_message_content)
                } else {
                    (None, None)
                };
                let finish_reason = if native_tools.is_some() {
                    upstream_finish_reason
                        .as_deref()
                        .filter(|f| *f != "stop")
                        .map(String::from)
                        .unwrap_or_else(|| "tool_calls".to_string())
                } else if tool_calls.is_some() {
                    "tool_calls".to_string()
                } else {
                    "stop".to_string()
                };
                // 已被解析成 tool_calls 的 JSON 块从正文里摘掉，避免客户端重复处理；
                // 顺带剥掉代码块头部 UI 文案（如 "JSON复制已复制"）。
                let content_out = match span {
                    Some((s, e)) if e + 1 <= final_message_content.len() => {
                        let head = strip_code_block_chrome(&final_message_content[..s]);
                        let tail = final_message_content[e + 1..].trim().to_string();
                        match (head.is_empty(), tail.is_empty()) {
                            (true, true) => String::new(),
                            (true, false) => tail,
                            (false, true) => head,
                            (false, false) => format!("{}\n\n{}", head, tail),
                        }
                    }
                    _ => final_message_content.clone(),
                };
                let mut message = json!({
                    "role": "assistant",
                    "content": content_out,
                    "reasoning_content": if reasoning.is_empty() { Value::Null } else { json!(reasoning) }
                });
                if let Some(tc) = &tool_calls {
                    message["tool_calls"] = json!(tc);
                }

                let result_json = json!({
                    "id": completion_id,
                    "object": "chat.completion",
                    "created": now_ms() / 1000,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": message,
                        "finish_reason": finish_reason
                    }],
                    "usage": {
                        "prompt_tokens": prompt.len() / 3,
                        "completion_tokens": final_message_content.len() / 3,
                        "total_tokens": (prompt.len() + final_message_content.len()) / 3
                    }
                });

                return Ok(GrokChatOutput::Json(result_json));
            }
            Err(chat_err) => {
                eprintln!(
                    "[grok] 请求失败 ({:?}, attempt={}/1): {}",
                    chat_err.kind, attempt, chat_err.message
                );
                let channel = channel_label.as_str();
                // g2a 桥接失败：grok2api 进程可能掉线——托管自愈（探活/拉起）后
                // 重试一次（attempt 0 → 1）；仍失败才按错误收尾。
                if served_by_g2a && attempt == 0 {
                    let _ = crate::modules::grok2api_manager::ensure_alive().await;
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    continue;
                }
                last_err = format!("Grok {channel} 通道失败：{}", chat_err.message);
                match retry_backoff_ms(chat_err.kind, attempt) {
                    Some(ms) => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        continue;
                    }
                    // 不重试（数据问题或已用完重试额度）：直接带着错误收尾。
                    None => break,
                }
            }
        }
    }

    // g2a 空内容回落：grok-4.7（免费 Build 档无内置联网）正文为空时，自动改走
    // web 账号池（页面侧自带搜索）——实时问题由此获得真实答案。递归带
    // _g2a_fallback 标记确保二跳跳过桥接；结果注入 wbFallback 字段供观测。
    if g2a_empty_fallback {
        let mut fallback_payload = payload.clone();
        fallback_payload["_g2a_fallback"] = json!(true);
        return match Box::pin(chat_completions_unified(fallback_payload)).await {
            Ok(GrokChatOutput::Json(mut v)) => {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "wbFallback".to_string(),
                        json!("grok-4.7 空正文 → web 通道回落（页面侧搜索）"),
                    );
                }
                Ok(GrokChatOutput::Json(v))
            }
            Ok(other) => Ok(other),
            Err(e) => Err(format!("grok-4.7 空正文回落 web 通道失败：{e}")),
        };
    }

    Err(if last_err.is_empty() {
        "Grok 请求未能成功完成".to_string()
    } else {
        last_err
    })
}

// ---------------------------------------------------------------------------
// 上游 HTTP 请求发送封装
// ---------------------------------------------------------------------------

async fn send_build_request(
    account_id: &str,
    token: &str,
    model: &str,
    payload: &Value,
) -> Result<reqwest::Response, crate::modules::grok_sso::ChatError> {
    use crate::modules::grok_sso::{ChatError, ChatErrorKind};
    let client = http_client();
    let url = format!("{}/v1/chat/completions", GROK_BUILD_BASE);

    let auth_header = if token.starts_with("Bearer ") {
        token.to_string()
    } else {
        format!("Bearer {}", token)
    };

    let mut upstream_payload = payload.clone();
    if upstream_payload.get("model").is_none() {
        upstream_payload["model"] = json!(model);
    }
    // 强制上游流式便于统一响应
    upstream_payload["stream"] = json!(true);

    let resp = client
        .post(&url)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/json")
        .json(&upstream_payload)
        .send()
        .await
        .map_err(|e| ChatError::new(ChatErrorKind::Infra, format!("Grok Build 连接失败: {e}")))?;

    let status = resp.status();
    if status.as_u16() == 429 {
        mark_account_cooldown(account_id, 60, "遇到 429 请求频率限制");
        return Err(ChatError::new(
            ChatErrorKind::RateLimited,
            "Grok Build 账号遇到 429 频率限制，已自动冷却调度",
        ));
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        mark_account_error(account_id, "Grok Build API Key 无效或权限不足");
        return Err(ChatError::new(
            ChatErrorKind::NotLoggedIn,
            "Grok Build 凭据无效或已失效",
        ));
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(ChatError::new(
            ChatErrorKind::Infra,
            format!("Grok Build 上游错误 ({}): {}", status, text.chars().take(200).collect::<String>()),
        ));
    }

    Ok(resp)
}

fn normalize_web_cookie(raw_cookie: &str) -> String {
    let trimmed = raw_cookie.trim();
    if trimmed.contains("sso=") {
        trimmed.to_string()
    } else {
        format!("sso={}; sso-rw={}", trimmed, trimmed)
    }
}

/// CDP fetch 桥的返回（合成流式响应用）。
pub struct WebReply {
    pub status: u16,
    pub body: String,
}

async fn send_web_request(
    account_id: &str,
    token: &str,
    model: &str,
    system_prompt: &str,
    prompt: &str,
) -> Result<WebReply, crate::modules::grok_sso::ChatError> {
    // 注：直连路径已移除；model / is_thinking_model 等参数不再使用（Web 模型由页面决定）。
    let _ = model;
    let cookie_val = normalize_web_cookie(token);

    let full_prompt = if !system_prompt.is_empty() {
        format!("系统指令: {}\n\n{}", system_prompt, prompt)
    } else {
        prompt.to_string()
    };

    // v1.5.0：web 通道唯一走 UI 驱动 + 账号路由（grok_sso::ui_chat_with_account）。
    //
    // 不再回退直连（2026-09-17 修正）：直连必被 Cloudflare 403，且当专用浏览器
    // 未运行时会把「浏览器挂了」误标成「SSO 过期」，污染账号状态 —— 实测导致多
    // 个 web 账号被错误标记为 error。ui_chat_with_account 内部会 ensure_browser
    // 自动拉起专用浏览器，因此浏览器崩溃也能自愈。
    match crate::modules::grok_sso::ui_chat_with_account(account_id, &cookie_val, &full_prompt).await {
        Ok(reply_text) => Ok(WebReply {
            status: 200,
            body: json!({
                "result": {
                    "response": {
                        "modelResponse": { "message": reply_text },
                        "modelResponseTokens": Value::Null,
                    }
                }
            })
            .to_string(),
        }),
        Err(chat_err) => {
            eprintln!(
                "[grok] UI 驱动对话失败 ({:?}): {}",
                chat_err.kind, chat_err.message
            );
            // 按错误类型分流：临时问题只冷却，账号失效才标 error。
            if chat_err.is_account_fatal() {
                mark_account_error(account_id, &format!("UI 驱动失败: {}", chat_err.message));
            } else {
                let secs = chat_err.cooldown_seconds();
                if secs > 0 {
                    mark_account_cooldown(account_id, secs, &chat_err.message);
                }
            }
            Err(chat_err)
        }
    }
}

// ---------------------------------------------------------------------------
// x.ai 官方模型清单拉取（model-sync 专用，契约 model-sync-contract.md §3.1）
// ---------------------------------------------------------------------------

/// 从 grok 账号池选一个含 build token 的账号（provider=build、启用、token 非空）。
/// 不走 pick_available_account（那是 chat 调度语义：容量守卫/驻留/round-robin），
/// 同步引擎只读清单，不该触碰账号调度状态。
fn pick_build_token() -> Option<String> {
    let now = now_ms();
    load_accounts().into_iter().find_map(|acc| {
        let provider = acc.get("provider").and_then(Value::as_str).unwrap_or("");
        let enabled = acc.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let token = acc
            .get("token")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        if provider == "build" && enabled && is_account_available(&acc, now) && !token.is_empty()
        {
            Some(token)
        } else {
            None
        }
    })
}

/// 解析 x.ai /v1/models 响应 → (id, name) 列表（纯函数，便于单测）。
/// 形状：`{"data": [{"id": "grok-4", "object": "model", ...}]}`；缺 data / id 缺失 /
/// 空 id 一律跳过；重复 id 只保留首个。
pub fn parse_xai_models_response(value: &Value) -> Vec<(String, Option<String>)> {
    let Some(arr) = value.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in arr {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        let name = item
            .get("name")
            .or_else(|| item.get("display_name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        out.push((id.to_string(), name));
    }
    out
}

/// 拉取 x.ai 官方模型清单：`GET https://api.x.ai/v1/models`，Bearer 取 grok 账号池
/// 任一含 build token 的账号（token 形态与 send_build_request 一致：自动补 Bearer 前缀）。
/// 无可用 build token / 请求失败 / 解析失败 → Err（该网关本轮记 error，不阻塞他网关）。
/// 🔴 本函数只读清单，不触碰账号调度/冷却状态，grok chat 路径零改动。
pub async fn fetch_xai_models() -> Result<Vec<(String, Option<String>)>, String> {
    let token = pick_build_token().ok_or_else(|| {
        "无可用 build token（Grok 账号池中无启用的 Build 通道账号）".to_string()
    })?;
    let auth_header = if token.starts_with("Bearer ") {
        token
    } else {
        format!("Bearer {token}")
    };
    let url = format!("{}/v1/models", GROK_BUILD_BASE);
    let resp = http_client()
        .get(&url)
        .header("Authorization", auth_header)
        .send()
        .await
        .map_err(|e| format!("x.ai /v1/models 请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "x.ai /v1/models 失败（{status}）：{}",
            text.chars().take(150).collect::<String>()
        ));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("x.ai /v1/models 解析失败: {e}"))?;
    Ok(parse_xai_models_response(&value))
}

// ---------------------------------------------------------------------------
// 网页前端在线测试接口
// ---------------------------------------------------------------------------

pub async fn test_chat(
    model: &str,
    prompt: &str,
    enable_thinking: bool,
    account_id: Option<&str>,
) -> Result<Value, String> {
    let start = Instant::now();

    let target_account = if let Some(id) = account_id {
        let accounts = load_accounts();
        accounts
            .into_iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| format!("未找到指定的账号: {}", id))?
    } else {
        pick_available_account()?
    };

    let selected_id = target_account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let account_name = target_account
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Grok 账号")
        .to_string();
    let provider = target_account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("web")
        .to_string();

    let actual_model = if enable_thinking {
        "grok-3-thinking"
    } else if model.is_empty() {
        "grok-2"
    } else {
        model
    };

    let payload = json!({
        "model": actual_model,
        "stream": false,
        "messages": [
            { "role": "user", "content": prompt }
        ]
    });

    let token = target_account
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let chat_res: Result<WebReply, String> = match provider.as_str() {
        "build" => match send_build_request(&selected_id, token, actual_model, &payload).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                Ok(crate::modules::grok::WebReply { status, body })
            }
            Err(e) => Err(e.to_string()),
        },
        _ => send_web_request(&selected_id, token, actual_model, "", prompt)
            .await
            .map_err(|e| e.to_string()),
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    match chat_res {
        Ok(reply) => {
            mark_account_success(&selected_id);
            let mut collected = String::new();
            let mut reasoning = String::new();

            // v1.1.0：WebReply.body 统一按行解析（CDP=完整 JSON / 直连=SSE 文本）。
            let resp_text = reply.body;
            {
                let text: &str = &resp_text;
                for line in text.lines().chain(std::iter::once("")) {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let data = if let Some(d) = trimmed.strip_prefix("data:") {
                        d.trim()
                    } else {
                        trimmed
                    };
                    if data == "[DONE]" || data.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(delta) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                            collected.push_str(delta);
                        } else if let Some(msg) = v.pointer("/result/response/modelResponse/message").and_then(Value::as_str) {
                            collected.push_str(msg);
                        } else if let Some(t) = v.pointer("/result/response/token").and_then(Value::as_str) {
                            collected.push_str(t);
                        }
                        if let Some(r) = v.pointer("/choices/0/delta/reasoning_content").and_then(Value::as_str) {
                            reasoning.push_str(r);
                        } else if let Some(r_str) = v.pointer("/result/response/reasoningToken").and_then(Value::as_str) {
                            reasoning.push_str(r_str);
                        }
                    }
                }
            }

            let reply = if !reasoning.is_empty() {
                format!("<think>\n{}\n</think>\n\n{}", reasoning, collected)
            } else if collected.is_empty() {
                "连接成功，Grok 服务已响应".to_string()
            } else {
                collected
            };

            Ok(json!({
                "ok": true,
                "reply": reply,
                "model": actual_model,
                "provider": provider,
                "accountId": selected_id,
                "accountName": account_name,
                "durationMs": duration_ms,
                "tokens": reply.len() / 3,
            }))
        }
        Err(e) => {
            // v1.5.0：不再在此处 mark_account_error —— send_web_request /
            // send_build_request 内部已按错误类型正确分流标记（临时问题冷却、
            // 失效才 error）。此处二次标记会把刚冷却的账号误打成 error（实测
            // 超时导致 de34bdcc 被标 fatal 即此因）。
            let _ = &selected_id;
            Err(format!("Grok 试调失败: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msgs(v: Value) -> Vec<Value> {
        v.as_array().cloned().unwrap_or_default()
    }

    /// 可用性矩阵：选号层与容量守卫共用 is_account_available，
    /// 五种状态组合必须逐一符合预期（含 error 退避期边界）。
    #[test]
    fn is_account_available_matrix() {
        let now = 1_000_000i64;
        let mk = |enabled: bool, cooldown: i64, status: &str, retry_at: i64| {
            json!({"enabled": enabled, "cooldownUntil": cooldown, "status": status, "errorRetryAt": retry_at})
        };
        assert!(is_account_available(&mk(true, 0, "ok", 0), now), "普通可用账号");
        assert!(is_account_available(&mk(true, now, "cooldown", 0), now), "冷却恰好到期可用");
        assert!(!is_account_available(&mk(true, now + 1, "cooldown", 0), now), "冷却中不可用");
        assert!(!is_account_available(&mk(false, 0, "ok", 0), now), "手动禁用不可用");
        assert!(!is_account_available(&mk(true, 0, "error", now + 1), now), "error 退避期内不可用");
        assert!(is_account_available(&mk(true, 0, "error", now), now), "error 退避恰好到期可用（假 error 自愈）");
        assert!(is_account_available(&json!({}), now), "缺字段的老账号默认可用");
    }

    /// 重试矩阵：Invalid 不重试；attempt>=1 一律不重试；
    /// 其余按类型分级退避（换号节奏由 mark_account_* 的冷却负责）。
    #[test]
    fn retry_backoff_matrix() {
        use crate::modules::grok_sso::ChatErrorKind::*;
        assert_eq!(retry_backoff_ms(Invalid, 0), None, "数据问题重试无意义");
        assert_eq!(retry_backoff_ms(NotLoggedIn, 0), Some(300));
        assert_eq!(retry_backoff_ms(RateLimited, 0), Some(300));
        assert_eq!(retry_backoff_ms(Timeout, 0), Some(600));
        assert_eq!(retry_backoff_ms(Infra, 0), Some(600));
        assert_eq!(retry_backoff_ms(Timeout, 1), None, "最多 1 次换号重试");
        assert_eq!(retry_backoff_ms(Infra, 2), None);
    }

    #[test]
    fn no_tools_means_no_injection() {
        let (sys, conv) =
            extract_prompt_from_messages(&msgs(json!([{"role": "user", "content": "hi"}])), None);
        assert!(!sys.contains("AVAILABLE_TOOLS"), "无 tools 时不应注入: {sys}");
        assert_eq!(conv, "User: hi");
    }

    #[test]
    fn empty_tools_array_is_ignored() {
        let (sys, _) =
            extract_prompt_from_messages(&msgs(json!([{"role": "user", "content": "hi"}])), Some(&[]));
        assert!(!sys.contains("AVAILABLE_TOOLS"));
    }

    #[test]
    fn tools_are_injected_into_system_prompt() {
        let tools = vec![json!({"type": "function", "function": {"name": "get_time"}})];
        let (sys, _) = extract_prompt_from_messages(
            &msgs(json!([{"role": "user", "content": "hi"}])),
            Some(&tools),
        );
        assert!(sys.contains("[AVAILABLE_TOOLS]"));
        assert!(sys.contains("get_time"), "工具名应在注入块里: {sys}");
        assert!(sys.contains("[/AVAILABLE_TOOLS]"));
        assert!(sys.contains("tool_calls"), "应给出约定的 JSON 格式示例");
    }

    #[test]
    fn tool_calls_and_tool_result_history_are_preserved() {
        let (_, conv) = extract_prompt_from_messages(
            &msgs(json!([
                {"role": "user", "content": "几点"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "get_time", "arguments": "{\"city\":\"sz\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "00:42"}
            ])),
            None,
        );
        assert!(conv.contains("Assistant: [TOOL_CALLS:"), "got: {conv}");
        assert!(conv.contains("get_time"), "got: {conv}");
        assert!(
            conv.contains("User: [TOOL_RESULT id=c1]: 00:42"),
            "got: {conv}"
        );
    }

    #[test]
    fn injected_block_roundtrips_through_the_sniffer() {
        // 注入块要求模型只回 JSON；验证「模型照做后的输出」能被
        // glm_web 的嗅探器还原成标准 tool_calls（伪工具调用闭环）。
        let tools = vec![json!({"type": "function", "function": {"name": "get_time"}})];
        let (sys, _) = extract_prompt_from_messages(
            &msgs(json!([{"role": "user", "content": "几点"}])),
            Some(&tools),
        );
        assert!(sys.contains("get_time"));

        let reply = "```json\n{\"tool_calls\":[{\"name\":\"get_time\",\"arguments\":{\"city\":\"深圳\"}}]}\n```";
        let calls = crate::modules::glm_web::try_extract_tool_calls(reply)
            .expect("应嗅探到工具调用");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].pointer("/function/name").and_then(Value::as_str),
            Some("get_time")
        );
        assert!(calls[0].get("id").is_some(), "应自动补 call_id");
    }

    #[test]
    fn plain_text_reply_is_not_mistaken_for_tool_call() {
        // 关键回归防护：普通回答绝不能被误判成工具调用
        assert!(crate::modules::glm_web::try_extract_tool_calls("深圳现在 00:42").is_none());
        assert!(crate::modules::glm_web::try_extract_tool_calls("9.9 更大。").is_none());
    }

    #[test]
    fn sniffer_handles_web_code_block_chrome_and_pretty_json() {
        // 0.1.87 实证：Grok 网页端把回复渲染成代码块，textContent 带 UI 头部文案
        // 且 JSON 是美化的多行 —— 共享嗅探器的三种策略全部失配。
        let raw = "JSON复制已复制{\n  \"tool_calls\": [\n    {\n      \"name\": \"get_time\",\n      \"arguments\": {\n        \"city\": \"深圳\"\n      }\n    }\n  ]\n}";
        assert!(
            crate::modules::glm_web::try_extract_tool_calls(raw).is_none(),
            "共享嗅探器确实认不出这个形状（这正是要修的场景）"
        );
        let (calls, span) = sniff_tool_calls(raw);
        let calls = calls.expect("包一层后应能识别");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].pointer("/function/name").and_then(Value::as_str),
            Some("get_time")
        );
        assert!(calls[0].to_string().contains("深圳"), "参数应保留: {}", calls[0]);
        assert!(span.is_some(), "应返回区间以便从正文摘除");
    }

    #[test]
    fn reminder_is_appended_at_the_end_when_tools_present() {
        // 格式约定首尾各放一次：开头是 [AVAILABLE_TOOLS]，末尾是 [REMINDER]
        let tools = vec![json!({"type": "function", "function": {"name": "get_time"}})];
        let (sys, conv) = extract_prompt_from_messages(
            &msgs(json!([{"role": "user", "content": "深圳几点"},
                         {"role": "assistant", "content": "稍等"}])),
            Some(&tools),
        );
        assert!(sys.contains("[AVAILABLE_TOOLS]"), "开头应有工具定义");
        assert!(conv.ends_with("[AVAILABLE_TOOLS]. Do not output anything else."),
            "末尾应有 REMINDER: {conv}");
    }

    #[test]
    fn strip_code_block_chrome_removes_ui_labels_only() {
        assert_eq!(strip_code_block_chrome("JSON复制已复制"), "");
        assert_eq!(strip_code_block_chrome("PythonCopy"), "");
        assert_eq!(strip_code_block_chrome("Bash已复制"), "");
        // 真正的正文要保留（含以英文单词结尾的）
        assert_eq!(strip_code_block_chrome("好的，我来查一下。"), "好的，我来查一下。");
        assert_eq!(strip_code_block_chrome("Let me check"), "Let me check");
    }

    #[test]
    fn sniffer_ignores_braces_in_ordinary_answers() {
        // 正文里有花括号但不是工具调用时，绝不能误判
        let (calls, _) = sniff_tool_calls("集合 {1, 2} 的元素个数是 2。");
        assert!(calls.is_none());
    }

    #[test]
    fn parse_xai_models_response_extracts_ids() {
        let resp = json!({
            "object": "list",
            "data": [
                {"id": "grok-4", "object": "model", "created": 1745000000},
                {"id": "grok-3", "object": "model", "created": 1735000000},
                {"id": "", "object": "model"},
                {"object": "model"},
                {"id": "grok-4-fast", "name": "Grok 4 Fast"}
            ]
        });
        let parsed = parse_xai_models_response(&resp);
        let ids: Vec<&str> = parsed.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["grok-4", "grok-3", "grok-4-fast"], "空 id / 缺 id 跳过");
        assert_eq!(parsed[2].1.as_deref(), Some("Grok 4 Fast"), "name 尽力透传");
        assert_eq!(parsed[0].1, None, "上游无 name 字段 → None");
    }

    #[test]
    fn parse_xai_models_response_dedupes_and_tolerates_garbage() {
        let resp = json!({"data": [
            {"id": "grok-4"},
            {"id": "  grok-4  "},
            {"id": "grok-9"}
        ]});
        assert_eq!(parse_xai_models_response(&resp).len(), 2, "重复 id 去重");
        assert!(parse_xai_models_response(&json!({})).is_empty(), "缺 data → 空");
        assert!(parse_xai_models_response(&json!({"data": "oops"})).is_empty(), "data 非数组 → 空");
    }

    #[test]
    fn models_payload_with_empty_registry_matches_legacy_byte_for_byte() {
        // 🔴 契约红线 §六 1：注册表为空时 models_payload 输出必须与改造前逐字节一致。
        // 用临时隔离目录保证注册表文件缺失（空表）。
        let dir = std::env::temp_dir()
            .join(format!("wb-grok-payload-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let payload = crate::modules::model_sync::with_test_store(&dir, || models_payload());
        // 2026-09-27：静态表新增 grok-4.7（置顶）与 grok-composer-2.5-fast，
        // grok-4.6 描述标注「已被上游模型目录下线」——红线改为「空注册表输出 == 当前静态表」。
        let expect = json!({
            "object": "list",
            "data": [
                {"id": "grok-4.7", "object": "model", "created": 1790400000, "owned_by": "xai", "description": "Grok 4.7 最新旗舰（Build 通道 · 支持原生工具调用；免费档无内置联网，实时信息请用 grok-4）"},
                {"id": "grok-composer-2.5-fast", "object": "model", "created": 1790300000, "owned_by": "xai", "description": "Grok Composer 2.5 Fast（Build 通道 · 极速生成）"},
                {"id": "grok-4.6", "object": "model", "created": 1780000000, "owned_by": "xai", "description": "Grok 4.6（已被上游模型目录下线，请求将回落 web 通道 Grok 4）"},
                {"id": "grok-4", "object": "model", "created": 1745000000, "owned_by": "xai", "description": "Grok 4 旗舰基座模型，推理能力全面升级"},
                {"id": "grok-4-fast", "object": "model", "created": 1745000000, "owned_by": "xai", "description": "Grok 4 高速版，速度与智能平衡"},
                {"id": "grok-4-thinking", "object": "model", "created": 1745000000, "owned_by": "xai", "description": "Grok 4 深度思考模式，完整思维链推演"},
                {"id": "grok-beta", "object": "model", "created": 1710000000, "owned_by": "xai", "description": "Grok 官方通用主力模型"},
                {"id": "grok-2", "object": "model", "created": 1720000000, "owned_by": "xai", "description": "Grok 2 标准大模型，具备高智能推理与编程表现"},
                {"id": "grok-2-mini", "object": "model", "created": 1720000000, "owned_by": "xai", "description": "Grok 2 轻量版，兼具极速首字延迟与轻快响应"},
                {"id": "grok-2-vision", "object": "model", "created": 1720000000, "owned_by": "xai", "description": "Grok 2 多模态视觉模型，支持图文理解"},
                {"id": "grok-3", "object": "model", "created": 1735000000, "owned_by": "xai", "description": "Grok 3 新一代旗舰基座模型"},
                {"id": "grok-3-mini", "object": "model", "created": 1735000000, "owned_by": "xai", "description": "Grok 3 轻量极速推理模型"},
                {"id": "grok-3-thinking", "object": "model", "created": 1735000000, "owned_by": "xai", "description": "Grok 3 深度思考模式（输出完整思维链与详细推演）"},
                {"id": "grok-chat-fast", "object": "model", "created": 1715000000, "owned_by": "xai", "description": "Grok 网页端极速通道（Fast Mode）"},
                {"id": "grok-chat-auto", "object": "model", "created": 1715000000, "owned_by": "xai", "description": "Grok 网页端自适应通道（Auto Mode）"},
                {"id": "grok-chat-expert", "object": "model", "created": 1715000000, "owned_by": "xai", "description": "Grok 网页端专家深度推理通道（Expert Mode）"}
            ]
        });
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            serde_json::to_string(&expect).unwrap(),
            "空注册表输出必须逐字节等于改造前静态表"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn models_payload_appends_registry_entries_deduped() {
        // 注册表条目追加到尾部；已存在 id 不重复。
        let dir = std::env::temp_dir()
            .join(format!("wb-grok-payload-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::modules::model_sync::with_test_store(&dir, || {
            let _ = crate::modules::model_sync::add_registry_entry(
                "grok", "grok-9", Some("Grok 9".into()), "sync", None,
            );
            let _ = crate::modules::model_sync::add_registry_entry(
                "grok", "grok-4", Some("重复应被跳过".into()), "sync", None,
            );
            let payload = models_payload();
            let data = payload.get("data").and_then(Value::as_array).unwrap();
            assert_eq!(data.len(), 17, "16 静态（含 grok-4.7/composer）+ 1 注册（grok-4 已存在不重复）");
            assert_eq!(data[16]["id"], "grok-9");
            assert_eq!(data[16]["owned_by"], "xai");
            assert_eq!(data[16]["description"], "Grok 9");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-27 grok2api 桥接：配置解析与模型匹配（Build 通道新旗舰 grok-4.7）。
    #[test]
    fn g2a_proxy_config_parsing_and_model_match() {
        // 完整配置：enabled + 三要素齐全
        let full = g2a_proxy_config_from_value(&json!({
            "enabled": true,
            "base_url": "http://127.0.0.1:8002/",
            "api_key": " g2a_abc ",
            "models": ["grok-4.7", "grok-composer-2.5-fast"]
        }));
        let cfg = full.expect("完整配置应解析成功");
        assert_eq!(cfg.base_url, "http://127.0.0.1:8002"); // 尾斜杠被剪
        assert_eq!(cfg.api_key, "g2a_abc"); // 空白被剪
        assert!(g2a_handles(&cfg, "grok-4.7"));
        assert!(!g2a_handles(&cfg, "grok-4"));
        // 归一化匹配：大小写/连字符/空格写法不一也能路由
        assert!(g2a_handles(&cfg, "Grok4.7"));
        assert!(g2a_handles(&cfg, "GROK-4.7"));
        assert!(g2a_handles(&cfg, "grok 4.7"));
        // 规范 ID 解析：归一化命中 → 上游规范 ID
        assert_eq!(canonical_g2a_model(&cfg, "Grok4.7"), "grok-4.7");
        assert_eq!(canonical_g2a_model(&cfg, "grok-4.7"), "grok-4.7");
        assert_eq!(canonical_g2a_model(&cfg, "totally-fake"), "totally-fake");
        // disabled / 缺字段 / 空 models → 桥接关闭
        assert!(g2a_proxy_config_from_value(&json!({"enabled": false, "base_url": "http://x", "api_key": "k", "models": ["m"]})).is_none());
        assert!(g2a_proxy_config_from_value(&json!({"enabled": true, "api_key": "k", "models": ["m"]})).is_none());
        assert!(g2a_proxy_config_from_value(&json!({"enabled": true, "base_url": "http://x", "models": ["m"]})).is_none());
        assert!(g2a_proxy_config_from_value(&json!({"enabled": true, "base_url": "http://x", "api_key": "k"})).is_none());
        // 1.0.5：缺省思考预算未配置时取默认值；显式 0 表示关闭注入
        assert_eq!(cfg.default_thinking_budget_tokens, DEFAULT_THINKING_BUDGET_TOKENS);
        let off = g2a_proxy_config_from_value(&json!({
            "enabled": true, "base_url": "http://x", "api_key": "k", "models": ["m"],
            "default_thinking_budget_tokens": 0
        }))
        .expect("显式 0 仍是有效配置");
        assert_eq!(off.default_thinking_budget_tokens, 0);
    }

    /// 1.0.5：缺省思考预算的边界（Anthropic 要求 1024 <= budget < max_tokens）。
    #[test]
    fn default_thinking_budget_respects_anthropic_constraints() {
        assert_eq!(default_thinking_budget(8000, 4000), Some(4000)); // 正常：取配置值
        assert_eq!(default_thinking_budget(2000, 4000), Some(1488)); // 自适应收缩（留 512 给正文）
        assert_eq!(default_thinking_budget(1600, 4000), Some(1088)); //
        assert_eq!(default_thinking_budget(1536, 4000), Some(1024)); // 下限 1024
        assert_eq!(default_thinking_budget(1024, 4000), None); // 放不下 → 不注入
        assert_eq!(default_thinking_budget(0, 4000), None); // 无 max_tokens → 不注入
        assert_eq!(default_thinking_budget(8000, 0), None); // 配置为 0 = 关闭
        // 不变量：任何 Some 都必须满足 1024 <= budget < max_tokens
        for mt in [1100u64, 1536, 2000, 4096, 8192, 20000] {
            if let Some(b) = default_thinking_budget(mt, 4000) {
                assert!((1024..mt).contains(&b), "mt={mt} budget={b}");
            }
        }
    }

    /// 1.0.5：请求体预处理 —— 缺 thinking 时注入；客户端显式传入不被覆盖；
    /// 逃生开关与 model 归一化同时生效。
    #[test]
    fn prepare_messages_body_injects_thinking_without_overriding_client() {
        let cfg = g2a_proxy_config_from_value(&json!({
            "enabled": true, "base_url": "http://x", "api_key": "k",
            "models": ["grok-4.7"], "default_thinking_budget_tokens": 4000
        }))
        .unwrap();

        // ① 未带 thinking + max_tokens 足够 → 注入，且 model 已归一化
        let out = prepare_messages_body(
            &cfg,
            br#"{"model":"Grok4.7","max_tokens":8000,"messages":[]}"#,
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "grok-4.7");
        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(v["thinking"]["budget_tokens"], 4000);

        // ② 客户端显式传 thinking → 原样保留（客户端优先）
        let out = prepare_messages_body(
            &cfg,
            br#"{"model":"grok-4.7","max_tokens":8000,"thinking":{"type":"enabled","budget_tokens":1234},"messages":[]}"#,
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["thinking"]["budget_tokens"], 1234);

        // ③ 逃生开关 → 不注入
        let out = prepare_messages_body(
            &cfg,
            br#"{"model":"grok-4.7","max_tokens":8000,"messages":[]}"#,
            true,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("thinking").is_none(), "逃生开关应关闭注入");

        // ④ max_tokens 太小 → 不注入（避免上游 400）
        let out = prepare_messages_body(
            &cfg,
            br#"{"model":"grok-4.7","max_tokens":512,"messages":[]}"#,
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("thinking").is_none());

        // ⑤ 非 JSON / 无 model → 原样返回（不猜测性改造）
        let raw = b"not-json-at-all";
        assert_eq!(prepare_messages_body(&cfg, raw, false), raw.to_vec());
    }

    /// 2.0.48：effort 档位归一化 —— 中文档位名是上游 400 的直接来源
    /// （ZCode 实测传 `output_config.effort="最高"`）。
    #[test]
    fn effort_is_normalized_to_upstream_tiers() {
        let cfg = g2a_proxy_config_from_value(&json!({
            "enabled": true, "base_url": "http://x", "api_key": "k",
            "models": ["grok-4.7"], "default_thinking_budget_tokens": 4000
        }))
        .unwrap();

        // ① 中文「最高」→ max（回归用例：ZCode TraceID abb916e3 的 400）
        let out = prepare_messages_body(
            &cfg,
            r#"{"model":"grok-4.7","max_tokens":8000,"output_config":{"effort":"最高"},"messages":[]}"#.as_bytes(),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["output_config"]["effort"], "max");

        // ② 其余中文档位与常见别名
        for (input, want) in [
            ("高", "high"),
            ("中", "medium"),
            ("低", "low"),
            ("最低", "minimal"),
            ("ultra", "max"),
            ("XHIGH", "xhigh"),
            (" minimal ", "minimal"),
        ] {
            let body = format!(
                r#"{{"model":"grok-4.7","max_tokens":8000,"output_config":{{"effort":"{input}"}},"messages":[]}}"#
            );
            let out = prepare_messages_body(&cfg, body.as_bytes(), false);
            let v: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(v["output_config"]["effort"], want, "input={input}");
        }

        // ③ thinking.effort 同一处校验，也要归一
        let out = prepare_messages_body(
            &cfg,
            r#"{"model":"grok-4.7","max_tokens":8000,"thinking":{"type":"enabled","budget_tokens":4000,"effort":"最高"},"messages":[]}"#.as_bytes(),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["thinking"]["effort"], "max");
        assert_eq!(v["thinking"]["budget_tokens"], 4000, "只改档位，不动预算");

        // ④ 无法识别 → 剥离该字段（不让整通请求 400）
        let out = prepare_messages_body(
            &cfg,
            r#"{"model":"grok-4.7","max_tokens":8000,"output_config":{"effort":"爆表"},"messages":[]}"#.as_bytes(),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("output_config").is_none(), "空 output_config 应整块摘掉");

        // ⑤ 剥离 effort 但保留 output_config 里其它字段（如 format）
        let out = prepare_messages_body(
            &cfg,
            r#"{"model":"grok-4.7","max_tokens":8000,"output_config":{"effort":"爆表","format":{"type":"json_schema","schema":{"type":"object"}}},"messages":[]}"#.as_bytes(),
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v["output_config"].get("effort").is_none());
        assert_eq!(v["output_config"]["format"]["type"], "json_schema");

        // ⑥ 合法英文原样不动（幂等）
        let out = prepare_messages_body(
            &cfg,
            br#"{"model":"grok-4.7","max_tokens":8000,"output_config":{"effort":"high"},"messages":[]}"#,
            false,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["output_config"]["effort"], "high");
    }

    /// 1.0.5：逃生开关取值（大小写/空白不敏感）。
    #[test]
    fn thinking_disabled_header_variants() {
        for yes in [Some("off"), Some("OFF"), Some(" off "), Some("disable"), Some("disabled"), Some("none"), Some("0")] {
            assert!(thinking_disabled_from_header(yes), "{yes:?} 应关闭注入");
        }
        for no in [None, Some("on"), Some("auto"), Some("enabled"), Some("")] {
            assert!(!thinking_disabled_from_header(no), "{no:?} 应保持注入");
        }
    }

    /// 1.1.0 工具协议开关取值（与 thinking 逃生头同口径）。
    #[test]
    fn tool_protocol_disabled_header_variants() {
        for yes in [Some("off"), Some("OFF"), Some(" off "), Some("0")] {
            assert!(tool_protocol_disabled_from_header(yes), "{yes:?} 应关闭协议");
        }
        for no in [None, Some("on"), Some("enabled"), Some("")] {
            assert!(!tool_protocol_disabled_from_header(no), "{no:?} 应保持协议");
        }
    }

    /// 1.1.0 工具协议配置：未配置默认开启；显式 false 关闭。
    #[test]
    fn tool_protocol_config_defaults() {
        let on = g2a_proxy_config_from_value(
            &json!({"enabled": true, "base_url": "http://x", "api_key": "k", "models": ["m"]}),
        )
        .unwrap();
        assert!(on.tool_protocol, "未配置应默认开启");
        let off = g2a_proxy_config_from_value(&json!(
            {"enabled": true, "base_url": "http://x", "api_key": "k", "models": ["m"], "tool_protocol": false}
        ))
        .unwrap();
        assert!(!off.tool_protocol, "显式 false 应关闭");
    }

    /// 1.1.0：tools 非空 → system 追加协议块 + 调用链摘要（字符串 system 追加，
    /// 原文保留）；无 tools / 开关关闭 → 原样返回。
    #[test]
    fn augment_messages_tools_injects_protocol_and_chain() {
        let cfg = g2a_proxy_config_from_value(
            &json!({"enabled": true, "base_url": "http://x", "api_key": "k", "models": ["grok-4.7"]}),
        )
        .unwrap();
        let body = r#"{
            "model":"grok-4.7","max_tokens":8000,
            "system":"你是助手。",
            "tools":[{"name":"Bash","description":"run","input_schema":{}}],
            "messages":[
                {"role":"user","content":"改代码"},
                {"role":"assistant","content":[
                    {"type":"text","text":"【规划】读文件"},
                    {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.rs"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t1","content":"fn main(){}"}
                ]},
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"a.rs"}}
                ]},
                {"role":"user","content":"继续"}
            ]
        }"#
        .as_bytes();
        let out = augment_messages_tools(&cfg, body, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_str().unwrap();
        assert!(sys.starts_with("你是助手。"), "客户端 system 原文必须保留在最前");
        assert!(sys.contains("【工具调用协议"), "应注入协议块");
        assert!(sys.contains("【已完成调用链】"), "应注入链摘要");
        assert!(sys.contains("1. Read(\"a.rs\"") || sys.contains("1. Read({"), "链里应有第 1 步 Read");
        assert!(sys.contains("2. Edit("), "链里应有第 2 步 Edit");
        assert!(sys.contains("→ ok("), "t1 已有结果应标 ok");
        assert!(sys.contains("⚠无结果"), "t2 悬空应标 ⚠");
        // 客户端 messages 不被改动
        assert_eq!(v["messages"].as_array().unwrap().len(), 5);
        // 位置性提醒贴在最后一条 user 消息上
        let last = v["messages"].as_array().unwrap().last().unwrap();
        assert!(
            last["content"].as_str().unwrap().contains("系统提醒"),
            "最后一条 user 消息应携带位置性提醒: {last}"
        );

        // 无 tools → 原样返回
        let no_tools = br#"{"model":"grok-4.7","max_tokens":8000,"system":"s","messages":[]}"#;
        assert_eq!(augment_messages_tools(&cfg, no_tools, false), no_tools.to_vec());

        // 逃生开关 → 原样返回
        assert_eq!(augment_messages_tools(&cfg, body, true), body.to_vec());

        // tool_protocol=false → 原样返回
        let cfg_off = g2a_proxy_config_from_value(&json!(
            {"enabled": true, "base_url": "http://x", "api_key": "k", "models": ["m"], "tool_protocol": false}
        ))
        .unwrap();
        assert_eq!(augment_messages_tools(&cfg_off, body, false), body.to_vec());
    }

    /// 1.1.0：system 为块数组形态时追加 text 块而非拼接字符串。
    #[test]
    fn augment_messages_tools_handles_block_system() {
        let cfg = g2a_proxy_config_from_value(
            &json!({"enabled": true, "base_url": "http://x", "api_key": "k", "models": ["m"]}),
        )
        .unwrap();
        let body = br#"{
            "model":"grok-4.7","max_tokens":8000,
            "system":[{"type":"text","text":"base","cache_control":{"type":"ephemeral"}}],
            "tools":[{"name":"Bash","input_schema":{}}],
            "messages":[{"role":"user","content":"hi"}]
        }"#;
        let out = augment_messages_tools(&cfg, body, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let arr = v["system"].as_array().unwrap();
        assert_eq!(arr[0]["text"], "base", "原块必须原样保留");
        assert_eq!(arr.len(), 2, "只追加一个块");
        assert!(arr[1]["text"].as_str().unwrap().contains("【工具调用协议"));
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral", "原块字段不丢");
    }

    /// 1.1.0：重复调用与错误结果都会被标注。
    #[test]
    fn extract_tool_chain_marks_dup_and_error() {
        let msgs = json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Bash","input":{"cmd":"ls"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"file1","is_error":false}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"b","name":"Bash","input":{"cmd":"ls"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"b","content":"boom","is_error":true}]}
        ])
        .as_array()
        .unwrap()
        .clone();
        let summary = extract_tool_chain(&msgs).unwrap().text;
        assert!(summary.contains("⚠重复调用"), "同参重复调用应标注: {summary}");
        assert!(summary.contains("→ 错误("), "错误结果应标注: {summary}");
    }

    /// 1.2.0：收尾上下文——有工具历史才 Some；goal 取首条 user 文本。
    #[test]
    fn wrap_up_context_extracts_goal_and_chain() {
        let cfg = g2a_proxy_config_from_value(
            &json!({"enabled": true, "base_url": "http://x", "api_key": "k", "models": ["m"]}),
        )
        .unwrap();
        let body = r#"{
            "model":"grok-4.7","max_tokens":8000,
            "tools":[{"name":"Bash","input_schema":{}}],
            "messages":[
                {"role":"user","content":"修复登录 bug 并验证"},
                {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"cmd":"cargo test"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok 601"}]}
            ]
        }"#
        .as_bytes();
        let ctx = wrap_up_context(&cfg, body).unwrap();
        assert_eq!(ctx.chain.calls, 1);
        assert_eq!(ctx.chain.dangling, 0);
        assert!(ctx.goal.contains("修复登录"), "goal 应来自首条 user 消息: {}", ctx.goal);

        // 首轮（tools 有、无历史调用）→ None
        let first = r#"{"model":"grok-4.7","max_tokens":8000,"tools":[{"name":"Bash","input_schema":{}}],"messages":[{"role":"user","content":"hi"}]}"#.as_bytes();
        assert!(wrap_up_context(&cfg, first).is_none(), "首轮无需收尾复盘");

        // 无 tools → None
        let plain = br#"{"model":"grok-4.7","max_tokens":8000,"messages":[{"role":"user","content":"hi"}]}"#;
        assert!(wrap_up_context(&cfg, plain).is_none());
    }

    /// 1.2.0：确定性复盘尾块与 3B 触发条件。
    #[test]
    fn wrap_up_tail_and_review_trigger() {
        let cfg = g2a_proxy_config_from_value(
            &json!({"enabled": true, "base_url": "http://x", "api_key": "k", "models": ["m"]}),
        )
        .unwrap();
        let body = r#"{
            "model":"grok-4.7","max_tokens":8000,
            "tools":[{"name":"Bash","input_schema":{}}],
            "messages":[
                {"role":"user","content":"测试目标"},
                {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"cmd":"ls"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}
            ]
        }"#
        .as_bytes();
        let ctx = wrap_up_context(&cfg, body).unwrap();
        let tail = wrap_up_tail(&ctx);
        assert!(tail.contains("【工具链路复盘·网关生成】"));
        assert!(tail.contains("测试目标"));
        assert!(tail.contains("全部有结果"));
        assert!(!wrap_up_review_needed(&ctx, &"这是一段足够长的回答。".repeat(30)), "链完整+回答够长 → 不触发模型复盘");
        assert!(wrap_up_review_needed(&ctx, "太短"), "回答过薄 → 触发模型复盘");
    }

    /// 1.2.0：非流式响应解析与尾块追加。
    #[test]
    fn parse_and_append_response_json() {
        let resp = json!({
            "content":[
                {"type":"thinking","thinking":"内部"},
                {"type":"text","text":"最终答案"},
                {"type":"tool_use","id":"x","name":"Bash","input":{}}
            ]
        });
        let (text, has_tool) = parse_response_content(&resp.to_string()).unwrap();
        assert_eq!(text, "最终答案");
        assert!(has_tool, "tool_use 应被识别");

        let resp2 = json!({"content":[{"type":"text","text":"答案"}],"stop_reason":"end_turn"});
        let (text2, has_tool2) = parse_response_content(&resp2.to_string()).unwrap();
        assert_eq!(text2, "答案");
        assert!(!has_tool2);
        let appended = wrap_up_append_to_json(&resp2.to_string(), "【工具链路复盘】内容");
        let v: Value = serde_json::from_str(&appended).unwrap();
        let arr = v["content"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1]["type"], "text");
        assert!(arr[1]["text"].as_str().unwrap().starts_with("【工具链路复盘】"));
        assert_eq!(v["stop_reason"], "end_turn", "其余字段不丢");
        // 非 JSON 原样
        assert_eq!(wrap_up_append_to_json("not-json", "t"), "not-json");
    }

    /// 1.2.0：SSE 注入器——完整事件流、跨 chunk 切分、tool_use 抑制、注入顺序。
    #[test]
    fn sse_injector_injects_tail_before_message_stop() {
        let tail = "【工具链路复盘·网关生成】测试复盘";
        let make = |text: &str| {
            format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
                 event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
                 event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
                 event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":9}}}}\n\n\
                 event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            )
        };
        // ① 整段喂入：尾块在 message_stop 之前、index=1
        let mut inj = WrapUpStreamInjector::new(true);
        let _ = inj.feed(make("答案").as_bytes());
        assert!(inj.is_holding_stop(), "message_stop 应被扣住");
        assert!(!inj.seen_tool_use());
        let out = inj.complete_with_tail(Some(tail.to_string()));
        let s = String::from_utf8(out).unwrap();
        let stop_pos = s.find("event: message_stop").unwrap();
        let tail_pos = s.find("content_block_start").unwrap();
        assert!(tail_pos < stop_pos, "尾块事件必须先于 message_stop: {s}");
        assert!(s.contains("\"index\":1"), "尾块 index = 已见最大 index + 1");
        assert!(s.contains("event: content_block_delta"));
        assert!(s.contains(tail), "尾块文本应在 delta 事件里: {s}");
        assert!(s.contains("\"type\":\"text_delta\""), "应为 text_delta 事件: {s}");
        // 完成后再喂 → 原样透传
        let later = inj.feed(b"event: ping\ndata: {}\n\n");
        assert_eq!(later, b"event: ping\ndata: {}\n\n".to_vec());

        // ② 跨 chunk 切分（事件被从中间劈开）仍能重组
        let mut inj2 = WrapUpStreamInjector::new(true);
        let full = make("跨chunk");
        let (a, b) = full.split_at(full.len() / 2);
        let _ = inj2.feed(a.as_bytes());
        assert!(!inj2.is_holding_stop());
        let _ = inj2.feed(b.as_bytes());
        assert!(inj2.is_holding_stop(), "跨 chunk 的 message_stop 也应被扣住");
        let out2 = inj2.complete_with_tail(Some("t".into()));
        assert!(String::from_utf8(out2).unwrap().contains("content_block_start"));

        // ③ 响应里出现过 tool_use → 不注入，message_stop 直接放行
        let with_tool = format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"Bash\",\"input\":{{}}}}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let mut inj3 = WrapUpStreamInjector::new(true);
        let _ = inj3.feed(with_tool.as_bytes());
        assert!(inj3.seen_tool_use());
        let out3 = inj3.complete_with_tail(None);
        let s3 = String::from_utf8(out3).unwrap();
        assert!(s3.contains("event: message_stop"));
        assert!(!s3.contains("content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1"), "tool_use 轮不得注入尾块: {s3}");

        // ④ 尾块开关关闭 → new(false) 时 complete 直接放行
        let mut inj4 = WrapUpStreamInjector::new(false);
        let _ = inj4.feed(make("x").as_bytes());
        let out4 = inj4.complete_with_tail(None);
        assert!(!String::from_utf8(out4).unwrap().contains("content_block_delta"));
    }

    /// grok-4.7 / grok-composer-2.5-fast 必须在对外模型清单置顶（用户要求上线最新模型）。
    #[test]
    fn models_payload_lists_grok47_first() {
        let payload = models_payload();
        let ids: Vec<&str> = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert_eq!(ids[0], "grok-4.7", "grok-4.7 应置顶: {ids:?}");
        assert!(ids.contains(&"grok-composer-2.5-fast"));
        assert!(ids.contains(&"grok-4.6")); // 历史模型保留（描述标注已下线）
    }

    /// 2026-09-27 原生工具调用透传：OpenAI 流式 tool_calls 分片按 index 累积、
    /// arguments 跨 chunk 拼接（网页反代模型工具适配的核心解析逻辑）。
    #[test]
    fn merge_tool_call_fragments_accumulates_openai_stream() {
        let mut acc: Vec<Value> = Vec::new();
        let frags = json!([
            {"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": ""}},
            {"index": 0, "function": {"arguments": "{\"city\":"}},
            {"index": 0, "function": {"arguments": "\"Tokyo\"}"}},
            {"index": 1, "id": "call_2", "type": "function", "function": {"name": "get_time", "arguments": "{}"}}
        ]);
        if let Some(list) = frags.as_array() {
            for frag in list {
                merge_tool_call_fragment(&mut acc, frag);
            }
        }
        assert_eq!(acc.len(), 2);
        assert_eq!(acc[0]["id"], json!("call_1"));
        assert_eq!(acc[0]["type"], json!("function"));
        assert_eq!(acc[0]["function"]["name"], json!("get_weather"));
        assert_eq!(
            acc[0]["function"]["arguments"],
            json!(r#"{"city":"Tokyo"}"#)
        );
        assert_eq!(acc[1]["id"], json!("call_2"));
        assert_eq!(acc[1]["function"]["name"], json!("get_time"));
        assert_eq!(acc[1]["function"]["arguments"], json!("{}"));
    }
}
