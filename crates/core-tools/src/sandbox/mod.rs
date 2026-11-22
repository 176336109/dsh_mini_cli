//! 沙箱接缝（`ctx.sandbox`）。
//!
//! dsh 的语义是**故障关闭**：没有可用的沙箱后端时不是降级成"无隔离执行"，
//! 而是拒绝执行工具。这里保留同样的契约：
//!
//! - [`SandboxKind::Landlock`]：后端不可用 → 启动即失败，不静默降级。
//! - [`SandboxKind::Auto`]：Linux 上等价于 Landlock（不可用就失败）；非 Linux 上
//!   没有对应机制，退化为 [`NoSandbox`] 并**明确警告** —— 那条路径只用于开发机。
//! - [`SandboxKind::None`]：显式关掉，需要用户自己知道风险。
//!
//! 为什么这个模块在 `core-tools` 而不是 `providers`：它和 [`crate::Guard`] 一样是
//! "工具执行策略"，而且实现只有 `libc` + `tokio` 两个依赖 —— 放在这里就能对 Linux
//! 目标单独做类型检查（`providers` 因为 reqwest 会拉进 C 工具链，交叉检查不干净）。
//!
//! Linux 实现见 [`landlock`]：不需要非特权 user namespace，也不需要 bwrap 二进制，
//! 也不需要 root —— 这正是 Rust 版相对 Node 版在车机上的决定性优势。

pub mod landlock;

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use tokio::process::Command;

pub use landlock::LandlockSandbox;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("沙箱不可用：{0}")]
    Unavailable(String),
    #[error("沙箱策略拒绝了该次执行：{0}")]
    Denied(String),
}

/// 启动时选择的沙箱后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxKind {
    /// Linux 上强制 Landlock；其他平台退化为 none（带警告）。
    Auto,
    /// 明确不要沙箱（开发调试用）。
    None,
    /// 强制 Landlock，不可用即启动失败。
    Landlock,
}

impl SandboxKind {
    /// 供 CLI 解析 `--sandbox`。
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "none" | "off" => Ok(Self::None),
            "landlock" | "strict" => Ok(Self::Landlock),
            other => Err(format!(
                "未知的沙箱模式 `{other}`（可选 auto / none / landlock）"
            )),
        }
    }
}

/// 在执行前包装即将 spawn 的进程。**必须**在命令真正跑起来之前调用。
pub trait Sandbox: Send + Sync + fmt::Debug {
    fn name(&self) -> &'static str;

    /// 启动时打给人看的一句话（可写范围这类信息只有实现方知道）。
    fn describe(&self) -> String {
        self.name().to_string()
    }

    /// 成功返回说明约束已经挂上；返回 `Err` 就**不能**执行该命令。
    fn wrap(&self, cmd: &mut Command) -> Result<(), SandboxError>;
}

/// 不做任何限制，但**如实报告自己没做**。
#[derive(Debug, Default)]
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn name(&self) -> &'static str {
        "none"
    }

    fn wrap(&self, _cmd: &mut Command) -> Result<(), SandboxError> {
        Ok(())
    }
}

pub type SharedSandbox = Arc<dyn Sandbox>;

/// 选择结果：后端 + 需要提示给用户的话（例如"这台机器没有 Landlock"）。
#[derive(Debug)]
pub struct Selection {
    pub sandbox: SharedSandbox,
    pub warning: Option<String>,
}

/// 按启动参数选择后端。`root` 是唯一可写根（工作目录）。
pub fn select(kind: SandboxKind, root: &Path) -> Result<Selection, SandboxError> {
    match kind {
        SandboxKind::None => Ok(Selection {
            sandbox: Arc::new(NoSandbox),
            warning: Some("已显式关闭沙箱：工具可以读写工作目录之外的路径".to_string()),
        }),
        SandboxKind::Landlock | SandboxKind::Auto => select_landlock(root, kind),
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn select_landlock(root: &Path, _kind: SandboxKind) -> Result<Selection, SandboxError> {
    let sandbox = LandlockSandbox::new(root)
        .map_err(|err| SandboxError::Unavailable(format!("{err}；可用 --sandbox none 显式关闭")))?;
    Ok(Selection {
        sandbox: Arc::new(sandbox),
        warning: None,
    })
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn select_landlock(_root: &Path, kind: SandboxKind) -> Result<Selection, SandboxError> {
    let reason = format!(
        "当前平台（{} / {}）没有 Landlock",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    match kind {
        SandboxKind::Auto => Ok(Selection {
            sandbox: Arc::new(NoSandbox),
            warning: Some(format!("{reason}：退化为无沙箱，仅可用于开发机")),
        }),
        _ => Err(SandboxError::Unavailable(reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sandbox_is_explicit_about_its_name() {
        let sandbox = NoSandbox;
        assert_eq!(sandbox.name(), "none");
    }

    #[test]
    fn parses_cli_values() {
        assert_eq!(SandboxKind::parse("auto").unwrap(), SandboxKind::Auto);
        assert_eq!(SandboxKind::parse("NONE").unwrap(), SandboxKind::None);
        assert_eq!(SandboxKind::parse("strict").unwrap(), SandboxKind::Landlock);
        assert!(SandboxKind::parse("bwrap").is_err());
    }

    #[test]
    fn explicit_none_always_succeeds_with_a_warning() {
        let selection = select(SandboxKind::None, Path::new(".")).unwrap();
        assert_eq!(selection.sandbox.name(), "none");
        assert!(selection.warning.is_some());
    }
}
