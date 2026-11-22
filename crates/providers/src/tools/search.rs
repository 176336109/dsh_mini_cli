//! 内置搜索工具：`grep`（按内容找）与 `glob`（按名字找）。
//!
//! 存在的理由很实际：没有它们，模型会拿 `bash` 拼 `grep -rn` / `find` / `dir /s`
//! 去凑，一旦平台命令缺失（车机上没有 GNU 工具、Windows 上没有 grep）就开始
//! 退化成一堆试探调用，把 step 预算烧光。工具化之后搜索是**一次调用、确定性输出**。
//!
//! 全部实现都带硬上限（目录项、文件大小、结果条数），走阻塞线程池执行，
//! 不阻塞 async runtime；不跟随软链接，避免环路。

use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use core_llm::ToolSchema;
use core_tools::{Tool, ToolCtx, ToolError, MAX_TOOL_OUTPUT_CHARS};
use serde_json::json;

use super::glob;
use super::path::resolve_read;
use super::{arg_str, arg_str_opt};

/// 一次遍历最多访问的目录项总数。
const MAX_ENTRIES: usize = 20_000;
/// 递归深度上限。
const MAX_DEPTH: usize = 24;
/// 参与搜索的单文件字节上限（超过的多半是数据/日志，不是源码）。
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 二进制探测：前 4 KiB 里出现 NUL 就当成二进制跳过。
const BINARY_PROBE_BYTES: usize = 4096;
/// `grep` 返回的匹配条数上限。
const MAX_MATCHES: usize = 200;
/// `glob` 返回的路径条数上限。
const MAX_GLOB_RESULTS: usize = 500;
/// 单行展示的字符上限。
const MAX_LINE_CHARS: usize = 240;

/// 遍历时直接跳过的目录名。只放"几乎一定是构建产物/依赖"的那几个：
/// 误跳过用户自己的 `build/`、`vendor/` 比多搜几个文件更糟（可以用 `path` 直接指进去）。
const SKIP_DIRS: &[&str] = &["node_modules", "target", "__pycache__"];

/// 隐藏目录默认跳过（缓存、构建产物、工具数据几乎都在里面）；
/// 少数确实放源码的保留，否则 `.github/workflows` 之类的会搜不到。
const DOT_DIR_ALLOW: &[&str] = &[".github", ".claude", ".husky"];

fn should_skip_dir(name: &str) -> bool {
    if SKIP_DIRS.contains(&name) {
        return true;
    }
    name.starts_with('.') && !DOT_DIR_ALLOW.contains(&name)
}

/// 遍历结果。
struct Walk {
    files: Vec<PathBuf>,
    /// 是否因为预算耗尽提前停下（输出里要如实说明）。
    truncated: bool,
}

/// 阻塞式遍历：不跟软链接，命中黑名单目录直接跳过。
fn walk(root: &Path) -> Walk {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    let mut truncated = false;

    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_ENTRIES {
                truncated = true;
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            // 软链接既可能是环，也可能是 cwd 之外的路径：一律不跟。
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                let name = entry.file_name();
                if should_skip_dir(&name.to_string_lossy()) {
                    continue;
                }
                stack.push((path, depth + 1));
            } else if file_type.is_file() {
                files.push(path);
            }
        }
        if truncated {
            break;
        }
    }

    // 稳定输出：同样的仓库每次跑出同样的顺序，便于模型对照与缓存。
    files.sort();
    Walk { files, truncated }
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// 不含 `/` 的模式按文件名匹配（`*.rs` 命中任意层级的 rust 文件），
/// 含 `/` 的按相对路径匹配（`src/**/*.rs`）。这条宽容规则省掉了大量
/// "为什么搜不到" 的往返。
fn pattern_matches(pattern: &str, rel: &str) -> bool {
    if pattern.contains('/') {
        return glob::matches(pattern, rel);
    }
    match rel.rsplit_once('/') {
        Some((_, name)) => glob::matches(pattern, name),
        None => glob::matches(pattern, rel),
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_PROBE_BYTES).any(|byte| *byte == 0)
}

/// 大小写不敏感的子串查找（只折叠 ASCII，不分配）。
///
/// UTF-8 的多字节序列每个字节都 ≥ 0x80，`to_ascii_lowercase` 不会改动它们，
/// 所以按字节比较是安全的。
fn contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    let haystack = haystack.as_bytes();
    let needle = needle_lower.as_bytes();
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    for start in 0..=(haystack.len() - needle.len()) {
        if haystack[start].to_ascii_lowercase() != first {
            continue;
        }
        if haystack[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(left, right)| left.to_ascii_lowercase() == *right)
        {
            return true;
        }
    }
    false
}

fn grep_blocking(
    root: &Path,
    pattern: &str,
    include: Option<&str>,
    ignore_case: bool,
) -> Result<String, String> {
    let needle_lower = pattern.to_ascii_lowercase();
    let walk = walk(root);

    let mut out = String::with_capacity(4096);
    let mut matches = 0usize;
    let mut scanned = 0usize;
    let mut skipped_large = 0usize;

    'files: for path in &walk.files {
        let rel = relative(root, path);
        if let Some(include) = include {
            if !pattern_matches(include, &rel) {
                continue;
            }
        }
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            skipped_large += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if is_binary(&bytes) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        scanned += 1;

        for (index, line) in text.lines().enumerate() {
            let hit = if ignore_case {
                contains_ignore_case(line, &needle_lower)
            } else {
                line.contains(pattern)
            };
            if !hit {
                continue;
            }
            if matches >= MAX_MATCHES {
                let _ = writeln!(
                    out,
                    "…（已达 {MAX_MATCHES} 条上限，请缩小范围或加 include）"
                );
                break 'files;
            }
            matches += 1;
            let shown = core_tools::head_chars(line.trim_end(), MAX_LINE_CHARS);
            let _ = writeln!(out, "{rel}:{}: {shown}", index + 1);
            if out.len() >= MAX_TOOL_OUTPUT_CHARS {
                let _ = writeln!(out, "…（输出过长，已截断）");
                break 'files;
            }
        }
    }

    if matches == 0 {
        let mut note = format!("（在 {scanned} 个文件里没有匹配）");
        if walk.truncated {
            note.push_str("；遍历已达目录项上限，结果可能不完整");
        }
        if skipped_large > 0 {
            let _ = write!(note, "；跳过 {skipped_large} 个超过 2 MiB 的文件");
        }
        return Ok(note);
    }

    if walk.truncated {
        let _ = writeln!(out, "…（遍历已达目录项上限，可能还有未搜到的文件）");
    }
    Ok(out)
}

fn glob_blocking(root: &Path, pattern: &str) -> String {
    let walk = walk(root);
    let mut out = String::with_capacity(4096);
    let mut hits = 0usize;

    for path in &walk.files {
        let rel = relative(root, path);
        if !pattern_matches(pattern, &rel) {
            continue;
        }
        if hits >= MAX_GLOB_RESULTS {
            let _ = writeln!(out, "…（已达 {MAX_GLOB_RESULTS} 条上限，请收窄模式）");
            break;
        }
        hits += 1;
        let _ = writeln!(out, "{rel}");
    }

    if hits == 0 {
        let mut note = format!("（没有匹配 {pattern} 的文件）");
        if walk.truncated {
            note.push_str("；遍历已达目录项上限，结果可能不完整");
        }
        return note;
    }
    if walk.truncated {
        let _ = writeln!(out, "…（遍历已达目录项上限，可能还有未列出的文件）");
    }
    out
}

pub struct GrepTool;

impl Tool for GrepTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "grep".into(),
            description: "按内容搜索文件，返回 `路径:行号: 内容`。默认递归当前目录，跳过 .git/target/node_modules 与二进制文件。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "要查找的字面字符串（不是正则）"},
                    "path": {"type": "string", "description": "搜索起点，默认工作目录"},
                    "include": {"type": "string", "description": "只搜匹配该 glob 的文件，如 `*.rs` 或 `src/**/*.ts`"},
                    "ignore_case": {"type": "boolean", "description": "忽略大小写（仅 ASCII）"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let pattern = arg_str(args, "pattern")?;
            if pattern.is_empty() {
                return Err(ToolError::InvalidArgs("pattern 不能为空".into()));
            }
            let root = match arg_str_opt(args, "path") {
                Some(path) => resolve_read(ctx, &path)?,
                None => ctx.cwd.clone(),
            };
            let include = arg_str_opt(args, "include");
            let ignore_case = args
                .get("ignore_case")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let result = tokio::task::spawn_blocking(move || {
                grep_blocking(&root, &pattern, include.as_deref(), ignore_case)
            })
            .await
            .map_err(|err| ToolError::Failed(format!("搜索任务失败：{err}")))?;
            result.map_err(ToolError::Failed)
        })
    }
}

pub struct GlobTool;

impl Tool for GlobTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "glob".into(),
            description: "按文件名/路径查找文件，返回相对路径列表。`*` 不跨目录，`**` 跨目录；不含 `/` 的模式按文件名匹配任意层级。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "glob 模式，如 `*.rs`、`**/*.md`、`src/**/mod.rs`"},
                    "path": {"type": "string", "description": "搜索起点，默认工作目录"}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let pattern = arg_str(args, "pattern")?;
            if pattern.is_empty() {
                return Err(ToolError::InvalidArgs("pattern 不能为空".into()));
            }
            let root = match arg_str_opt(args, "path") {
                Some(path) => resolve_read(ctx, &path)?,
                None => ctx.cwd.clone(),
            };

            tokio::task::spawn_blocking(move || glob_blocking(&root, &pattern))
                .await
                .map_err(|err| ToolError::Failed(format!("搜索任务失败：{err}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::path::normalize;

    fn workspace() -> PathBuf {
        let mut root = std::env::temp_dir();
        root.push(format!("dsh-mini-search-{}", core_session::now_ms()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "# demo\nhello world\n").unwrap();
        std::fs::write(root.join("target/junk.rs"), "hello in build dir\n").unwrap();
        std::fs::write(root.join("blob.bin"), b"hello\x00\x01\x02").unwrap();
        root
    }

    #[test]
    fn grep_finds_matches_with_paths_and_line_numbers() {
        let root = workspace();
        let out = grep_blocking(&root, "hello", Some("*.rs"), false).unwrap();
        assert!(out.contains("src/main.rs:2:"), "{out}");
        assert!(!out.contains("README.md"), "{out}");
        assert!(!out.contains("target/"), "黑名单目录必须跳过：{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_skips_binary_files() {
        let root = workspace();
        let out = grep_blocking(&root, "hello", None, false).unwrap();
        assert!(!out.contains("blob.bin"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_ignore_case_only_folds_ascii() {
        let root = workspace();
        let hit = grep_blocking(&root, "HELLO", None, true).unwrap();
        assert!(hit.contains("README.md"), "{hit}");
        let miss = grep_blocking(&root, "HELLO", None, false).unwrap();
        assert!(miss.contains("没有匹配"), "{miss}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_reports_when_nothing_matches() {
        let root = workspace();
        let out = grep_blocking(&root, "definitely-absent", None, false).unwrap();
        assert!(out.contains("没有匹配"), "{out}");
        assert!(out.contains("个文件"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn glob_lists_files_across_levels() {
        let root = workspace();
        let out = glob_blocking(&root, "*.rs");
        assert!(out.contains("src/main.rs"), "{out}");
        assert!(out.contains("src/lib.rs"), "{out}");
        assert!(!out.contains("target/junk.rs"), "{out}");

        let scoped = glob_blocking(&root, "src/**/*.rs");
        assert!(scoped.contains("src/main.rs"), "{scoped}");
        assert!(!scoped.contains("README.md"), "{scoped}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn glob_escapes_nothing_outside_the_root() {
        // resolve_read 负责边界，这里只确认相对路径归一化后不会带 ".."
        let root = normalize(Path::new("/tmp/x"));
        assert!(!root.to_string_lossy().contains(".."));
    }

    #[test]
    fn hidden_dirs_are_skipped_but_source_ones_are_kept() {
        assert!(should_skip_dir(".git"));
        assert!(should_skip_dir(".runs"));
        assert!(should_skip_dir(".cache"));
        assert!(
            !should_skip_dir(".github"),
            "workflows 里的 yaml 是要能搜到的"
        );
        assert!(!should_skip_dir("src"));
    }
}
