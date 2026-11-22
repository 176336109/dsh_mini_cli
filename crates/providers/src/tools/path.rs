//! 路径解析：把工具收到的路径钉回工作目录。
//!
//! 这是**词法**约束，不是沙箱：绝对路径、`..`、`~` 之类能逃出 cwd 的写法在这里被
//! 拒绝；软链接指向外部则仍需真正的沙箱（Landlock）来兜。两者职责不同：
//! 这一层保证"模型手滑时写不出去"，沙箱保证"恶意命令跑不出去"。

use std::path::{Component, Path, PathBuf};

use core_tools::{ToolCtx, ToolError};

/// 把 `path` 解析成**可写**范围内的绝对路径：只能是 `cwd`。越界即报错，不静默夹断。
pub fn resolve(ctx: &ToolCtx, path: &str) -> Result<PathBuf, ToolError> {
    let candidate = resolve_any(ctx, path)?;
    let root = normalize(&ctx.cwd);
    if !candidate.starts_with(&root) {
        return Err(ToolError::Failed(format!(
            "路径越界（写）：{} 不在工作目录 {} 内",
            candidate.display(),
            root.display()
        )));
    }
    Ok(candidate)
}

/// 读路径：`cwd` 或任意 [`ToolCtx::read_roots`]（例如 `--skills-dir`）。
///
/// 读范围比写范围宽是刻意为之：`bash` 本来就能读全盘，把 `read_file` 限制得比
/// `bash` 更严只会让模型退化成 `cat`，反而更贵更难审。
pub fn resolve_read(ctx: &ToolCtx, path: &str) -> Result<PathBuf, ToolError> {
    let candidate = resolve_any(ctx, path)?;
    let allowed = std::iter::once(&ctx.cwd)
        .chain(ctx.read_roots.iter())
        .any(|root| candidate.starts_with(normalize(root)));
    if !allowed {
        return Err(ToolError::Failed(format!(
            "路径越界（读）：{} 既不在工作目录 {} 内，也不在只读根内",
            candidate.display(),
            ctx.cwd.display()
        )));
    }
    Ok(candidate)
}

/// 相对路径以 `cwd` 为基准展开并做词法归一化，不判断范围。
fn resolve_any(ctx: &ToolCtx, path: &str) -> Result<PathBuf, ToolError> {
    if path.trim().is_empty() {
        return Err(ToolError::InvalidArgs("路径不能为空".into()));
    }
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ctx.cwd.join(candidate)
    };
    Ok(normalize(&joined))
}

/// 纯词法归一化：消掉 `.`、折叠 `..`（不越过根），不碰文件系统。
/// 因此对"还不存在的文件"同样有效 —— `canonicalize` 做不到这一点。
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::with_capacity(path.as_os_str().len());
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 只在上一段是普通名字时才回退，`/..` 保持在根上。
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir().join("dsh-mini-path-test"))
    }

    #[test]
    fn relative_paths_stay_inside() {
        let ctx = ctx();
        let resolved = resolve(&ctx, "src/main.rs").unwrap();
        assert!(resolved.starts_with(&ctx.cwd));
        assert!(resolved.ends_with("src/main.rs"));
    }

    #[test]
    fn dotdot_escape_is_rejected() {
        let ctx = ctx();
        assert!(resolve(&ctx, "../../etc/passwd").is_err());
        assert!(resolve(&ctx, "a/../../outside.txt").is_err());
    }

    #[test]
    fn dotdot_that_comes_back_is_allowed() {
        let ctx = ctx();
        let resolved = resolve(&ctx, "a/../b.txt").unwrap();
        assert_eq!(resolved, ctx.cwd.join("b.txt"));
    }

    #[test]
    fn absolute_paths_outside_are_rejected() {
        let ctx = ctx();
        assert!(resolve(&ctx, "/etc/passwd").is_err());
    }

    #[test]
    fn absolute_paths_inside_are_accepted() {
        let ctx = ctx();
        let inside = ctx.cwd.join("note.txt");
        let resolved = resolve(&ctx, &inside.to_string_lossy()).unwrap();
        assert_eq!(resolved, inside);
    }

    #[test]
    fn empty_path_is_an_argument_error() {
        let ctx = ctx();
        assert!(matches!(
            resolve(&ctx, "  "),
            Err(ToolError::InvalidArgs(_))
        ));
    }

    #[test]
    fn read_roots_extend_reads_but_not_writes() {
        let extra = std::env::temp_dir().join("dsh-mini-path-skills");
        let ctx = ctx().with_read_roots(vec![extra.clone()]);
        let skill_file = extra.join("demo/SKILL.md");

        // 读：只读根内的路径放行。
        assert_eq!(
            resolve_read(&ctx, &skill_file.to_string_lossy()).unwrap(),
            skill_file
        );
        // 写：同一个路径必须被拒。
        assert!(resolve(&ctx, &skill_file.to_string_lossy()).is_err());
        // 既不在 cwd 也不在只读根：读也拒。
        assert!(resolve_read(&ctx, "/etc/passwd").is_err());
    }

    #[test]
    fn read_roots_do_not_leak_sibling_directories() {
        let extra = std::env::temp_dir().join("dsh-mini-path-skills");
        let ctx = ctx().with_read_roots(vec![extra.clone()]);
        // 前缀相同的兄弟目录不能被误判为在根内。
        let sibling = extra.with_file_name("dsh-mini-path-skills-other/x.md");
        assert!(resolve_read(&ctx, &sibling.to_string_lossy()).is_err());
    }
}
