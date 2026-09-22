//! Loomy 代理的 Anthropic Claude Messages 协议兼容层。
//!
//! 把 Anthropic Messages API (POST /loomy/v1/messages) 请求转换为
//! OpenAI chat/completions 载荷，复用 Loomy 上游链路；响应（含 SSE 流）
//! 再转换回 Anthropic 事件序列。

use bytes::Bytes;
use serde_json::{json, Value};

use super::glm_anthropic::{from_openai_response, to_openai_payload, AnthropicSseTransformer};
use super::loomy_proxy::{LoomyChatCompletion, LoomyUpstreamStream};

/// Anthropic Messages 入口：支持流式与非流式。
pub async fn messages(body: Value) -> Result<LoomyChatCompletion, String> {
    let (openai_body, model, is_stream) = to_openai_payload(&body)?;
    if is_stream {
        let upstream = super::loomy_proxy::chat_completions_stream(openai_body).await?;
        let anthropic_stream = pump_loomy_anthropic_stream(upstream).await;
        Ok(LoomyChatCompletion::Stream(anthropic_stream))
    } else {
        let json_resp = super::loomy_proxy::chat_completions_json(openai_body).await?;
        let anthropic_resp = from_openai_response(&json_resp, &model);
        Ok(LoomyChatCompletion::Json(anthropic_resp))
    }
}

/// 将 Loomy OpenAI SSE 字节流转换为 Anthropic SSE 事件流。
async fn pump_loomy_anthropic_stream(mut upstream: LoomyUpstreamStream) -> LoomyUpstreamStream {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    let guard = upstream._concurrency_guard.take();

    tokio::spawn(async move {
        let _guard = guard; // 保持并发槽位直到传输完成
        let mut transformer = AnthropicSseTransformer::new();
        let mut rx = upstream.rx;
        // 跨 chunk 的 UTF-8 半字符尾部：严格 from_utf8 失败会整块丢弃，
        // 导致大帧（中文 tool_calls）被误判「空流」。见 decode_stream_chunk。
        let mut carry: Vec<u8> = Vec::new();

        while let Some(chunk_res) = rx.recv().await {
            match chunk_res {
                Ok(chunk) => {
                    // 如果是保活注释帧（以 : 开头），直接透传给客户端，保持连接活性
                    if chunk.starts_with(b":") {
                        if tx.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    let text = super::glm_anthropic::decode_stream_chunk(&mut carry, &chunk);
                    for event in transformer.push(&text) {
                        if tx.send(Ok(Bytes::from(event))).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    // e 已由 loomy_proxy 翻译成可行动文案（含"上游流中断"）。
                    transformer.mark_error(format!("Loomy 上游流异常中断：{e}"));
                    // 关键：用 `overloaded_error`（Anthropic 语义=上游过载/瞬时故障，
                    // 客户端应重试）而不是 `api_error`（业务错误，客户端判定
                    // retryable=false 直接放弃）。流中断属于前者，必须让 ZCode 重试。
                    let err_event = format!(
                        "event: error\ndata: {}\n\n",
                        json!({"type": "error", "error": {"type": "overloaded_error", "message": transformer.error()}})
                    );
                    let _ = tx.send(Ok(Bytes::from(err_event))).await;
                    return;
                }
            }
        }
        if !carry.is_empty() {
            let tail = String::from_utf8_lossy(&carry).into_owned();
            for event in transformer.push(&tail) {
                let _ = tx.send(Ok(Bytes::from(event))).await;
            }
        }

        // 结束流
        for event in transformer.finish() {
            if tx.send(Ok(Bytes::from(event))).await.is_err() {
                return;
            }
        }
    });

    LoomyUpstreamStream {
        content_type: "text/event-stream; charset=utf-8".to_string(),
        rx,
        _concurrency_guard: None,
    }
}
