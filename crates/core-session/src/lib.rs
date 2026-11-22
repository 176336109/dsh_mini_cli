//! 追加式会话事件日志 + 投影回放。
//!
//! 对应 dsh 的 `@deepseek-ai/dsh-session`（`ctx.sessions`）。Mini 的唯一约定是：
//!
//! 1. **只追加**，`seq` 单调递增，绝不原地改写。
//! 2. **事件词汇是唯一的持久化格式**，也是回放协议 —— 不另存一份 messages。
//! 3. 不认识的必读事件必须**拒绝重建**，不能静默跳过（见 `providers::jsonl`）。
//!
//! 刻意删除：`compaction` 的 `replace` 表面操作、`subagent/*`、`team/*`、
//! `goal/*`、`schedule/*` 等事件族 —— 对应产品能力本次不做。
//!
//! 模块划分：
//! - [`event`]：事件词汇表与信封形状（持久化格式，改这里等于改协议）。
//! - [`log`]：内存日志、历史投影缓存、落盘接缝 [`SessionSink`]。

mod event;
mod log;

pub use event::{
    AssistantData, Env, MessageData, Seq, SessionEvent, SessionStartData, StepData, StepEndData,
    SurfaceOp, ToolCallData, ToolResultData, TurnData, TurnEndData, TurnEndReason,
};
pub use log::{now_ms, SessionLog, SessionSink};
