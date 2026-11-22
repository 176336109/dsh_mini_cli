//! 事件词汇表：Mini 保留的十类会话事件，及其序列化形状。
//!
//! 序列化形状对齐 dsh 的 v4 信封：
//! `{"type": "...", "seq": n, "time": ms, "data": {...}, "surfaceOp": "append"}`。

use core_llm::ContentBlock;
use serde::{Deserialize, Serialize};

pub type Seq = u64;

/// 事件信封里的事件序号与时间戳。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env {
    pub seq: Seq,
    pub time: i64,
}

/// 消息类事件如何进入"有序表面"。Mini 只支持 `append`；
/// 被删除的 compaction 才会产生 `replace`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceOp {
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    /// 模型说完且没有欠下工具请求。
    Completed,
    /// 首个输入被拒或为空：**一个 step 都没有**的 turn。
    Empty,
    /// 撞到 `max_steps` 上限。
    MaxSteps,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartData {
    pub cwd: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnData {
    pub turn: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEndData {
    pub turn: u64,
    pub reason: TurnEndReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepData {
    pub turn: u64,
    pub step: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepEndData {
    pub turn: u64,
    pub step: u64,
    pub tool_calls: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageData {
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantData {
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<core_llm::ToolCall>,
    /// 本次请求的 token 用量。写进日志是为了让评测能算成本，而不只是算步数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<core_llm::Usage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallData {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultData {
    pub call_id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
}

/// 会话日志中的一条不可变记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    #[serde(rename = "session/start")]
    SessionStart {
        seq: Seq,
        time: i64,
        data: SessionStartData,
    },
    #[serde(rename = "turn/start")]
    TurnStart { seq: Seq, time: i64, data: TurnData },
    #[serde(rename = "turn/end")]
    TurnEnd {
        seq: Seq,
        time: i64,
        data: TurnEndData,
    },
    #[serde(rename = "step/start")]
    StepStart { seq: Seq, time: i64, data: StepData },
    #[serde(rename = "step/end")]
    StepEnd {
        seq: Seq,
        time: i64,
        data: StepEndData,
    },
    #[serde(rename = "system/message")]
    SystemMessage {
        seq: Seq,
        time: i64,
        data: MessageData,
        #[serde(rename = "surfaceOp", default, skip_serializing_if = "Option::is_none")]
        surface_op: Option<SurfaceOp>,
    },
    #[serde(rename = "user/message")]
    UserMessage {
        seq: Seq,
        time: i64,
        data: MessageData,
        #[serde(rename = "surfaceOp", default, skip_serializing_if = "Option::is_none")]
        surface_op: Option<SurfaceOp>,
    },
    #[serde(rename = "assistant/message")]
    AssistantMessage {
        seq: Seq,
        time: i64,
        data: AssistantData,
        #[serde(rename = "surfaceOp", default, skip_serializing_if = "Option::is_none")]
        surface_op: Option<SurfaceOp>,
    },
    #[serde(rename = "tool/call")]
    ToolCall {
        seq: Seq,
        time: i64,
        data: ToolCallData,
    },
    #[serde(rename = "tool/result")]
    ToolResult {
        seq: Seq,
        time: i64,
        data: ToolResultData,
        #[serde(rename = "surfaceOp", default, skip_serializing_if = "Option::is_none")]
        surface_op: Option<SurfaceOp>,
    },
}

impl SessionEvent {
    pub fn seq(&self) -> Seq {
        match self {
            Self::SessionStart { seq, .. }
            | Self::TurnStart { seq, .. }
            | Self::TurnEnd { seq, .. }
            | Self::StepStart { seq, .. }
            | Self::StepEnd { seq, .. }
            | Self::SystemMessage { seq, .. }
            | Self::UserMessage { seq, .. }
            | Self::AssistantMessage { seq, .. }
            | Self::ToolCall { seq, .. }
            | Self::ToolResult { seq, .. } => *seq,
        }
    }

    pub fn time(&self) -> i64 {
        match self {
            Self::SessionStart { time, .. }
            | Self::TurnStart { time, .. }
            | Self::TurnEnd { time, .. }
            | Self::StepStart { time, .. }
            | Self::StepEnd { time, .. }
            | Self::SystemMessage { time, .. }
            | Self::UserMessage { time, .. }
            | Self::AssistantMessage { time, .. }
            | Self::ToolCall { time, .. }
            | Self::ToolResult { time, .. } => *time,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::SessionStart { .. } => "session/start",
            Self::TurnStart { .. } => "turn/start",
            Self::TurnEnd { .. } => "turn/end",
            Self::StepStart { .. } => "step/start",
            Self::StepEnd { .. } => "step/end",
            Self::SystemMessage { .. } => "system/message",
            Self::UserMessage { .. } => "user/message",
            Self::AssistantMessage { .. } => "assistant/message",
            Self::ToolCall { .. } => "tool/call",
            Self::ToolResult { .. } => "tool/result",
        }
    }

    pub fn session_start(env: Env, cwd: impl Into<String>, model: impl Into<String>) -> Self {
        Self::SessionStart {
            seq: env.seq,
            time: env.time,
            data: SessionStartData {
                cwd: cwd.into(),
                model: model.into(),
            },
        }
    }

    pub fn turn_start(env: Env, turn: u64) -> Self {
        Self::TurnStart {
            seq: env.seq,
            time: env.time,
            data: TurnData { turn },
        }
    }

    pub fn turn_end(env: Env, turn: u64, reason: TurnEndReason) -> Self {
        Self::TurnEnd {
            seq: env.seq,
            time: env.time,
            data: TurnEndData { turn, reason },
        }
    }

    pub fn step_start(env: Env, turn: u64, step: u64) -> Self {
        Self::StepStart {
            seq: env.seq,
            time: env.time,
            data: StepData { turn, step },
        }
    }

    pub fn step_end(env: Env, turn: u64, step: u64, tool_calls: u32) -> Self {
        Self::StepEnd {
            seq: env.seq,
            time: env.time,
            data: StepEndData {
                turn,
                step,
                tool_calls,
            },
        }
    }

    pub fn system_message(env: Env, content: Vec<ContentBlock>) -> Self {
        Self::SystemMessage {
            seq: env.seq,
            time: env.time,
            data: MessageData { content },
            surface_op: Some(SurfaceOp::Append),
        }
    }

    pub fn user_message(env: Env, content: Vec<ContentBlock>) -> Self {
        Self::UserMessage {
            seq: env.seq,
            time: env.time,
            data: MessageData { content },
            surface_op: Some(SurfaceOp::Append),
        }
    }

    pub fn assistant_message(
        env: Env,
        content: Vec<ContentBlock>,
        tool_calls: Vec<core_llm::ToolCall>,
        usage: Option<core_llm::Usage>,
    ) -> Self {
        Self::AssistantMessage {
            seq: env.seq,
            time: env.time,
            data: AssistantData {
                content,
                tool_calls,
                usage,
            },
            surface_op: Some(SurfaceOp::Append),
        }
    }

    pub fn tool_call(env: Env, call: &core_llm::ToolCall) -> Self {
        Self::ToolCall {
            seq: env.seq,
            time: env.time,
            data: ToolCallData {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        }
    }

    /// 拥有所有权的版本：循环里已经不再需要这个 `ToolCall` 时用它，省一次克隆。
    pub fn tool_call_owned(env: Env, call: core_llm::ToolCall) -> Self {
        Self::ToolCall {
            seq: env.seq,
            time: env.time,
            data: ToolCallData {
                call_id: call.id,
                name: call.name,
                arguments: call.arguments,
            },
        }
    }

    pub fn tool_result(
        env: Env,
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::ToolResult {
            seq: env.seq,
            time: env.time,
            data: ToolResultData {
                call_id: call_id.into(),
                name: name.into(),
                content: content.into(),
                is_error,
            },
            surface_op: Some(SurfaceOp::Append),
        }
    }
}
