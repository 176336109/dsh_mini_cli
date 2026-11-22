//! `bash` 工具：唯一的"任意代码执行"入口。
//!
//! 这是整个 harness 里最需要防守的地方，除了沙箱之外还有一条硬要求：
//! **子进程输出必须有上限**。老实现用 `Command::output()` 把 stdout/stderr
//! 全量读进内存，一条 `yes` 或 `cat /dev/urandom` 就能把车机的内存吃光。
//! 现在两条流都是边读边限，超限立刻 `kill`，内存占用与命令的输出量无关。

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;

use core_llm::ToolSchema;
use core_tools::sandbox::SharedSandbox;
use core_tools::{guard_process_group, isolate_process_group, Tool, ToolCtx, ToolError};
use serde_json::json;
use tokio::io::AsyncReadExt;

use super::{arg_str, cap_chars};

/// 单条流进入模型上下文前的字符上限。
const STREAM_CHARS: usize = 16_000;

/// 单条流在内存里的字节上限（读满即 kill）。按 4 字节/字符留足余量。
const STREAM_BYTES: usize = 64 * 1024;

/// 读子进程输出的分块大小。
const READ_CHUNK: usize = 8 * 1024;

#[cfg(windows)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("cmd");
    // 注意：不要在此加 `chcp 65001`。实测它会让 cmd 把内部消息按 ANSI 代码页输出，
    // 反而把中文变成 GBK 字节；不加则 UTF-8 内容原样透传。Linux 下用 sh，无此问题。
    cmd.args(["/C", command]);
    cmd
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", command]);
    cmd
}

/// 执行一条 shell 命令。这是唯一的"任意代码执行"入口，也是沙箱唯一需要守的地方。
pub struct BashTool {
    sandbox: SharedSandbox,
}

impl BashTool {
    pub fn new(sandbox: SharedSandbox) -> Self {
        Self { sandbox }
    }
}

impl Tool for BashTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bash".into(),
            description: "在工作目录内执行一条 shell 命令，返回退出码与 stdout/stderr。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "要执行的命令"}
                },
                "required": ["command"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let command = arg_str(args, "command")?;
            let mut cmd = shell_command(&command);
            cmd.current_dir(&ctx.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // 超时被 Registry 丢弃 future 时，子进程随之被杀。
                .kill_on_drop(true);

            // 让命令自成一个进程组：超时被丢弃时才能整组杀掉，
            // 否则 `sh -c 'sleep 100 &'` 的孙进程会活下来。
            isolate_process_group(&mut cmd);

            // 故障关闭：沙箱拒绝包装时，命令不会被执行。
            self.sandbox
                .wrap(&mut cmd)
                .map_err(|err| ToolError::Failed(err.to_string()))?;

            let mut child = cmd
                .spawn()
                .map_err(|err| ToolError::Failed(format!("启动进程失败：{err}")))?;
            // 保险丝：future 被丢弃（超时/取消）时杀整个进程组。
            let mut group = guard_process_group(child.id());
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| ToolError::Failed("拿不到子进程 stdout".into()))?;
            let mut stderr = child
                .stderr
                .take()
                .ok_or_else(|| ToolError::Failed("拿不到子进程 stderr".into()))?;

            let mut out = Vec::with_capacity(READ_CHUNK);
            let mut err = Vec::with_capacity(READ_CHUNK);
            let mut out_open = true;
            let mut err_open = true;
            let mut overflow = false;
            let mut buf_out = [0u8; READ_CHUNK];
            let mut buf_err = [0u8; READ_CHUNK];

            while out_open || err_open {
                tokio::select! {
                    res = stdout.read(&mut buf_out), if out_open => {
                        match res {
                            Ok(0) | Err(_) => out_open = false,
                            Ok(n) => {
                                let room = STREAM_BYTES.saturating_sub(out.len());
                                out.extend_from_slice(&buf_out[..n.min(room)]);
                                if n > room {
                                    overflow = true;
                                    out_open = false;
                                }
                            }
                        }
                    }
                    res = stderr.read(&mut buf_err), if err_open => {
                        match res {
                            Ok(0) | Err(_) => err_open = false,
                            Ok(n) => {
                                let room = STREAM_BYTES.saturating_sub(err.len());
                                err.extend_from_slice(&buf_err[..n.min(room)]);
                                if n > room {
                                    overflow = true;
                                    err_open = false;
                                }
                            }
                        }
                    }
                }
                if overflow {
                    // 立刻掐掉：否则子进程会堵在满的管道上，我们得一直等到超时。
                    let _ = child.start_kill();
                }
            }

            let status = child
                .wait()
                .await
                .map_err(|err| ToolError::Failed(format!("等待进程失败：{err}")))?;
            // 正常收尾后解除保险：用户故意留在后台的服务不该被我们杀掉。
            group.disarm();
            let code = status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string());

            let stdout = String::from_utf8_lossy(&out);
            let stderr = String::from_utf8_lossy(&err);
            let stdout = cap_chars(&stdout, STREAM_CHARS);
            let stderr = cap_chars(&stderr, STREAM_CHARS);

            let mut text = String::with_capacity(stdout.len() + stderr.len() + 96);
            text.push_str("exit_code: ");
            text.push_str(&code);
            text.push('\n');
            if overflow {
                text.push_str("（输出超过上限，进程已被终止）\n");
            }
            text.push_str("--- stdout ---\n");
            text.push_str(&stdout);
            if !stderr.trim().is_empty() {
                text.push_str("\n--- stderr ---\n");
                text.push_str(&stderr);
            }
            Ok(text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_tools::sandbox::NoSandbox;
    use std::sync::Arc;
    use std::time::Duration;

    async fn run(command: &str) -> String {
        let tool = BashTool::new(Arc::new(NoSandbox));
        let ctx = ToolCtx::new(std::env::temp_dir()).with_timeout(Duration::from_secs(20));
        tool.run(&json!({ "command": command }), &ctx)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn reports_exit_code_and_streams() {
        // `echo` 在 cmd 与 sh 下语义一致，正好覆盖两个平台的 shell 分支。
        let out = run("echo hi").await;
        assert!(out.contains("exit_code: 0"), "{out}");
        assert!(out.contains("hi"), "{out}");
        assert!(out.contains("--- stdout ---"), "{out}");
    }

    #[tokio::test]
    async fn huge_output_does_not_blow_memory() {
        // 至少产出 8 MiB；实现必须在上限处截断并杀掉进程，而不是全读进来。
        let cmd = if cfg!(windows) {
            "for /L %i in (1,1,200000) do @echo 0123456789012345678901234567890123456789"
        } else {
            "i=0; while [ $i -lt 200000 ]; do echo 0123456789012345678901234567890123456789; i=$((i+1)); done"
        };
        let out = run(cmd).await;
        assert!(
            out.contains("超过上限") || out.chars().count() < STREAM_CHARS + 200,
            "{out}"
        );
        assert!(
            out.chars().count() < 40_000,
            "输出必须被限制，实际 {}",
            out.chars().count()
        );
    }
}
