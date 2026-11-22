//! `ctx.llm` 的假适配器：不需要网络和 API Key，用来验证循环与日志。
//!
//! 两条模式：
//! - [`MockAdapter::auto`]：按"最后一条消息是谁"自己决定下一步，CLI 演示用它。
//! - [`MockAdapter::scripted`]：按脚本逐轮出牌，测试用它。
//!
//! 它同时是"防 D1 踩坑"的活教材：`ToolCallDelta` 被拆成两片发出去，
//! 逼着消费者练习增量累积，而不是假设一次拿全。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use core_llm::{
    Completion, ContentBlock, LlmAdapter, LlmError, LlmEvent, LlmRequest, Message, StopReason,
    ToolCall, Usage,
};
use core_tools::head_chars;
use serde_json::json;

#[derive(Debug, Clone, PartialEq)]
pub enum MockTurn {
    Text(String),
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
}

pub struct MockAdapter {
    script: Mutex<VecDeque<MockTurn>>,
    auto: bool,
    calls: AtomicU64,
}

impl MockAdapter {
    pub fn auto() -> Self {
        Self {
            script: Mutex::new(VecDeque::new()),
            auto: true,
            calls: AtomicU64::new(0),
        }
    }

    pub fn scripted(turns: Vec<MockTurn>) -> Self {
        Self {
            script: Mutex::new(turns.into()),
            auto: false,
            calls: AtomicU64::new(0),
        }
    }

    fn next_turn(&self, req: &LlmRequest<'_>) -> MockTurn {
        if let Some(turn) = self
            .script
            .lock()
            .expect("mock script poisoned")
            .pop_front()
        {
            return turn;
        }
        if !self.auto {
            return MockTurn::Text("（mock）脚本已耗尽。".into());
        }
        match req.messages.last() {
            Some(Message::Tool { content, .. }) => MockTurn::Text(format!(
                "（mock）工具回来了 {} 字节，我看过了。",
                content.len()
            )),
            Some(Message::User { .. }) => self.explicit_call(req).unwrap_or(MockTurn::ToolCall {
                name: "bash".into(),
                arguments: json!({"command": "echo hello from dsh-mini"}),
            }),
            _ => MockTurn::Text("（mock）没有更多内容。".into()),
        }
    }

    /// 开发期便利：输入以 `!` 开头时直接触发指定工具，例如
    /// `!mcp__demo__echo {"text":"hi"}`。这让"循环 + 工具"可以被确定性驱动，
    /// 不用等模型。
    fn explicit_call(&self, req: &LlmRequest<'_>) -> Option<MockTurn> {
        let Message::User { content } = req.messages.last()? else {
            return None;
        };
        let text: String = content.iter().map(|block| block.as_str()).collect();
        let rest = text.trim().strip_prefix('!')?;
        let (name, raw_args) = match rest.split_once(char::is_whitespace) {
            Some((name, rest)) => (name, rest.trim()),
            None => (rest, ""),
        };
        if name.is_empty() {
            return None;
        }
        let arguments = if raw_args.is_empty() {
            json!({})
        } else {
            serde_json::from_str(raw_args).unwrap_or_else(|_| json!({}))
        };
        Some(MockTurn::ToolCall {
            name: name.to_string(),
            arguments,
        })
    }
}

impl LlmAdapter for MockAdapter {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn complete<'a>(
        &'a self,
        req: &'a LlmRequest<'a>,
        sink: &'a mut (dyn FnMut(LlmEvent) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<Completion, LlmError>> + Send + 'a>> {
        let turn = self.next_turn(req);
        let call_id = format!("call_{}", self.calls.fetch_add(1, Ordering::SeqCst) + 1);
        // 只数长度，不把每条消息拼成 String 再数字符。
        let input_tokens = req.messages.iter().map(|m| m.text_len() as u32 / 4).sum();

        Box::pin(async move {
            match turn {
                MockTurn::Text(text) => {
                    // 按字符切片直发，不先构造一个 Vec<String> 再遍历。
                    let mut rest = text.as_str();
                    while !rest.is_empty() {
                        let chunk = head_chars(rest, 12);
                        sink(LlmEvent::TextDelta(chunk.to_string()));
                        rest = &rest[chunk.len()..];
                    }
                    let output_tokens = core_tools::char_len(&text) as u32 / 4;
                    Ok(Completion {
                        content: vec![ContentBlock::text(text)],
                        tool_calls: Vec::new(),
                        stop: StopReason::EndTurn,
                        usage: Usage {
                            input_tokens,
                            output_tokens,
                        },
                    })
                }
                MockTurn::ToolCall { name, arguments } => {
                    let raw = serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".into());
                    let mid = core_tools::char_len(&raw) / 2;
                    let cut = head_chars(&raw, mid).len();
                    let (head, tail) = raw.split_at(cut);
                    sink(LlmEvent::ToolCallDelta {
                        index: 0,
                        id: Some(call_id.clone()),
                        name: Some(name.clone()),
                        arguments_delta: head.to_string(),
                    });
                    sink(LlmEvent::ToolCallDelta {
                        index: 0,
                        id: None,
                        name: None,
                        arguments_delta: tail.to_string(),
                    });
                    let label = format!("调用 {name}");
                    Ok(Completion {
                        content: vec![ContentBlock::text(label)],
                        tool_calls: vec![ToolCall {
                            id: call_id,
                            name,
                            arguments,
                        }],
                        stop: StopReason::ToolCalls,
                        usage: Usage {
                            input_tokens,
                            output_tokens: 8,
                        },
                    })
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_llm::Message;

    #[tokio::test]
    async fn auto_mode_calls_a_tool_then_summarises() {
        let adapter = MockAdapter::auto();
        let deltas = Mutex::new(Vec::new());
        let mut sink = |ev: LlmEvent| deltas.lock().unwrap().push(ev);

        let messages = vec![Message::user("hi")];
        let first = adapter
            .complete(&LlmRequest::new("mock", &messages, &[]), &mut sink)
            .await
            .unwrap();
        assert_eq!(first.stop, StopReason::ToolCalls);
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(deltas.lock().unwrap().len(), 2, "arguments 必须分片下发");

        let messages = vec![
            Message::user("hi"),
            Message::Tool {
                call_id: "call_1".into(),
                name: "bash".into(),
                content: "ok".into(),
                is_error: false,
            },
        ];
        let second = adapter
            .complete(&LlmRequest::new("mock", &messages, &[]), &mut sink)
            .await
            .unwrap();
        assert_eq!(second.stop, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn scripted_mode_is_deterministic() {
        let adapter = MockAdapter::scripted(vec![
            MockTurn::Text("甲".into()),
            MockTurn::Text("乙".into()),
        ]);
        let mut sink = |_e: LlmEvent| {};
        let mut messages = vec![Message::user("hi")];
        let a = adapter
            .complete(&LlmRequest::new("mock", &messages, &[]), &mut sink)
            .await
            .unwrap();
        messages.push(Message::assistant("甲"));
        let b = adapter
            .complete(&LlmRequest::new("mock", &messages, &[]), &mut sink)
            .await
            .unwrap();
        assert_eq!(a.content[0].as_str(), "甲");
        assert_eq!(b.content[0].as_str(), "乙");
    }
}
