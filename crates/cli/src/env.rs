//! 运行环境解析：数据目录、`.env`、密钥。
//!
//! 目标平台是 Linux（WSL / 车机），开发机是 Windows，所以两边都要能跑：
//!
//! - 家目录按平台偏好取（unix 认 `HOME`，windows 认 `USERPROFILE`）；
//! - `.env` 允许 `export ` 前缀、单双引号、`#` 注释，和 `/bin/sh` 的写法一致，
//!   车机上可以直接复用 systemd 的 `EnvironmentFile`。
//!
//! 密钥只从环境变量或 `.env` 读，**不接受命令行参数** —— 否则会出现在进程列表
//! 和 shell 历史里。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 默认数据目录：`<当前目录>/.dsh-mini`。
///
/// 保持"跟着工作目录走"的行为：`--resume` 的语义是"回到这个项目上一次的会话"。
/// 车机上目录只读时用 `--home` 指到可写分区。
pub fn default_home() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".dsh-mini")
}

/// 用户家目录。按平台取首选变量，另一个留作兜底。
pub fn user_home() -> Option<PathBuf> {
    // 顺序在编译期定死，运行期不做两套分支。
    const KEYS: [&str; 2] = if cfg!(windows) {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };

    KEYS.iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|path| !path.as_os_str().is_empty())
}

/// `.env` 的解析结果。查找顺序：进程环境变量 → `<home>/.env` → `~/.dsh-mini/.env`。
#[derive(Debug, Default)]
pub struct DotEnv {
    values: BTreeMap<String, String>,
}

impl DotEnv {
    pub fn load(home: &Path) -> Self {
        let mut dotenv = Self::default();
        // 先读全局，再读项目内的：后读的覆盖先读的。
        if let Some(dir) = user_home() {
            dotenv.merge_file(&dir.join(".dsh-mini").join(".env"));
        }
        dotenv.merge_file(&home.join(".env"));
        dotenv
    }

    fn merge_file(&mut self, path: &Path) {
        if let Ok(text) = std::fs::read_to_string(path) {
            self.merge_str(&text);
        }
    }

    fn merge_str(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            if !value.is_empty() {
                self.values.insert(name.to_string(), value.to_string());
            }
        }
    }

    /// 进程环境变量优先，其次 `.env`。
    pub fn lookup(&self, key: &str) -> Option<String> {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        self.values.get(key).cloned()
    }
}

/// 去掉 UTF-8 BOM。
///
/// Windows 上的记事本 / PowerShell `Set-Content -Encoding utf8` 都会写 BOM；
/// BOM 混进提示词或密钥里不会有任何报错，只会让人怀疑人生。
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// 从文件读密钥：取第一行非空、非注释的内容。
///
/// 专为 systemd `LoadCredential=` / Docker secrets 准备 —— 那种文件里只有裸密钥，
/// 没有 `KEY=` 前缀，也不该被塞进 argv（`/proc/<pid>/cmdline` 全世界可见）。
pub fn read_key_file(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("读不到 key 文件 {}：{err}", path.display()))?;

    let key = strip_bom(&text)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .ok_or_else(|| format!("key 文件 {} 里没有内容", path.display()))?;
    let key = key.trim_matches('"').trim_matches('\'').trim();
    if key.is_empty() {
        return Err(format!("key 文件 {} 里没有有效内容", path.display()));
    }

    warn_if_world_readable(path);
    Ok(key.to_string())
}

/// Unix 上顺手检查权限：密钥文件被任何用户可读是常见部署事故。
#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "警告：{} 的权限是 {mode:o}，建议 chmod 600（同组/其他用户可读）",
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

/// 打码后的密钥，用于启动提示。无论密钥多短都不会漏出完整值。
pub fn mask(secret: &str) -> String {
    if secret.chars().count() <= 10 {
        return "***".to_string();
    }
    let head = core_tools::head_chars(secret, 6);
    let tail = core_tools::tail_chars(secret, 4);
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quotes_comments_and_export() {
        let mut dotenv = DotEnv::default();
        dotenv.merge_str(
            "# 注释\nexport DEEPSEEK_API_KEY=\"sk-1234567890abcdef\"\nPLAIN=value\nEMPTY=\n",
        );
        assert_eq!(
            dotenv.values.get("DEEPSEEK_API_KEY").map(String::as_str),
            Some("sk-1234567890abcdef")
        );
        assert_eq!(
            dotenv.values.get("PLAIN").map(String::as_str),
            Some("value")
        );
        assert!(!dotenv.values.contains_key("EMPTY"));
    }

    #[test]
    fn process_env_wins_over_file() {
        // 这个变量名足够独特，不会和真实环境撞车。
        std::env::set_var("DSH_MINI_TEST_LOOKUP", "from-process");
        let mut dotenv = DotEnv::default();
        dotenv
            .values
            .insert("DSH_MINI_TEST_LOOKUP".into(), "from-file".into());
        assert_eq!(
            dotenv.lookup("DSH_MINI_TEST_LOOKUP").as_deref(),
            Some("from-process")
        );
        std::env::remove_var("DSH_MINI_TEST_LOOKUP");
    }

    #[test]
    fn mask_never_leaks_short_secrets() {
        assert_eq!(mask("short"), "***");
        assert_eq!(mask("sk-1234567890abcdef"), "sk-123…cdef");
    }

    fn temp_file(tag: &str, content: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("dsh-mini-key-{tag}-{}.txt", core_session::now_ms()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn key_file_takes_the_first_meaningful_line() {
        let path = temp_file("first", "\n# systemd 注释\n  \"sk-abc123\"  \nsk-second\n");
        assert_eq!(read_key_file(&path).unwrap(), "sk-abc123");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn key_file_strips_utf8_bom() {
        // 记事本与 PowerShell 的 utf8 都会写 BOM；带进密钥里只会在 401 时才被发现。
        let path = temp_file("bom", "\u{feff}sk-bom123456789");
        assert_eq!(read_key_file(&path).unwrap(), "sk-bom123456789");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_key_file_is_rejected() {
        let path = temp_file("empty", "\n\n# 只有注释\n");
        assert!(read_key_file(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
