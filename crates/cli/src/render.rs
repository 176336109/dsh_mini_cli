//! 终端呈现：把循环事件打成人能看的一行行输出。
//!
//! 这里只做展示，不参与任何状态推导；流式增量丢了也不影响会话日志的正确性。

use std::io::Write;

use core_loop::LoopEvent;
use core_tools::head_chars;

/// 单次展示的截断长度。
const ARG_LIMIT: usize = 120;
const RESULT_LIMIT: usize = 160;

pub fn print_event(event: LoopEvent) {
    match event {
        LoopEvent::TurnStart { turn } => println!("\n── turn {turn} ──"),
        LoopEvent::StepStart { step, .. } => println!("[step {step}]"),
        LoopEvent::TextDelta(text) => {
            print!("{text}");
            // 流式输出必须立即刷屏，否则用户看到的是一段一段的延迟。
            let _ = std::io::stdout().flush();
        }
        LoopEvent::AssistantMessage { .. } => println!(),
        LoopEvent::ToolCall { name, arguments } => {
            let args = arguments.to_string();
            println!("  → {name} {}", shorten(&args, ARG_LIMIT));
        }
        LoopEvent::ToolResult {
            name,
            is_error,
            content,
        } => {
            let status = if is_error { "失败" } else { "ok" };
            let body = content.trim().replace('\n', " ⏎ ");
            println!("  ← {name} [{status}] {}", shorten(&body, RESULT_LIMIT));
        }
        LoopEvent::TurnEnd { turn, reason } => println!("── turn {turn} 结束：{reason:?} ──"),
        LoopEvent::ReasoningDelta(_) | LoopEvent::ToolCallDelta { .. } => {}
    }
}

/// 截到 `limit` 个字符并加省略号。零分配（够长时才建一个短 String）。
pub fn shorten(text: &str, limit: usize) -> String {
    let head = head_chars(text, limit);
    if head.len() == text.len() {
        return text.to_string();
    }
    let mut out = String::with_capacity(head.len() + 3);
    out.push_str(head);
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_keeps_short_text_intact() {
        assert_eq!(shorten("abc", 10), "abc");
    }

    #[test]
    fn shorten_cuts_on_char_boundary() {
        let text = "中文字符";
        assert_eq!(shorten(text, 2), "中文…");
    }
}
