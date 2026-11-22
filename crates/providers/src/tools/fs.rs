//! 文件类工具：`read_file` / `write_file` / `str_replace` / `list_dir`。
//!
//! 三条贯穿全部四个工具的规矩：
//! 1. 路径必须落在工作目录内（[`super::path::resolve`]）。
//! 2. 读文件按行流式读，只保留请求的那一段 —— 不把整份日志读进内存。
//! 3. 输出有上限，超出显式标注，绝不静默截断。

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use core_llm::ToolSchema;
use core_tools::{Tool, ToolCtx, ToolError, MAX_TOOL_OUTPUT_CHARS};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::path::{resolve, resolve_read};
use super::{arg_str, arg_str_opt};

/// `read_file` 单次输出的字节上限。Registry 还会按字符再截一次，
/// 这里提前刹车是为了不先把几十 MB 拼进内存。
const READ_OUTPUT_BYTES: usize = MAX_TOOL_OUTPUT_CHARS * 4;

/// `list_dir` 单次返回的目录项上限，防止超大目录把上下文挤爆。
const LIST_LIMIT: usize = 1000;

/// 读文件，带行号。行号是模型定位问题的主要锚点，不能省。
pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".into(),
            description: "读取文本文件，返回带行号的内容。可指定起始行与行数。若文件末尾没有换行符会注明；若还有更多内容会提示用 offset 继续。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "起始行，1 起"},
                    "limit": {"type": "integer", "description": "读取行数，默认 2000"}
                },
                "required": ["path"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            // 读路径允许落在只读根里（例如 --skills-dir 指向的技能目录）。
            let path = resolve_read(ctx, &arg_str(args, "path")?)?;
            let offset = args
                .get("offset")
                .and_then(|v| v.as_u64())
                .unwrap_or(1)
                .max(1);
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000);

            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|err| ToolError::Failed(format!("读取 {} 失败：{err}", path.display())))?;
            let mut reader = BufReader::with_capacity(64 * 1024, file);

            let mut out = String::with_capacity(64 * 1024);
            let mut line = String::with_capacity(1024);
            let mut number = 0u64;
            let mut emitted = 0u64;
            let mut reached_eof = false;
            let mut last_line_terminated = true;

            loop {
                line.clear();
                let read = reader.read_line(&mut line).await.map_err(|err| {
                    ToolError::Failed(format!("读取 {} 失败：{err}", path.display()))
                })?;
                if read == 0 {
                    reached_eof = true;
                    break;
                }
                number += 1;
                if number < offset {
                    continue;
                }
                if emitted >= limit {
                    // 明确告诉模型"还没读完"，否则它会去数总行数而白烧步数。
                    out.push_str("…（还有更多内容，用 offset/limit 继续读）\n");
                    break;
                }
                last_line_terminated = line.ends_with('\n');
                // 兼容 CRLF：read_file 对外统一成 LF，str_replace 负责补回去。
                let text = line.trim_end_matches(['\r', '\n']);
                let _ = writeln!(out, "{number:>6}| {text}");
                emitted += 1;
                if out.len() >= READ_OUTPUT_BYTES {
                    out.push_str("…（输出过长，已截断）\n");
                    break;
                }
            }

            if emitted == 0 {
                out.push_str("（该区间为空）\n");
            } else if reached_eof {
                // 追加类任务最需要的两件事：文件到没到头、末尾有没有换行。
                // 以前这两个信息都得模型自己 od / certutil 去翻字节，实测会把步数烧光。
                // 这里连"确实到末尾了"也明确写出来 —— 沉默会被误读成"还没读完"。
                out.push_str(if last_line_terminated {
                    "（已读到文件末尾，末尾有换行符）\n"
                } else {
                    "（已读到文件末尾，末尾没有换行符）\n"
                });
            }
            Ok(out)
        })
    }
}

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".into(),
            description: "整体写入文件（覆盖已有内容）。父目录会自动创建。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let path = resolve(ctx, &arg_str(args, "path")?)?;
            let content = arg_str(args, "content")?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|err| ToolError::Failed(format!("创建目录失败：{err}")))?;
            }
            tokio::fs::write(&path, &content)
                .await
                .map_err(|err| ToolError::Failed(format!("写入失败：{err}")))?;
            Ok(format!("已写入 {} 字节：{}", content.len(), path.display()))
        })
    }
}

/// 精确字符串替换。要求唯一匹配 —— 这是防止"改错地方"最便宜的一道闸。
pub struct StrReplaceTool;

impl Tool for StrReplaceTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "str_replace".into(),
            description:
                "把文件中的 old_string 替换为 new_string。old_string 必须在文件中唯一出现。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let path = resolve(ctx, &arg_str(args, "path")?)?;
            let old = arg_str(args, "old_string")?;
            let new = arg_str(args, "new_string")?;
            if old.is_empty() {
                return Err(ToolError::InvalidArgs("old_string 不能为空".into()));
            }

            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|err| ToolError::Failed(format!("读取 {} 失败：{err}", path.display())))?;

            // 换行符容错：模型读到的内容是我们归一化过 LF 的，但磁盘上可能是 CRLF。
            // 直接按 LF 匹配会「找不到」，逼模型去 od/hex 折腾——这是实测踩到的坑。
            let (old, new) = if content.contains("\r\n") && !old.contains('\r') {
                (
                    old.replace('\n', "\r\n"),
                    new.replace("\r\n", "\n").replace('\n', "\r\n"),
                )
            } else {
                (old, new)
            };

            // 只找前两处就够判定唯一性，不必扫完整份文件。
            let mut first = 0usize;
            let mut hits = 0usize;
            for (at, _) in content.match_indices(&old) {
                if hits == 0 {
                    first = at;
                }
                hits += 1;
                if hits > 1 {
                    break;
                }
            }

            match hits {
                0 => Err(ToolError::Failed("未找到 old_string，文件未改动".into())),
                1 => {
                    let mut updated = String::with_capacity(content.len() - old.len() + new.len());
                    updated.push_str(&content[..first]);
                    updated.push_str(&new);
                    updated.push_str(&content[first + old.len()..]);
                    tokio::fs::write(&path, &updated)
                        .await
                        .map_err(|err| ToolError::Failed(format!("写入失败：{err}")))?;
                    Ok(format!("已替换 1 处：{}", path.display()))
                }
                n => Err(ToolError::Failed(format!(
                    "old_string 匹配到至少 {n} 处，必须唯一。请提供更长的上下文。"
                ))),
            }
        })
    }
}

pub struct ListDirTool;

impl Tool for ListDirTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_dir".into(),
            description: "列目录内容，目录以 / 结尾。默认列工作目录。".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": []
            }),
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let path = match arg_str_opt(args, "path") {
                Some(p) => resolve_read(ctx, &p)?,
                None => ctx.cwd.clone(),
            };
            let mut entries = tokio::fs::read_dir(&path)
                .await
                .map_err(|err| ToolError::Failed(format!("列目录失败：{err}")))?;

            let mut names: Vec<String> = Vec::new();
            let mut truncated = false;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|err| ToolError::Failed(format!("读取目录项失败：{err}")))?
            {
                if names.len() >= LIST_LIMIT {
                    truncated = true;
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                names.push(if is_dir { format!("{name}/") } else { name });
            }
            names.sort();
            if names.is_empty() {
                return Ok("（空目录）".into());
            }
            let mut out = names.join("\n");
            if truncated {
                let _ = write!(out, "\n…（目录项超过 {LIST_LIMIT} 条，已截断）");
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("dsh-mini-fs-{tag}-{}", core_session::now_ms()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    async fn run(
        tool: &dyn Tool,
        dir: &PathBuf,
        args: serde_json::Value,
    ) -> Result<String, ToolError> {
        tool.run(&args, &ToolCtx::new(dir)).await
    }

    #[tokio::test]
    async fn read_file_streams_only_the_requested_window() {
        let dir = temp_dir("read");
        let body: String = (1..=5000).map(|i| format!("line{i}\n")).collect();
        std::fs::write(dir.join("big.txt"), body).unwrap();

        let out = run(
            &ReadFileTool,
            &dir,
            json!({"path": "big.txt", "offset": 10, "limit": 3}),
        )
        .await
        .unwrap();
        assert_eq!(out.lines().count(), 4);
        assert!(out.starts_with("    10| line10"));
        assert!(
            out.contains("还有更多内容"),
            "截断时必须提示还能继续读：{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_normalises_crlf() {
        let dir = temp_dir("crlf");
        std::fs::write(dir.join("win.txt"), "a\r\nb\r\n").unwrap();
        let out = run(&ReadFileTool, &dir, json!({"path": "win.txt"}))
            .await
            .unwrap();
        assert!(!out.contains('\r'), "{out:?}");
        assert!(out.contains("1| a"));
        assert!(
            !out.contains("没有换行符"),
            "CRLF 文件是以换行结尾的：{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_reports_a_missing_trailing_newline() {
        let dir = temp_dir("noeol");
        std::fs::write(dir.join("a.txt"), "first\nsecond").unwrap();
        std::fs::write(dir.join("b.txt"), "first\nsecond\n").unwrap();

        let no_eol = run(&ReadFileTool, &dir, json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert!(
            no_eol.contains("已读到文件末尾，末尾没有换行符"),
            "{no_eol}"
        );

        let with_eol = run(&ReadFileTool, &dir, json!({"path": "b.txt"}))
            .await
            .unwrap();
        assert!(
            with_eol.contains("已读到文件末尾，末尾有换行符"),
            "{with_eol}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_then_replace_roundtrips() {
        let dir = temp_dir("replace");
        run(
            &WriteFileTool,
            &dir,
            json!({"path": "d/x.txt", "content": "alpha beta"}),
        )
        .await
        .unwrap();
        let out = run(
            &StrReplaceTool,
            &dir,
            json!({"path": "d/x.txt", "old_string": "beta", "new_string": "gamma"}),
        )
        .await
        .unwrap();
        assert!(out.contains("已替换 1 处"));
        assert_eq!(
            std::fs::read_to_string(dir.join("d/x.txt")).unwrap(),
            "alpha gamma"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn duplicate_match_is_rejected() {
        let dir = temp_dir("dup");
        std::fs::write(dir.join("x.txt"), "dup dup").unwrap();
        let err = run(
            &StrReplaceTool,
            &dir,
            json!({"path": "x.txt", "old_string": "dup", "new_string": "y"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("唯一"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn escaping_the_workdir_is_refused() {
        let dir = temp_dir("escape");
        let err = run(
            &WriteFileTool,
            &dir,
            json!({"path": "../outside.txt", "content": "x"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("越界"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_dir_marks_directories() {
        let dir = temp_dir("list");
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        let out = run(&ListDirTool, &dir, json!({})).await.unwrap();
        assert!(out.contains("sub/"), "{out}");
        assert!(out.contains("f.txt"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
