//! 按 UTF-8 字符边界切片的原语。
//!
//! 放在最底层的 crate 里，`core-session`（压缩历史）与 `core-tools`（截断工具输出）
//! 共用同一份实现 —— 字符边界这类逻辑一旦抄两份，迟早会有一份漏掉多字节情况。

/// 前 `limit` 个字符的切片。超出部分丢弃，永不切在字符中间。
pub fn head_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// 末尾 `limit` 个字符的切片。
pub fn tail_chars(text: &str, limit: usize) -> &str {
    if limit == 0 {
        return &text[text.len()..];
    }
    match text.char_indices().rev().nth(limit - 1) {
        Some((start, _)) => &text[start..],
        None => text,
    }
}

/// 字符数。不分配，但仍需走一遍字符边界。
pub fn char_len(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_and_tail_respect_char_boundaries() {
        let text = "中文abc";
        assert_eq!(head_chars(text, 2), "中文");
        assert_eq!(head_chars(text, 0), "");
        assert_eq!(head_chars(text, 99), text);
        assert_eq!(tail_chars(text, 3), "abc");
        assert_eq!(tail_chars(text, 0), "");
        assert_eq!(tail_chars(text, 99), text);
    }

    #[test]
    fn char_len_counts_scalars_not_bytes() {
        assert_eq!(char_len("中a"), 2);
        assert_eq!(char_len(""), 0);
    }
}
