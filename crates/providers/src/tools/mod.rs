//! 模型侧工具的后端实现（`ctx.tools` 的 provider 侧）。
//!
//! 对应 dsh 的 `dsh-tool-bash` / `dsh-tool-fs` / `dsh-tool-fs-search`。
//! Mini 只保留五个工具：`bash`、`read_file`、`write_file`、`str_replace`、`list_dir`。
//! 每个工具的结果都必须是模型能读懂、且能被日志原样回放的纯文本。
//!
//! 模块划分：
//! - [`bash`]：唯一的"任意代码执行"入口，输出必须边读边限。
//! - [`fs`]：四个文件工具，路径一律走 [`path::resolve`]。
//! - [`search`]：内置 `grep` / `glob`，让搜索不必退化成 `bash` 拼命令。
//! - [`path`]：把相对/绝对路径钉回工作目录的词法约束。
//! - [`glob`]：`*` / `**` / `?` 的极简匹配器。

pub mod bash;
pub mod fs;
pub mod glob;
pub mod path;
pub mod search;

pub use bash::BashTool;
pub use fs::{ListDirTool, ReadFileTool, StrReplaceTool, WriteFileTool};
pub use search::{GlobTool, GrepTool};

use std::borrow::Cow;

use core_tools::ToolError;

/// 取必填字符串参数。错误信息里带上键名，模型才知道该改哪个字段。
pub(crate) fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidArgs(format!("缺少字符串参数 `{key}`")))
}

pub(crate) fn arg_str_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// 按字符上限截断，附一句"已截断"说明。未超限时零拷贝。
pub(crate) fn cap_chars(text: &str, limit: usize) -> Cow<'_, str> {
    match core_tools::head_chars(text, limit).len() {
        len if len == text.len() => Cow::Borrowed(text),
        len => Cow::Owned(format!(
            "{}\n…（本流已截断，超过 {limit} 字符）",
            &text[..len]
        )),
    }
}
