//! 消息与流式词汇 + 模型适配器接缝。
//!
//! 对应 dsh 的 `@deepseek-ai/dsh-llm`（`ctx.llm` seam）：**服务定义**在这里，
//! **服务提供方**在 `providers`（mock / deepseek），**消费者**是 `core-loop`。
//!
//! 设计取舍：适配器返回 `Completion`（权威结果），流式增量只用于展示。
//! 这样"增量拼接错误"这类 bug 只会影响显示，不会污染会话日志。

pub mod text;

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

/// 消息内容块。Mini 只保留文本与思维链，图片/附件在 D4 之后再议。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Reasoning { text: String },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn reasoning(text: impl Into<String>) -> Self {
        Self::Reasoning { text: text.into() }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Text { text } | Self::Reasoning { text } => text,
        }
    }
}

/// 一次模型侧工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// 送入模型的消息。角色用 tag 表达，序列化后与 OpenAI 风格接近但不绑定线格式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    System {
        content: Vec<ContentBlock>,
    },
    User {
        content: Vec<ContentBlock>,
    },
    Assistant {
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self::System {
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![ContentBlock::text(text)],
            tool_calls: Vec::new(),
        }
    }

    /// 纯文本拼接，供日志和 CLI 展示用。
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        self.push_plain_text(&mut out);
        out
    }

    /// 追加式拼接，避免为展示回调反复分配中间 `Vec<&str>`。
    pub fn push_plain_text(&self, out: &mut String) {
        match self {
            Self::System { content } | Self::User { content } | Self::Assistant { content, .. } => {
                for block in content {
                    out.push_str(block.as_str());
                }
            }
            Self::Tool { content, .. } => out.push_str(content),
        }
    }

    /// 纯文本字符数。用于估算 token，避免为了计数而克隆整段内容。
    pub fn text_len(&self) -> usize {
        match self {
            Self::System { content } | Self::User { content } | Self::Assistant { content, .. } => {
                content
                    .iter()
                    .map(|block| block.as_str().chars().count())
                    .sum()
            }
            Self::Tool { content, .. } => content.chars().count(),
        }
    }

    /// 纯文本字节数。历史预算按字节算：比按字符快一个量级，也不必遍历字符边界。
    pub fn byte_len(&self) -> usize {
        match self {
            Self::System { content } | Self::User { content } | Self::Assistant { content, .. } => {
                content.iter().map(|block| block.as_str().len()).sum()
            }
            Self::Tool { content, .. } => content.len(),
        }
    }
}

/// 暴露给模型的工具声明。只白名单 name / description / parameters。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema 对象，`{"type":"object","properties":{...},"required":[...]}`。
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 模型自然说完，本轮结束。
    EndTurn,
    /// 模型要求调用工具，step 还需要继续。
    ToolCalls,
    MaxTokens,
    Other,
}

/// 一次补全请求。
///
/// 全部字段都是借用：历史消息由 `core-session` 的投影缓存持有，工具声明由
/// `Registry` 缓存持有。这样每个 step 都不需要克隆整段对话历史 —— 在多 step、
/// 长会话的场景里，这是循环里最贵的一笔分配。
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSchema],
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl<'a> LlmRequest<'a> {
    pub fn new(model: &'a str, messages: &'a [Message], tools: &'a [ToolSchema]) -> Self {
        Self {
            model,
            messages,
            tools,
            temperature: None,
            max_tokens: None,
        }
    }
}

/// 流式增量。仅用于实时展示；权威结果以 [`Completion`] 为准。
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        /// arguments 的字符串碎片，按 index 顺序累积后再解析。
        arguments_delta: String,
    },
}

/// 适配器返回的权威结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub content: Vec<ContentBlock>,
    pub tool_calls: Vec<ToolCall>,
    pub stop: StopReason,
    pub usage: Usage,
}

impl Completion {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            tool_calls: Vec::new(),
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("模型适配器 {adapter} 失败：{message}")]
    Adapter {
        adapter: &'static str,
        message: String,
    },
    #[error("已取消")]
    Cancelled,
}

pub type DeltaSink<'a> = &'a mut (dyn FnMut(LlmEvent) + Send);

/// `ctx.llm` 的服务定义。实现方只需保证：错误不 panic，取消能及时返回。
pub trait LlmAdapter: Send + Sync {
    fn name(&self) -> &'static str;

    /// `req` 以借用传入：适配器在内部序列化一次即可，不必拥有历史。
    fn complete<'a>(
        &'a self,
        req: &'a LlmRequest<'a>,
        sink: DeltaSink<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Completion, LlmError>> + Send + 'a>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_tag_shape_is_stable() {
        let value = serde_json::to_value(Message::user("hi")).unwrap();
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"][0]["kind"], "text");
    }

    #[test]
    fn assistant_without_tool_calls_omits_the_field() {
        let value = serde_json::to_value(Message::assistant("done")).unwrap();
        assert!(value.get("tool_calls").is_none());
    }
}
