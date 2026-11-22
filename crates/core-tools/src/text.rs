//! 工具输出的截断策略。
//!
//! 字符边界切片本身在 `core_llm::text`（和会话历史的压缩共用一份），这里只定义
//! "工具结果进上下文之前怎么砍"的策略。

use std::fmt::Write as _;

pub use core_llm::text::{char_len, head_chars, tail_chars};

/// 单个工具结果进入模型上下文前的硬上限（字符）。
/// 超长输出必须显式截断，否则会挤爆上下文窗口。
pub const MAX_TOOL_OUTPUT_CHARS: usize = 30_000;

/// 头尾保留式截断：保住开头的报错上下文和结尾的汇总行。
pub fn truncate(content: &str) -> String {
    truncate_owned(content.to_string())
}

/// 同上，但吃掉所有权：未超限时原地返回，零拷贝。
pub fn truncate_owned(content: String) -> String {
    let total = char_len(&content);
    if total <= MAX_TOOL_OUTPUT_CHARS {
        return content;
    }

    let head_len = MAX_TOOL_OUTPUT_CHARS * 6 / 10;
    let tail_len = MAX_TOOL_OUTPUT_CHARS - head_len;
    let head = head_chars(&content, head_len);
    let tail = tail_chars(&content, tail_len);

    let mut out = String::with_capacity(head.len() + tail.len() + 64);
    out.push_str(head);
    out.push_str("\n\n…");
    let _ = write!(out, "（已截断 {} 字符）", total - MAX_TOOL_OUTPUT_CHARS);
    out.push_str("…\n\n");
    out.push_str(tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_head_and_tail() {
        let long = "a".repeat(MAX_TOOL_OUTPUT_CHARS + 100);
        let out = truncate(&long);
        assert!(out.chars().count() < MAX_TOOL_OUTPUT_CHARS + 60);
        assert!(out.contains("已截断 100 字符"));
    }

    #[test]
    fn truncate_owned_does_not_clone_short_text() {
        let short = "abc".to_string();
        let ptr = short.as_ptr();
        let out = truncate_owned(short);
        assert_eq!(out.as_ptr(), ptr, "未超限时必须原地返回");
    }

    #[test]
    fn multibyte_truncation_is_safe() {
        let long = "汉".repeat(MAX_TOOL_OUTPUT_CHARS + 7);
        let out = truncate(&long);
        assert!(out.starts_with('汉'));
        assert!(out.ends_with('汉'));
    }
}
