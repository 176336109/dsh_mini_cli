//! JSONL 会话持久化（`ctx.sessionPersistence` 的 provider 侧）。
//!
//! 对应 dsh 的 `dsh-session-persistence-jsonl`。Mini 只写 v4 信封的一个子集，
//! 但**保留了读侧的故障关闭规则**：
//!
//! > 遇到不认识的、且没有 `ignorable: true` 标记的事件类型，必须拒绝重建会话。
//!
//! 因为静默跳过一个必需事件，会重建出一个"看起来正常但其实缺了一块"的会话。
//!
//! 写侧的两个关键取舍（车机 eMMC 上直接决定评测跑批的耗时）：
//!
//! 1. **一个会话一个句柄**：[`Appender`] 打开一次，之后每条事件一次 `write`。
//!    老实现每条事件都要 `create_dir_all` + `open` + 两次 `write` + `flush`。
//! 2. **序列化进复用缓冲**：`serde_json::to_writer` 写进 `Vec<u8>` 再落盘，
//!    既避免了每条事件一个 `String`，也保证序列化失败时不会往磁盘留下半行。

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};

use core_session::{SessionEvent, SessionLog, SessionSink};

/// 读取时的行缓冲上限告警阈值（不强制截断，只用于预留容量）。
const DEFAULT_READ_CAPACITY: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("I/O 失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("事件序列化失败：{0}")]
    Serialize(String),
    #[error("第 {line} 行是未知且不可忽略的事件，拒绝重建会话（{detail}）")]
    UnknownRequiredEvent { line: usize, detail: String },
}

#[derive(Debug, Clone)]
pub struct JsonlStore {
    root: PathBuf,
}

/// 单个会话的追加写句柄。实现 [`SessionSink`]，可直接挂到 `AgentLoop` 上。
#[derive(Debug)]
pub struct Appender {
    file: std::fs::File,
    buf: Vec<u8>,
    path: PathBuf,
}

impl Appender {
    /// 追加一条事件。序列化在写盘之前完成，失败不会留下半行。
    pub fn write(&mut self, event: &SessionEvent) -> Result<(), StoreError> {
        self.write_many(std::slice::from_ref(event))
    }

    /// 批量追加：一次系统调用写完一批事件。
    pub fn write_many(&mut self, events: &[SessionEvent]) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        self.buf.clear();
        for event in events {
            serde_json::to_writer(&mut self.buf, event)
                .map_err(|err| StoreError::Serialize(err.to_string()))?;
            self.buf.push(b'\n');
        }
        self.file.write_all(&self.buf)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SessionSink for Appender {
    fn append(&mut self, event: &SessionEvent) -> Result<(), String> {
        self.write(event).map_err(|err| err.to_string())
    }
}

impl JsonlStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    pub fn exists(&self, id: &str) -> bool {
        self.path_for(id).exists()
    }

    /// 打开（或创建）某个会话的追加写句柄。整个会话只应持有一个。
    pub fn appender(&self, id: &str) -> Result<Appender, StoreError> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.path_for(id);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Appender {
            file,
            buf: Vec::with_capacity(8 * 1024),
            path,
        })
    }

    /// 只追加，一条事件一行。绝不用临时文件 + 改名覆盖 —— 崩溃安全靠"只写尾部"。
    pub fn append(&self, id: &str, event: &SessionEvent) -> Result<(), StoreError> {
        self.appender(id)?.write(event)
    }

    /// 流式读取，不把整个文件先读成 `String`：长会话（几十 MB）在车机上不该翻倍占内存。
    pub fn load(&self, id: &str) -> Result<Vec<SessionEvent>, StoreError> {
        let path = self.path_for(id);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };

        let mut reader = BufReader::with_capacity(DEFAULT_READ_CAPACITY, file);
        let mut events = Vec::with_capacity(256);
        let mut line = String::with_capacity(16 * 1024);
        let mut number = 0usize;

        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            number += 1;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionEvent>(&line) {
                Ok(event) => events.push(event),
                Err(err) => {
                    // 读侧兼容机制：只有写方显式标了 ignorable 才能跳过。
                    let ignorable = serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|raw| raw.get("ignorable").and_then(serde_json::Value::as_bool))
                        .unwrap_or(false);
                    if ignorable {
                        continue;
                    }
                    return Err(StoreError::UnknownRequiredEvent {
                        line: number,
                        detail: err.to_string(),
                    });
                }
            }
        }
        Ok(events)
    }

    /// 把内存日志中尚未落盘的部分写出去。返回本次写入的条数。
    /// 保留给"不挂 sink、只在 turn 结束时同步"的场景；正常路径应使用 [`Appender`]。
    pub fn sync_log(&self, log: &SessionLog, written: &mut usize) -> Result<usize, StoreError> {
        let events = log.events();
        let pending = &events[(*written).min(events.len())..];
        if pending.is_empty() {
            *written = events.len();
            return Ok(0);
        }
        self.appender(log.id())?.write_many(pending)?;
        *written = events.len();
        Ok(pending.len())
    }

    /// 最近一个会话的 id，供 `--resume` 无参时使用。
    pub fn latest_id(&self) -> Option<String> {
        let entries = std::fs::read_dir(&self.root).ok()?;
        entries
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .filter_map(|e| {
                let modified = e.metadata().ok()?.modified().ok()?;
                let stem = e.path().file_stem()?.to_string_lossy().to_string();
                Some((modified, stem))
            })
            .max_by_key(|(modified, _)| *modified)
            .map(|(_, stem)| stem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_session::TurnEndReason;

    fn temp_root(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("dsh-mini-test-{tag}-{}", core_session::now_ms()));
        path
    }

    #[test]
    fn append_then_load_roundtrips() {
        let store = JsonlStore::new(temp_root("roundtrip"));
        let mut log = SessionLog::new("s1");
        let env = log.next_env();
        log.push(SessionEvent::turn_start(env, 1));

        let mut written = 0usize;
        store.sync_log(&log, &mut written).unwrap();
        let loaded = store.load("s1").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].type_name(), "turn/start");
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn appender_writes_one_line_per_event() {
        let store = JsonlStore::new(temp_root("appender"));
        let mut log = SessionLog::new("s1");
        let mut appender = store.appender("s1").unwrap();
        for turn in 1..=3 {
            let env = log.next_env();
            let event = SessionEvent::turn_start(env, turn);
            log.push(event.clone());
            appender.append(&event).unwrap();
        }
        let text = std::fs::read_to_string(appender.path()).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.ends_with('\n'));
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn incremental_sync_does_not_duplicate() {
        let store = JsonlStore::new(temp_root("incremental"));
        let mut log = SessionLog::new("s1");
        let mut written = 0usize;

        let env = log.next_env();
        log.push(SessionEvent::turn_start(env, 1));
        assert_eq!(store.sync_log(&log, &mut written).unwrap(), 1);

        let env = log.next_env();
        log.push(SessionEvent::turn_end(env, 1, TurnEndReason::Completed));
        assert_eq!(store.sync_log(&log, &mut written).unwrap(), 1);
        assert_eq!(store.sync_log(&log, &mut written).unwrap(), 0);

        assert_eq!(store.load("s1").unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn unknown_required_event_refuses_to_reconstruct() {
        let root = temp_root("unknown");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("s1.jsonl"),
            "{\"type\":\"future/thing\",\"seq\":1,\"time\":1,\"data\":{}}\n",
        )
        .unwrap();
        let store = JsonlStore::new(&root);
        assert!(matches!(
            store.load("s1"),
            Err(StoreError::UnknownRequiredEvent { .. })
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_ignorable_event_is_skipped() {
        let root = temp_root("ignorable");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("s1.jsonl"),
            "{\"type\":\"future/thing\",\"seq\":1,\"time\":1,\"data\":{},\"ignorable\":true}\n",
        )
        .unwrap();
        let store = JsonlStore::new(&root);
        assert!(store.load("s1").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let store = JsonlStore::new(temp_root("missing"));
        assert!(store.load("nope").unwrap().is_empty());
    }
}
