//! 系统提示拼装。
//!
//! 对应 dsh 的 `@deepseek-ai/dsh-system-prompt`（`ctx.systemPrompt`）：该包负责
//! "提示分节 + 工具 schema 装配"。Mini 的分节顺序固定：
//!
//! 1. 身份 / 环境事实 / 工具清单权威性 / 交付纪律 / 作业范围 / 行为规则（内置）
//! 2. 项目约定（`Agents.md`，可选）
//! 3. 可用技能索引（`SKILL.md` 的元数据，可选）
//!
//! 工具 schema **不进提示文本**，只走进请求的 `tools` 字段 —— 这是 dsh 的做法，
//! 也省 token。
//!
//! 两条纪律：
//! - 外部内容（Agents.md / 技能描述）必须**截断后**才进提示：车机上下文很贵。
//! - 注入内容只出现在 `session/start` 之后的 `system/message` 事件里（日志是唯一
//!   事实来源），所以 `--resume` 不会因为换了文件而发生提示漂移。

use std::fmt::Write as _;
use std::path::Path;

pub struct EnvFacts<'a> {
    pub cwd: &'a Path,
    pub model: &'a str,
    /// 直接借自 `Registry` 的名字数组，不额外克隆。
    pub tools: &'a [String],
}

/// 项目约定：`Agents.md` 里人写的那份规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRules {
    /// 来源路径，写进提示让模型知道出处在哪。
    pub source: String,
    /// 已截断的正文。
    pub text: String,
    /// 原始字节数，用于提示里的"已截断"说明。
    pub original_bytes: usize,
}

/// 一个技能在提示里的元数据。正文**不进提示**：模型需要时用 `read_file` 去读。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// `SKILL.md` 的绝对路径。
    pub path: String,
}

/// 提示里的可选外部输入。默认全空 = 只有内置分节。
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    pub project_rules: Option<ProjectRules>,
    /// 命令行显式给的附加指令（`--system-prompt` / `--system-file`）。
    pub extra_instructions: Option<String>,
    pub skills: Vec<SkillMeta>,
}

impl PromptContext {
    pub fn is_empty(&self) -> bool {
        self.project_rules.is_none() && self.extra_instructions.is_none() && self.skills.is_empty()
    }

    /// 启动时打给人看的一行摘要。
    pub fn summary(&self) -> String {
        let rules = match &self.project_rules {
            Some(rules) => format!("{}（{} 字节）", rules.source, rules.original_bytes),
            None => "无".to_string(),
        };
        let extra = match &self.extra_instructions {
            Some(text) => format!("{} 字节", text.len()),
            None => "无".to_string(),
        };
        format!(
            "约定={rules} · 附加指令={extra} · 技能={} 个",
            self.skills.len()
        )
    }
}

pub fn build(facts: &EnvFacts<'_>, context: &PromptContext) -> String {
    let cwd = facts.cwd.display();
    let model = facts.model;
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let tools = if facts.tools.is_empty() {
        "（无）".to_string()
    } else {
        facts.tools.join(", ")
    };

    let mut out = format!(
        "# 你是谁\n\
你是一个运行在命令行里的编码代理（dsh-mini）。你通过调用工具来读取文件、执行命令、修改代码。\n\
\n\
# 环境事实\n\
- 工作目录：{cwd}\n\
- 操作系统：{os} / {arch}\n\
- 模型：{model}\n\
- 可用工具：{tools}\n\
\n\
# 工具清单是权威的\n\
- 上面列出的工具就是你能用的全部工具。这份清单由运行时注入，是事实，不需要验证。\n\
- 不要为了确认「有哪些工具」「某个工具是否存在」去读源码、翻配置文件、探测进程，或手工给子进程发协议报文。\n\
- 只有工具真实返回的错误才能推翻这份清单；你的怀疑不能。\n\
\n\
# 交付纪律\n\
- 用户要求把结果写进某个文件时，那个文件必须真的存在，否则任务就没有完成。\n\
- 写完要确认：读一次刚写的文件，或列一次目录。没确认就别说完成。\n\
- 只把结果写在回复里、文件没落地，算失败。\n\
- 一次只做一件事，做完再做下一件；不要让探索挤掉交付。\n\
- 改文件一律用 write_file / str_replace。不要用 shell 重定向（`>`、`>>`）改文件：\n\
  换行符和编码会被 shell 改掉，而且你拿不到任何确认。\n\
- 需要追加内容：先 read_file（结果会说明是否已读到末尾、末尾有没有换行），\n\
  再用 str_replace 做精确替换或 write_file 整体写回。不要为了查字节去 od / certutil / powershell。\n\
\n\
# 作业范围\n\
- 工作目录是你唯一的作业范围。不要读写工作目录之外的路径。\n\
- 路径一律相对工作目录；需要绝对路径时先确认它仍在工作目录内。\n\
- 用户说「当前目录」「这个文件」就只处理它，不要顺手递归子目录或扩大范围。\n\
- 范围确实不清楚时，按最小范围做完再说明，不要先扩大探索。\n\
\n\
# 行为规则\n\
1. 先读后写：改动任何文件之前，先读它的当前内容。\n\
2. 工具结果就是事实来源。不要凭记忆猜测文件内容或命令输出。\n\
3. 失败不要重试同样的调用；读报错信息，换一种做法。\n\
4. 多步骤任务先列出待办，再逐项完成。\n\
5. 完成后用一两句话说明改了什么，不要复述工具输出。\n"
    );

    if let Some(rules) = &context.project_rules {
        out.push_str("\n# 项目约定（优先级高于上面的默认行为规则）\n");
        let _ = writeln!(out, "- 来源：{}", rules.source);
        if rules.text.len() < rules.original_bytes {
            let _ = writeln!(
                out,
                "- 注意：原文 {} 字节，此处只保留前 {} 字节",
                rules.original_bytes,
                rules.text.len()
            );
        }
        out.push_str("- 以下是该项目自己声明的要求，冲突时以项目约定为准：\n\n");
        out.push_str(&rules.text);
        if !rules.text.ends_with('\n') {
            out.push('\n');
        }
    }

    // 放在所有内置分节之后：这是本次运行的用户显式要求，优先级最高。
    if let Some(extra) = &context.extra_instructions {
        out.push_str("\n# 附加要求（本次运行由命令行指定，优先级最高）\n");
        out.push_str(extra);
        if !extra.ends_with('\n') {
            out.push('\n');
        }
    }

    if !context.skills.is_empty() {
        out.push_str("\n# 可用技能\n");
        out.push_str(
            "- 下面每一条是一份技能说明（工作流程 + 脚本 + 判定标准）。**只有元数据在这里，正文没进提示。**\n\
- 当任务与某个技能的描述匹配时，用 read_file 读取它的 path 拿到完整步骤，再照着做。\n\
- 任务不匹配就不要读，别为了探索而读。\n\n",
        );
        for skill in &context.skills {
            let _ = writeln!(
                out,
                "- {} — {}\n  path: {}",
                skill.name, skill.description, skill.path
            );
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn facts<'a>(cwd: &'a Path, tools: &'a [String]) -> EnvFacts<'a> {
        EnvFacts {
            cwd,
            model: "mock",
            tools,
        }
    }

    #[test]
    fn mentions_cwd_and_tools() {
        let cwd = PathBuf::from("/work/repo");
        let tools = vec!["bash".to_string(), "read_file".to_string()];
        let text = build(&facts(&cwd, &tools), &PromptContext::default());
        assert!(text.contains("/work/repo"));
        assert!(text.contains("bash, read_file"));
        assert!(text.contains("# 行为规则"));
        assert!(!text.contains("# 项目约定"), "没传约定就不该出现该分节");
    }

    #[test]
    fn project_rules_are_appended_with_source() {
        let cwd = PathBuf::from("/work/repo");
        let tools: Vec<String> = Vec::new();
        let context = PromptContext {
            project_rules: Some(ProjectRules {
                source: "/work/repo/Agents.md".into(),
                text: "- 目标平台是 Linux".into(),
                original_bytes: 24,
            }),
            extra_instructions: None,
            skills: Vec::new(),
        };
        let text = build(&facts(&cwd, &tools), &context);
        assert!(text.contains("# 项目约定"));
        assert!(text.contains("- 目标平台是 Linux"));
        assert!(text.contains("/work/repo/Agents.md"));
    }

    #[test]
    fn extra_instructions_come_last() {
        let cwd = PathBuf::from("/work/repo");
        let tools: Vec<String> = Vec::new();
        let context = PromptContext {
            project_rules: None,
            extra_instructions: Some("只用 rustls，不要引入 openssl".into()),
            skills: Vec::new(),
        };
        let text = build(&facts(&cwd, &tools), &context);
        let extra_at = text.find("# 附加要求").expect("应当有附加要求分节");
        let tail = &text[extra_at..];
        assert!(tail.contains("不要引入 openssl"));
        // 附加要求必须是最后一节，不能排在内置规则前面。
        assert!(!tail.contains("# 行为规则"));
    }

    #[test]
    fn skills_only_contribute_metadata_not_content() {
        let cwd = PathBuf::from("/work/repo");
        let tools: Vec<String> = Vec::new();
        let context = PromptContext {
            project_rules: None,
            extra_instructions: None,
            skills: vec![SkillMeta {
                name: "eval-runner".into(),
                description: "跑评测".into(),
                path: "/work/repo/.skills/eval/SKILL.md".into(),
            }],
        };
        let text = build(&facts(&cwd, &tools), &context);
        assert!(text.contains("# 可用技能"));
        assert!(text.contains("eval-runner — 跑评测"));
        assert!(text.contains("/work/repo/.skills/eval/SKILL.md"));
        assert!(text.contains("正文没进提示"));
    }
}
