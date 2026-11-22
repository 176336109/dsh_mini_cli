//! 命令行参数定义。

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "dsh-mini",
    version,
    about = "DeepSeek Harness 的 Rust 迷你实现（loop + MCP）"
)]
pub struct Cli {
    /// 一次性任务。省略则进入交互模式。
    pub prompt: Vec<String>,

    /// 工作目录（工具的相对路径基准，同时也是路径越界判断的根）。
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,

    /// 数据目录，会话日志写在 <home>/sessions/。
    #[arg(long, value_name = "DIR")]
    pub home: Option<PathBuf>,

    /// `mock` 为离线假模型；其他值当作 DeepSeek 模型名（如 deepseek-flash）。
    #[arg(long, default_value = "mock")]
    pub model: String,

    /// DeepSeek API 基址。
    #[arg(long, default_value = "https://api.deepseek.com")]
    pub base_url: String,

    /// 打开思维链（更慢更贵，复杂任务可试）。
    #[arg(long)]
    pub thinking: bool,

    /// 单个 turn 内 step 上限。
    #[arg(long, default_value_t = 24)]
    pub max_steps: usize,

    /// 续跑会话：`--resume` 取最近一个，`--resume <id>` 指定。
    #[arg(long, num_args = 0..=1, default_missing_value = "latest")]
    pub resume: Option<String>,

    /// MCP 服务器配置（JSON）。会把每个服务器的工具注册进工具集。
    #[arg(long, value_name = "FILE")]
    pub mcp: Option<PathBuf>,

    /// 沙箱后端：`auto`（Linux 上用 Landlock）、`none`、`landlock`。
    #[arg(long, default_value = "auto", value_name = "MODE")]
    pub sandbox: String,

    /// 项目约定文件（默认自动找 <cwd>/Agents.md）。内容会注入系统提示。
    #[arg(long, value_name = "FILE")]
    pub agents_file: Option<PathBuf>,

    /// 附加到系统提示末尾的指令（内联文本）。与 --system-file 同时给时以后者为准。
    #[arg(long, value_name = "TEXT")]
    pub system_prompt: Option<String>,

    /// 附加到系统提示末尾的指令（从文件读）。
    #[arg(long, value_name = "FILE")]
    pub system_file: Option<PathBuf>,

    /// 技能目录：扫描 <DIR>/*/SKILL.md，只把名字与描述注入系统提示，
    /// 正文由模型按需 read_file。该目录同时加入文件工具的只读根。
    #[arg(long, value_name = "DIR")]
    pub skills_dir: Option<PathBuf>,

    /// 从文件读 API Key（取第一行非空内容）。用于 systemd LoadCredential /
    /// Docker secrets 这类"密钥在文件里、且不叫 DEEPSEEK_API_KEY"的场景。
    #[arg(long, value_name = "FILE")]
    pub key_file: Option<PathBuf>,

    /// 单次工具执行超时（秒）。超时后整个进程组被杀掉。
    #[arg(long, default_value_t = 120, value_name = "SECS")]
    pub tool_timeout: u64,

    /// 网关/代理地址（http://、https://、socks5://）。留空则用 reqwest 的默认行为。
    #[arg(long, value_name = "URL")]
    pub proxy: Option<String>,

    /// 单个会话日志的字节上限（MB）。到上限后停止落盘，进程继续跑。
    /// 传 0 关闭限制。
    #[arg(long, default_value_t = 32, value_name = "MB")]
    pub log_max_mb: u64,

    /// 模型可见历史的预算（MB）：超出后最旧的工具输出会被压成省略标记。
    /// 传 0 关闭压缩。磁盘上的会话日志始终完整。
    #[arg(long, default_value_t = 2, value_name = "MB")]
    pub history_budget: usize,
}
