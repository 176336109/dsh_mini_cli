//! 技能目录扫描：只收元数据，正文留给模型按需 `read_file`。
//!
//! 目录约定是 `<skills-dir>/<skill>/SKILL.md`，文件开头可以带一段最简
//! frontmatter（不做完整 YAML，只认 `key: value`）：
//!
//! ```text
//! ---
//! name: dsh-mini-eval
//! description: 跑评测、看通过率、定位失败用例
//! ---
//! 正文……
//! ```
//!
//! 为什么不把正文塞进提示：一个技能正文动辄几千字，车机上塞两个就把上下文吃掉
//! 一大半。只给"名字 + 一句话 + 路径"，模型判断匹配后再去读，是按需付费。

use std::path::{Path, PathBuf};

use core_llm::text::head_chars;

use crate::prompt::SkillMeta;

/// 单次最多收进来的技能数。超出直接截断并告警 —— 提示词里的清单长了反而干扰判断。
pub const MAX_SKILLS: usize = 32;

/// 单条描述的字符上限。
const MAX_DESCRIPTION_CHARS: usize = 200;

/// 只读文件头：frontmatter 必须在这个范围内结束，否则视为没有元数据。
const FRONTMATTER_SCAN_BYTES: usize = 8 * 1024;

#[derive(Debug, Default)]
pub struct SkillScan {
    pub skills: Vec<SkillMeta>,
    /// 启动时要打给用户看的告警（目录不可读、元数据缺失等）。
    pub warnings: Vec<String>,
}

/// 扫描技能目录。目录不存在只产出一条告警，不报错 —— 技能是可选能力。
pub fn scan(dir: &Path) -> SkillScan {
    let mut scan = SkillScan::default();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            scan.warnings
                .push(format!("技能目录 {} 不可读：{err}", dir.display()));
            return scan;
        }
    };

    let mut candidates: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    // 顺序稳定，便于比对与断言。
    candidates.sort();

    for skill_dir in candidates {
        let manifest = skill_dir.join("SKILL.md");
        if !manifest.is_file() {
            continue;
        }
        match load_one(&skill_dir, &manifest) {
            Ok(skill) => {
                if scan.skills.len() >= MAX_SKILLS {
                    scan.warnings.push(format!(
                        "技能数量超过 {MAX_SKILLS}，后续已忽略（当前目录：{}）",
                        dir.display()
                    ));
                    break;
                }
                scan.skills.push(skill);
            }
            Err(reason) => scan.warnings.push(reason),
        }
    }

    scan.skills.sort_by(|a, b| a.name.cmp(&b.name));
    scan
}

fn load_one(skill_dir: &Path, manifest: &Path) -> Result<SkillMeta, String> {
    let bytes = std::fs::read(manifest)
        .map_err(|err| format!("读不到技能说明 {}：{err}", manifest.display()))?;
    let head = &bytes[..bytes.len().min(FRONTMATTER_SCAN_BYTES)];
    let text = String::from_utf8_lossy(head);

    let fallback_name = skill_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "未命名技能".to_string());

    let (name, description) = parse_frontmatter(&text);
    let name = name
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback_name);
    let description = match description.filter(|text| !text.is_empty()) {
        Some(text) => head_chars(&text, MAX_DESCRIPTION_CHARS).to_string(),
        None => "（无描述）".to_string(),
    };

    Ok(SkillMeta {
        name,
        description,
        path: absolute(manifest).display().to_string(),
    })
}

/// 解析 `--- ... ---` 里的 `name` / `description`。没 frontmatter 时返回两个 `None`。
fn parse_frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return (None, None);
    };
    if first.trim() != "---" {
        return (None, None);
    }

    let mut name = None;
    let mut description = None;
    for line in lines {
        let line = line.trim_end();
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    (name, description)
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|inner| inner.strip_suffix('\''))
        })
        .unwrap_or(value)
        .to_string()
}

/// 尽力把路径变成绝对路径，但不用 `canonicalize`：它要求文件存在，
/// 而且在 Windows 上会加 `\\?\` 前缀，模型读起来别扭。
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|base| base.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("dsh-mini-skills-{tag}-{}", core_session::now_ms()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_skill(root: &Path, dir: &str, content: &str) -> PathBuf {
        let skill_dir = root.join(dir);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
        skill_dir
    }

    #[test]
    fn collects_frontmatter_metadata() {
        let root = temp_dir("frontmatter");
        write_skill(
            &root,
            "eval",
            "---\nname: dsh-mini-eval\ndescription: 跑评测时用\n---\n正文不该进提示\n",
        );
        write_skill(&root, "plain", "没有 frontmatter 的说明\n");
        std::fs::create_dir_all(root.join("empty-dir")).unwrap();

        let scan = scan(&root);
        assert_eq!(scan.skills.len(), 2);
        assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);

        let eval = scan
            .skills
            .iter()
            .find(|s| s.name == "dsh-mini-eval")
            .unwrap();
        assert_eq!(eval.description, "跑评测时用");
        assert!(eval.path.ends_with("SKILL.md"));

        // 没有 frontmatter 时退回目录名，描述给占位。
        let plain = scan.skills.iter().find(|s| s.name == "plain").unwrap();
        assert_eq!(plain.description, "（无描述）");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_directory_is_a_warning_not_an_error() {
        let scan = scan(Path::new("/definitely/not/a/real/skills/dir"));
        assert!(scan.skills.is_empty());
        assert_eq!(scan.warnings.len(), 1);
    }

    #[test]
    fn long_descriptions_are_cut() {
        let root = temp_dir("long");
        let long = "技".repeat(MAX_DESCRIPTION_CHARS + 50);
        write_skill(
            &root,
            "big",
            &format!("---\nname: big\ndescription: {long}\n---\n"),
        );
        let scan = scan(&root);
        assert_eq!(
            scan.skills[0].description.chars().count(),
            MAX_DESCRIPTION_CHARS
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frontmatter_parser_tolerates_quotes_and_colons() {
        let (name, description) =
            parse_frontmatter("---\nname: \"quoted\"\ndescription: '含: 冒号 的描述'\n---\nbody\n");
        assert_eq!(name.as_deref(), Some("quoted"));
        assert_eq!(description.as_deref(), Some("含: 冒号 的描述"));
    }
}
