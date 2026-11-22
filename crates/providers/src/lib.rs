//! 能力接缝的 Rust 原生实现集合。
//!
//! dsh 里每个接缝都是"服务定义 + 服务提供方 + 消费者"三件套。Mini 保留了
//! 接缝的形状（trait 在 `core-*` 里，实现在这里），但每类只留一个提供方：
//!
//! | 接缝 | dsh 提供方 | Mini 提供方 |
//! |---|---|---|
//! | `ctx.llm` | llm-deepseek / llm-pi-ai | [`deepseek::DeepSeekAdapter`]、[`mock::MockAdapter`] |
//! | `ctx.sessionPersistence` | session-persistence-jsonl | [`jsonl::JsonlStore`] + [`jsonl::Appender`] |
//! | `ctx.shell` | bash-local / bash-sandbox | [`tools::BashTool`] |
//! | `ctx.fs` | fs-local / fs-sandbox | [`tools::ReadFileTool`] 等 |
//! | `ctx.sandbox` | sandbox-local（bwrap / Landlock） | `core_tools::sandbox::LandlockSandbox`（实现在 core-tools） |
//! | `ctx.mcpClient` | dsh-mcp-client / dsh-mcp-manager | [`mcp::McpClient`] |
//!
//! 注意 MCP 的接缝用法和别的不同：MCP 不是一个可替换的 provider，而是
//! **provider 的工厂** —— 每连一个服务器就往 `ctx.tools` 里注册若干工具，
//! 循环侧完全无感。
//!
//! 目标平台是 Linux（WSL / 车机），Windows 只作为开发机保留：平台差异集中在
//! [`tools::bash`] 的 shell 选择上，其余实现都是跨平台的。

pub mod deepseek;
pub mod jsonl;
pub mod mcp;
pub mod mock;
pub mod tools;

pub use deepseek::DeepSeekAdapter;
pub use jsonl::{Appender, JsonlStore, StoreError};
pub use mcp::{register_server, McpClient, McpConfig, McpError, McpServerConfig, McpTool};
pub use mock::{MockAdapter, MockTurn};

// 沙箱的实现跟着 `core_tools::Guard` 一起放在 core-tools（见那边的模块文档），
// 这里只是保住 `providers::NoSandbox` 这类老导入路径。
pub use core_tools::sandbox::{
    select as select_sandbox, LandlockSandbox, NoSandbox, Sandbox, SandboxError, SandboxKind,
    Selection as SandboxSelection, SharedSandbox,
};
