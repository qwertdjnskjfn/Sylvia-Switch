//! Qwen 网页版通道的 MCP（Model Context Protocol）反向代理。
//!
//! 把 Sylvia Switch 的 `/qwen/v1` 能力以 MCP 工具形式暴露给支持 MCP 的客户端
//! （Claude Desktop / Cursor / zcode 等）。协议层实现 **Streamable HTTP**：
//! 单端点 `POST /mcp`，JSON-RPC 2.0 请求体进出，响应为 `application/json`
//! （需要流式时返回 `text/event-stream`）。
//!
//! ## 方法集（2026-09-20 实装）
//! - `initialize` → 返回 protocolVersion / capabilities / serverInfo
//! - `notifications/initialized`（通知，无响应体）
//! - `tools/list` → 暴露 `qwen_chat`（对话）与 `qwen_models`（模型目录）
//! - `tools/call` → 转发到本机 `/qwen/v1/chat/completions`（内部直调 core，
//!   不走 HTTP 回环，省一跳并复用鉴权与日志）
//! - `ping` → 空结果
//!
//! ## 上下文传递
//! MCP 的 `tools/call` 是**无状态单次调用**：客户端每次传入完整 `messages`
//! 数组（或 `prompt` 单轮），网关把消息透传给 `/qwen/v1` 的 OpenAI 形状链路
//! （`flatten_messages` 会把多轮拍平为 chat.qwen.ai 的单 prompt 转写）。
//! 因此「上下文」由调用方维护并按次传入，网关不保存会话状态——这与 MCP
//! 规范一致，也让上游 WAF 无跨请求关联可抓。

use serde_json::{json, Value};

use crate::modules::qwen;

/// MCP 协议版本（客户端不指定时回退此值）。
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
pub const SERVER_NAME: &str = "sylvia-qwen-web";
pub const SERVER_VERSION: &str = "1.0.0";

/// MCP 工具清单（tools/list 返回体）。
pub fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "qwen_chat",
                "description": "通过 Qwen 网页版通道（chat.qwen.ai）发起对话。支持多轮上下文（传入完整 messages）、思考过程（reasoning_content）与流式输出。可用模型见 qwen_models 工具：qwen3.8-max（旗舰 1M 上下文）、qwen3.7-plus（高性能多模态）、qwen3.8-omni-flash（全模态）等。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "messages": {
                            "type": "array",
                            "description": "OpenAI 格式消息数组；多轮对话请传入完整历史（含本轮的 user 消息）。与 prompt 二选一。",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "role": {"type": "string", "enum": ["system", "user", "assistant"]},
                                    "content": {"type": "string"}
                                },
                                "required": ["role", "content"]
                            }
                        },
                        "prompt": {
                            "type": "string",
                            "description": "单轮快捷输入（等价于一条 user 消息）。与 messages 二选一，messages 优先。"
                        },
                        "model": {
                            "type": "string",
                            "description": "模型 ID，默认 qwen3.7-plus。可用值见 qwen_models。"
                        },
                        "stream": {
                            "type": "boolean",
                            "description": "是否流式（网关内部聚合后返回完整文本；MCP 响应本身为单次 JSON）。默认 false。"
                        },
                        "tools": {
                            "type": "array",
                            "description": "可选：下游工具定义（OpenAI tools 形状）。网页版上游不原生支持 tools，网关会注入为提示词协议并在回复中嗅探合成 tool_calls。"
                        }
                    }
                }
            },
            {
                "name": "qwen_models",
                "description": "列出 Qwen 网页版通道当前可用的模型目录（动态取自上游）。",
                "inputSchema": {"type": "object", "properties": {}}
            }
        ]
    })
}

/// MCP tools/call 分发。
pub async fn call_tool(name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "qwen_models" => {
            let models = qwen::models_payload().await;
            let list: Vec<String> = models
                .get("data")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Ok(mcp_text_result(&format!(
                "Qwen 网页版通道可用模型（{} 个）：\n{}",
                list.len(),
                list.join("\n")
            )))
        }
        "qwen_chat" => {
            let body = build_chat_body(arguments)?;
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("qwen3.7-plus")
                .to_string();
            match crate::modules::qwen_web::chat_completions(body).await {
                Ok(crate::modules::qwen_web::QwenWebOutput::Json(value)) => {
                    let message = value.pointer("/choices/0/message").cloned().unwrap_or(Value::Null);
                    let content = message
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let reasoning = message
                        .get("reasoning_content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let mut text = String::new();
                    if !reasoning.is_empty() {
                        text.push_str(&format!("[思考过程]\n{reasoning}\n\n[回复]\n"));
                    }
                    text.push_str(&content);
                    let mut result = mcp_text_result(&text);
                    // 结构化附带：模型、用量、是否有工具调用
                    if let Some(obj) = result.get_mut("structuredContent") {
                        obj["model"] = json!(model);
                        obj["usage"] = value.get("usage").cloned().unwrap_or(Value::Null);
                        obj["toolCalls"] = message.get("tool_calls").cloned().unwrap_or(Value::Null);
                    }
                    Ok(result)
                }
                Ok(crate::modules::qwen_web::QwenWebOutput::Stream(mut rx)) => {
                    // MCP 响应是单次 JSON：把 SSE 流聚合为完整文本。
                    let mut agg_text = String::new();
                    let mut reasoning = String::new();
                    let mut usage = Value::Null;
                    while let Some(chunk) = rx.recv().await {
                        let Ok(bytes) = chunk else { continue };
                        let text = String::from_utf8_lossy(&bytes);
                        for line in text.lines() {
                            let Some(data) = line.strip_prefix("data:") else { continue };
                            let data = data.trim();
                            if data.is_empty() || data == "[DONE]" {
                                continue;
                            }
                            let Ok(frame) = serde_json::from_str::<Value>(data) else { continue };
                            if let Some(delta) = frame.pointer("/choices/0/delta") {
                                if let Some(c) = delta.get("content").and_then(Value::as_str) {
                                    agg_text.push_str(c);
                                }
                                if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
                                    reasoning.push_str(r);
                                }
                            }
                            if let Some(u) = frame.get("usage").filter(|u| u.is_object()) {
                                usage = u.clone();
                            }
                        }
                    }
                    let mut text = String::new();
                    if !reasoning.is_empty() {
                        text.push_str(&format!("[思考过程]\n{reasoning}\n\n[回复]\n"));
                    }
                    text.push_str(&agg_text);
                    let mut result = mcp_text_result(&text);
                    if let Some(obj) = result.get_mut("structuredContent") {
                        obj["model"] = json!(model);
                        obj["usage"] = usage;
                    }
                    Ok(result)
                }
                Err(e) => Ok(json!({
                    "content": [{"type": "text", "text": format!("Qwen 网页版通道调用失败：{e}")}],
                    "isError": true,
                })),
            }
        }
        other => Err(format!("未知工具: {other}")),
    }
}

/// 把 MCP 工具参数转成 `/qwen/v1` 的 OpenAI 请求体。
fn build_chat_body(arguments: &Value) -> Result<Value, String> {
    let model = arguments
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("qwen3.7-plus")
        .to_string();
    let messages = match arguments.get("messages") {
        Some(Value::Array(arr)) if !arr.is_empty() => arr.clone(),
        _ => {
            let prompt = arguments
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| "缺少 messages 或 prompt 参数".to_string())?;
            if prompt.trim().is_empty() {
                return Err("prompt 为空".to_string());
            }
            vec![json!({"role": "user", "content": prompt})]
        }
    };
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": arguments.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if let Some(tools) = arguments.get("tools").filter(|t| t.is_array()) {
        body["tools"] = tools.clone();
    }
    Ok(body)
}

/// MCP 工具结果标准形状（content 数组 + 结构化附带字段）。
fn mcp_text_result(text: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": {},
        "isError": false,
    })
}

/// JSON-RPC 2.0 单条请求的 MCP 分发。
/// 返回 `None` 表示该消息是通知（无响应体，HTTP 层回 202）。
pub async fn handle_message(msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").cloned();
    // 通知（无 id）不产生响应。
    if id.is_none() {
        return None;
    }
    let id = id.unwrap_or(Value::Null);
    let respond = |result: Value| Some(json!({"jsonrpc": "2.0", "id": id.clone(), "result": result}));
    let fail = |code: i64, message: String| {
        Some(json!({"jsonrpc": "2.0", "id": id.clone(), "error": {"code": code, "message": message}}))
    };

    match method {
        "initialize" => {
            let protocol = msg
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION)
                .to_string();
            respond(json!({
                "protocolVersion": protocol,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                "instructions": "通过 Sylvia Switch 反代访问 Qwen 网页版（chat.qwen.ai）。多轮对话请每次传入完整 messages。",
            }))
        }
        "ping" => respond(json!({})),
        "tools/list" => respond(tools_list()),
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = msg
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call_tool(&name, &args).await {
                Ok(result) => respond(result),
                Err(e) => fail(-32602, e),
            }
        }
        "" => fail(-32600, "缺少 method 字段".to_string()),
        other => fail(-32601, format!("不支持的方法: {other}")),
    }
}
