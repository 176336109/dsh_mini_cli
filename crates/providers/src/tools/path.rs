//! 路径解析：把工具收到的路径钉回工作目录。
//!
//! 这是**词法**约束，不是沙箱：绝对路径、`..`、`~` 之类能逃出 cwd 的写法在这里被
//! 拒绝；软链接指向外部则仍需真正的沙箱（Landlock）来兜。两者职责不同：
//! 这一层保证"模型手滑时写不出去"，沙箱保证"恶意命令跑不出去"。

use std::path::{Component, Path, PathBuf};

use core_tools::{ToolCtx, ToolError};

/// 把 `path` 解析成工作目录内的绝对路径。越界即报错，不静默夹断。
pub fn resolve(ctx: &ToolCtx, path: &str) -> Result<PathBuf, ToolError> {
    if path.trim().is_empty() {
        return Err(ToolError::InvalidArgs("路径不能为空".into()));
    }

    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ctx.cwd.join(candidate)
    };

    let root = normalize(&ctx.cwd);
    let resolved = normalize(&joined);
    if !resolved.starts_with(&root) {
        return Err(ToolError::Failed(format!(
            "路径越界：{} 不在工作目录 {} 内",
            resolved.display(),
            root.display()
        )));
    }
    Ok(resolved)
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
}
