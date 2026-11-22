//! 工具注册表 + 受守卫的执行管线。
//!
//! 对应 dsh 的 `@deepseek-ai/dsh-tools`（`ctx.tools`）。管线形状保持一致：
//!
//! ```text
//! tool/call  ->  guard.check  ->  tool.run  ->  结果归一化（截断/错误码）  ->  tool/result
//! ```
//!
//! 刻意删除：`run_code` / PTC 执行器、批量并行调度、presentation 渲染。
//! Mini 串行执行工具 —— bash 本身不是并行安全的，串行也更好调试。

pub mod process;
pub mod sandbox;

mod registry;
mod text;

pub use process::{guard_process_group, isolate_process_group, ProcessGroupGuard};
pub use registry::{Registry, ToolOutcome};
pub use sandbox::{NoSandbox, Sandbox, SandboxError, SandboxKind, Selection, SharedSandbox};
pub use text::{char_len, head_chars, tail_chars, truncate, truncate_owned, MAX_TOOL_OUTPUT_CHARS};

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use core_llm::ToolSchema;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("参数不合法：{0}")]
    InvalidArgs(String),
    #[error("执行失败：{0}")]
    Failed(String),
    #[error("工具不存在：{0}")]
    NotFound(String),
}

#[derive(Debug, Clone)]
pub struct ToolCtx {
    /// 工具的相对路径基准。沙箱生效后同时是唯一可写根。
    pub cwd: PathBuf,
    pub timeout: Duration,
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        // 相对工作目录按进程当前目录展开一次：后面的越界判断需要绝对路径做基准。
        // 不用 canonicalize —— 它会引入 Windows 的 `\\?\` 前缀，且要求目录已存在。
        let cwd = if cwd.is_absolute() {
            cwd
        } else {
            std::env::current_dir()
                .map(|base| base.join(&cwd))
                .unwrap_or(cwd)
        };
        Self {
            cwd,
            timeout: Duration::from_secs(120),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

pub trait Tool: Send + Sync {
    /// 工具的静态声明。会被 [`Registry`] 在注册时缓存一次，运行期不再调用。
    fn schema(&self) -> ToolSchema;

    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>>;
}

/// 执行前守卫。D0 只有 `AllowAll`；D3 换成沙箱策略 + 人工审批。
pub trait Guard: Send + Sync {
    fn check(&self, tool: &str, args: &serde_json::Value) -> Result<(), String>;
}

pub struct AllowAll;

impl Guard for AllowAll {
    fn check(&self, _tool: &str, _args: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }
}
