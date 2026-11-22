//! `dsh-mini` 命令行入口。
//!
//! 模型可选：`--model mock`（不联网，用于自检）或任意 DeepSeek 模型名
//! （`deepseek-flash` / `deepseek-v4-pro`），走同一个 `LlmAdapter` 接缝。
//!
//! 装配顺序固定：cwd → home/.env → 工具集（含 MCP）→ 模型 → 会话 → 循环。
//! 任何一步失败都直接退出，不留半初始化的状态。

mod cli;
mod env;
mod render;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use core_llm::LlmAdapter;
use core_loop::{AgentLoop, LoopConfig, ProjectRules, PromptContext};
use core_session::{now_ms, SessionLog, TurnEndReason};
use core_tools::sandbox::{select as select_sandbox, SandboxKind};
use core_tools::{head_chars, AllowAll, Registry};
use providers::tools::path::normalize;
use providers::tools::{
    BashTool, GlobTool, GrepTool, ListDirTool, ReadFileTool, StrReplaceTool, WriteFileTool,
};
use providers::{register_server, DeepSeekAdapter, JsonlStore, McpConfig, MockAdapter};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::cli::Cli;
use crate::env::DotEnv;
use crate::render::print_event;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("错误：{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// 车机通常是 2~4 核。worker 线程数按小值封顶：每个 worker 都有独立的栈，
/// 在内存紧张的目标上没必要按开发机的核数铺开。
fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 4)
}

fn run(cli: Cli) -> Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads())
        .enable_all()
        .build()
        .context("初始化异步运行时失败")?;
    runtime.block_on(run_agent(cli))
}

async fn run_agent(cli: Cli) -> Result<ExitCode> {
    let cwd = absolute(&cli.cwd)?;
    let home = absolute(&cli.home.clone().unwrap_or_else(env::default_home))?;
    let dotenv = DotEnv::load(&home);
    let store = JsonlStore::new(home.join("sessions"));

    // 沙箱在启动阶段就定下来：不可用即失败，绝不"先跑起来再说"。
    let kind = SandboxKind::parse(&cli.sandbox).map_err(|err| anyhow!(err))?;
    let selection = select_sandbox(kind, &cwd)?;
    if let Some(warning) = &selection.warning {
        eprintln!("警告：{warning}");
    }
    let sandbox = selection.sandbox;

    let mut registry = Registry::new()
        .with(BashTool::new(Arc::clone(&sandbox)))
        .with(ReadFileTool)
        .with(WriteFileTool)
        .with(StrReplaceTool)
        .with(ListDirTool)
        .with(GrepTool)
        .with(GlobTool);

    if let Some(path) = &cli.mcp {
        let config = McpConfig::load(path)?;
        if config.servers.is_empty() {
            eprintln!("警告：{} 里没有 servers", path.display());
        }
        for server in &config.servers {
            let count = register_server(&mut registry, server)
                .await
                .with_context(|| format!("连接 MCP 服务器 {} 失败", server.name))?;
            eprintln!("MCP · {} · {count} 个工具", server.name);
        }
    }
    let tool_names = registry.names().join(",");
    let tools = Arc::new(registry);

    // 项目约定 + 技能索引：只在首次生成系统提示时用到（续跑沿用日志里的那一份）。
    let project_rules = load_project_rules(&cli, &cwd)?;
    let skills_dir = match &cli.skills_dir {
        Some(dir) => Some(absolute(dir)?),
        None => None,
    };
    let mut skills = Vec::new();
    if let Some(dir) = &skills_dir {
        let scan = core_loop::skills::scan(dir);
        for warning in &scan.warnings {
            eprintln!("警告：{warning}");
        }
        skills = scan.skills;
    }
    let extra_instructions = load_extra_instructions(&cli)?;
    let prompt_ctx = PromptContext {
        project_rules,
        extra_instructions,
        skills,
    };

    let llm = build_adapter(&cli, &dotenv)?;
    let cfg = LoopConfig {
        model: cli.model.clone(),
        cwd: cwd.clone(),
        max_steps: cli.max_steps,
        // 0 秒等于"立刻超时"，没有意义；至少给 1 秒。
        tool_timeout: Duration::from_secs(cli.tool_timeout.max(1)),
    };

    let history_budget = cli.history_budget.saturating_mul(1024 * 1024);
    let (log, resumed_events) = match &cli.resume {
        Some(requested) => {
            let id = if requested == "latest" {
                store
                    .latest_id()
                    .context("没有可续跑的会话，先跑一次不带 --resume 的任务")?
            } else {
                requested.clone()
            };
            let events = store
                .load(&id)
                .with_context(|| format!("加载会话 {id} 失败"))?;
            let count = events.len();
            (
                SessionLog::resume(id, events).with_history_budget(history_budget),
                count,
            )
        }
        None => (
            SessionLog::new(format!("s{}", now_ms())).with_history_budget(history_budget),
            0,
        ),
    };

    let session_id = log.id().to_string();
    let mut agent = AgentLoop::resume(llm, tools, cfg, log)
        .with_prompt_context(prompt_ctx.clone())
        // 技能目录同时作为文件工具的只读根：模型要能 read_file 到 SKILL.md 正文。
        .with_read_roots(skills_dir.iter().cloned().collect());

    // 逐条落盘的句柄：整个会话只打开一次文件。磁盘不可写时降级为纯内存运行，
    // 不让"日志写不进去"变成"任务跑不了"。
    let log_limit = cli.log_max_mb.saturating_mul(1024 * 1024);
    match store.appender(&session_id) {
        Ok(appender) => agent = agent.with_sink(Box::new(appender.with_max_bytes(log_limit))),
        Err(err) => eprintln!("警告：会话无法落盘（{err}），本次运行不留日志"),
    }

    if resumed_events > 0 {
        eprintln!("续跑会话 {session_id}（已有 {resumed_events} 条事件）");
    }
    eprintln!(
        "dsh-mini · 模型={} · 会话={} · 沙箱={} · 工具={}",
        cli.model,
        session_id,
        sandbox.describe(),
        tool_names
    );
    eprintln!(
        "工作目录：{} · 数据目录：{} · 工具超时：{}s · 历史预算：{} · 日志上限：{}",
        cwd.display(),
        home.display(),
        cli.tool_timeout.max(1),
        if cli.history_budget == 0 {
            "不限".to_string()
        } else {
            format!("{} MB", cli.history_budget)
        },
        if cli.log_max_mb == 0 {
            "不限".to_string()
        } else {
            format!("{} MB", cli.log_max_mb)
        }
    );
    if resumed_events > 0 {
        eprintln!(
            "提示 · 沿用日志里的系统提示（{} 不看本次的约定/技能）",
            session_id
        );
    } else {
        eprintln!("提示 · {}", prompt_ctx.summary());
    }

    let mut sink = print_event;

    if cli.prompt.is_empty() {
        interactive(&mut agent, &mut sink).await?;
        return Ok(ExitCode::SUCCESS);
    }

    let input = cli.prompt.join(" ");
    let outcome = agent.run_turn(&input, &AllowAll, &mut sink).await?;
    Ok(if outcome.reason == TurnEndReason::Failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn interactive(
    agent: &mut AgentLoop,
    sink: &mut (dyn FnMut(core_loop::LoopEvent) + Send),
) -> Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    loop {
        eprint!("\n> ");
        std::io::stderr().flush().ok();
        line.clear();
        // 异步读：不把 runtime 的工作线程堵在 read 上。
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input, "exit" | "quit" | ":q") {
            break;
        }
        agent.run_turn(input, &AllowAll, sink).await?;
    }
    Ok(())
}

/// 系统提示里项目约定的字符上限。Agents.md 是给人看的，正常几百到几千字。
const MAX_PROJECT_RULES_CHARS: usize = 16_000;

/// 读取项目约定：`--agents-file` 优先，否则自动找 `<cwd>/Agents.md`。
fn load_project_rules(cli: &Cli, cwd: &Path) -> Result<Option<ProjectRules>> {
    let path = match &cli.agents_file {
        Some(path) => Some(absolute(path)?),
        None => ["Agents.md", "AGENTS.md"]
            .iter()
            .map(|name| cwd.join(name))
            .find(|candidate| candidate.is_file()),
    };

    let Some(path) = path else { return Ok(None) };
    let bytes = std::fs::read(&path).with_context(|| format!("读不到 {}", path.display()))?;
    let original_bytes = bytes.len();
    let text = String::from_utf8_lossy(&bytes);
    let text = head_chars(env::strip_bom(&text), MAX_PROJECT_RULES_CHARS);
    eprintln!("约定 · {}（{original_bytes} 字节）", path.display());

    Ok(Some(ProjectRules {
        source: path.display().to_string(),
        text: text.to_string(),
        original_bytes,
    }))
}

/// 读取命令行显式给的附加指令：`--system-file` 优先于 `--system-prompt`。
fn load_extra_instructions(cli: &Cli) -> Result<Option<String>> {
    let text = match (&cli.system_file, &cli.system_prompt) {
        (Some(path), _) => {
            let path = absolute(path)?;
            let bytes =
                std::fs::read(&path).with_context(|| format!("读不到 {}", path.display()))?;
            eprintln!("附加指令 · {}（{} 字节）", path.display(), bytes.len());
            String::from_utf8_lossy(&bytes).into_owned()
        }
        (None, Some(text)) => text.clone(),
        (None, None) => return Ok(None),
    };

    let text = env::strip_bom(&text).trim();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(head_chars(text, MAX_PROJECT_RULES_CHARS).to_string()))
}

fn build_adapter(cli: &Cli, dotenv: &DotEnv) -> Result<Arc<dyn LlmAdapter>> {
    if cli.model == "mock" {
        eprintln!("模型 · mock（离线，不联网）");
        return Ok(Arc::new(MockAdapter::auto()));
    }

    // 显式给的文件优先：systemd LoadCredential / Docker secrets 都是裸文件。
    let key = match &cli.key_file {
        Some(path) => env::read_key_file(path).map_err(|err| anyhow!(err))?,
        None => dotenv.lookup("DEEPSEEK_API_KEY").context(
            "找不到 DEEPSEEK_API_KEY。设置环境变量、用 --key-file <file>，或写入 ~/.dsh-mini/.env",
        )?,
    };
    let base_url = dotenv
        .lookup("DEEPSEEK_BASE_URL")
        .unwrap_or_else(|| cli.base_url.clone());
    // 代理：命令行优先，其次 .env 里的 DSH_PROXY（车机上常写死一个车内网关）。
    let proxy = cli.proxy.clone().or_else(|| dotenv.lookup("DSH_PROXY"));
    let adapter = DeepSeekAdapter::new(key.clone(), base_url.clone(), cli.model.clone())?
        .with_thinking(cli.thinking)
        .with_proxy(proxy.as_deref())?;
    eprintln!(
        "模型 · {} @ {} · key={} · thinking={} · proxy={}",
        cli.model,
        base_url,
        env::mask(&key),
        if cli.thinking { "on" } else { "off" },
        proxy.as_deref().unwrap_or("默认")
    );
    Ok(Arc::new(adapter))
}

/// 把 `--cwd` 展开成不含 `.` / `..` 的绝对路径。
///
/// 不做 `canonicalize`：一是它要求目录已存在（`--cwd` 指向待创建目录时会在启动阶段
/// 就报错），二是 Windows 上会加 `\\?\` 前缀，污染提示词和日志里的路径。
fn absolute(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(normalize(&joined))
}
