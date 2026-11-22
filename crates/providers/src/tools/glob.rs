//! 极简 glob 匹配。
//!
//! 只支持模型真正会用到的三种语法：`*`（不跨目录）、`**`（跨目录）、`?`（单字符）。
//! 方括号字符类故意不做 —— 模型几乎不用，而实现它与测试它的成本远高于收益。
//!
//! 为什么自己写：`globset` / `regex` 都是几十万字节的依赖，对一个目标是车机的
//! 单二进制来说不划算（当前整包 3 MB 左右）。

/// `*` 不跨 `/`，`**` 跨任意层级。输入一律用 `/` 分隔（调用方负责把 `\` 换掉）。
pub fn matches(pattern: &str, text: &str) -> bool {
    match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn match_bytes(pattern: &[u8], text: &[u8]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }

    match pattern[0] {
        b'*' if pattern.len() > 1 && pattern[1] == b'*' => {
            // `**` 可以吞掉任意字符（包括 `/`）；吃掉紧跟的 `/` 让 `**/x` 也能匹配 `x`。
            let rest = &pattern[2..];
            let rest = if rest.first() == Some(&b'/') {
                &rest[1..]
            } else {
                rest
            };
            (0..=text.len()).any(|cut| match_bytes(rest, &text[cut..]))
        }
        b'*' => {
            let rest = &pattern[1..];
            let mut cut = 0usize;
            loop {
                if match_bytes(rest, &text[cut..]) {
                    return true;
                }
                // 单星号不跨目录。
                if cut >= text.len() || text[cut] == b'/' {
                    return false;
                }
                cut += 1;
            }
        }
        b'?' => !text.is_empty() && text[0] != b'/' && match_bytes(&pattern[1..], &text[1..]),
        byte => !text.is_empty() && text[0] == byte && match_bytes(&pattern[1..], &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_does_not_cross_directories() {
        assert!(matches("*.rs", "main.rs"));
        assert!(!matches("*.rs", "src/main.rs"));
    }

    #[test]
    fn double_star_crosses_directories() {
        assert!(matches("**/*.rs", "src/main.rs"));
        assert!(matches("**/*.rs", "main.rs"));
        assert!(matches("src/**/*.rs", "src/a/b/main.rs"));
        assert!(!matches("src/**/*.rs", "tests/main.rs"));
        assert!(matches("src/**", "src/a/b/main.rs"));
    }

    #[test]
    fn question_mark_is_one_char() {
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("a?c", "a/c"));
    }

    #[test]
    fn exact_names_and_multi_star() {
        assert!(matches("Cargo.toml", "Cargo.toml"));
        assert!(matches("*_test.rs", "core_test.rs"));
        assert!(matches("a*b*c", "axxbyyc"));
        assert!(!matches("a*b*c", "ab"));
    }

    #[test]
    fn empty_pattern_matches_only_empty_text() {
        assert!(matches("", ""));
        assert!(!matches("", "x"));
    }
}
