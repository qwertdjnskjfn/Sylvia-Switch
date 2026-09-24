//! WorkBuddy 网关协议补全（P2：OpenAI Responses ↔ Chat、Anthropic Messages ↔ Chat 双向转换）。
//!
//! 契约：`docs/workbuddy-gateway-refactor-p2p3-contract.md` §1（唯一事实来源）；主契约
//! `docs/workbuddy-gateway-refactor-contract.md` §13。§1 钉定的 7 个入口签名一字不改：
//! responses_req_to_chat / responses_resp_from_text / responses_stream_frames /
//! anthropic_req_to_chat / anthropic_resp_from_text / anthropic_stream_frames /
//! anthropic_slot_to_model。
//! 另有一个**附加**入口 `anthropic_sniff_tool_calls`（§1 未列）：Anthropic 响应侧的
//! tool_calls 嗅探在钉定签名里没有容身之处（`anthropic_resp_from_text` 只收已嗅探的
//! calls），故单独拆出，供路由层「请求带 tools 才嗅探」时调用——只增不改。
//!
//! 路由侧接线（新增 `/workbuddy-pool/v1/responses` 与 `/workbuddy-pool/v1/messages`）
//! 由接线工程师负责，本文件只做**纯函数**转换：不碰网络、不碰磁盘、不持状态。
//!
//! # 为什么需要这一层
//!
//! - Codex CLI 0.146 起移除了 `wire_api = "chat"`，只讲 Responses 协议；
//! - Claude Code / CC Switch 只讲 Anthropic Messages 协议；
//! - 上游（copilot.tencent.com / www.workbuddy.ai）只认 OpenAI Chat Completions。
//! 因此三边必须互译：请求侧把客户端私有协议翻成内部 chat payload（出站前再过
//! `workbuddy_upstream::prepare_body`），响应侧把聚合结果还原成客户端协议的对象与
//! SSE 帧序列。
//!
//! # 帧格式两协议不同（调用方勿混用）
//!
//! - **Responses**：契约 §1 钉定「每帧一个 `data: {json}\n\n`」，`data: [DONE]\n\n` 收尾；
//! - **Anthropic**：真实客户端规范要求 `event:` 名（Claude Code 按事件名分派），
//!   故每帧 `event: <name>\ndata: {json}\n\n`，**无 `[DONE]` 哨兵**（`message_stop` 即结束）。
//! 口径分别对齐参考实现 `_workbuddy-gateway-ref/.../responses_stream.go:43` 与本仓
//! `glm_anthropic.rs:427 sse_event`。
//!
//! # 口径来源（逐条对应，防漂移）
//!
//! | 能力 | 口径 |
//! | --- | --- |
//! | Responses input/instructions/tools | 参考 `responses.go:60 responsesToChat` |
//! | Responses 非流式对象 | 参考 `responses.go:330 chatToResponses` + 契约 §1 字段表 |
//! | Responses 流式帧序 | 参考 `responses_stream.go:53 streamResponses` |
//! | Anthropic system 三形态 | `api.rs:4522 workbuddy_pool_system_text` |
//! | Anthropic blocks → messages | `glm_anthropic.rs:212-318`（含 image→image_url） |
//! | Anthropic tools 转换 | `glm_anthropic.rs:28 to_openai_tools` + 参考 `messages.go:303` |
//! | Anthropic tool_use/tool_result 历史 | `grok.rs:680-702`（历史拍平）+ 参考 `messages.go:184` |
//! | Anthropic 非流式对象 | 参考 `messages.go:370 chatToAnthropic` |
//! | Anthropic 流式帧序 | 参考 `messages_stream.go:20 streamAnthropic` |
//! | tool_calls 嗅探 | `grok.rs:552 sniff_tool_calls`（共享 `glm_web::try_extract_tool_calls`） |
//! | 槽位映射 | 参考 `messages.go:552 resolveClaudeModel` / `:570 slotFallback` |

use serde_json::{json, Value};

use crate::modules::config::now_ms;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// Claude 槽位关键词（参考 `messages.go:571` 顺序：先 opus 再 sonnet，重叠名时前者胜出）。
const ANTHROPIC_SLOTS: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

/// 流式 delta 的分片长度（字符数）。
///
/// 上游给的是**聚合后的整段文本**，这里按固定片长切出「增量」再逐帧下发：客户端
/// （Codex / Claude Code）只要求「有 delta 且顺序正确」，片长只影响逐字节奏。
/// 取 64 字符：不至于一帧塞整段（失去流式意义），也不至于把长文切出上千帧。
const STREAM_DELTA_CHUNK_CHARS: usize = 64;

/// usage 缺失时回落到 0（客户端按 0 展示，不会因 null 解析失败；口径同参考
/// `responsesUsage` / `anthropicUsage` 的 nil 分支）。
fn usage_triplet(usage: &Value) -> (i64, i64, i64) {
    if !usage.is_object() {
        return (0, 0, 0);
    }
    let input = usage_num(usage, &["prompt_tokens", "promptTokens", "input_tokens", "inputTokens"]);
    let output = usage_num(
        usage,
        &["completion_tokens", "completionTokens", "output_tokens", "outputTokens"],
    );
    // total 缺失时按「输入 + 输出」兜底，避免用量面板显示 0。
    let total = usage_num(usage, &["total_tokens", "totalTokens"]).max(input + output);
    (input, output, total)
}

/// 按候选键名读一个用量数字（兼容 OpenAI snake_case 与本仓 camelCase 两种上游形状）。
fn usage_num(usage: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        let Some(v) = usage.get(*key) else { continue };
        if let Some(n) = v
            .as_i64()
            .or_else(|| v.as_u64().map(|u| u as i64))
            .or_else(|| v.as_f64().map(|f| f as i64))
        {
            return n;
        }
    }
    0
}

/// 响应对象 id：Responses 为 `resp_` + UUID，Anthropic 为 `msg_` + UUID。
///
/// 为什么必须每响应唯一：Claude 客户端把 `message.id` 当消息身份键（历史重建 /
/// 去重 / 上下文压缩都按它对齐），恒定 id 会让不同轮次的响应被误并为同一条，
/// tool_use / tool_result 随即错配（口径同参考 `messages.go:354 newMessageID`）。
fn new_response_id() -> String {
    format!("resp_{}", uuid::Uuid::new_v4().simple())
}

fn new_message_id() -> String {
    format!("msg_{}", uuid::Uuid::new_v4().simple())
}

/// 把文本按字符切成定长分片（按 char 而非 byte，中文多字节不会切坏码点）。
fn chunk_text(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        buf.push(ch);
        count += 1;
        if count >= STREAM_DELTA_CHUNK_CHARS {
            out.push(std::mem::take(&mut buf));
            count = 0;
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Responses SSE 帧：契约 §1 钉定「每帧一个 `data: {json}\n\n`」（不带 event 名）。
fn responses_frame(payload: &Value) -> String {
    format!("data: {payload}\n\n")
}

/// Anthropic SSE 帧：真实客户端规范要求 event 名（口径同 `glm_anthropic.rs:427`）。
fn anthropic_frame(event: &str, payload: &Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

/// 推一帧 Responses 事件并自增 `sequence_number`。
///
/// 序号递收取在一处：手写 10 个帧号容易错号，而 Codex 对序号单调性有校验。
fn push_responses_frame(frames: &mut Vec<String>, seq: &mut usize, payload: Value) {
    let n = *seq as i64;
    *seq += 1;
    let mut payload = payload;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("sequence_number".to_string(), json!(n));
    }
    frames.push(responses_frame(&payload));
}

// ---------------------------------------------------------------------------
// OpenAI Responses API（Codex CLI wire_api="responses"）
// ---------------------------------------------------------------------------

/// Responses 请求 → 内部 chat payload。
///
/// `input`（string 或 [{role,content}] 数组）→ messages；`instructions` → system message；
/// 缺失 input 时用 messages 兼容字段；tools 原样透传；stream 透传。
pub fn responses_req_to_chat(body: &Value) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // instructions → 首条 system（空白串不插：给上游塞无意义 system 反而干扰判定）。
    if let Some(text) = body.get("instructions").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    match body.get("input") {
        // 纯字符串 input 直接当 user 消息（口径同参考 responses.go:141）。
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                responses_input_item(&mut messages, item);
            }
        }
        // input 缺失（含 null）：退回 messages 兼容字段 —— 自研客户端/代理会直接带
        // OpenAI 形状，少了这条会把整段上下文丢掉。
        _ => {
            if let Some(compat) = body.get("messages").and_then(Value::as_array) {
                messages.extend(compat.iter().cloned());
            }
        }
    }

    let mut payload = serde_json::Map::new();
    // model 只在请求带了非空值时才透传：槽位/别名映射由调用方在出站前覆写
    // （Anthropic 侧尤其如此，见 anthropic_slot_to_model），本函数不改写模型名。
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        if !model.is_empty() {
            payload.insert("model".to_string(), json!(model));
        }
    }
    payload.insert("messages".to_string(), Value::Array(messages));
    payload.insert(
        "stream".to_string(),
        json!(body.get("stream").and_then(Value::as_bool).unwrap_or(false)),
    );
    // tools 原样透传（契约 §1 钉定）：Codex 发的就是 function 工具声明，
    // 归一化/裁剪交给出站前的 prepare_body。
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        if !tools.is_empty() {
            payload.insert("tools".to_string(), Value::Array(tools.clone()));
        }
    }
    Value::Object(payload)
}

/// input 数组的单项分派（口径同参考 `responses.go:155 responsesInputToMessages`）。
fn responses_input_item(out: &mut Vec<Value>, item: &Value) {
    // 缺 type 按 message 兜底：部分客户端省略该字段。
    match item.get("type").and_then(Value::as_str).unwrap_or("message") {
        "message" | "" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            out.push(json!({"role": role, "content": responses_content_to_chat(item.get("content"))}));
        }
        // Codex 的工具历史：function_call → assistant.tool_calls。
        "function_call" => {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("");
            out.push(json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }],
            }));
        }
        // function_call_output → role:"tool"（必须紧跟 assistant 的 tool_calls）。
        "function_call_output" => {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let content = responses_content_to_chat(item.get("output"));
            out.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
        }
        // 推理条目不回转：上游不接受该形状，且内容已体现在后续 assistant 消息里
        // （口径同参考 responses.go:165）。
        "reasoning" => {}
        // 未知条目（web_search_call / local_shell_call 等 Codex 私有扩展）：
        // 上游不认识，丢弃而不报错——丢一个搜索记录远好过整请求 400。
        _ => {}
    }
}

/// Responses 的 content（字符串 / input_text 分片数组 / input_image 分片数组）→ Chat content。
///
/// 纯文本时拍平成字符串（上游最稳）；带图片时才保留分片数组。
/// 口径同参考 `responses.go:194 responsesContentToChat`。
fn responses_content_to_chat(content: Option<&Value>) -> Value {
    match content {
        None | Some(Value::Null) => Value::String(String::new()),
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            let mut pieces: Vec<Value> = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    // "" / None：客户端省略 type 时按文本分片处理（参考同款兜底）。
                    Some("input_text") | Some("output_text") | Some("text") | Some("") | None => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            text.push_str(t);
                            pieces.push(json!({"type": "text", "text": t}));
                        }
                    }
                    // 图片分片 → image_url 结构转换（能否真用取决于上游模型，见契约 §6）
                    Some("input_image") => {
                        if let Some(url) = part.get("image_url").and_then(Value::as_str) {
                            if !url.is_empty() {
                                pieces.push(json!({"type": "image_url", "image_url": {"url": url}}));
                            }
                        }
                    }
                    _ => {}
                }
            }
            if pieces
                .iter()
                .any(|p| p.get("type").and_then(Value::as_str) == Some("image_url"))
            {
                Value::Array(pieces)
            } else {
                Value::String(text)
            }
        }
        Some(other) => other.clone(),
    }
}

/// 聚合结果 → Responses 非流式对象：
/// {id:"resp_...", object:"response", created_at, model, status:"completed",
///  output:[{type:"message", role:"assistant", content:[{type:"output_text", text}]}],
///  usage:{input_tokens, output_tokens, total_tokens}}
///
/// `reasoning` 不单独出项（契约 §1 未列该字段）：上游只回推理内容时降级为正文，
/// 避免空 output（口径同 `glm_anthropic.rs:363`）。
/// usage 无入参（签名钉定），按未知给全 0（口径同参考 `responsesUsage` 的 nil 分支）。
/// `usage` 为上游聚合用量（OpenAI usage 对象或 Null； Null → 全 0）。
pub fn responses_resp_from_text(
    model: &str,
    content: &str,
    reasoning: &str,
    usage: &Value,
) -> Value {
    let text = if content.is_empty() && !reasoning.is_empty() {
        reasoning
    } else {
        content
    };
    let id = new_response_id();
    json!({
        "id": id,
        "object": "response",
        "created_at": now_ms() / 1000,
        "model": model,
        "status": "completed",
        "output": [responses_message_item(&format!("msg_{id}"), "completed", Some(text))],
        "usage": {
            "input_tokens": usage_triplet(usage).0,
            "output_tokens": usage_triplet(usage).1,
            "total_tokens": usage_triplet(usage).2,
        },
    })
}

/// Responses 流式事件帧序列（每帧一个 `data: {json}\n\n`，Codex CLI 兼容最小集，顺序固定）：
/// response.created → response.in_progress → response.output_item.added(message) →
/// response.content_part.added(output_text) → response.output_text.delta(可多次,
/// 携带 upstream 聚合增量) → response.output_text.done → response.content_part.done →
/// response.output_item.done → response.completed(带 usage) → data: [DONE]
pub fn responses_stream_frames(model: &str, content: &str, usage: &Value) -> Vec<String> {
    let id = new_response_id();
    let item_id = format!("msg_{id}");
    let created_at = now_ms() / 1000;
    let (input_tokens, output_tokens, total_tokens) = usage_triplet(usage);
    let mut seq: usize = 0;
    let mut frames: Vec<String> = Vec::new();

    // snapshot：created / in_progress / completed 三态共用的响应对象外壳。
    // usage 必须出现（全 0 而不是省略字段）：Codex 恒以 stream:true 调用，
    // 只在 completed 里读用量，缺失会被解析成 null 报错（参考 responses_stream.go:162）。
    let snapshot = |status: &str, output: Vec<Value>| {
        json!({
            "id": id,
            "object": "response",
            "created_at": created_at,
            "status": status,
            "model": model,
            "output": output,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": total_tokens,
            },
        })
    };

    // 1) created + in_progress：Codex 以这两个事件建立流；缺 created 会报
    //    "stream closed before response.completed"（参考 responses_stream.go:5-8）。
    push_responses_frame(
        &mut frames,
        &mut seq,
        json!({"type": "response.created", "response": snapshot("in_progress", vec![])}),
    );
    push_responses_frame(
        &mut frames,
        &mut seq,
        json!({"type": "response.in_progress", "response": snapshot("in_progress", vec![])}),
    );

    // 2) 输出项与内容分片声明：item 的 content 先为空数组，正文走 delta 逐片到达。
    push_responses_frame(
        &mut frames,
        &mut seq,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": responses_message_item(&item_id, "in_progress", None),
        }),
    );
    push_responses_frame(
        &mut frames,
        &mut seq,
        json!({
            "type": "response.content_part.added",
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []},
        }),
    );

    // 3) delta：聚合文本按定长切分逐片下发（可多次；空正文时一帧都不发）。
    for chunk in chunk_text(content) {
        push_responses_frame(
            &mut frames,
            &mut seq,
            json!({
                "type": "response.output_text.delta",
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "delta": chunk,
            }),
        );
    }

    // 4) 收尾四连：text 完成 → content_part 完成 → item 完成（Codex 0.146 只在
    //    output_item.done 时把输出收进会话状态，缺它 last_agent_message 为空，
    //    参考 responses_stream.go:103-106）→ completed 带完整 output 与 usage。
    push_responses_frame(
        &mut frames,
        &mut seq,
        json!({
            "type": "response.output_text.done",
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "text": content,
        }),
    );
    push_responses_frame(
        &mut frames,
        &mut seq,
        json!({
            "type": "response.content_part.done",
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "part": {"type": "output_text", "text": content, "annotations": []},
        }),
    );
    push_responses_frame(
        &mut frames,
        &mut seq,
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": responses_message_item(&item_id, "completed", Some(content)),
        }),
    );
    push_responses_frame(
        &mut frames,
        &mut seq,
        json!({
            "type": "response.completed",
            "response": snapshot("completed", vec![responses_message_item(&item_id, "completed", Some(content))]),
        }),
    );

    // 5) [DONE] 哨兵（契约 §1 钉定）。
    frames.push("data: [DONE]\n\n".to_string());
    frames
}

/// Responses message 输出项（text 为 None 时 content 为空数组，用于 added 帧）。
fn responses_message_item(item_id: &str, status: &str, text: Option<&str>) -> Value {
    let content: Vec<Value> = match text {
        Some(t) => vec![json!({"type": "output_text", "text": t, "annotations": []})],
        None => Vec::new(),
    };
    json!({
        "id": item_id,
        "type": "message",
        "role": "assistant",
        "status": status,
        "content": content,
    })
}

// ---------------------------------------------------------------------------
// Anthropic Messages API（Claude Code / CC Switch）
// ---------------------------------------------------------------------------

/// Anthropic 请求 → 内部 chat payload：
/// system（string 或 [{type:"text",text}]）→ 首条 system message；
/// messages[].content（string 或 blocks）→ OpenAI content（text block 提文本，
/// image block 转 image_url，tool_use → assistant.tool_calls，tool_result → role:"tool"）；
/// tools（[{name, description, input_schema}]）→ OpenAI tools（input_schema→parameters）；
/// max_tokens/stop_sequences 透传（stop_sequences→stop）。
pub fn anthropic_req_to_chat(body: &Value) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // system 三形态：string / blocks 数组 / 缺失（第三态不插 system）。
    match body.get("system") {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            messages.push(json!({"role": "system", "content": text}));
        }
        Some(Value::Array(blocks)) => {
            let text = anthropic_system_text(blocks);
            if !text.is_empty() {
                messages.push(json!({"role": "system", "content": text}));
            }
        }
        _ => {}
    }

    if let Some(items) = body.get("messages").and_then(Value::as_array) {
        for message in items {
            anthropic_message_to_chat(&mut messages, message);
        }
    }

    let mut payload = serde_json::Map::new();
    // model 同 Responses：只透传原始值，槽位映射由调用方出站前覆写。
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        if !model.is_empty() {
            payload.insert("model".to_string(), json!(model));
        }
    }
    payload.insert("messages".to_string(), Value::Array(messages));
    payload.insert(
        "stream".to_string(),
        json!(body.get("stream").and_then(Value::as_bool).unwrap_or(false)),
    );
    // max_tokens / stop_sequences 透传（Anthropic 的 max_tokens 必填，语义与 OpenAI 一致）。
    if let Some(max_tokens) = body.get("max_tokens") {
        if max_tokens.is_number() {
            payload.insert("max_tokens".to_string(), max_tokens.clone());
        }
    }
    if let Some(stops) = body.get("stop_sequences").and_then(Value::as_array) {
        if !stops.is_empty() {
            payload.insert("stop".to_string(), Value::Array(stops.clone()));
        }
    }
    if let Some(tools) = anthropic_tools_to_chat(body) {
        payload.insert("tools".to_string(), tools);
    }
    Value::Object(payload)
}

/// system blocks → 纯文本（只取 text 块；口径同 `api.rs:4522 workbuddy_pool_system_text`）。
fn anthropic_system_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 单条 Anthropic 消息 → OpenAI messages（可能产出多条：tool_result 独立成 role:"tool"）。
///
/// 历史拍平口径仿 `grok.rs:680-702`：assistant 历史里的 tool_calls 与 role:"tool"
/// 回灌都要能过，不能因为是「历史轮」就丢掉或把顺序打乱。
fn anthropic_message_to_chat(out: &mut Vec<Value>, message: &Value) {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
    match message.get("content") {
        Some(Value::String(text)) => out.push(json!({"role": role, "content": text})),
        Some(Value::Array(blocks)) => anthropic_blocks_to_chat(out, role, blocks),
        // content 缺失 / null：不产消息（上游不接受空 content 的消息）。
        _ => {}
    }
}

/// content block 数组 → OpenAI 消息序列。
///
/// 顺序是硬约束：tool_result 转成的 role:"tool" 必须排在 assistant.tool_calls
/// **之后**、user 正文**之前**；插到最前会形成 assistant → user → tool 的断裂序列，
/// 上游按 tool_call_sequence_broken 拒绝（口径同参考 `messages.go:247-258` 与
/// `glm_anthropic.rs:299`）。
fn anthropic_blocks_to_chat(out: &mut Vec<Value>, role: &str, blocks: &[Value]) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut image_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_messages: Vec<Value> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text_parts.push(text.to_string());
                    }
                }
            }
            Some("image") => {
                // image block → image_url 结构转换（复用 glm_anthropic 的同一口径）。
                if let Some(part) = crate::modules::glm_anthropic::openai_image_part(block) {
                    image_parts.push(part);
                }
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                // 缺 id/name 的块无法回灌成 tool_calls（上游按 id 配对），跳过。
                if id.is_empty() || name.is_empty() {
                    continue;
                }
                let arguments = block
                    .get("input")
                    .filter(|v| v.is_object())
                    .and_then(|v| serde_json::to_string(v).ok())
                    .unwrap_or_else(|| "{}".to_string());
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }));
            }
            Some("tool_result") => {
                let tool_use_id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                if tool_use_id.is_empty() {
                    continue;
                }
                tool_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": anthropic_tool_result_text(block),
                }));
            }
            // thinking / redacted_thinking 及未知块：上游不接受该形状，丢弃。
            _ => {}
        }
    }

    // tool_result 消息先入队（排在 assistant.tool_calls 之后）。
    out.append(&mut tool_messages);

    if !tool_calls.is_empty() {
        // assistant 的 tool_use：多块时聚成一条 assistant + tool_calls
        // （口径同 glm_anthropic.rs:212 assistant_message_from_blocks）。
        let mut message = serde_json::Map::new();
        message.insert("role".to_string(), json!("assistant"));
        if !text_parts.is_empty() {
            message.insert("content".to_string(), json!(text_parts.join("\n")));
        }
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
        out.push(Value::Object(message));
        return;
    }

    if !image_parts.is_empty() {
        // 有图片：文本在上、图片在下（多数多模态模型的阅读顺序，口径同 glm_anthropic.rs:303）。
        let mut content: Vec<Value> = text_parts
            .iter()
            .map(|t| json!({"type": "text", "text": t}))
            .collect();
        content.extend(image_parts);
        out.push(json!({"role": role, "content": content}));
    } else if !text_parts.is_empty() {
        out.push(json!({"role": role, "content": text_parts.join("\n")}));
    }
    // 只有图片且图片转换失败 / 只有 thinking 块：不产消息（上游不接受空 content）。
}

/// tool_result 的 content 拍平（字符串或 block 数组；口径同 `glm_anthropic.rs:104`）。
fn anthropic_tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(inner)) => inner
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Anthropic tools → OpenAI function tools（input_schema→parameters）。
///
/// 口径同 `glm_anthropic.rs:28 to_openai_tools` 与参考 `messages.go:303`：
/// 缺 input_schema 时给空对象 schema，否则上游会因 parameters 缺失 400。
fn anthropic_tools_to_chat(body: &Value) -> Option<Value> {
    let tools = body.get("tools").and_then(Value::as_array)?;
    let mapped: Vec<Value> = tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?.trim();
            if name.is_empty() {
                return None;
            }
            let mut function = serde_json::Map::new();
            function.insert("name".to_string(), json!(name));
            if let Some(description) = tool.get("description").and_then(Value::as_str) {
                if !description.is_empty() {
                    function.insert("description".to_string(), json!(description));
                }
            }
            let parameters = tool
                .get("input_schema")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            function.insert("parameters".to_string(), parameters);
            Some(json!({"type": "function", "function": Value::Object(function)}))
        })
        .collect();
    if mapped.is_empty() {
        None
    } else {
        Some(Value::Array(mapped))
    }
}

/// 聚合结果 → Anthropic message：
/// {id:"msg_...", type:"message", role:"assistant", model,
///  content:[{type:"text", text}], stop_reason:"end_turn"|"tool_use",
///  usage:{input_tokens, output_tokens}}
/// 嗅探到 tool_calls 时 content 追加 {type:"tool_use", id, name, input} 块。
///
/// `tool_calls` 是 OpenAI 形状（`{id, type:"function", function:{name, arguments}}`），
/// 由调用方经 [`anthropic_sniff_tool_calls`] 嗅探后传入。usage 无入参（签名钉定），
/// 按未知给 0（口径同参考 `anthropicUsage` 的 nil 分支）。
/// `usage`：上游聚合用量（Null → 全 0）。
pub fn anthropic_resp_from_text(
    model: &str,
    content: &str,
    tool_calls: &[Value],
    usage: &Value,
) -> Value {
    let mut blocks: Vec<Value> = Vec::new();
    if !content.is_empty() {
        blocks.push(json!({"type": "text", "text": content}));
    }
    for call in tool_calls {
        let name = match call.pointer("/function/name").and_then(Value::as_str) {
            Some(n) if !n.is_empty() => n,
            // 缺 name 的调用无法让客户端执行，跳过而不是塞个空名块。
            _ => continue,
        };
        // arguments 是 JSON 字符串，Anthropic 的 input 要对象：解析失败回落空对象
        // （口径同 glm_anthropic.rs:390）。
        let input = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
            "name": name,
            "input": input,
        }));
    }
    let stop_reason = if blocks
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        "tool_use"
    } else {
        "end_turn"
    };
    json!({
        "id": new_message_id(),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": blocks,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage_triplet(usage).0,
            "output_tokens": usage_triplet(usage).1,
        },
    })
}

/// 流式事件序列：message_start → content_block_start(text) →
/// content_block_delta(text_delta 可多次) → content_block_stop → message_delta
/// (stop_reason/usage) → message_stop。
///
/// 每帧带 `event:` 名（真实客户端规范，见模块头「帧格式」）。工具调用不在本函数
/// 覆盖范围（签名无 tool_calls 入参）：流式 tool_use 帧留待后续契约增补。
pub fn anthropic_stream_frames(model: &str, content: &str, usage: &Value) -> Vec<String> {
    let (input_tokens, output_tokens, _) = usage_triplet(usage);
    let mut frames: Vec<String> = Vec::new();

    // message_start：必须在任何内容块之前，Claude 据此建立消息。
    // 此处 usage 给 0（上游聚合用量还没到手），真实值放 message_delta
    // （口径同参考 messages_stream.go:38 + glm_anthropic.rs:568）。
    frames.push(anthropic_frame(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": new_message_id(),
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            },
        }),
    ));
    // 文本块下标固定 0：本函数只产文本块。
    frames.push(anthropic_frame(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        }),
    ));
    for chunk in chunk_text(content) {
        frames.push(anthropic_frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": chunk},
            }),
        ));
    }
    frames.push(anthropic_frame(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": 0}),
    ));
    frames.push(anthropic_frame(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": Value::Null},
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        }),
    ));
    frames.push(anthropic_frame("message_stop", &json!({"type": "message_stop"})));
    frames
}

/// Claude 槽位名 → 上游模型名（映射表配置化；未命中原样透传）。
/// 槽位：claude-sonnet-4-5 / claude-opus-4-1 / claude-haiku-4-5 / claude-fable-*（前缀匹配）。
///
/// 匹配口径与参考 `messages.go:570 slotFallback` 一致：按关键词在模型名中的出现判定，
/// 覆盖「claude-sonnet-4-5」直排与「claude-3-5-sonnet-20241022」中置两种形态；
/// 与参考的差异是**不做 sonnet 兜底**（契约钉定「未命中原样透传」——宁可让上游报
/// 11102 可定位，也不静默替换成用户没选的模型）。
pub fn anthropic_slot_to_model(model: &str, mapping: &Value) -> String {
    let lower = model.to_ascii_lowercase();
    let hit = |v: Option<&Value>| -> Option<String> {
        v.and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    // 1) 完整名（小写规范化后）精确匹配优先：路由层映射表用全名键时命中
    //    （P2+P3 E2E 实测：只用短键查表会让全名键配置静默失效——槽位名
    //    原样透传上游，拿到 400 model not found）。
    if let Some(m) = hit(mapping.get(&lower)) {
        return m;
    }
    // 2) 短关键词包含匹配兜底：映射表用 opus/sonnet/haiku/fable 短键时命中
    //    （旧版语义保留；同时享受「claude-3-5-sonnet-* 也映射到同一槽位」的前缀效果）。
    for slot in ANTHROPIC_SLOTS {
        if !lower.contains(slot) {
            continue;
        }
        if let Some(m) = hit(mapping.get(slot)) {
            return m;
        }
    }
    model.to_string()
}

/// Anthropic 响应侧 tool_calls 嗅探：上游正文里含标准 `{"tool_calls":[...]}` JSON 时取出。
///
/// 与 `grok.rs:552 sniff_tool_calls` 同款（共享 `glm_web::try_extract_tool_calls` +
/// 括号配平兜底），多一层是因为模型常把 JSON 美化成多行、前面还带 UI 文案，
/// 单靠共享嗅探器的 `{"tool_calls"` 字面量会失配。
///
/// **何时调用由调用方决定**：仅当请求带 tools 时才值得嗅探（没有工具声明时模型
/// 不会也不该产 tool_calls，嗅探只会把普通正文误判成调用）。
pub fn anthropic_sniff_tool_calls(content: &str) -> Option<Vec<Value>> {
    if let Some(calls) = crate::modules::glm_web::try_extract_tool_calls(content) {
        return Some(calls);
    }
    // 括号配平：截出首个完整 JSON 对象再交共享嗅探器。
    let start = content.find('{')?;
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut esc = false;
    let mut end: Option<usize> = None;
    for (off, ch) in content[start..].char_indices() {
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
    let end = end?;
    crate::modules::glm_web::try_extract_tool_calls(&content[start..=end])
}

// ---------------------------------------------------------------------------
// 测试（纯函数矩阵；无网络、无磁盘）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 帧解析小工具（把帧字符串还原成结构，供顺序断言用） ----

    /// Responses 帧的 JSON 载荷（剥 `data: ` 前缀与尾部换行）。
    fn resp_payload(frame: &str) -> Value {
        let raw = frame
            .strip_prefix("data: ")
            .expect("Responses 帧应以 data: 开头");
        serde_json::from_str(raw.trim_end()).expect("帧体是合法 JSON")
    }

    /// Responses 帧的事件名（取 JSON 的 type 字段）。
    fn resp_types(frames: &[String]) -> Vec<String> {
        frames
            .iter()
            .filter(|f| !f.contains("[DONE]"))
            .map(|f| {
                resp_payload(f.as_str())["type"]
                    .as_str()
                    .expect("帧带 type")
                    .to_string()
            })
            .collect()
    }

    /// Anthropic 帧的事件名（event: 行）。
    fn anth_events(frames: &[String]) -> Vec<String> {
        frames
            .iter()
            .map(|f| {
                f.lines()
                    .next()
                    .and_then(|l| l.strip_prefix("event: "))
                    .expect("Anthropic 帧应以 event: 开头")
                    .to_string()
            })
            .collect()
    }

    /// Anthropic 帧的 JSON 载荷（第二行 data:）。
    fn anth_payload(frame: &str) -> Value {
        let data = frame.lines().nth(1).expect("第二行是 data:");
        let raw = data.strip_prefix("data: ").expect("data: 前缀");
        serde_json::from_str(raw).expect("帧体是合法 JSON")
    }

    /// 长文本（超过一个 delta 分片，用于验证多次 delta）。
    fn long_text(n: usize) -> String {
        "甲".repeat(n)
    }

    // ---- Responses 请求转换：input 两形态 / instructions / 兼容字段 ----

    #[test]
    fn responses_req_string_input_becomes_user_message() {
        let payload = responses_req_to_chat(&json!({
            "model": "gpt-5-codex",
            "input": "你好",
            "stream": true,
        }));
        assert_eq!(payload["model"], "gpt-5-codex");
        assert_eq!(payload["stream"], true);
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "你好");
    }

    #[test]
    fn responses_req_array_input_keeps_order_and_roles() {
        let payload = responses_req_to_chat(&json!({
            "model": "m",
            "input": [
                {"role": "user", "content": "第一句"},
                {"role": "assistant", "content": "第一答"},
                {"role": "user", "content": "第二句"},
            ],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "第二句");
    }

    #[test]
    fn responses_req_array_input_item_without_type_is_message() {
        let payload = responses_req_to_chat(&json!({
            "input": [{"role": "user", "content": "没带 type"}],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "没带 type");
    }

    #[test]
    fn responses_req_instructions_become_first_system_message() {
        let payload = responses_req_to_chat(&json!({
            "model": "m",
            "instructions": "你是助手",
            "input": "hi",
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "你是助手");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn responses_req_instructions_absent_inserts_no_system() {
        let payload = responses_req_to_chat(&json!({"input": "hi"}));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_ne!(msgs[0]["role"], "system");
    }

    #[test]
    fn responses_req_blank_instructions_inserts_no_system() {
        let payload = responses_req_to_chat(&json!({"instructions": "   ", "input": "hi"}));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn responses_req_falls_back_to_messages_field_when_input_absent() {
        let payload = responses_req_to_chat(&json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"},
            ],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["content"], "hi");
    }

    #[test]
    fn responses_req_empty_body_yields_empty_messages() {
        let payload = responses_req_to_chat(&json!({}));
        assert_eq!(payload["messages"], json!([]));
        assert_eq!(payload["stream"], false);
        assert!(payload.get("model").is_none());
        assert!(payload.get("tools").is_none());
    }

    #[test]
    fn responses_req_tools_and_stream_pass_through() {
        let tools = json!([{
            "type": "function",
            "name": "shell",
            "description": "跑命令",
            "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}},
        }]);
        let payload = responses_req_to_chat(&json!({"model": "m", "input": "hi", "tools": tools}));
        assert_eq!(payload["tools"], tools);
        assert_eq!(payload["stream"], false);
    }

    #[test]
    fn responses_req_empty_tools_are_dropped() {
        let payload = responses_req_to_chat(&json!({"input": "hi", "tools": []}));
        assert!(payload.get("tools").is_none());
    }

    #[test]
    fn responses_req_content_parts_flatten_to_text() {
        let payload = responses_req_to_chat(&json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "第一段"},
                    {"type": "input_text", "text": "第二段"},
                ],
            }],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "第一段第二段");
    }

    #[test]
    fn responses_req_function_call_history_maps_to_tool_calls_and_tool_role() {
        let payload = responses_req_to_chat(&json!({
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "file.txt"},
                {"type": "message", "role": "user", "content": "继续"},
            ],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        // function_call → assistant.tool_calls
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["arguments"], "{\"cmd\":\"ls\"}");
        // function_call_output → role:"tool"
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_1");
        assert_eq!(msgs[1]["content"], "file.txt");
        assert_eq!(msgs[2]["content"], "继续");
    }

    #[test]
    fn responses_req_reasoning_items_are_dropped() {
        let payload = responses_req_to_chat(&json!({
            "input": [
                {"type": "reasoning", "summary": " Internal "},
                {"type": "message", "role": "user", "content": "hi"},
            ],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "hi");
    }

    // ---- Responses 响应对象字段完整性 ----

    #[test]
    fn responses_resp_object_has_all_contract_fields() {
        let value = responses_resp_from_text("gpt-5-codex", "你好", "", &Value::Null);
        assert_eq!(value["object"], "response");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["model"], "gpt-5-codex");
        assert!(value["id"].as_str().unwrap().starts_with("resp_"));
        assert!(value["created_at"].as_i64().unwrap() > 1_700_000_000i64);
        let output = value["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["role"], "assistant");
        assert_eq!(output[0]["status"], "completed");
        assert_eq!(output[0]["content"][0]["type"], "output_text");
        assert_eq!(output[0]["content"][0]["text"], "你好");
        assert_eq!(value["usage"]["input_tokens"], 0);
        assert_eq!(value["usage"]["output_tokens"], 0);
        assert_eq!(value["usage"]["total_tokens"], 0);
    }

    #[test]
    fn responses_resp_falls_back_to_reasoning_when_content_empty() {
        let value = responses_resp_from_text("m", "", "推理内容", &Value::Null);
        assert_eq!(value["output"][0]["content"][0]["text"], "推理内容");
    }

    #[test]
    fn responses_resp_keeps_content_when_both_present() {
        let value = responses_resp_from_text("m", "正文", "推理", &Value::Null);
        assert_eq!(value["output"][0]["content"][0]["text"], "正文");
    }

    #[test]
    fn responses_resp_ids_are_unique_per_response() {
        let a = responses_resp_from_text("m", "x", "", &Value::Null);
        let b = responses_resp_from_text("m", "x", "", &Value::Null);
        assert_ne!(a["id"], b["id"]);
    }

    // ---- Responses 流式帧序列 ----

    #[test]
    fn responses_stream_frames_follow_pinned_order() {
        let frames = responses_stream_frames("m", "你好", &json!({}));
        assert_eq!(
            resp_types(&frames),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        // [DONE] 收尾，且每帧一个 data 行
        assert_eq!(frames.last().unwrap(), "data: [DONE]\n\n");
        for f in &frames {
            assert!(f.starts_with("data: "));
            assert!(f.ends_with("\n\n"));
        }
    }

    #[test]
    fn responses_stream_sequence_numbers_increase_from_zero() {
        let frames = responses_stream_frames("m", &long_text(200), &json!({}));
        let seqs: Vec<i64> = frames
            .iter()
            .filter(|f| !f.contains("[DONE]"))
            .map(|f| resp_payload(f.as_str())["sequence_number"].as_i64().unwrap())
            .collect();
        assert_eq!(seqs, (0..seqs.len() as i64).collect::<Vec<_>>());
    }

    #[test]
    fn responses_stream_delta_is_chunked_and_carries_full_text() {
        let text = long_text(200);
        let frames = responses_stream_frames("m", &text, &json!({}));
        let deltas: Vec<&String> = frames
            .iter()
            .filter(|f| f.contains("response.output_text.delta"))
            .collect();
        // 200 字符 / 64 = 4 帧（64+64+64+8）
        assert_eq!(deltas.len(), 4);
        let joined: String = deltas
            .iter()
            .map(|f| resp_payload(f.as_str())["delta"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(joined, text);
        // done 帧携带完整文本
        let done = frames
            .iter()
            .find(|f| f.contains("response.output_text.done"))
            .unwrap();
        assert_eq!(resp_payload(done)["text"], text);
    }

    #[test]
    fn responses_stream_empty_content_has_no_delta_but_full_tail() {
        let frames = responses_stream_frames("m", "", &json!({}));
        assert!(!frames.iter().any(|f| f.contains("output_text.delta")));
        let types = resp_types(&frames);
        // 空正文时 delta 一帧不发，事件帧仍是完整的 8 帧（created / in_progress /
        // output_item.added / content_part.added / output_text.done /
        // content_part.done / output_item.done / completed）—— 骨架不缺，
        // 客户端不会因为「没正文」而卡在等 completed。
        assert_eq!(types.len(), 8);
        assert_eq!(*types.last().unwrap(), "response.completed");
    }

    #[test]
    fn responses_stream_completed_carries_usage_and_output() {
        let frames = responses_stream_frames(
            "m",
            "hi",
            &json!({"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}),
        );
        let completed = frames
            .iter()
            .find(|f| f.contains("response.completed"))
            .unwrap();
        let payload = resp_payload(completed);
        assert_eq!(payload["response"]["status"], "completed");
        assert_eq!(payload["response"]["model"], "m");
        assert_eq!(payload["response"]["usage"]["input_tokens"], 11);
        assert_eq!(payload["response"]["usage"]["output_tokens"], 7);
        assert_eq!(payload["response"]["usage"]["total_tokens"], 18);
        assert_eq!(payload["response"]["output"][0]["content"][0]["text"], "hi");
        // created 帧也带 usage（客户端在 created 就读用量，缺字段会解析成 null）
        let created = frames
            .iter()
            .find(|f| f.contains("response.created"))
            .unwrap();
        assert_eq!(resp_payload(created)["response"]["usage"]["total_tokens"], 18);
    }

    #[test]
    fn responses_stream_usage_accepts_camel_case_upstream_shape() {
        let frames = responses_stream_frames(
            "m",
            "hi",
            &json!({"promptTokens": 3, "completionTokens": 5}),
        );
        let delta = frames
            .iter()
            .find(|f| f.contains("message_delta") || f.contains("response.completed"))
            .unwrap();
        assert_eq!(resp_payload(delta)["response"]["usage"]["input_tokens"], 3);
        assert_eq!(resp_payload(delta)["response"]["usage"]["output_tokens"], 5);
    }

    #[test]
    fn responses_stream_missing_total_tokens_falls_back_to_sum() {
        let frames = responses_stream_frames("m", "hi", &json!({"prompt_tokens": 4, "completion_tokens": 6}));
        let completed = frames
            .iter()
            .find(|f| f.contains("response.completed"))
            .unwrap();
        assert_eq!(resp_payload(completed)["response"]["usage"]["total_tokens"], 10);
    }

    #[test]
    fn responses_stream_ids_are_consistent_within_one_stream() {
        let frames = responses_stream_frames("m", "hi", &json!({}));
        let added = frames
            .iter()
            .find(|f| f.contains("response.output_item.added"))
            .unwrap();
        let item_id = resp_payload(added)["item"]["id"].as_str().unwrap().to_string();
        let delta = frames
            .iter()
            .find(|f| f.contains("response.output_text.delta"))
            .unwrap();
        assert_eq!(resp_payload(delta)["item_id"], item_id);
        assert!(item_id.starts_with("msg_resp_"));
    }

    // ---- Anthropic 请求转换：system 三形态 ----

    #[test]
    fn anthropic_req_system_string() {
        let payload = anthropic_req_to_chat(&json!({
            "model": "claude-sonnet-4-5",
            "system": "你是助手",
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "你是助手");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn anthropic_req_system_blocks_array() {
        let payload = anthropic_req_to_chat(&json!({
            "system": [
                {"type": "text", "text": "第一段"},
                {"type": "text", "text": "第二段"},
            ],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "第一段\n第二段");
    }

    #[test]
    fn anthropic_req_system_absent_inserts_no_system() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn anthropic_req_empty_system_string_inserts_no_system() {
        let payload = anthropic_req_to_chat(&json!({
            "system": "",
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    // ---- Anthropic 请求转换：content 形态 ----

    #[test]
    fn anthropic_req_content_string_passes_through() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{"role": "user", "content": "纯文本"}],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "纯文本");
    }

    #[test]
    fn anthropic_req_content_text_blocks_flatten() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "第一段"},
                    {"type": "text", "text": "第二段"},
                ],
            }],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "第一段\n第二段");
    }

    #[test]
    fn anthropic_req_image_block_becomes_image_url_part() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "看图"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                ],
            }],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn anthropic_req_image_url_source_passes_url() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.invalid/a.png"}},
                ],
            }],
        }));
        let content = payload["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["image_url"]["url"], "https://example.invalid/a.png");
    }

    #[test]
    fn anthropic_req_thinking_blocks_are_dropped() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "内部推理"},
                    {"type": "text", "text": "正文"},
                ],
            }],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "正文");
    }

    #[test]
    fn anthropic_req_missing_content_produces_no_message() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{"role": "user"}, {"role": "user", "content": "hi"}],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "hi");
    }

    // ---- Anthropic 请求转换：tools / 参数 ----

    #[test]
    fn anthropic_req_tools_map_input_schema_to_parameters() {
        let payload = anthropic_req_to_chat(&json!({
            "tools": [{
                "name": "Bash",
                "description": "跑命令",
                "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}},
            }],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let tools = payload["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "Bash");
        assert_eq!(tools[0]["function"]["description"], "跑命令");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert!(tools[0]["function"].get("input_schema").is_none());
    }

    #[test]
    fn anthropic_req_tools_without_input_schema_get_empty_object_schema() {
        let payload = anthropic_req_to_chat(&json!({
            "tools": [{"name": "NoArgs"}],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let tools = payload["tools"].as_array().unwrap();
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(tools[0]["function"]["parameters"]["properties"], json!({}));
    }

    #[test]
    fn anthropic_req_tools_empty_or_nameless_are_dropped() {
        let payload = anthropic_req_to_chat(&json!({
            "tools": [{"name": "  "}, {"description": "无名"}],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert!(payload.get("tools").is_none());
    }

    #[test]
    fn anthropic_req_stop_sequences_map_to_stop_and_max_tokens_passthrough() {
        let payload = anthropic_req_to_chat(&json!({
            "max_tokens": 1024,
            "stop_sequences": ["\n\nHuman:"],
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(payload["max_tokens"], 1024);
        assert_eq!(payload["stop"], json!(["\n\nHuman:"]));
        assert_eq!(payload["stream"], true);
        // 原字段名不残留
        assert!(payload.get("stop_sequences").is_none());
    }

    #[test]
    fn anthropic_req_empty_stop_sequences_are_dropped() {
        let payload = anthropic_req_to_chat(&json!({
            "stop_sequences": [],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert!(payload.get("stop").is_none());
    }

    #[test]
    fn anthropic_req_model_passthrough_and_absent() {
        let with_model = anthropic_req_to_chat(&json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(with_model["model"], "claude-haiku-4-5");
        let without = anthropic_req_to_chat(&json!({"messages": []}));
        assert!(without.get("model").is_none());
        assert_eq!(without["messages"], json!([]));
    }

    // ---- Anthropic 请求转换：tool_use + tool_result 历史 ----

    #[test]
    fn anthropic_req_tool_use_history_becomes_assistant_tool_calls() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "我来查"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"cmd": "ls"}},
                ],
            }],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "我来查");
        let calls = msgs[0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "toolu_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "Bash");
        assert_eq!(calls[0]["function"]["arguments"], "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn anthropic_req_tool_result_history_becomes_tool_role_message() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "file.txt"},
                    {"type": "text", "text": "继续"},
                ],
            }],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        // tool_result 独立成消息且排在 user 正文之前（断裂序列会让上游 400）
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "toolu_1");
        assert_eq!(msgs[0]["content"], "file.txt");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "继续");
    }

    #[test]
    fn anthropic_req_full_tool_round_trip_sequence() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [
                {"role": "user", "content": "列出文件"},
                {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"cmd": "ls"}}],
                },
                {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt"}],
                },
                {"role": "assistant", "content": "有 a.txt"},
                {"role": "user", "content": "谢谢"},
            ],
        }));
        let roles: Vec<&str> = payload["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "assistant", "user"]);
    }

    #[test]
    fn anthropic_req_tool_result_with_block_content_flattens_text() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": [
                        {"type": "text", "text": "第一行"},
                        {"type": "text", "text": "第二行"},
                    ],
                }],
            }],
        }));
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["content"], "第一行\n第二行");
    }

    #[test]
    fn anthropic_req_tool_use_without_id_or_name_is_skipped() {
        let payload = anthropic_req_to_chat(&json!({
            "messages": [{
                "role": "assistant",
                "content": [{"type": "tool_use", "name": "Bash", "input": {}}],
            }],
        }));
        // 缺 id 的 tool_use 无法回灌：整条 assistant 消息不产出（不能塞半条 tool_calls）
        assert_eq!(payload["messages"], json!([]));
    }

    // ---- Anthropic 响应对象字段完整性 ----

    #[test]
    fn anthropic_resp_message_has_all_contract_fields() {
        let value = anthropic_resp_from_text("claude-sonnet-4-5", "你好", &[], &Value::Null);
        assert_eq!(value["id"].as_str().unwrap().chars().take(4).collect::<String>(), "msg_");
        assert_eq!(value["type"], "message");
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["model"], "claude-sonnet-4-5");
        let content = value["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "你好");
        assert_eq!(value["stop_reason"], "end_turn");
        assert_eq!(value["stop_sequence"], Value::Null);
        assert_eq!(value["usage"]["input_tokens"], 0);
        assert_eq!(value["usage"]["output_tokens"], 0);
    }

    #[test]
    fn anthropic_resp_appends_tool_use_blocks_and_flips_stop_reason() {
        let calls = vec![
            json!({"id": "call_1", "type": "function", "function": {"name": "Bash", "arguments": "{\"cmd\":\"ls\"}"}}),
            json!({"id": "call_2", "type": "function", "function": {"name": "Read", "arguments": "{}"}}),
        ];
        let value = anthropic_resp_from_text("claude-sonnet-4-5", "我来查", &calls, &Value::Null);
        let content = value["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "call_1");
        assert_eq!(content[1]["name"], "Bash");
        assert_eq!(content[1]["input"]["cmd"], "ls");
        assert_eq!(content[2]["name"], "Read");
        assert_eq!(value["stop_reason"], "tool_use");
    }

    #[test]
    fn anthropic_resp_tool_call_with_bad_arguments_gets_empty_input() {
        let calls = vec![json!({"id": "c1", "function": {"name": "Bash", "arguments": "not-json"}})];
        let value = anthropic_resp_from_text("m", "", &calls, &Value::Null);
        assert_eq!(value["content"][0]["input"], json!({}));
        assert_eq!(value["stop_reason"], "tool_use");
    }

    #[test]
    fn anthropic_resp_tool_call_without_name_is_skipped() {
        let calls = vec![json!({"id": "c1", "function": {"arguments": "{}"}})];
        let value = anthropic_resp_from_text("m", "正文", &calls, &Value::Null);
        assert_eq!(value["content"].as_array().unwrap().len(), 1);
        assert_eq!(value["stop_reason"], "end_turn");
    }

    #[test]
    fn anthropic_resp_empty_content_and_calls_yield_empty_array() {
        let value = anthropic_resp_from_text("m", "", &[], &Value::Null);
        assert_eq!(value["content"], json!([]));
        assert_eq!(value["stop_reason"], "end_turn");
    }

    #[test]
    fn anthropic_resp_ids_are_unique_per_response() {
        let a = anthropic_resp_from_text("m", "x", &[], &Value::Null);
        let b = anthropic_resp_from_text("m", "x", &[], &Value::Null);
        assert_ne!(a["id"], b["id"]);
    }

    // ---- Anthropic 流式帧序列 ----

    #[test]
    fn anthropic_stream_frames_follow_pinned_order() {
        let frames = anthropic_stream_frames("claude-sonnet-4-5", "你好", &json!({}));
        assert_eq!(
            anth_events(&frames),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        // 无 [DONE] 哨兵（message_stop 即结束）
        assert!(!frames.iter().any(|f| f.contains("[DONE]")));
    }

    #[test]
    fn anthropic_stream_frames_carry_event_and_data_lines() {
        let frames = anthropic_stream_frames("m", "hi", &json!({}));
        for f in &frames {
            let lines: Vec<&str> = f.lines().collect();
            assert_eq!(lines.len(), 3, "每帧两行内容 + 空行：{f:?}");
            assert!(lines[0].starts_with("event: "));
            assert!(lines[1].starts_with("data: "));
            assert_eq!(lines[2], "");
        }
    }

    #[test]
    fn anthropic_stream_message_start_shell_is_complete() {
        let frames = anthropic_stream_frames("claude-sonnet-4-5", "hi", &json!({}));
        let message = anth_payload(&frames[0])["message"].clone();
        assert!(message["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["model"], "claude-sonnet-4-5");
        assert_eq!(message["content"], json!([]));
        assert_eq!(message["stop_reason"], Value::Null);
        assert_eq!(message["stop_sequence"], Value::Null);
    }

    #[test]
    fn anthropic_stream_delta_chunks_reassemble_full_text() {
        let text = long_text(200);
        let frames = anthropic_stream_frames("m", &text, &json!({}));
        let deltas: Vec<&String> = frames
            .iter()
            .filter(|f| f.contains("content_block_delta"))
            .collect();
        assert_eq!(deltas.len(), 4);
        let joined: String = deltas
            .iter()
            .map(|f| {
                anth_payload(f.as_str())["delta"]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(joined, text);
        // delta 一律指向 index 0 的文本块
        for f in &deltas {
            assert_eq!(anth_payload(f.as_str())["index"], 0);
            assert_eq!(anth_payload(f.as_str())["delta"]["type"], "text_delta");
        }
    }

    #[test]
    fn anthropic_stream_message_delta_carries_stop_reason_and_usage() {
        let frames = anthropic_stream_frames(
            "m",
            "hi",
            &json!({"prompt_tokens": 12, "completion_tokens": 8}),
        );
        let delta = frames
            .iter()
            .find(|f| f.contains("message_delta"))
            .unwrap();
        let payload = anth_payload(delta);
        assert_eq!(payload["delta"]["stop_reason"], "end_turn");
        assert_eq!(payload["delta"]["stop_sequence"], Value::Null);
        assert_eq!(payload["usage"]["input_tokens"], 12);
        assert_eq!(payload["usage"]["output_tokens"], 8);
        // 收尾帧是 message_stop
        let last = anth_payload(frames.last().unwrap());
        assert_eq!(last["type"], "message_stop");
    }

    #[test]
    fn anthropic_stream_empty_content_still_completes_sequence() {
        let frames = anthropic_stream_frames("m", "", &json!({}));
        assert!(!frames.iter().any(|f| f.contains("content_block_delta")));
        let stop = frames
            .iter()
            .find(|f| f.contains("content_block_stop"))
            .unwrap();
        assert_eq!(anth_payload(stop)["index"], 0);
    }

    #[test]
    fn anthropic_stream_every_frame_type_matches_event_name() {
        let frames = anthropic_stream_frames("m", "hi", &json!({}));
        for f in &frames {
            let event = anth_events(&[f.clone()])[0].clone();
            assert_eq!(anth_payload(f)["type"], event);
        }
    }

    // ---- 槽位映射 ----

    #[test]
    fn slot_mapping_hits_every_slot() {
        let mapping = json!({
            "sonnet": "deepseek-v4-flash",
            "opus": "deepseek-v4.1-flash",
            "haiku": "glm-5.3-air",
            "fable": "kimi-k2",
        });
        assert_eq!(
            anthropic_slot_to_model("claude-sonnet-4-5", &mapping),
            "deepseek-v4-flash"
        );
        assert_eq!(
            anthropic_slot_to_model("claude-opus-4-1", &mapping),
            "deepseek-v4.1-flash"
        );
        assert_eq!(
            anthropic_slot_to_model("claude-haiku-4-5", &mapping),
            "glm-5.3-air"
        );
        assert_eq!(
            anthropic_slot_to_model("claude-fable-1", &mapping),
            "kimi-k2"
        );
        // 全名键表（路由层 workbuddy_pool_claude_slots 的风格）也必须命中
        let full = json!({
            "claude-sonnet-4-5": "deepseek-v4-flash",
            "claude-opus-4-1": "glm-5.2",
        });
        assert_eq!(
            anthropic_slot_to_model("claude-sonnet-4-5", &full),
            "deepseek-v4-flash"
        );
        assert_eq!(
            anthropic_slot_to_model("CLAUDE-OPUS-4-1", &full),
            "glm-5.2"
        );
        // 前缀型号在全名键表下不命中精确键 → 回落短键 → 也查不到 → 原样透传
        assert_eq!(
            anthropic_slot_to_model("claude-3-5-sonnet-20241022", &full),
            "claude-3-5-sonnet-20241022"
        );
    }

    #[test]
    fn slot_mapping_embedded_slot_name_also_hits() {
        let mapping = json!({"sonnet": "deepseek-v4-flash"});
        assert_eq!(
            anthropic_slot_to_model("claude-3-5-sonnet-20241022", &mapping),
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn slot_mapping_is_case_insensitive() {
        let mapping = json!({"opus": "deepseek-v4.1-flash"});
        assert_eq!(
            anthropic_slot_to_model("CLAUDE-OPUS-4-1", &mapping),
            "deepseek-v4.1-flash"
        );
    }

    #[test]
    fn slot_mapping_miss_passes_through_unchanged() {
        let mapping = json!({"sonnet": "deepseek-v4-flash"});
        assert_eq!(
            anthropic_slot_to_model("deepseek-v4-flash", &mapping),
            "deepseek-v4-flash"
        );
        assert_eq!(anthropic_slot_to_model("claude-3-7-unknown", &mapping), "claude-3-7-unknown");
    }

    #[test]
    fn slot_mapping_empty_table_passes_through() {
        assert_eq!(anthropic_slot_to_model("claude-sonnet-4-5", &json!({})), "claude-sonnet-4-5");
    }

    #[test]
    fn slot_mapping_non_object_table_passes_through() {
        assert_eq!(
            anthropic_slot_to_model("claude-opus-4-1", &json!(null)),
            "claude-opus-4-1"
        );
        assert_eq!(
            anthropic_slot_to_model("claude-opus-4-1", &json!({"sonnet": "x"})),
            "claude-opus-4-1"
        );
    }

    #[test]
    fn slot_mapping_blank_value_is_ignored() {
        let mapping = json!({"sonnet": "   "});
        assert_eq!(
            anthropic_slot_to_model("claude-sonnet-4-5", &mapping),
            "claude-sonnet-4-5"
        );
    }

    #[test]
    fn slot_mapping_value_is_trimmed() {
        let mapping = json!({"sonnet": "  deepseek-v4-flash  "});
        assert_eq!(
            anthropic_slot_to_model("claude-sonnet-4-5", &mapping),
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn slot_mapping_priority_opus_over_sonnet_on_overlap() {
        let mapping = json!({"sonnet": "a", "opus": "b"});
        // 名字同时含两个关键词时按 ANTHROPIC_SLOTS 顺序（opus 先）取，保证确定性
        assert_eq!(
            anthropic_slot_to_model("claude-opus-sonnet-hybrid", &mapping),
            "b"
        );
    }

    #[test]
    fn slot_mapping_empty_model_name_passes_through() {
        let mapping = json!({"sonnet": "x"});
        assert_eq!(anthropic_slot_to_model("", &mapping), "");
    }

    // ---- tool_calls 嗅探 ----

    #[test]
    fn sniff_extracts_plain_tool_calls_json() {
        let calls = anthropic_sniff_tool_calls("{\"tool_calls\":[{\"name\":\"Bash\",\"arguments\":{\"cmd\":\"ls\"}}]}")
            .expect("应嗅探到 tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        // arguments 归一成 JSON 字符串（OpenAI 形状，Anthropic 侧再解析回对象）
        assert_eq!(calls[0]["function"]["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(calls[0]["type"], "function");
    }

    #[test]
    fn sniff_extracts_fenced_json_block() {
        let text = "```json\n{\"tool_calls\":[{\"name\":\"Read\",\"arguments\":{\"path\":\"a.txt\"}}]}\n```";
        let calls = anthropic_sniff_tool_calls(text).expect("围栏内 JSON 应被嗅探到");
        assert_eq!(calls[0]["function"]["name"], "Read");
    }

    #[test]
    fn sniff_handles_pretty_multiline_json_with_prefix() {
        let text = "JSON复制已复制\n{\n  \"tool_calls\": [\n    {\n      \"name\": \"Bash\",\n      \"arguments\": {\"cmd\": \"ls\"}\n    }\n  ]\n}";
        let calls = anthropic_sniff_tool_calls(text).expect("美化多行 JSON 应被嗅探到");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
    }

    #[test]
    fn sniff_returns_none_for_plain_text() {
        assert!(anthropic_sniff_tool_calls("今天天气不错").is_none());
        assert!(anthropic_sniff_tool_calls("集合 {1, 2} 的元素个数是 2。").is_none());
        assert!(anthropic_sniff_tool_calls("").is_none());
    }

    #[test]
    fn sniff_returns_none_for_json_without_tool_calls() {
        assert!(anthropic_sniff_tool_calls("{\"answer\": 42}").is_none());
    }

    #[test]
    fn sniff_result_feeds_anthropic_response() {
        // 端到端小闭环：嗅探 → Anthropic message（供接线工程师参照调用顺序）
        let text = "{\"tool_calls\":[{\"name\":\"Bash\",\"arguments\":{\"cmd\":\"ls\"}}]}";
        let calls = anthropic_sniff_tool_calls(text).unwrap();
        let message = anthropic_resp_from_text("claude-sonnet-4-5", "", &calls, &Value::Null);
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["content"][0]["type"], "tool_use");
        assert_eq!(message["content"][0]["name"], "Bash");
    }

    // ---- chunk 工具行为 ----

    #[test]
    fn chunk_text_splits_by_char_not_byte() {
        let chunks = chunk_text("你好世界");
        assert_eq!(chunks.len(), 1);
        let chunks = chunk_text(&long_text(65));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), STREAM_DELTA_CHUNK_CHARS);
        assert_eq!(chunks[1].chars().count(), 1);
        // 拼回原串不丢字
        assert_eq!(chunks.concat(), long_text(65));
    }

    #[test]
    fn usage_triplet_defaults_to_zero_for_non_object() {
        assert_eq!(usage_triplet(&json!(null)), (0, 0, 0));
        assert_eq!(usage_triplet(&json!([])), (0, 0, 0));
        assert_eq!(usage_triplet(&json!({})), (0, 0, 0));
    }
}
