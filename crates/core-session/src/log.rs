//! 内存会话日志 + 增量投影 + 落盘接缝。

use core_llm::Message;

use crate::event::{Env, Seq, SessionEvent};

/// 事件落盘接缝。
///
/// 循环每次追加事件都会同步调用它（实现方负责"一条事件一次写"），这样进程崩溃
/// 只会丢掉正在生成的那一条，而不是整个 turn。失败不 panic：调用方记一次警告后
/// 停用该 sink，agent 继续跑完。
pub trait SessionSink: Send {
    fn append(&mut self, event: &SessionEvent) -> Result<(), String>;
}

impl<F> SessionSink for F
where
    F: FnMut(&SessionEvent) -> Result<(), String> + Send,
{
    fn append(&mut self, event: &SessionEvent) -> Result<(), String> {
        self(event)
    }
}

/// 内存中的会话日志。持久化在 `providers::jsonl`，本模块不碰 I/O。
///
/// 除事件数组外还维护两份派生状态，避免每次取用都全量重算：
/// - `messages`：模型可见历史的投影（`append` 时增量更新，等价于一次性 replay）。
/// - `turns` / `started`：turn 编号与 session/start 存在性。
#[derive(Debug, Clone)]
pub struct SessionLog {
    id: String,
    events: Vec<SessionEvent>,
    messages: Vec<Message>,
    next_seq: Seq,
    turns: u64,
    started: bool,
    /// 投影里消息的字节数，增量维护。
    history_bytes: usize,
    /// 投影的字节预算；0 表示不限制。
    history_budget: usize,
}

/// 小于这个体量的工具输出不值得压缩 —— 省不出多少，还会让模型多读一坨占位文字。
const TRIM_THRESHOLD_BYTES: usize = 4 * 1024;
/// 压缩后仍保留的开头长度（字符）：足够让模型认出"这是哪一次调用"。
const TRIM_KEEP_CHARS: usize = 200;

impl SessionLog {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            events: Vec::new(),
            messages: Vec::new(),
            next_seq: 0,
            turns: 0,
            started: false,
            history_bytes: 0,
            history_budget: 0,
        }
    }

    /// 设置模型可见历史的字节预算。
    ///
    /// 超出预算时，**最旧的工具输出**会被替换成一行省略标记（保留开头一小段）。
    /// 只动工具输出：它占体积的绝对多数，而且删掉不破坏 `tool_calls` 与
    /// `tool` 结果之间的对应关系。磁盘上的事件日志保持完整，`--resume` 依旧能
    /// 原样重建（重建时同样套用预算，结果一致）。
    ///
    /// `bytes == 0` 表示不限制。
    pub fn with_history_budget(mut self, bytes: usize) -> Self {
        self.history_budget = bytes;
        self.trim_history();
        self
    }

    pub fn history_budget(&self) -> usize {
        self.history_budget
    }

    /// 当前投影占用的字节数（估算，按纯文本计）。
    pub fn history_bytes(&self) -> usize {
        self.history_bytes
    }

    /// 从已有日志恢复。`next_seq` 取最大 seq + 1，绝不回退。
    pub fn resume(id: impl Into<String>, events: Vec<SessionEvent>) -> Self {
        let mut log = Self::new(id);
        log.events.reserve(events.len());
        log.messages.reserve(events.len());
        let next_seq = events
            .iter()
            .map(SessionEvent::seq)
            .max()
            .map_or(0, |s| s + 1);
        for event in events {
            log.apply(&event);
            log.events.push(event);
        }
        log.next_seq = next_seq;
        log
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// 预留容量，避免长会话反复 realloc。
    pub fn reserve(&mut self, additional: usize) {
        self.events.reserve(additional);
        self.messages.reserve(additional);
    }

    /// 取下一个信封。**唯一**产生 seq 的地方。
    pub fn next_env(&mut self) -> Env {
        let seq = self.next_seq;
        self.next_seq += 1;
        Env {
            seq,
            time: now_ms(),
        }
    }

    pub fn push(&mut self, event: SessionEvent) {
        debug_assert!(
            self.events
                .last()
                .map_or(true, |last| event.seq() > last.seq()),
            "seq must be strictly increasing"
        );
        self.apply(&event);
        self.events.push(event);
    }

    /// 已完成的 turn 数，用于续跑时接着编号。O(1)。
    pub fn turn_count(&self) -> u64 {
        self.turns
    }

    /// 是否已有 `session/start`。O(1)。
    pub fn started(&self) -> bool {
        self.started
    }

    /// 把事件投影成模型可见的历史消息。
    ///
    /// 这是回放协议的核心：换一个运行时不重放 UI，而是重放这份日志。
    /// 投影随 `push` 增量维护，这里只借出切片 —— 每个 step 都不再克隆整段历史。
    pub fn replay(&self) -> &[Message] {
        &self.messages
    }

    fn apply(&mut self, event: &SessionEvent) {
        let message = match event {
            SessionEvent::SessionStart { .. } => {
                self.started = true;
                None
            }
            SessionEvent::TurnStart { data, .. } => {
                self.turns = data.turn;
                None
            }
            SessionEvent::SystemMessage { data, .. } => Some(Message::System {
                content: data.content.clone(),
            }),
            SessionEvent::UserMessage { data, .. } => Some(Message::User {
                content: data.content.clone(),
            }),
            SessionEvent::AssistantMessage { data, .. } => Some(Message::Assistant {
                content: data.content.clone(),
                tool_calls: data.tool_calls.clone(),
            }),
            SessionEvent::ToolResult { data, .. } => Some(Message::Tool {
                call_id: data.call_id.clone(),
                name: data.name.clone(),
                content: data.content.clone(),
                is_error: data.is_error,
            }),
            SessionEvent::TurnEnd { .. }
            | SessionEvent::StepStart { .. }
            | SessionEvent::StepEnd { .. }
            | SessionEvent::ToolCall { .. } => None,
        };

        if let Some(message) = message {
            self.history_bytes += message.byte_len();
            self.messages.push(message);
            self.trim_history();
        }
    }

    /// 把最旧的工具输出压成一行省略标记，直到投影回到预算内。
    ///
    /// 只压工具输出：它是体积的大头，而且 `tool` 消息的 `call_id` 与配对的
    /// `assistant.tool_calls` 都不动，对话结构仍然合法。
    fn trim_history(&mut self) {
        if self.history_budget == 0 || self.history_bytes <= self.history_budget {
            return;
        }

        let mut excess = self.history_bytes - self.history_budget;
        let mut freed_total = 0usize;
        for message in self.messages.iter_mut() {
            if excess == 0 {
                break;
            }
            let Message::Tool { content, .. } = message else {
                continue;
            };
            if content.len() <= TRIM_THRESHOLD_BYTES {
                continue;
            }
            let head = core_llm::text::head_chars(content, TRIM_KEEP_CHARS);
            let mut replacement = String::with_capacity(head.len() + 64);
            replacement.push_str("（历史工具输出已省略 ");
            replacement.push_str(&(content.len() - head.len()).to_string());
            replacement.push_str(" 字节）\n");
            replacement.push_str(head);
            replacement.push('…');

            let freed = content.len().saturating_sub(replacement.len());
            if freed == 0 {
                continue;
            }
            *content = replacement;
            excess = excess.saturating_sub(freed);
            freed_total += freed;
        }

        self.history_bytes = self.history_bytes.saturating_sub(freed_total);
    }
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{SurfaceOp, TurnEndReason};
    use core_llm::{ContentBlock, ToolCall};

    #[test]
    fn replay_projects_messages_in_order() {
        let mut log = SessionLog::new("s1");
        let env = log.next_env();
        log.push(SessionEvent::system_message(
            env,
            vec![ContentBlock::text("sys")],
        ));
        let env = log.next_env();
        log.push(SessionEvent::user_message(
            env,
            vec![ContentBlock::text("hi")],
        ));
        let env = log.next_env();
        log.push(SessionEvent::assistant_message(
            env,
            vec![ContentBlock::text("calling")],
            vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            }],
            Some(core_llm::Usage {
                input_tokens: 12,
                output_tokens: 3,
            }),
        ));
        let env = log.next_env();
        log.push(SessionEvent::tool_result(env, "c1", "bash", "out", false));

        let messages = log.replay();
        assert_eq!(messages.len(), 4);
        assert!(matches!(messages[0], Message::System { .. }));
        assert!(matches!(messages[3], Message::Tool { .. }));
        match &messages[2] {
            Message::Assistant { tool_calls, .. } => assert_eq!(tool_calls.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn resume_rebuilds_the_projection() {
        let mut log = SessionLog::new("s1");
        let env = log.next_env();
        log.push(SessionEvent::user_message(
            env,
            vec![ContentBlock::text("hi")],
        ));
        let resumed = SessionLog::resume("s1", log.events().to_vec());
        assert_eq!(resumed.replay().len(), 1);
        assert_eq!(resumed.replay(), log.replay());
    }

    #[test]
    fn resume_continues_seq_without_regression() {
        let mut log = SessionLog::new("s1");
        for _ in 0..3 {
            let env = log.next_env();
            log.push(SessionEvent::turn_start(env, 1));
        }
        let mut resumed = SessionLog::resume("s1", log.events().to_vec());
        assert_eq!(resumed.next_env().seq, 3);
    }

    #[test]
    fn envelope_shape_matches_dsh_v4() {
        let mut log = SessionLog::new("s1");
        let env = log.next_env();
        log.push(SessionEvent::turn_start(env, 1));
        let value = serde_json::to_value(&log.events()[0]).unwrap();
        assert_eq!(value["type"], "turn/start");
        assert_eq!(value["seq"], 0);
        assert_eq!(value["data"]["turn"], 1);
        assert!(value["time"].is_number());
    }

    #[test]
    fn surface_events_carry_append_op() {
        let mut log = SessionLog::new("s1");
        let env = log.next_env();
        log.push(SessionEvent::user_message(
            env,
            vec![ContentBlock::text("hi")],
        ));
        let value = serde_json::to_value(&log.events()[0]).unwrap();
        assert_eq!(value["surfaceOp"], "append");
        assert_eq!(
            serde_json::from_value::<SessionEvent>(value).unwrap(),
            log.events()[0],
            "surfaceOp 必须能往返"
        );
        let _ = SurfaceOp::Append;
    }

    #[test]
    fn history_budget_trims_the_oldest_tool_outputs_first() {
        // 5 条各 10 KiB = 50 KiB，预算 25 KiB：应当只压掉最旧的几条。
        let budget = 25 * 1024;
        let mut log = SessionLog::new("s1").with_history_budget(budget);
        let big = "x".repeat(10 * 1024);
        for index in 0..5u32 {
            let env = log.next_env();
            log.push(SessionEvent::tool_result(
                env,
                format!("c{index}"),
                "bash",
                big.clone(),
                false,
            ));
        }

        assert!(log.history_bytes() <= budget, "投影必须回到预算内");
        let contents: Vec<&String> = log
            .replay()
            .iter()
            .map(|message| match message {
                Message::Tool { content, .. } => content,
                other => panic!("unexpected {other:?}"),
            })
            .collect();

        assert!(contents[0].contains("已省略"), "最旧的输出应先被压缩");
        assert_eq!(contents[4].len(), 10 * 1024, "最新的输出必须原样保留");
        // 保留的条数应当刚好够回到预算内：不少于 2 条。
        let intact = contents.iter().filter(|c| c.len() == 10 * 1024).count();
        assert!(intact >= 2, "不该过度压缩，完整条数={intact}");
    }

    #[test]
    fn no_budget_means_no_trimming() {
        let mut log = SessionLog::new("s1");
        let env = log.next_env();
        log.push(SessionEvent::tool_result(
            env,
            "c1",
            "bash",
            "y".repeat(100 * 1024),
            false,
        ));
        assert_eq!(log.history_budget(), 0);
        assert_eq!(log.history_bytes(), 100 * 1024);
    }

    #[test]
    fn small_outputs_are_left_alone() {
        let mut log = SessionLog::new("s1").with_history_budget(1024);
        let env = log.next_env();
        log.push(SessionEvent::tool_result(
            env,
            "c1",
            "bash",
            "z".repeat(2048),
            false,
        ));
        let content = match &log.replay()[0] {
            Message::Tool { content, .. } => content,
            other => panic!("unexpected {other:?}"),
        };
        assert!(!content.contains("已省略"), "低于阈值的输出不值得压缩");
    }

    #[test]
    fn turn_count_and_started_are_tracked_incrementally() {
        let mut log = SessionLog::new("s1");
        assert!(!log.started());
        let env = log.next_env();
        log.push(SessionEvent::session_start(env, "/w", "mock"));
        assert!(log.started());
        for turn in 1..=2 {
            let env = log.next_env();
            log.push(SessionEvent::turn_start(env, turn));
            let env = log.next_env();
            log.push(SessionEvent::turn_end(env, turn, TurnEndReason::Completed));
        }
        assert_eq!(log.turn_count(), 2);
    }
}
