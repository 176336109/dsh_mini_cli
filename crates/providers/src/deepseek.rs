//! DeepSeek 适配器（OpenAI 兼容的 `/chat/completions` + SSE 流式）。
//!
//! 这是 `ctx.llm` 接缝的第二个提供方，和 `mock` 完全等价替换 —— 循环不知道
//! 背后是真网络还是假数据。
//!
//! 三个必须做对的点：
//! 1. **分帧**：SSE 是 `data: {...}\n\n`，且一个 chunk 可能截断在多字节字符中间，
//!    所以按**字节**缓冲、只按 `\n` 切行，绝不对半个 chunk 做 UTF-8 解码。
//! 2. **tool_calls 分片**：`arguments` 是按 `index` 分片下发的字符串碎片，
//!    要按 index 累积后再整体解析 JSON。
//! 3. **思维链不回灌**：`reasoning_content` 只用于展示，回传给 API 会报错。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use core_llm::{
    Completion, ContentBlock, DeltaSink, LlmAdapter, LlmError, LlmEvent, LlmRequest, Message,
    StopReason, ToolCall, Usage,
};
use core_tools::{char_len, head_chars};
use serde_json::{json, Value};

const ADAPTER: &str = "deepseek";

/// SSE 单行负载的上限。正常一帧几 KB，超过这个量级只可能是协议异常，
/// 直接判错比默默吃掉内存安全。
const MAX_SSE_LINE_BYTES: usize = 8 * 1024 * 1024;

pub struct DeepSeekAdapter {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    thinking: bool,
    max_tokens: Option<u32>,
}

/// 构建 HTTP 客户端。代理为空时交给 reqwest 的默认行为（读 `HTTPS_PROXY` 等环境变量）。
fn build_client(proxy: Option<&str>) -> Result<reqwest::Client, LlmError> {
    let mut builder = reqwest::Client::builder()
        // 连接超时给短：车机网络差时要快速失败，别让用户干等。
        .connect_timeout(Duration::from_secs(15))
        // 整体超时给长：流式响应要一直读到最后一个 token，180s 在长回答 +
        // 弱网下会被中途掐断，那是最难查的一类"答案缺一半"。
        .timeout(Duration::from_secs(600))
        .user_agent(concat!("dsh-mini/", env!("CARGO_PKG_VERSION")));

    if let Some(url) = proxy {
        let url = url.trim();
        if !url.is_empty() {
            builder = builder.proxy(reqwest::Proxy::all(url).map_err(|err| LlmError::Adapter {
                adapter: ADAPTER,
                message: format!("代理地址 `{url}` 不合法：{err}"),
            })?);
        }
    }

    builder.build().map_err(|err| LlmError::Adapter {
        adapter: ADAPTER,
        message: format!("构建 HTTP 客户端失败：{err}"),
    })
}

impl DeepSeekAdapter {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        Ok(Self {
            client: build_client(None)?,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            thinking: false,
            max_tokens: None,
        })
    }

    /// 显式指定网关/代理（`http://`、`https://`、`socks5://` 之类）。
    ///
    /// 车机上常见两种用法：走车内网关的 HTTP 代理，或者走手机热点的 socks5。
    pub fn with_proxy(mut self, proxy: Option<&str>) -> Result<Self, LlmError> {
        if let Some(url) = proxy.map(str::trim).filter(|url| !url.is_empty()) {
            self.client = build_client(Some(url))?;
        }
        Ok(self)
    }

    pub fn with_thinking(mut self, enabled: bool) -> Self {
        self.thinking = enabled;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    fn build_body(&self, req: &LlmRequest<'_>) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": req.messages.iter().map(to_wire_message).collect::<Vec<_>>(),
            "stream": true,
            // 让最后一帧带上 usage，评测才有 token 成本可看。
            "stream_options": {"include_usage": true},
        });

        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = Value::Array(tools);
        }

        if self.thinking {
            body["thinking"] = json!({"type": "enabled"});
        }
        if let Some(temperature) = req.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = req.max_tokens.or(self.max_tokens) {
            body["max_tokens"] = json!(max_tokens);
        }
        body
    }
}

fn join_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        // 思维链不回传给 API。
        if let ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

fn to_wire_message(message: &Message) -> Value {
    match message {
        Message::System { content } => json!({"role": "system", "content": join_text(content)}),
        Message::User { content } => json!({"role": "user", "content": join_text(content)}),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let text = join_text(content);
            let mut value = json!({"role": "assistant"});
            value["content"] = if text.is_empty() && !tool_calls.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            };
            if !tool_calls.is_empty() {
                let calls: Vec<Value> = tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments.to_string(),
                            }
                        })
                    })
                    .collect();
                value["tool_calls"] = Value::Array(calls);
            }
            value
        }
        Message::Tool {
            call_id, content, ..
        } => {
            json!({"role": "tool", "tool_call_id": call_id, "content": content})
        }
    }
}

#[derive(Default)]
struct CallAcc {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
struct StreamAcc {
    text: String,
    reasoning: String,
    calls: BTreeMap<usize, CallAcc>,
    finish: Option<String>,
    usage: Option<Usage>,
}

impl StreamAcc {
    /// 预分配输出缓冲：一个 chunk 通常在几百字节量级，避免从 0 开始反复扩容。
    fn with_capacity(bytes: usize) -> Self {
        Self {
            text: String::with_capacity(bytes),
            reasoning: String::new(),
            calls: BTreeMap::new(),
            finish: None,
            usage: None,
        }
    }

    /// 消费一行 `data:` 负载。返回 true 表示收到 `[DONE]`。
    fn consume(
        &mut self,
        payload: &str,
        sink: &mut (dyn FnMut(LlmEvent) + Send),
    ) -> Result<bool, LlmError> {
        let payload = payload.trim();
        if payload == "[DONE]" {
            return Ok(true);
        }
        let value: Value = serde_json::from_str(payload).map_err(|err| LlmError::Adapter {
            adapter: ADAPTER,
            message: format!(
                "SSE 负载不是合法 JSON：{err}；片段={}",
                head_chars(payload, 200)
            ),
        })?;

        if let Some(error) = value.get("error") {
            return Err(LlmError::Adapter {
                adapter: ADAPTER,
                message: format!("服务端返回错误：{error}"),
            });
        }

        if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(Usage {
                input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as u32,
            });
        }

        let Some(choice) = value["choices"].get(0) else {
            // 只带 usage 的收尾帧没有 choices，正常。
            return Ok(false);
        };

        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish = Some(reason.to_string());
        }

        let delta = &choice["delta"];
        if let Some(text) = delta["reasoning_content"].as_str() {
            if !text.is_empty() {
                self.reasoning.push_str(text);
                sink(LlmEvent::ReasoningDelta(text.to_string()));
            }
        }
        if let Some(text) = delta["content"].as_str() {
            if !text.is_empty() {
                self.text.push_str(text);
                sink(LlmEvent::TextDelta(text.to_string()));
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let index = call["index"].as_u64().unwrap_or(0) as usize;
                let entry = self.calls.entry(index).or_default();

                let id = call["id"].as_str();
                let name = call["function"]["name"].as_str();
                let args = call["function"]["arguments"].as_str();

                if let Some(id) = id {
                    entry.id.push_str(id);
                }
                if let Some(name) = name {
                    entry.name.push_str(name);
                }
                if let Some(args) = args {
                    entry.args.push_str(args);
                }

                sink(LlmEvent::ToolCallDelta {
                    index,
                    id: id.map(str::to_string),
                    name: name.map(str::to_string),
                    arguments_delta: args.unwrap_or_default().to_string(),
                });
            }
        }

        Ok(false)
    }

    fn into_completion(self) -> Result<Completion, LlmError> {
        // 先算估算值，后面 self.text 会被 move 进 content。
        let estimated_output = (char_len(&self.text) / 4) as u32;

        let mut content = Vec::new();
        if !self.reasoning.is_empty() {
            content.push(ContentBlock::reasoning(self.reasoning));
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::text(self.text));
        }

        let mut tool_calls = Vec::new();
        for (index, call) in self.calls {
            let arguments = if call.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.args).map_err(|err| LlmError::Adapter {
                    adapter: ADAPTER,
                    message: format!(
                        "工具 {} 的 arguments 不是合法 JSON：{err}；原文={}",
                        call.name,
                        head_chars(&call.args, 300)
                    ),
                })?
            };
            tool_calls.push(ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                },
                name: call.name,
                arguments,
            });
        }

        let stop = match self.finish.as_deref() {
            Some("length") => StopReason::MaxTokens,
            Some("tool_calls") => StopReason::ToolCalls,
            _ if !tool_calls.is_empty() => StopReason::ToolCalls,
            _ => StopReason::EndTurn,
        };

        // 没有 usage 时按字符数粗估，而不是报 0 —— 报 0 会让成本统计失去意义。
        let usage = self.usage.unwrap_or(Usage {
            input_tokens: 0,
            output_tokens: estimated_output,
        });

        Ok(Completion {
            content,
            tool_calls,
            stop,
            usage,
        })
    }
}

/// 处理一行 SSE：注释行、空行、非 `data:` 行都直接跳过。
fn process_line(
    line: &[u8],
    acc: &mut StreamAcc,
    sink: &mut (dyn FnMut(LlmEvent) + Send),
) -> Result<bool, LlmError> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return Ok(false);
    }
    let Some(payload) = line.strip_prefix("data:") else {
        return Ok(false);
    };
    acc.consume(payload, sink)
}

impl LlmAdapter for DeepSeekAdapter {
    fn name(&self) -> &'static str {
        ADAPTER
    }

    fn complete<'a>(
        &'a self,
        req: &'a LlmRequest<'a>,
        sink: DeltaSink<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Completion, LlmError>> + Send + 'a>> {
        let body = self.build_body(req);
        Box::pin(async move {
            let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
            let response = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|err| LlmError::Adapter {
                    adapter: ADAPTER,
                    message: format!("请求 {url} 失败：{err}"),
                })?;

            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(LlmError::Adapter {
                    adapter: ADAPTER,
                    message: format!("HTTP {status}：{}", head_chars(&text, 500)),
                });
            }

            let mut response = response;
            let mut acc = StreamAcc::with_capacity(2048);
            // 缓冲复用：按字节累积，只在确有完整行时才前进读指针，避免老实现
            // 每行 `drain` 一次（O(n) 搬移 + 每行一次堆分配）。
            let mut buffer: Vec<u8> = Vec::with_capacity(8 * 1024);
            let mut done = false;

            'stream: while !done {
                let chunk = response.chunk().await.map_err(|err| LlmError::Adapter {
                    adapter: ADAPTER,
                    message: format!("读取流失败：{err}"),
                })?;
                let Some(chunk) = chunk else { break };
                buffer.extend_from_slice(&chunk);

                let mut start = 0usize;
                while let Some(offset) = buffer[start..].iter().position(|byte| *byte == b'\n') {
                    let line_start = start;
                    let end = start + offset;
                    start = end + 1;
                    if process_line(&buffer[line_start..end], &mut acc, &mut *sink)? {
                        done = true;
                        break 'stream;
                    }
                }
                buffer.drain(..start);
                if buffer.len() > MAX_SSE_LINE_BYTES {
                    return Err(LlmError::Adapter {
                        adapter: ADAPTER,
                        message: format!("SSE 单行超过 {MAX_SSE_LINE_BYTES} 字节，疑似协议异常"),
                    });
                }
            }

            // 流末尾可能没有换行：最后一段仍要尝试解析（例如 `data: [DONE]`）。
            if !done && !buffer.is_empty() {
                process_line(&buffer, &mut acc, &mut *sink)?;
            }

            acc.into_completion()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(acc: &mut StreamAcc, payloads: &[&str]) -> Vec<LlmEvent> {
        let mut deltas = Vec::new();
        let mut sink = |event: LlmEvent| deltas.push(event);
        for payload in payloads {
            acc.consume(payload, &mut sink).expect("payload");
        }
        deltas
    }

    #[test]
    fn accumulates_text_and_reasoning_separately() {
        let mut acc = StreamAcc::default();
        feed(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"reasoning_content":"先想"}}]}"#,
                r#"{"choices":[{"delta":{"content":"答"}}]}"#,
                r#"{"choices":[{"delta":{"content":"案"},"finish_reason":"stop"}]}"#,
            ],
        );
        let completion = acc.into_completion().unwrap();
        assert_eq!(completion.content.len(), 2);
        assert_eq!(completion.content[0].as_str(), "先想");
        assert_eq!(completion.content[1].as_str(), "答案");
        assert_eq!(completion.stop, StopReason::EndTurn);
    }

    #[test]
    fn accumulates_fragmented_tool_call_arguments() {
        let mut acc = StreamAcc::default();
        feed(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"bash","arguments":"{\"comm"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls -la\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        let completion = acc.into_completion().unwrap();
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].id, "call_a");
        assert_eq!(completion.tool_calls[0].name, "bash");
        assert_eq!(completion.tool_calls[0].arguments["command"], "ls -la");
        assert_eq!(completion.stop, StopReason::ToolCalls);
    }

    #[test]
    fn keeps_two_parallel_tool_calls_separate() {
        let mut acc = StreamAcc::default();
        feed(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{\"path\":\"x\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"bash","arguments":"{}"}}]}}]}"#,
            ],
        );
        let completion = acc.into_completion().unwrap();
        assert_eq!(completion.tool_calls.len(), 2);
        assert_eq!(completion.tool_calls[0].name, "read_file");
        assert_eq!(completion.tool_calls[1].name, "bash");
    }

    #[test]
    fn usage_frame_is_captured_even_without_choices() {
        let mut acc = StreamAcc::default();
        feed(
            &mut acc,
            &[r#"{"choices":[],"usage":{"prompt_tokens":123,"completion_tokens":45}}"#],
        );
        let completion = acc.into_completion().unwrap();
        assert_eq!(completion.usage.input_tokens, 123);
        assert_eq!(completion.usage.output_tokens, 45);
    }

    #[test]
    fn server_error_frame_becomes_an_error() {
        let mut acc = StreamAcc::default();
        let mut sink = |_e: LlmEvent| {};
        let result = acc.consume(r#"{"error":{"message":"invalid api key"}}"#, &mut sink);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid api key"));
    }

    #[test]
    fn malformed_arguments_surface_as_error() {
        let mut acc = StreamAcc::default();
        feed(
            &mut acc,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"bash","arguments":"{oops"}}]}}]}"#,
            ],
        );
        assert!(acc.into_completion().is_err());
    }

    #[test]
    fn wire_message_omits_reasoning_blocks() {
        let value = to_wire_message(&Message::Assistant {
            content: vec![
                ContentBlock::reasoning("内部推理"),
                ContentBlock::text("给用户看的"),
            ],
            tool_calls: Vec::new(),
        });
        assert_eq!(value["content"], "给用户看的");
    }
}
