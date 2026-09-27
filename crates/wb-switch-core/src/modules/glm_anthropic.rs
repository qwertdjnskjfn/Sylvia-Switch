//! GLM 代理的 Anthropic 协议兼容层。
//!
//! 把 Anthropic Messages API（`POST /v1/messages`）的请求转换为 OpenAI
//! chat/completions 载荷复用既有上游链路；响应（含 SSE 流）再转换回
//! Anthropic 事件序列。
//!
//! 工具调用（tools / tool_use / tool_result）双向映射已实现：编码类客户端
//! （ZCode 等）依赖 function calling 驱动 agent 循环，丢弃 tools 会让模型
//! 只能回一句文本就 end_turn，表现为「输出几秒就被截断」。
//! 图片块（Anthropic `image` → OpenAI `image_url`）也已支持——上游对多模态
//! 是接受的，丢掉图片会让「看图」请求变成纯文本请求。
//! thinking 块透传：上游在正文前吐的 `reasoning_content` 会被转成 Anthropic
//! 标准 `thinking` 块（`content_block_start type=thinking` → `thinking_delta`
//! → `content_block_stop`），而不是丢弃。这样既能让支持思考块的客户端把推理过程
//! 展示出来，又能在思考期持续产字节（消除「卡住」感——早期版本只补 `: ping`
//! 注释，内容层仍是空壳）。thinking 必须排在 text 块之前，正文已开始到达的
//! 情况下后续的 reasoning 片段会被丢弃（顺序不合法）。
//!
//! SSE 转换状态机（`AnthropicSseTransformer`）与请求/响应转换均为纯函数，
//! 单测覆盖；I/O 泵在 [`crate::modules::glm_proxy`] 中组装。

use serde_json::{json, Value};

use super::glm_proxy::GlmUpstreamStream;

/// Anthropic tools → OpenAI tools（`{type:"function", function:{name,description,parameters}}`）。
/// 无有效工具时返回 None，避免给上游塞空数组。
fn to_openai_tools(body: &Value) -> Option<Value> {
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
                function.insert("description".to_string(), json!(description));
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

/// Anthropic tool_choice → OpenAI tool_choice。
fn to_openai_tool_choice(body: &Value) -> Option<Value> {
    let choice = body.get("tool_choice")?;
    match choice.get("type").and_then(Value::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") => choice
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    }
}

/// Anthropic 图片块 → OpenAI `image_url` 内容块（支持 base64 与 url 两种 source）。
pub(crate) fn openai_image_part(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = source.get("data").and_then(Value::as_str)?;
            if data.is_empty() {
                return None;
            }
            Some(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{media};base64,{data}")},
            }))
        }
        Some("url") => {
            let url = source.get("url").and_then(Value::as_str)?;
            if url.is_empty() {
                return None;
            }
            Some(json!({"type": "image_url", "image_url": {"url": url}}))
        }
        _ => None,
    }
}

/// 抽取 tool_result 的内嵌文本（字符串或 content block 数组）。
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(inner)) => concat_block_texts(inner),
        _ => String::new(),
    }
}

/// Anthropic 请求 → OpenAI chat/completions 载荷。
/// 返回 (OpenAI 载荷, 请求的模型名, 是否流式)。
pub(crate) fn to_openai_payload(body: &Value) -> Result<(Value, String, bool), String> {
    if !body.is_object() {
        return Err("请求体必须是 JSON 对象".to_string());
    }
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("glm-5.3")
        .to_string();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut messages: Vec<Value> = Vec::new();
    // system：字符串或 content block 数组，取全部 text 拼接。
    match body.get("system") {
        Some(Value::String(text)) if !text.is_empty() => {
            messages.push(json!({"role": "system", "content": text}));
        }
        Some(Value::Array(blocks)) => {
            let text = concat_block_texts(blocks);
            if !text.is_empty() {
                messages.push(json!({"role": "system", "content": text}));
            }
        }
        _ => {}
    }
    let Some(body_messages) = body.get("messages").and_then(Value::as_array) else {
        return Err("messages 字段缺失（Anthropic /v1/messages 需要）".to_string());
    };
    if body_messages.is_empty() {
        return Err("messages 不能为空".to_string());
    }
    for message in body_messages {
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            return Err("messages[] 缺少 role".to_string());
        };
        if role != "user" && role != "assistant" {
            return Err(format!("不支持的 role: {role:?}（仅 user / assistant）"));
        }
        match message.get("content") {
            Some(Value::String(text)) => {
                messages.push(json!({"role": role, "content": text}));
            }
            Some(Value::Array(blocks)) => {
                if role == "assistant" {
                    messages.push(assistant_message_from_blocks(blocks));
                } else {
                    append_user_message_from_blocks(&mut messages, blocks);
                }
            }
            _ => {}
        }
    }

    let mut payload = json!({
        "model": requested_model,
        "messages": messages,
        "stream": stream,
    });
    // Anthropic 的 max_tokens 必填；缺省给一个安全值（最高钳位到 131072 避免上游 400）。
    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .map(|v| v.min(131072))
        .unwrap_or(4096);
    payload["max_tokens"] = json!(max_tokens);
    for (from, to) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(v) = body.get(from) {
            if v.is_number() {
                payload[to] = v.clone();
            }
        }
    }
    if let Some(stops) = body.get("stop_sequences").and_then(Value::as_array) {
        if !stops.is_empty() {
            payload["stop"] = json!(stops);
        }
    }
    // 推理档位透传：支持 reasoning_effort 或 Anthropic thinking 模式映射
    if let Some(effort) = body.get("reasoning_effort") {
        payload["reasoning_effort"] = effort.clone();
    } else if let Some(thinking) = body.get("thinking") {
        if thinking.get("type").and_then(Value::as_str) == Some("enabled") {
            payload["reasoning_effort"] = json!("medium");
        }
    }
    // 工具调用：编码 agent（ZCode 等）依赖它驱动多步任务，必须透传。
    if let Some(tools) = to_openai_tools(body) {
        payload["tools"] = tools;
    }
    if let Some(tool_choice) = to_openai_tool_choice(body) {
        payload["tool_choice"] = tool_choice;
    }
    Ok((payload, requested_model, stream))
}

/// assistant 的 content block 数组 → 单条 OpenAI assistant 消息
/// （文本聚合成 content，`tool_use` 块转 `tool_calls`）。
fn assistant_message_from_blocks(blocks: &[Value]) -> Value {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text_parts.push(text.to_string());
                    }
                }
            }
            Some("tool_use") => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
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
            // thinking / redacted_thinking / image 等：上游不支持，丢弃。
            _ => {}
        }
    }
    let text = text_parts.join("\n");
    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), json!("assistant"));
    if !text.is_empty() || tool_calls.is_empty() {
        message.insert("content".to_string(), json!(text));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    Value::Object(message)
}

/// user 的 content block 数组 → OpenAI 消息序列：
/// `tool_result` 块转成独立的 `role: "tool"` 消息（必须紧跟 assistant 的
/// tool_calls）；文本与图片块按原顺序组成一条 user 消息（有图片时用内容数组，
/// 这样上游才能收到图片，速度优先开关也能识别出多模态请求）。
fn append_user_message_from_blocks(messages: &mut Vec<Value>, blocks: &[Value]) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut content_parts: Vec<Value> = Vec::new();
    let mut has_image = false;
    let mut tool_messages: Vec<Value> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if tool_use_id.is_empty() {
                    continue;
                }
                tool_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": tool_result_text(block),
                }));
            }
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text_parts.push(text.to_string());
                        content_parts.push(json!({"type": "text", "text": text}));
                    }
                }
            }
            Some("image") => {
                if let Some(part) = openai_image_part(block) {
                    has_image = true;
                    content_parts.push(part);
                }
            }
            _ => {}
        }
    }
    messages.append(&mut tool_messages);
    let text = text_parts.join("\n");
    if has_image {
        // 文本在上、图片在下，符合多数多模态模型的阅读顺序。
        let mut ordered: Vec<Value> = content_parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .cloned()
            .collect();
        ordered.extend(
            content_parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
                .cloned(),
        );
        messages.push(json!({"role": "user", "content": ordered}));
    } else if !text.is_empty() || tool_messages.is_empty() {
        messages.push(json!({"role": "user", "content": text}));
    }
}

/// 递归抽取 content block 数组里的全部文本（text 直接取；tool_result 取其内嵌文本）。
fn concat_block_texts(blocks: &[Value]) -> String {
    let mut out: Vec<String> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    out.push(text.to_string());
                }
            }
            Some("tool_result") => match block.get("content") {
                Some(Value::String(text)) => out.push(text.clone()),
                Some(Value::Array(inner)) => out.push(concat_block_texts(inner)),
                _ => {}
            },
            _ => {}
        }
    }
    out.join("\n")
}

/// OpenAI finish_reason → Anthropic stop_reason。
fn map_stop_reason(finish: Option<&str>) -> Value {
    match finish {
        Some("stop") => json!("end_turn"),
        Some("length") => json!("max_tokens"),
        Some("stop_sequence") => json!("stop_sequence"),
        Some("tool_calls") | Some("function_call") => json!("tool_use"),
        _ => Value::Null,
    }
}

/// OpenAI 非流式响应 → Anthropic message 对象。
pub(crate) fn from_openai_response(openai: &Value, requested_model: &str) -> Value {
    let choice = openai
        .pointer("/choices/0")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let content_text = choice
        .pointer("/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let content_text = if content_text.is_empty() {
        // 上游只回了推理内容时降级为纯文本，避免空回复。
        choice
            .pointer("/message/reasoning_content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    } else {
        content_text
    };
    let content: Vec<Value> = if content_text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "text", "text": content_text})]
    };
    // 工具调用 → Anthropic tool_use 块（input 由 arguments JSON 字符串解析）。
    let mut content = content;
    for call in choice
        .pointer("/message/tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = call.pointer("/function/name").and_then(Value::as_str) else {
            continue;
        };
        let input = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({}));
        content.push(json!({
            "type": "tool_use",
            "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
            "name": name,
            "input": input,
        }));
    }
    let stop_reason = match map_stop_reason(choice.get("finish_reason").and_then(Value::as_str)) {
        Value::Null if content.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")) => {
            json!("tool_use")
        }
        other => other,
    };
    let usage = openai.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": openai.get("id").and_then(Value::as_str)
            .map(|id| format!("msg_{id}"))
            .unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4().simple())),
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0),
        },
    })
}

/// SSE 事件块：`event: <name>\ndata: <json>\n\n`。
fn sse_event(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// 按 UTF-8 边界安全解码上游 chunk，返回「可立即解析的文本」与
/// 「留到下一个 chunk 拼接的不完整尾部」。
///
/// 上游大帧（含中文的多 KB tool_calls arguments）会在 TCP 层被切成多个
/// chunk，切点可能正好落在一个多字节字符中间。各处原来用严格
/// `std::str::from_utf8` 解码，失败就把整个 chunk 静默丢弃——导致 SSE
/// 帧内容缺失，转换器误判「上游 200 但空流」而报「非流式异常响应」
/// （2026-09-16 23:57 glm-5.3-flash 事故根因）。此函数保证字节不丢：
/// 尾部不完整的码点字节留给下一块，其余全部产出。
pub fn decode_stream_chunk(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    let mut combined = std::mem::take(carry);
    combined.extend_from_slice(chunk);
    // 仅当尾部是「未完成的码点」（error_len 为 None）时才截留；若是真正的
    // 非法字节序列则整体 lossy 转换，避免坏字节永久卡住 carry。
    let split_at = match std::str::from_utf8(&combined) {
        Ok(_) => combined.len(),
        Err(e) => {
            if e.error_len().is_some() {
                combined.len()
            } else {
                e.valid_up_to()
            }
        }
    };
    let text = String::from_utf8_lossy(&combined[..split_at]).into_owned();
    carry.clear();
    carry.extend_from_slice(&combined[split_at..]);
    text
}

/// 上游 OpenAI SSE → Anthropic SSE 的转换状态机。
///
/// `push()` 喂入任意切分的上游文本，返回当前可下发的事件块；
/// 流结束后调用 `finish()` 收尾（content_block_stop / message_delta /
/// message_stop）。`usage` 记录上游最终 usage 块（计量用）。
/// 当前打开的 content block 类型。
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Tool,
    /// 上游推理内容（`reasoning_content`）透传成的 Anthropic thinking 块。
    /// 必须排在 Text 之前。
    Thinking,
}

pub(crate) struct AnthropicSseTransformer {
    buffer: String,
    started: bool,
    /// 当前打开的块类型；None 表示没有打开的块。
    open_kind: Option<BlockKind>,
    /// 下一个 content block 的 index（Anthropic 按顺序从 0 递增）。
    next_index: usize,
    /// 上一个已打开的 OpenAI tool_calls 序号，用于识别新工具块。
    last_tool_index: Option<u64>,
    /// 本次流是否已产生 tool_use 块（收尾时兜底 stop_reason）。
    saw_tool: bool,
    /// 是否已产出内容块（识别上游「200 但空流」用）。
    emitted: bool,
    /// 是否收到过思考内容（`delta.reasoning_content`）。超长上下文下上游可能
    /// 长时间只吐思考、还没来得及吐正文就被切断，不能按空响应当成异常体。
    saw_reasoning: bool,
    /// 上次向客户端补发 SSE 保活注释的时间。
    ///
    /// 思考型模型（glm-5.3-flash 等）在输出正文前会先吐 3~10s 的
    /// `reasoning_content`；此期间若不产任何事件，客户端会一直收不到字节，
    /// 表现为「加载特别慢 / 卡住」。故思考期按固定间隔补 `: ping` 注释
    /// （SSE 规范中以 `:` 开头的行必须被忽略，不影响事件语义）。
    last_keepalive: Option<std::time::Instant>,
    stop_reason: Value,
    usage: Option<Value>,
    errored: bool,
    error_message: Option<String>,
}

impl Default for AnthropicSseTransformer {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicSseTransformer {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            started: false,
            open_kind: None,
            next_index: 0,
            last_tool_index: None,
            saw_tool: false,
            emitted: false,
            saw_reasoning: false,
            last_keepalive: None,
            stop_reason: Value::Null,
            usage: None,
            errored: false,
            error_message: None,
        }
    }

    pub fn usage(&self) -> Value {
        self.usage
            .clone()
            .unwrap_or_else(|| json!({"promptTokens": 0.0, "completionTokens": 0.0, "totalTokens": 0.0, "reasoningTokens": Value::Null}))
    }

    pub fn ok(&self) -> bool {
        !self.errored
    }

    pub fn error(&self) -> String {
        self.error_message
            .clone()
            .unwrap_or_else(|| "上游返回错误".to_string())
    }

    /// 是否已产出过内容块（上游 200 但空流时用于诊断）。
    pub fn emitted_content(&self) -> bool {
        self.emitted
    }

    /// 上游是否真的吐过东西（正文或思考都算）。
    /// 用于区分「网关返回了非流式错误体」与「上游输出思考后被切断」。
    pub fn emitted_anything(&self) -> bool {
        self.emitted || self.saw_reasoning
    }

    /// 是否收到过上游 usage（空流通常连 usage 也没有）。
    pub fn has_usage(&self) -> bool {
        self.usage.is_some()
    }

    /// 由外部标记错误（如上游空响应），供 I/O 泵使用。
    pub fn mark_error(&mut self, message: String) {
        self.errored = true;
        self.error_message = Some(message);
    }

    fn ensure_started(&mut self, out: &mut Vec<String>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(sse_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
                    "type": "message",
                    "role": "assistant",
                    "model": Value::Null,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                },
            }),
        ));
    }

    /// 思考期保活：按固定间隔补一条 SSE 注释行。
    ///
    /// 上游思考阶段只吐 `reasoning_content`，转换器不产内容事件；若不加保活，
    /// 下游在这几秒内一个字节都收不到（HTTP 层表现为「卡住不动」）。
    /// 注释行以 `:` 开头，任何符合 SSE 规范的解析器都会忽略它。
    fn keepalive(&mut self, out: &mut Vec<String>) {
        // 保活间隔（毫秒）。取 2s：远小于常见 30s 空闲超时，又不会刷屏。
        const KEEPALIVE_INTERVAL_MS: u128 = 2000;
        let now = std::time::Instant::now();
        let due = match self.last_keepalive {
            None => true,
            Some(prev) => now.saturating_duration_since(prev).as_millis() >= KEEPALIVE_INTERVAL_MS,
        };
        if due {
            self.last_keepalive = Some(now);
            out.push(": ping\n\n".to_string());
        }
    }

    /// 打开文本块（已在文本块中则复用）。
    fn open_text_block(&mut self, out: &mut Vec<String>) {
        if self.open_kind == Some(BlockKind::Text) {
            return;
        }
        self.close_block(out);
        out.push(sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.next_index,
                "content_block": {"type": "text", "text": ""},
            }),
        ));
        self.open_kind = Some(BlockKind::Text);
        self.emitted = true;
    }

    /// 打开 thinking 块（已在 thinking 块中则复用）。
    ///
    /// thinking 块必须排在 text 块之前；若正文块已经打开，后续到达的
    /// `reasoning_content` 片段不再单独开块（顺序非法，直接丢弃）。不设置
    /// `emitted`（该标志专用于正文，避免把思考误判为「已产出内容」）。
    fn open_thinking_block(&mut self, out: &mut Vec<String>) {
        if self.open_kind == Some(BlockKind::Thinking) {
            return;
        }
        if self.open_kind == Some(BlockKind::Text) {
            return;
        }
        self.close_block(out);
        out.push(sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.next_index,
                "content_block": {"type": "thinking", "thinking": ""},
            }),
        ));
        self.open_kind = Some(BlockKind::Thinking);
    }

    /// 打开工具块（tool_use）。
    fn open_tool_block(&mut self, out: &mut Vec<String>, id: &str, name: &str) {
        self.close_block(out);
        let block_id = if id.is_empty() {
            format!("toolu_{}", uuid::Uuid::new_v4().simple())
        } else {
            id.to_string()
        };
        out.push(sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.next_index,
                "content_block": {"type": "tool_use", "id": block_id, "name": name, "input": {}},
            }),
        ));
        self.open_kind = Some(BlockKind::Tool);
        self.saw_tool = true;
        self.emitted = true;
    }

    fn close_block(&mut self, out: &mut Vec<String>) {
        if self.open_kind.take().is_some() {
            out.push(sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": self.next_index}),
            ));
            self.next_index += 1;
        }
    }

    /// 喂入上游文本切片，返回完整 Anthropic SSE 事件块。
    pub fn push(&mut self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        if self.errored {
            return out;
        }
        self.buffer.push_str(text);
        while let Some(pos) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=pos).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            if let Some(error) = value.get("error") {
                // 上游以错误块收场：映射为 Anthropic error 事件。
                self.errored = true;
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("上游返回错误")
                    .to_string();
                self.error_message = Some(message.clone());
                out.push(sse_event(
                    "error",
                    &json!({"type": "error", "error": {"type": "api_error", "message": message}}),
                ));
                return out;
            }
            if value.get("usage").map(Value::is_object).unwrap_or(false) {
                self.usage = Some(super::model_usage::extract_usage(&value));
            }
            if let Some(finish) = value
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .filter(|finish| !finish.is_empty())
            {
                self.stop_reason = map_stop_reason(Some(finish));
            }
            // 工具调用增量：首个 delta 带 id/name → 开 tool_use 块；
            // 之后的 arguments 片段 → input_json_delta。
            if let Some(calls) = value
                .pointer("/choices/0/delta/tool_calls")
                .and_then(Value::as_array)
            {
                for call in calls {
                    let openai_index = call.get("index").and_then(Value::as_u64);
                    let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let arguments = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let starts_new_tool = !id.is_empty()
                        || !name.is_empty()
                        || (openai_index.is_some()
                            && openai_index != self.last_tool_index
                            && self.open_kind == Some(BlockKind::Tool));
                    if starts_new_tool {
                        self.ensure_started(&mut out);
                        self.open_tool_block(&mut out, id, name);
                        self.last_tool_index = openai_index.or(self.last_tool_index);
                    } else if self.open_kind != Some(BlockKind::Tool) {
                        // arguments 先到、id/name 缺失：仍需一个工具块承载。
                        self.ensure_started(&mut out);
                        self.open_tool_block(&mut out, id, name);
                    }
                    if !arguments.is_empty() {
                        self.ensure_started(&mut out);
                        out.push(sse_event(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": self.next_index,
                                "delta": {"type": "input_json_delta", "partial_json": arguments},
                            }),
                        ));
                    }
                }
            }
            // 思考内容：上游在输出 reasoning 时也是"活着"的，且应透传成
            // Anthropic thinking 块——既能让支持它的客户端展示推理过程，
            // 又能在思考期持续产字节（消除「加载特别慢 / 卡住」）。只吐思考、
            // 没吐正文的流（超长上下文常见）不能被误判成「非流式异常响应」。
            let reasoning_text = value
                .pointer("/choices/0/delta/reasoning_content")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !reasoning_text.is_empty() {
                self.saw_reasoning = true;
                // 思考期同样是「流已经开始」：必须立刻下发 message_start，
                // 否则客户端在整段思考期间（实测 3~10s）收不到任何字节。
                self.ensure_started(&mut out);
                // 透传成标准 thinking 块（thinking 必须排在 text 之前）。
                self.open_thinking_block(&mut out);
                if self.open_kind == Some(BlockKind::Thinking) {
                    out.push(sse_event(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": self.next_index,
                            "delta": {"type": "thinking_delta", "thinking": reasoning_text},
                        }),
                    ));
                }
                // 保活注释保留作为兜底：万一上游推理片段间隔 >2s，仍不会真静默。
                self.keepalive(&mut out);
            }
            let delta_text = value
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !delta_text.is_empty() {
                self.ensure_started(&mut out);
                self.open_text_block(&mut out);
                out.push(sse_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.next_index,
                        "delta": {"type": "text_delta", "text": delta_text},
                    }),
                ));
            }
        }
        out
    }

    /// 上游流结束：补齐收尾事件（幂等；error 后为空）。
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.errored {
            return out;
        }
        self.ensure_started(&mut out);
        let output_tokens = self
            .usage
            .as_ref()
            .and_then(|u| u.get("completionTokens"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as u64;
        let input_tokens = self
            .usage
            .as_ref()
            .and_then(|u| u.get("promptTokens"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as u64;
        // 上游全程无文本（如被安全拦截置空）：仍给一个空 text 块，
        // 保证客户端的事件序列完整。
        if self.open_kind.is_none() && self.next_index == 0 {
            self.open_text_block(&mut out);
        }
        self.close_block(&mut out);
        let stop_reason = match self.stop_reason.clone() {
            Value::Null if self.saw_tool => json!("tool_use"),
            Value::Null => json!("end_turn"),
            other => other,
        };
        out.push(sse_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
                "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
            }),
        ));
        out.push(sse_event("message_stop", &json!({"type": "message_stop"})));
        out
    }
}

/// 展开错误链：把 `source()` 一层层拼成一个字符串。
///
/// 🔴 reqwest 的 `Display` **只有最外层包装**——流被截断/超时时它恒为
/// `error decoding response body`，真正的原因（`operation timed out`、
/// `connection closed before message completed`、`connection reset` …）
/// 只存在于 `source()` 链里。只用 `e.to_string()` 会让下面
/// [`upstream_interrupt_message`] 的细分分类**永远落空**，只能给出含糊文案，
/// 拿不到任何可定位的证据（2026-09-18 事故根因之一）。
pub(crate) fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        out.push_str(": ");
        out.push_str(&e.to_string());
        cur = e.source();
    }
    out
}

/// 把 reqwest 的底层流错误翻译成可行动的人话。
///
/// 超长上下文（十几万 token）时上游常在思考阶段被网关切断，原始文案
/// `error decoding response body` 对使用者毫无帮助。GLM / Loomy 两条通路共用。
///
/// ⚠️ 判定顺序有讲究：**必须先判超时/连接类真因**。reqwest 超时或半途断开时
/// `Display` 同样是 `error decoding response body`，若把 decoding 分支放前面，
/// 带上 [`error_chain`] 也没用——细化分类会被包装文案抢走。
pub(crate) fn upstream_interrupt_message(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    // ⚠️ 中文 Windows 的 OS 错误文案是本地化的（如「远程主机强迫关闭了一个现有的连接。
    // (os error 10054)」），不含英文 `connection reset` 字样 —— 必须同时匹配
    // winsock 错误码与中文文案，否则连接类故障又会漏判（2026-09-18 第二次事故）。
    let is_timeout = lower.contains("timed out") || lower.contains("timeout")
        || lower.contains("os error 10060");
    let is_conn_lost = lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("connection aborted")
        || lower.contains("unexpected eof")
        || lower.contains("incomplete message")
        || lower.contains("broken pipe")
        || lower.contains("channel closed")
        // rustls 的「上游没发 TLS close_notify 就断开」：原文是
        // `peer closed connection without sending TLS close_notify`，
        // 注意词序是 `peer closed connection` 而非 `connection closed`，
        // 漏了会把连接类故障误判成「上下文过长」（2026-09-18 ZCode 事故）。
        || lower.contains("peer closed")
        || lower.contains("close_notify")
        // Windows winsock：10054=WSAECONNRESET（远程主机强迫关闭），
        // 10053=WSAECONNABORTED（本机软件中止连接）。
        || lower.contains("os error 10054")
        || lower.contains("os error 10053")
        || raw.contains("远程主机强迫关闭")
        || raw.contains("强迫关闭");
    let detail = if is_timeout {
        "本机侧读超时上限触发（连续无数据到达超过上限，上游长时间不吐字节）"
    } else if is_conn_lost {
        "上游连接被重置/切断（上游主动断开，或中间链路中断）"
    } else if lower.contains("decoding response body") {
        "响应在传输中被切断（多半是上下文过长，上游未输出完就被网关断开）"
    } else if lower.contains("body") {
        "响应体读取失败（上游可能在输出过程中断开）"
    } else {
        "上游流中断"
    };
    // 超时/链路类属于瞬时故障，直接重试即可；上下文类才需要压缩会话。
    let advice = if is_timeout || is_conn_lost {
        "属瞬时故障，可直接重试；若持续出现请查看网关侧超时配置"
    } else {
        "建议新开会话或压缩上下文后重试"
    };
    format!("上游流中断：{detail}（原文: {raw}）。{advice}")
}

/// 把上游 OpenAI SSE 流泵成 Anthropic SSE 流（含计量落库）。
pub(crate) async fn pump_anthropic_stream(
    prepared: super::glm_proxy::PreparedStream,
    mut resp: reqwest::Response,
) -> GlmUpstreamStream {
    use bytes::Bytes;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    tokio::spawn(async move {
        let mut transformer = AnthropicSseTransformer::new();
        let mut resp = resp;
        // 留存上游原文片段：上游 200 但返回非 SSE（或空流）时用于诊断。
        let mut raw_sample = String::new();
        let mut raw_len = 0usize;
        let mut client_gone = false;
        // 跨 chunk 的 UTF-8 半字符尾部（见 decode_stream_chunk 注释）。
        let mut carry: Vec<u8> = Vec::new();
        // 首字延迟：上游首个字节到达耗时（预填充时间）。
        let mut ttfb_ms: Option<i64> = None;
        'pump: loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if ttfb_ms.is_none() {
                        ttfb_ms = Some(crate::modules::config::now_ms().saturating_sub(prepared.started));
                    }
                    raw_len += chunk.len();
                    if raw_sample.len() < 512 {
                        raw_sample.push_str(&String::from_utf8_lossy(&chunk));
                    }
                    let text = decode_stream_chunk(&mut carry, &chunk);
                    for event in transformer.push(&text) {
                        if tx.send(Ok(Bytes::from(event))).await.is_err() {
                            // 客户端已断开（用户中止/切页）：不算错误。
                            client_gone = true;
                            break 'pump;
                        }
                    }
                }
                Ok(None) => break 'pump,
                Err(e) => {
                    // 上游中断：透出 error 事件，避免客户端挂在半开流上。
                    // reqwest 的 Display（`error decoding response body`）会掩盖真因，
                    // 取完整 source 链再翻译成可行动的说明。
                    transformer.mark_error(upstream_interrupt_message(&error_chain(&e)));
                    let _ = tx
                        .send(Ok(Bytes::from(sse_event(
                            "error",
                            // overloaded_error = 上游过载/瞬时故障，客户端可重试；
                            // api_error 会让客户端判定 retryable=false 直接放弃。
                            &json!({"type": "error", "error": {"type": "overloaded_error", "message": transformer.error()}}),
                        ))))
                        .await;
                    break 'pump;
                }
            }
        }
        if !carry.is_empty() {
            // 流结束时仍有未凑齐的码点尾部：lossy 产出最后一个字符，
            // 避免把完整帧的结尾字节丢掉导致该行解析失败。
            let tail = String::from_utf8_lossy(&carry).into_owned();
            carry.clear();
            for event in transformer.push(&tail) {
                if tx.send(Ok(Bytes::from(event))).await.is_err() {
                    client_gone = true;
                    break;
                }
            }
        }
        // 上游 HTTP 200 却既无内容也无 usage：多半是网关返回了非流式的
        // 错误体/空体。必须显式报错，否则客户端只看到「输出莫名中断」。
        //
        // 注意：流已标记过错误时不能再覆盖——否则真实的「上游流中断」
        // 会被这条兜底文案顶掉，记录里只剩无意义的「非流式异常响应」。
        if !client_gone
            && transformer.ok()
            && !transformer.emitted_anything()
            && !transformer.has_usage()
        {
            let sample = raw_sample
                .replace(['\n', '\r'], " ")
                .trim()
                .chars()
                .take(300)
                .collect::<String>();
            let message = if sample.is_empty() {
                format!("上游返回空响应（收到 {raw_len} 字节，无内容无 usage）")
            } else {
                format!("上游返回非流式异常响应（{raw_len} 字节）：{sample}")
            };
            transformer.mark_error(message);
            let _ = tx
                .send(Ok(Bytes::from(sse_event(
                    "error",
                    // 同样按瞬时故障处理：空响应/非流式异常体多为网关抖动，值得重试。
                    &json!({"type": "error", "error": {"type": "overloaded_error", "message": transformer.error()}}),
                ))))
                .await;
        }
        let final_events = transformer.finish();
        for event in final_events {
            let _ = tx.send(Ok(Bytes::from(event))).await;
        }
        // 计量：usage 取自上游 SSE，积分走流水/余额差归因。
        let mut usage = transformer.usage();
        if let (Some(ttfb), Some(obj)) = (ttfb_ms, usage.as_object_mut()) {
            obj.insert("ttfbMs".to_string(), json!(ttfb));
        }
        super::glm_proxy::note_substitution(
            &mut usage,
            &prepared.requested_model,
            &prepared.route.body_model,
            prepared.skipped_multimodal,
        );
        let (credits, ledger_id) = prepared.measurement.finish().await;
        if transformer.ok() {
            super::glm_proxy::record_success(
                &prepared.route,
                &prepared.account,
                &usage,
                credits,
                ledger_id,
                prepared.started,
                crate::modules::config::now_ms(),
            );
        } else {
            super::glm_proxy::record_and_notify_failure(
                Some(&prepared.route),
                &prepared.requested_model,
                prepared.started,
                &transformer.error(),
                prepared.skipped_multimodal,
            );
        }
    });
    GlmUpstreamStream {
        content_type: "text/event-stream".to_string(),
        rx,
    }
}

/// 让 **ChatGLM 网页引擎兜底**的输出也能走 Anthropic SSE：复用同一个
/// `AnthropicSseTransformer`，保证事件序列与 autoclaw 链路逐帧一致。
///
/// 背景（1.0.4）：`glm_proxy::chat_completions()` 在 autoclaw 账号池失败时会兜底到
/// `glm_web`，而 `/v1/messages`（`anthropic_messages()`）原先没有这层兜底 ——
/// 同一个上游、两条协议路径命运不同：kind=anthropic 的客户端（ZCode 等）在账号池
/// 被限流/封号时必然 502，而 OpenAI 路径却正常。本函数补齐该不对称。
///
/// 两种入参都归一成 OpenAI 风格 SSE 文本再喂转换器：
/// - `Stream`：逐块透传（跨 chunk 的 UTF-8 半字符交给 `decode_stream_chunk`，见其注释）
/// - `Json`：把内容/思维链合成为 SSE 帧，避免为「非流式兜底结果」单写一套事件生成
pub(crate) fn pump_anthropic_from_web(
    out: super::glm_web::GlmChatOutput,
) -> GlmUpstreamStream {
    use bytes::Bytes;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    tokio::spawn(async move {
        let mut t = AnthropicSseTransformer::new();

        async fn emit(
            tx: &tokio::sync::mpsc::Sender<Result<Bytes, String>>,
            events: Vec<String>,
        ) -> bool {
            for event in events {
                if tx.send(Ok(Bytes::from(event))).await.is_err() {
                    return false; // 客户端已断开
                }
            }
            true
        }

        match out {
            super::glm_web::GlmChatOutput::Stream(mut web) => {
                let mut carry: Vec<u8> = Vec::new();
                while let Some(item) = web.rx.recv().await {
                    match item {
                        Ok(chunk) => {
                            let text = decode_stream_chunk(&mut carry, &chunk);
                            if !emit(&tx, t.push(&text)).await {
                                return;
                            }
                        }
                        Err(e) => {
                            t.mark_error(e);
                            break;
                        }
                    }
                }
            }
            super::glm_web::GlmChatOutput::Json(value) => {
                let content = value
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let reasoning = value
                    .pointer("/choices/0/message/reasoning_content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut text = String::new();
                if !reasoning.is_empty() {
                    text.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [{"index": 0, "delta": {"reasoning_content": reasoning}}]})
                    ));
                }
                if !content.is_empty() {
                    text.push_str(&format!(
                        "data: {}\n\n",
                        json!({"choices": [{"index": 0, "delta": {"content": content}}]})
                    ));
                }
                text.push_str("data: [DONE]\n\n");
                if !emit(&tx, t.push(&text)).await {
                    return;
                }
            }
        }

        // 与 autoclaw 链路同样：错误流先下发 error 事件，再补齐收尾（幂等）。
        if !t.ok() {
            let _ = tx
                .send(Ok(Bytes::from(sse_event(
                    "error",
                    &json!({"type": "error", "error": {"type": "overloaded_error", "message": t.error()}}),
                ))))
                .await;
        }
        let _ = emit(&tx, t.finish()).await;
    });

    GlmUpstreamStream {
        content_type: "text/event-stream".to_string(),
        rx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 多字节 UTF-8 码点被 TCP chunk 切在中间时，decode_stream_chunk 必须
    /// 把不完整的尾部留到下一次拼接，绝不整块丢弃——这正是 2026-09-16
    /// 「上游返回非流式异常响应」事故的根因回归测试。
    #[test]
    fn decode_stream_chunk_survives_split_multibyte() {
        // "数据" 每个汉字 3 字节：E6 95 B0 E6 8D B0
        let full = "数据".as_bytes();
        assert_eq!(full.len(), 6);
        let mut carry = Vec::new();
        // 切点落在第一个字中间（前 2 字节 + 后 4 字节）
        let a = decode_stream_chunk(&mut carry, &full[..2]);
        // 第一个字还不完整，尚未产出任何字符
        assert_eq!(a, "");
        let b = decode_stream_chunk(&mut carry, &full[2..]);
        // 第二个完整字 + 第一个拼回的字都产出
        assert_eq!(a + &b, "数据");
        assert!(carry.is_empty());
    }

    #[test]
    fn decode_stream_chunk_assembles_sse_frame_across_chunks() {
        // 模拟真实上游：一帧含中文的 tool_calls arguments 被切成两半，
        // 转换器必须最终收到完整帧并解析出内容。
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"你好世界\"}}]}\n\n";
        let bytes = frame.as_bytes();
        // 在「你」字中间切（你 = E4 BD A0，切在前 1 字节后）
        let cut = frame.find('你').unwrap();
        let split = cut + 1; // 落在「你」的第 2 字节之前
        let mut t = AnthropicSseTransformer::new();
        let mut carry = Vec::new();
        let t1 = decode_stream_chunk(&mut carry, &bytes[..split]);
        for _ in t.push(&t1) {}
        let t2 = decode_stream_chunk(&mut carry, &bytes[split..]);
        let events = t.push(&t2);
        assert!(carry.is_empty(), "帧结束后 carry 应清空");
        // 转换器应产出 message_start + content_block_start(text) + delta
        let joined = events.concat();
        assert!(joined.contains("message_start"), "应已下发 message_start");
        assert!(t.emitted_content(), "含正文的帧必须判定为已产出内容");
    }

    /// 事故形态回归：tool_calls 的 arguments 里含中文 Windows 路径，帧被
    /// TCP 切在中文字符中间。修复前整块丢弃 → 转换器零产出 → 误报
    /// 「上游返回非流式异常响应」；修复后必须能完整还原且 JSON 可解析。
    #[test]
    fn tool_call_arguments_survive_utf8_split() {
        let command = "cd \"E:\\WorkBuddy自动签到助手\" && cargo build";
        let frame = json!({
            "id": "chunk_1",
            "object": "chat.completion.chunk",
            "model": "glm-5.3-flash",
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": [{
                    "id": "call_abc",
                    "index": 0,
                    "type": "function",
                    "function": {"name": "Bash", "arguments": serde_json::to_string(&json!({"command": command})).unwrap()},
                }]},
            }],
        });
        let sse = format!("data: {frame}\n\n");
        let bytes = sse.as_bytes();

        // 在第一个中文字符（"WorkBuddy" 后的「自」）中间切一刀，逐块喂入
        let cut = sse.find('自').unwrap() + 1;
        let mut carry = Vec::new();
        let mut t = AnthropicSseTransformer::new();
        let mut all = Vec::new();
        all.extend(t.push(&decode_stream_chunk(&mut carry, &bytes[..cut])));
        all.extend(t.push(&decode_stream_chunk(&mut carry, &bytes[cut..])));
        assert!(carry.is_empty());

        // 必须已开 tool_use 块，且判定为「有产出」
        assert!(t.emitted_content(), "含 tool_calls 的帧必须判定已产出内容");
        assert!(all.concat().contains("tool_use"), "应下发 tool_use 块");

        // 拼回的 partial_json 必须是完整可解析的 JSON，且 command 原样保留中文
        let mut partial = String::new();
        for ev in &all {
            for line in ev.lines() {
                let Some(data) = line.strip_prefix("data: ") else { continue };
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
                if v.get("type").and_then(Value::as_str) == Some("content_block_delta") {
                    if let Some(pj) = v.pointer("/delta/partial_json").and_then(Value::as_str) {
                        partial.push_str(pj);
                    }
                }
            }
        }
        let parsed: Value =
            serde_json::from_str(&partial).expect("跨 chunk 拼回的 arguments 必须是合法 JSON");
        assert_eq!(parsed["command"], command, "中文路径必须一字不差");
        assert!(partial.contains("自动签到助手"), "中文不得被替换为 U+FFFD");
    }

    #[test]
    fn anthropic_request_converts_to_openai_payload() {
        let body = json!({
            "model": "glm-5.3",
            "max_tokens": 256,
            "system": "你是助手",
            "stop_sequences": ["END"],
            "temperature": 0.5,
            "messages": [
                {"role": "user", "content": "你好"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "你好！"},
                    {"type": "tool_use", "id": "t1", "name": "x", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "结果"},
                    {"type": "text", "text": "继续"}
                ]}
            ],
        });
        let (payload, model, stream) = to_openai_payload(&body).unwrap();
        assert_eq!(model, "glm-5.3");
        assert!(!stream);
        assert_eq!(payload["max_tokens"], 256);
        assert_eq!(payload["stop"], json!(["END"]));
        assert_eq!(payload["temperature"], 0.5);
        let messages = payload["messages"].as_array().unwrap();
        // system + user + assistant(text+tool_calls) + tool + user
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "你是助手");
        assert_eq!(messages[2]["content"], "你好！");
        // assistant 的 tool_use → OpenAI tool_calls
        assert_eq!(messages[2]["tool_calls"][0]["id"], "t1");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "x");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["arguments"], "{}");
        // user 的 tool_result → role:"tool" 消息
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "t1");
        assert_eq!(messages[3]["content"], "结果");
        // 其余文本合成 user 消息
        assert_eq!(messages[4]["role"], "user");
        assert_eq!(messages[4]["content"], "继续");
    }

    #[test]
    fn anthropic_image_blocks_convert_to_openai_parts() {
        let body = json!({
            "model": "glm-5.3-flash",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "这是什么颜色？"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}}
            ]}]
        });
        let (payload, _, _) = to_openai_payload(&body).unwrap();
        let content = &payload["messages"][0]["content"];
        assert!(content.is_array(), "含图片的消息应为内容数组：{content}");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "这是什么颜色？");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAA");

        // url 型 source
        let url_body = json!({
            "model": "glm-5.3-flash",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}}
            ]}]
        });
        let (payload, _, _) = to_openai_payload(&url_body).unwrap();
        assert_eq!(
            payload["messages"][0]["content"][0]["image_url"]["url"],
            "https://example.com/a.png"
        );

        // 纯文本消息仍保持字符串形式（不回归）
        let plain = json!({
            "model": "glm-5.3",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "你好"}]}]
        });
        let (payload, _, _) = to_openai_payload(&plain).unwrap();
        assert_eq!(payload["messages"][0]["content"], "你好");

        // 无 source 的异常图片块被安全忽略
        let broken = json!({
            "model": "glm-5.3-flash",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": [{"type": "image"}]}]
        });
        let (payload, _, _) = to_openai_payload(&broken).unwrap();
        assert_eq!(payload["messages"][0]["content"], "");
    }

    #[test]
    fn anthropic_tools_and_tool_choice_map_to_openai() {
        let body = json!({
            "model": "glm-5.3-flash",
            "max_tokens": 1024,
            "tools": [
                {
                    "name": "Read",
                    "description": "读文件",
                    "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
                },
                {"description": "缺少 name，应被忽略"}
            ],
            "tool_choice": {"type": "any"},
            "messages": [{"role": "user", "content": "hi"}],
        });
        let (payload, _, _) = to_openai_payload(&body).unwrap();
        let tools = payload["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "Read");
        assert_eq!(tools[0]["function"]["description"], "读文件");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(payload["tool_choice"], "required");

        let named = json!({
            "model": "glm-5.3-flash",
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "Bash"},
            "messages": [{"role": "user", "content": "hi"}],
        });
        let (payload, _, _) = to_openai_payload(&named).unwrap();
        assert_eq!(payload["tool_choice"]["function"]["name"], "Bash");

        // 没有 tools 时不得输出 tools 字段（避免上游 500）。
        let plain = json!({"model": "glm-5.3", "messages": [{"role": "user", "content": "hi"}]});
        let (payload, _, _) = to_openai_payload(&plain).unwrap();
        assert!(payload.get("tools").is_none());
        assert!(payload.get("tool_choice").is_none());
    }

    #[test]
    fn missing_max_tokens_falls_back_to_4096() {
        let body = json!({
            "model": "glm-5.3",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let (payload, _, _) = to_openai_payload(&body).unwrap();
        assert_eq!(payload["max_tokens"], 4096);
    }

    #[test]
    fn anthropic_request_rejects_bad_roles_and_empty_messages() {
        let bad_role = json!({
            "model": "glm-5.3",
            "messages": [{"role": "tool", "content": "x"}],
        });
        assert!(to_openai_payload(&bad_role).is_err());
        let empty = json!({"model": "glm-5.3", "messages": []});
        assert!(to_openai_payload(&empty).is_err());
        let missing = json!({"model": "glm-5.3"});
        assert!(to_openai_payload(&missing).is_err());
    }

    #[test]
    fn openai_response_converts_to_anthropic_message() {
        let openai = json!({
            "id": "chatcmpl-1",
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": "答案"}
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 34, "total_tokens": 46},
        });
        let message = from_openai_response(&openai, "glm-5.3");
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["model"], "glm-5.3");
        assert_eq!(message["id"], "msg_chatcmpl-1");
        assert_eq!(message["content"][0]["type"], "text");
        assert_eq!(message["content"][0]["text"], "答案");
        assert_eq!(message["stop_reason"], "end_turn");
        assert_eq!(message["usage"]["input_tokens"], 12);
        assert_eq!(message["usage"]["output_tokens"], 34);
    }

    #[test]
    fn finish_reason_maps_to_stop_reasons() {
        assert_eq!(map_stop_reason(Some("length")), json!("max_tokens"));
        assert_eq!(map_stop_reason(Some("tool_calls")), json!("tool_use"));
        assert_eq!(map_stop_reason(Some("other")), Value::Null);
        assert_eq!(map_stop_reason(None), Value::Null);
    }

    fn delta(text: &str) -> String {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": text}}]})
        )
    }

    #[test]
    fn sse_transformer_emits_full_anthropic_event_sequence() {
        let mut t = AnthropicSseTransformer::new();
        // 故意把上游切块打断在事件中间，验证缓冲逻辑。
        let upstream = format!(
            "{}{}{}",
            delta("你"),
            delta("好"),
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": {}}], "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}})
            )
        );
        let mut events = String::new();
        for chunk in [upstream.as_str(), ""] {
            for event in t.push(chunk) {
                events.push_str(&event);
            }
        }
        for event in t.finish() {
            events.push_str(&event);
        }
        assert!(events.contains("event: message_start"));
        assert!(events.contains("event: content_block_start"));
        assert!(events.contains("\"text\":\"你\""));
        assert!(events.contains("好"));
        assert!(events.contains("event: content_block_delta"));
        assert!(events.contains("event: content_block_stop"));
        assert!(events.contains("event: message_delta"));
        assert!(events.contains("\"output_tokens\":2"));
        assert!(events.contains("event: message_stop"));
        assert!(events.ends_with("\n\n"));
        let usage = t.usage();
        eprintln!("=== EVENTS ===
{events}
=== USAGE: {usage} ===");
        assert_eq!(usage["promptTokens"], 7.0);
        assert_eq!(usage["completionTokens"], 2.0);
        assert!(t.ok());
    }

    #[test]
    fn sse_transformer_without_content_still_completes() {
        let mut t = AnthropicSseTransformer::new();
        let mut events = String::new();
        for event in t.push("data: [DONE]\n\n") {
            events.push_str(&event);
        }
        for event in t.finish() {
            events.push_str(&event);
        }
        assert!(events.contains("event: message_start"));
        assert!(events.contains("event: content_block_start"));
        assert!(events.contains("event: content_block_stop"));
        assert!(events.contains("event: message_stop"));
    }

    #[test]
    fn sse_transformer_maps_upstream_error_event() {
        let mut t = AnthropicSseTransformer::new();
        let events = t.push("data: {\"error\": {\"message\": \"限流\"}}\n\n");
        let joined = events.join("");
        assert!(joined.contains("event: error"));
        assert!(joined.contains("限流"));
        assert!(!t.ok());
        // 出错后不再吐事件，finish 也为空。
        assert!(t.push(&delta("x")).is_empty());
        assert!(t.finish().is_empty());
    }

    #[test]
    fn sse_transformer_tolerates_event_and_comment_lines() {
        let mut t = AnthropicSseTransformer::new();
        let chunk = ": keep-alive\nevent: ping\ndata: {\"choices\":[{\"delta\":{\"content\":\"A\"}}]}\n\n";
        let events = t.push(chunk);
        let joined = events.join("");
        assert!(joined.contains("content_block_delta"));
        assert!(joined.contains("A"));
    }

    #[test]
    fn openai_tool_calls_convert_to_tool_use_blocks() {
        let openai = json!({
            "id": "chatcmpl-2",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": "先跑一下：",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "Bash", "arguments": "{\"cmd\":\"python -V\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20},
        });
        let message = from_openai_response(&openai, "glm-5.3-flash");
        assert_eq!(message["content"][0]["type"], "text");
        assert_eq!(message["content"][1]["type"], "tool_use");
        assert_eq!(message["content"][1]["id"], "call_1");
        assert_eq!(message["content"][1]["name"], "Bash");
        assert_eq!(message["content"][1]["input"]["cmd"], "python -V");
        assert_eq!(message["stop_reason"], "tool_use");
    }

    #[test]
    fn transformer_flags_empty_stream_for_diagnosis() {
        let mut t = AnthropicSseTransformer::new();
        assert!(!t.emitted_content());
        assert!(!t.has_usage());
        // 上游只给了 [DONE]：典型的「200 但空流」。
        assert!(t.push("data: [DONE]\n\n").is_empty());
        assert!(!t.emitted_content());
        assert!(!t.has_usage());
        // 泵据此标记错误：不再静默返回空回答。
        t.mark_error("上游返回空响应（收到 12 字节，无内容无 usage）".to_string());
        assert!(!t.ok());
        assert!(t.finish().is_empty());
        assert!(t.error().contains("空响应"));
    }

    #[test]
    fn transformer_separates_content_and_usage_signals() {
        let mut t = AnthropicSseTransformer::new();
        t.push(&delta("hi"));
        assert!(t.emitted_content());
        assert!(!t.has_usage());
        t.push(&format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {}}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
        ));
        assert!(t.has_usage());
    }

    /// 回归：上游长时间只吐思考（`reasoning_content`）时，不能按「空流/非流式
    /// 异常响应」处理——超长上下文下这是常态。
    #[test]
    fn reasoning_only_stream_counts_as_emitted() {
        let mut t = AnthropicSseTransformer::new();
        t.push(&format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"reasoning_content": "让我先梳理一下…"}}]})
        ));
        assert!(!t.emitted_content(), "思考不应算作正文");
        assert!(t.emitted_anything(), "思考也算上游有产出，避免误判为空流");
        // 流正常结束时仍给出完整的 Anthropic 事件序列（空 text 块 + stop）。
        let events = t.finish();
        assert!(!events.is_empty());
        assert!(events.iter().any(|e| e.contains("message_stop")));
    }

    /// 回归：思考内容透传成标准 Anthropic thinking 块，且排在 text 块之前。
    #[test]
    fn reasoning_passthrough_emits_thinking_block_before_text() {
        let mut t = AnthropicSseTransformer::new();
        let r1 = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"让我先想一下…\"}}]}\n\n";
        let r2 = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"继续推理\"}}]}\n\n";
        let c = "data: {\"choices\":[{\"delta\":{\"content\":\"答案\"}}]}\n\n";
        let usage =
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n\n";
        let mut events = String::new();
        for chunk in [r1, r2, c, usage, "data: [DONE]\n\n"] {
            for e in t.push(chunk) {
                events.push_str(&e);
            }
        }
        for e in t.finish() {
            events.push_str(&e);
        }
        assert!(events.contains("\"type\":\"thinking\""), "应有 thinking 块起始");
        assert!(
            events.contains("\"type\":\"thinking_delta\""),
            "应有 thinking_delta"
        );
        assert!(events.contains("让我先想一下…"), "思考内容应透传");
        assert!(events.contains("继续推理"), "后续思考片段应追加到同一块");
        assert!(events.contains("\"type\":\"text\""), "正文块仍存在");
        assert!(events.contains("答案"));
        assert!(t.ok());
        // thinking 块必须排在 text 块之前（与 JSON key 顺序无关）。
        let pos_think = events
            .find("\"type\":\"thinking\"")
            .expect("thinking content_block_start");
        let pos_text = events
            .find("\"type\":\"text\"")
            .expect("text content_block_start");
        assert!(pos_think < pos_text, "thinking 块必须排在 text 块之前");
        // 两个 content 块应各自 start + stop。
        assert_eq!(events.matches("event: content_block_start").count(), 2);
        assert_eq!(events.matches("event: content_block_stop").count(), 2);
    }

    /// 回归：流已标记「上游流中断」后，兜底文案不得覆盖它。
    #[test]
    fn interrupt_error_is_not_overwritten_by_fallback() {
        let mut t = AnthropicSseTransformer::new();
        t.mark_error(upstream_interrupt_message("error decoding response body"));
        assert!(!t.ok());
        // 模拟泵里那段兜底判定：errored 时必须跳过。
        let should_fallback = t.ok() && !t.emitted_anything() && !t.has_usage();
        assert!(!should_fallback, "已报错的流不能被兜底文案顶掉");
        assert!(t.error().contains("上游流中断"));
        assert!(t.error().contains("上下文过长"), "应给出可行动的建议");
        assert!(t.error().contains("error decoding response body"), "保留原文便于排查");
    }

    #[test]
    fn interrupt_message_maps_common_causes() {
        let cased = [
            ("error decoding response body", "上下文过长"),
            ("connection reset by peer", "连接被重置"),
            ("operation timed out", "超时"),
        ];
        for (raw, expect) in cased {
            assert!(
                upstream_interrupt_message(raw).contains(expect),
                "{raw} 应映射到 {expect}"
            );
        }
    }

    /// 回归（2026-09-18 ZCode 事故）：rustls 的「上游未发 close_notify 就断开」
    /// 原文词序是 `peer closed connection`，必须归入连接类（可直接重试），
    /// 不得落进「上下文过长」分支误导用户去压缩会话。
    #[test]
    fn peer_closed_without_close_notify_is_conn_lost() {
        let raw = "error decoding response body: error reading a body from connection: \
                   peer closed connection without sending TLS close_notify: \
                   https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof";
        let msg = upstream_interrupt_message(raw);
        assert!(msg.contains("连接被重置/切断"), "应归入连接类：{msg}");
        assert!(msg.contains("可直接重试"), "连接类是瞬时故障：{msg}");
        assert!(!msg.contains("压缩上下文"), "不得建议压缩上下文：{msg}");
    }

    /// 回归（2026-09-18 14:45 ZCode 二次事故）：中文 Windows 的 OS 错误文案是
    /// 本地化的，`os error 10054`（WSAECONNRESET）+「远程主机强迫关闭」必须归入连接类。
    #[test]
    fn localized_wsaconnreset_is_conn_lost() {
        let raw = "error decoding response body: error reading a body from connection: \
                   远程主机强迫关闭了一个现有的连接。 (os error 10054)";
        let msg = upstream_interrupt_message(raw);
        assert!(msg.contains("连接被重置/切断"), "应归入连接类：{msg}");
        assert!(msg.contains("可直接重试"), "连接类是瞬时故障：{msg}");
        assert!(!msg.contains("压缩上下文"), "不得建议压缩上下文：{msg}");
    }

    /// 模拟 reqwest 的两层错误：外层 Display 恒为 `error decoding response body`，
    /// 真因在 source 里。
    #[derive(Debug)]
    struct LeafErr(String);
    impl std::fmt::Display for LeafErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for LeafErr {}

    #[derive(Debug)]
    struct WrapErr(LeafErr);
    impl std::fmt::Display for WrapErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "error decoding response body")
        }
    }
    impl std::error::Error for WrapErr {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// 回归：真因藏在 source 链里时，必须命中真因分支，不能被外层包装文案抢走。
    #[test]
    fn error_chain_reveals_nested_cause() {
        let err = WrapErr(LeafErr("operation timed out".to_string()));
        let chain = error_chain(&err);
        assert!(chain.contains("error decoding response body"));
        assert!(chain.contains("operation timed out"), "必须展开 source: {chain}");
    }

    /// 回归（2026-09-18 事故）：带 source 链的超时不得再被误判成「上下文过长」。
    #[test]
    fn timeout_in_source_chain_beats_decoding_wrapper() {
        for cause in ["operation timed out", "connection reset by peer"] {
            let chain = error_chain(&WrapErr(LeafErr(cause.to_string())));
            let msg = upstream_interrupt_message(&chain);
            assert!(
                !msg.contains("上下文过长"),
                "真因 {cause} 被外层 decoding 包装误导了: {msg}"
            );
        }
        let msg = upstream_interrupt_message(&error_chain(&WrapErr(LeafErr(
            "operation timed out".to_string(),
        ))));
        assert!(msg.contains("读超时"), "应指向本机侧读超时: {msg}");
        // 超时属瞬时故障，建议应是「可直接重试」而非「压缩上下文」。
        assert!(msg.contains("可直接重试"), "应给出正确的处置建议: {msg}");
    }

    #[test]
    fn sse_transformer_emits_tool_use_stream_events() {
        let mut t = AnthropicSseTransformer::new();
        let upstream = [
            json!({"choices": [{"delta": {"content": "先验证："}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "type": "function",
                 "function": {"name": "Bash", "arguments": ""}}
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"cmd\":"}}
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "\"python -V\"}"}}
            ]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        ]
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>();
        let mut events = String::new();
        for event in t.push(&upstream) {
            events.push_str(&event);
        }
        for event in t.finish() {
            events.push_str(&event);
        }
        assert!(events.contains("\"type\":\"text\""));
        assert!(events.contains("\"call_1\""));
        assert!(events.contains("\"Bash\""));
        assert!(events.contains("\"type\":\"tool_use\""));
        assert!(events.contains("input_json_delta"));
        assert!(events.contains("partial_json"));
        assert!(events.contains("{\\\"cmd\\\":"));
        assert!(events.contains("\"stop_reason\":\"tool_use\""));
        // 文本块 index 0、工具块 index 1，且各自独立闭合。
        assert_eq!(events.matches("event: content_block_start").count(), 2);
        assert!(events.contains("\"index\":1"));
        assert_eq!(events.matches("event: content_block_stop").count(), 2);
        assert!(t.ok());
    }

    /// 1.0.4：glm_web 非流式兜底结果 → 必须合成出完整 Anthropic SSE 事件序列。
    #[tokio::test]
    async fn pump_from_web_json_synthesizes_anthropic_events() {
        let out = crate::modules::glm_web::GlmChatOutput::Json(json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "你好", "reasoning_content": "想一下"}
            }]
        }));
        let mut stream = pump_anthropic_from_web(out);
        let mut all = String::new();
        while let Some(item) = stream.rx.recv().await {
            if let Ok(bytes) = item {
                all.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        assert!(all.contains("event: message_start"), "缺 message_start");
        assert!(all.contains("event: content_block_delta"), "缺 delta");
        assert!(all.contains("你好"), "正文未透传");
        assert!(all.contains("message_stop"), "缺收尾事件");
        assert_eq!(stream.content_type, "text/event-stream");
    }

    /// 1.0.4：glm_web 流式兜底 → 逐块喂转换器；且**多字节码点被切在 chunk 中间**
    /// 时不得丢字节（沿用 decode_stream_chunk 的跨 chunk 拼接）。
    #[tokio::test]
    async fn pump_from_web_stream_keeps_split_utf8_intact() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, String>>(8);
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"中文\"}}]}\n\n";
        let bytes = frame.as_bytes().to_vec();
        let cut = frame.find('中').unwrap() + 1; // 切在「中」的第 2 字节前
        let (a, b) = (bytes[..cut].to_vec(), bytes[cut..].to_vec());
        tokio::spawn(async move {
            let _ = tx.send(Ok(bytes::Bytes::from(a))).await;
            let _ = tx.send(Ok(bytes::Bytes::from(b))).await;
        });
        let out = crate::modules::glm_web::GlmChatOutput::Stream(crate::modules::glm_web::GlmWebStream { rx });
        let mut stream = pump_anthropic_from_web(out);
        let mut all = String::new();
        while let Some(item) = stream.rx.recv().await {
            if let Ok(chunk) = item {
                all.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
        assert!(all.contains("中文"), "跨 chunk 的中文被丢字节: {all}");
        assert!(all.contains("event: message_start"));
    }
}
