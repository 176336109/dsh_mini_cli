//! turn / step 驱动器 —— Mini 的心脏。
//!
//! 对应 dsh 的 `@deepseek-ai/dsh-agent-loop`（`ctx.agentLoop`）。这里必须一比一
//! 保留的语义只有两条，但两条都不能省：
//!
//! 1. **一个 turn 可以包含 0 个 step**：首个输入为空或被拒时，turn 开完就关。
//! 2. **step 的推进条件不是"有 tool_calls 就再问一次"**，而是"工具还欠一次请求"。
//!
//! 已知简化（与 dsh 的差异，写在这里以免误读）：dsh 的 inbox 支持 turn 进行中
//! 追加输入，把新的 step 续到同一个 turn 上；Mini 简化为"一个 turn 一个输入"，
//! 没有 inbox / steering。要补的话，入口在 `run_turn` 的 step 循环条件处。
//!
//! ```text
//! turn/start
//!   -> step/start -> 组装 prompt+schemas -> 模型流式返回
//!   -> assistant/message
//!   -> tool/call* -> 执行 -> tool/result*
//!   -> step/end
//!   -> 仍有 tool_calls ? 下一个 step : turn/end
//! ```

mod prompt;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_llm::{Completion, ContentBlock, LlmAdapter, LlmError, LlmEvent, LlmRequest};
use core_session::{now_ms, SessionEvent, SessionLog, SessionSink, TurnEndReason};
use core_tools::{Guard, Registry, ToolCtx};

pub use prompt::EnvFacts;

#[derive(Debug, Clone)]
pub struct LoopConfig {
    pub model: String,
    pub cwd: PathBuf,
    /// 单个 turn 内 step 的硬上限，防止工具死循环。
    pub max_steps: usize,
    /// 单次工具执行的超时。超时后整个进程组会被杀掉（unix）。
    pub tool_timeout: Duration,
}

/// 工具超时的默认值：编译、跑测试这类正常工具调用都需要几分钟。
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

impl LoopConfig {
    pub fn new(cwd: impl Into<PathBuf>, model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            cwd: cwd.into(),
            max_steps: 24,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
        }
    }
}

/// 循环向外广播的实时事件。仅用于展示/持久化，不参与状态推导。
#[derive(Debug, Clone)]
pub enum LoopEvent {
    TurnStart {
        turn: u64,
    },
    StepStart {
        turn: u64,
        step: u64,
    },
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallDelta {
        name: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        name: String,
        is_error: bool,
        content: String,
    },
    TurnEnd {
        turn: u64,
        reason: TurnEndReason,
    },
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub turn: u64,
    pub steps: u64,
    pub text: String,
    pub reason: TurnEndReason,
}

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error(transparent)]
    Llm(#[from] LlmError),
}

pub struct AgentLoop {
    llm: Arc<dyn LlmAdapter>,
    tools: Arc<Registry>,
    cfg: LoopConfig,
    log: SessionLog,
    /// 增量落盘；每条事件写一次，崩溃只丢当前事件而不是整个 turn。
    sink: Option<Box<dyn SessionSink>>,
}

impl AgentLoop {
    pub fn new(llm: Arc<dyn LlmAdapter>, tools: Arc<Registry>, cfg: LoopConfig) -> Self {
        let log = SessionLog::new(format!("s{}", now_ms()));
        Self {
            llm,
            tools,
            cfg,
            log,
            sink: None,
        }
    }

    /// 从已有日志续跑：`next_seq` 与 turn 编号都接着走。
    pub fn resume(
        llm: Arc<dyn LlmAdapter>,
        tools: Arc<Registry>,
        cfg: LoopConfig,
        log: SessionLog,
    ) -> Self {
        Self {
            llm,
            tools,
            cfg,
            log,
            sink: None,
        }
    }

    /// 挂上落盘 sink。事件按产生顺序逐条写出，不再"turn 结束才整体同步"。
    pub fn with_sink(mut self, sink: Box<dyn SessionSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn log(&self) -> &SessionLog {
        &self.log
    }

    pub fn into_log(self) -> SessionLog {
        self.log
    }

    /// 唯一的事件入口：内存日志 + 落盘 sink 一起推进。
    fn record(&mut self, event: SessionEvent) {
        let failure = match self.sink.as_mut() {
            Some(sink) => sink.append(&event).err(),
            None => None,
        };
        if let Some(err) = failure {
            // 磁盘满/只读之类的持久化故障不该让 agent 停摆：记一次然后停用 sink。
            eprintln!("警告：会话落盘失败，后续事件只留在内存：{err}");
            self.sink = None;
        }
        self.log.push(event);
    }

    fn system_prompt(&self) -> String {
        prompt::build(&EnvFacts {
            cwd: &self.cfg.cwd,
            model: &self.cfg.model,
            tools: self.tools.name_slice(),
        })
    }

    /// 跑一个 turn。输入为空时仍会开/关一个 turn（0 step），与 dsh 一致。
    pub async fn run_turn(
        &mut self,
        input: &str,
        guard: &dyn Guard,
        emit: &mut (dyn FnMut(LoopEvent) + Send),
    ) -> Result<TurnOutcome, LoopError> {
        let turn = self.log.turn_count() + 1;
        let ctx = ToolCtx::new(&self.cfg.cwd).with_timeout(self.cfg.tool_timeout);

        if !self.log.started() {
            let cwd = self.cfg.cwd.display().to_string();
            let model = self.cfg.model.clone();
            let env = self.log.next_env();
            self.record(SessionEvent::session_start(env, cwd, model));

            let prompt = self.system_prompt();
            let env = self.log.next_env();
            self.record(SessionEvent::system_message(
                env,
                vec![ContentBlock::text(prompt)],
            ));
        }

        let env = self.log.next_env();
        self.record(SessionEvent::turn_start(env, turn));
        emit(LoopEvent::TurnStart { turn });

        if input.trim().is_empty() {
            let env = self.log.next_env();
            self.record(SessionEvent::turn_end(env, turn, TurnEndReason::Empty));
            emit(LoopEvent::TurnEnd {
                turn,
                reason: TurnEndReason::Empty,
            });
            return Ok(TurnOutcome {
                turn,
                steps: 0,
                text: String::new(),
                reason: TurnEndReason::Empty,
            });
        }

        let env = self.log.next_env();
        self.record(SessionEvent::user_message(
            env,
            vec![ContentBlock::text(input)],
        ));

        let mut text = String::new();
        let mut steps = 0u64;
        let mut reason = TurnEndReason::Completed;
        let max_steps = self.cfg.max_steps.max(1) as u64;

        for step in 1..=max_steps {
            steps = step;
            let env = self.log.next_env();
            self.record(SessionEvent::step_start(env, turn, step));
            emit(LoopEvent::StepStart { turn, step });

            // 每个 step 都从日志投影重新推导历史：日志是唯一事实来源。
            // 投影与 schema 都由下面的对象持有，这里只借切片，不克隆历史。
            let result = {
                let req = LlmRequest::new(&self.cfg.model, self.log.replay(), self.tools.schemas());
                let mut sink = |ev: LlmEvent| match ev {
                    LlmEvent::ReasoningDelta(text) => emit(LoopEvent::ReasoningDelta(text)),
                    LlmEvent::TextDelta(text) => emit(LoopEvent::TextDelta(text)),
                    LlmEvent::ToolCallDelta { name, .. } => {
                        if let Some(name) = name {
                            emit(LoopEvent::ToolCallDelta { name });
                        }
                    }
                };
                // 增量只用于展示；权威结果以返回的 Completion 为准。
                self.llm.complete(&req, &mut sink).await
            };

            let completion = match result {
                Ok(completion) => completion,
                Err(err) => {
                    let env = self.log.next_env();
                    self.record(SessionEvent::turn_end(env, turn, TurnEndReason::Failed));
                    emit(LoopEvent::TurnEnd {
                        turn,
                        reason: TurnEndReason::Failed,
                    });
                    return Err(err.into());
                }
            };

            let Completion {
                content,
                tool_calls,
                usage,
                ..
            } = completion;

            // 本 step 的文本单独结算：`text` 只保留最后一个 step 的输出，
            // 否则多 step 的中间态会被拼进最终回答。
            let mut step_text = String::new();
            for block in &content {
                if let ContentBlock::Text { text: t } = block {
                    step_text.push_str(t);
                }
            }
            text = step_text;

            // content 直接 move 进事件（不再整段克隆）；tool_calls 需要留到下面执行，
            // 是这一步唯一的一次克隆，且通常只有 1~3 个。
            let env = self.log.next_env();
            self.record(SessionEvent::assistant_message(
                env,
                content,
                tool_calls.clone(),
                Some(usage),
            ));
            emit(LoopEvent::AssistantMessage { text: text.clone() });

            if tool_calls.is_empty() {
                let env = self.log.next_env();
                self.record(SessionEvent::step_end(env, turn, step, 0));
                break;
            }

            let mut executed = 0u32;
            for call in tool_calls {
                emit(LoopEvent::ToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });

                let outcome = self.tools.execute(&call, &ctx, guard).await;

                let env = self.log.next_env();
                self.record(SessionEvent::tool_call_owned(env, call));
                emit(LoopEvent::ToolResult {
                    name: outcome.name.clone(),
                    is_error: outcome.is_error,
                    content: outcome.content.clone(),
                });
                let env = self.log.next_env();
                self.record(SessionEvent::tool_result(
                    env,
                    outcome.call_id,
                    outcome.name,
                    outcome.content,
                    outcome.is_error,
                ));
                executed += 1;
            }

            let env = self.log.next_env();
            self.record(SessionEvent::step_end(env, turn, step, executed));

            if step == max_steps {
                reason = TurnEndReason::MaxSteps;
            }
        }

        let env = self.log.next_env();
        self.record(SessionEvent::turn_end(env, turn, reason));
        emit(LoopEvent::TurnEnd { turn, reason });

        Ok(TurnOutcome {
            turn,
            steps,
            text,
            reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_llm::{Completion, StopReason, ToolCall, ToolSchema};
    use core_tools::{Tool, ToolError};
    use serde_json::json;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Echo;

    impl Tool for Echo {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "echo".into(),
                description: "回显".into(),
                parameters: json!({"type": "object"}),
            }
        }

        fn run<'a>(
            &'a self,
            args: &'a serde_json::Value,
            _ctx: &'a ToolCtx,
        ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
            Box::pin(async move { Ok(args["text"].as_str().unwrap_or_default().to_string()) })
        }
    }

    struct ScriptedAdapter {
        calls: AtomicUsize,
    }

    impl LlmAdapter for ScriptedAdapter {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn complete<'a>(
            &'a self,
            _req: &'a LlmRequest<'a>,
            sink: &'a mut (dyn FnMut(LlmEvent) + Send),
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Completion, LlmError>> + Send + 'a>>
        {
            Box::pin(async move {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                sink(LlmEvent::TextDelta("…".into()));
                if n == 0 {
                    Ok(Completion {
                        content: vec![ContentBlock::text("调查中")],
                        tool_calls: vec![ToolCall {
                            id: "c1".into(),
                            name: "echo".into(),
                            arguments: json!({"text": "hi"}),
                        }],
                        stop: StopReason::ToolCalls,
                        usage: Default::default(),
                    })
                } else {
                    Ok(Completion::text("完成"))
                }
            })
        }
    }

    fn build_loop() -> AgentLoop {
        let llm = Arc::new(ScriptedAdapter {
            calls: AtomicUsize::new(0),
        });
        let tools = Arc::new(Registry::new().with(Echo));
        AgentLoop::new(llm, tools, LoopConfig::new(".", "scripted"))
    }

    #[tokio::test]
    async fn tool_calls_extend_the_turn_into_a_second_step() {
        let mut agent = build_loop();
        let mut sink = |_e: LoopEvent| {};
        let outcome = agent
            .run_turn("做事", &core_tools::AllowAll, &mut sink)
            .await
            .unwrap();
        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.reason, TurnEndReason::Completed);
        assert_eq!(outcome.text, "完成");
    }

    #[tokio::test]
    async fn tool_results_land_in_the_log_and_replay() {
        let mut agent = build_loop();
        let mut sink = |_e: LoopEvent| {};
        agent
            .run_turn("做事", &core_tools::AllowAll, &mut sink)
            .await
            .unwrap();

        let log = agent.log();
        let types: Vec<&str> = log.events().iter().map(SessionEvent::type_name).collect();
        assert_eq!(types[0], "session/start");
        assert!(types.contains(&"tool/call"));
        assert!(types.contains(&"tool/result"));

        let messages = log.replay();
        assert!(messages
            .iter()
            .any(|m| matches!(m, core_llm::Message::Tool { .. })));
    }

    #[tokio::test]
    async fn empty_input_closes_the_turn_with_zero_steps() {
        let mut agent = build_loop();
        let mut sink = |_e: LoopEvent| {};
        let outcome = agent
            .run_turn("   ", &core_tools::AllowAll, &mut sink)
            .await
            .unwrap();
        assert_eq!(outcome.steps, 0);
        assert_eq!(outcome.reason, TurnEndReason::Empty);
    }

    #[tokio::test]
    async fn second_turn_continues_turn_and_seq_numbering() {
        let mut agent = build_loop();
        let mut sink = |_e: LoopEvent| {};
        agent
            .run_turn("一", &core_tools::AllowAll, &mut sink)
            .await
            .unwrap();
        let second = agent
            .run_turn("二", &core_tools::AllowAll, &mut sink)
            .await
            .unwrap();
        assert_eq!(second.turn, 2);
        let seqs: Vec<_> = agent.log().events().iter().map(SessionEvent::seq).collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[tokio::test]
    async fn sink_sees_every_event_in_order() {
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorder = Arc::clone(&seen);
        let agent = build_loop().with_sink(Box::new(move |event: &SessionEvent| {
            recorder.lock().unwrap().push(event.type_name().to_string());
            Ok(())
        }));

        let mut agent = agent;
        let mut sink = |_e: LoopEvent| {};
        agent
            .run_turn("做事", &core_tools::AllowAll, &mut sink)
            .await
            .unwrap();

        let recorded = seen.lock().unwrap().clone();
        let in_memory: Vec<String> = agent
            .log()
            .events()
            .iter()
            .map(|e| e.type_name().to_string())
            .collect();
        assert_eq!(recorded, in_memory, "落盘顺序必须与内存日志一致");
        assert_eq!(recorded.first().map(String::as_str), Some("session/start"));
        assert_eq!(recorded.last().map(String::as_str), Some("turn/end"));
    }

    #[tokio::test]
    async fn a_failing_sink_does_not_stop_the_turn() {
        let agent =
            build_loop().with_sink(Box::new(
                |_event: &SessionEvent| Err("磁盘只读".to_string()),
            ));
        let mut agent = agent;
        let mut sink = |_e: LoopEvent| {};
        let outcome = agent
            .run_turn("做事", &core_tools::AllowAll, &mut sink)
            .await
            .unwrap();
        assert_eq!(outcome.reason, TurnEndReason::Completed);
    }
}
