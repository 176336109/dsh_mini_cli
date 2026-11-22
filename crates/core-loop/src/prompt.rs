//! 系统提示拼装。
//!
//! 对应 dsh 的 `@deepseek-ai/dsh-system-prompt`（`ctx.systemPrompt`）：该包负责
//! "提示分节 + 工具 schema 装配"。Mini 压缩成三段固定分节：
//! 身份 / 环境事实 / 行为规则。工具 schema **不进提示文本**，只走进请求的
//! `tools` 字段 —— 这是 dsh 的做法，也省 token。

use std::path::Path;

pub struct EnvFacts<'a> {
    pub cwd: &'a Path,
    pub model: &'a str,
    /// 直接借自 `Registry` 的名字数组，不额外克隆。
    pub tools: &'a [String],
}

pub fn build(facts: &EnvFacts<'_>) -> String {
    let cwd = facts.cwd.display();
    let model = facts.model;
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let tools = if facts.tools.is_empty() {
        "（无）".to_string()
    } else {
        facts.tools.join(", ")
    };

    format!(
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
\n\
# 作业范围\n\
- 工作目录是你唯一的作业范围。不要读写工作目录之外的路径。\n\
- 路径一律相对工作目录；需要绝对路径时先确认它仍在工作目录内。\n\
\n\
# 行为规则\n\
1. 先读后写：改动任何文件之前，先读它的当前内容。\n\
2. 工具结果就是事实来源。不要凭记忆猜测文件内容或命令输出。\n\
3. 失败不要重试同样的调用；读报错信息，换一种做法。\n\
4. 多步骤任务先列出待办，再逐项完成。\n\
5. 完成后用一两句话说明改了什么，不要复述工具输出。\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn mentions_cwd_and_tools() {
        let cwd = PathBuf::from("/work/repo");
        let tools = vec!["bash".to_string(), "read_file".to_string()];
        let text = build(&EnvFacts {
            cwd: &cwd,
            model: "mock",
            tools: &tools,
        });
        assert!(text.contains("/work/repo"));
        assert!(text.contains("bash, read_file"));
        assert!(text.contains("# 行为规则"));
    }
}
