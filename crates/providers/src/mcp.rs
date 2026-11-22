//! MCP（Model Context Protocol）客户端 —— 走 stdio 传输的 JSON-RPC 2.0。
//!
//! 对应 dsh 的 `dsh-mcp-client` / `dsh-mcp-manager`：把外部 MCP 服务器提供的工具
//! 注册进 `ctx.tools`，让它们和内置工具长一个样。这就是"接缝"的用法 ——
//! 循环完全不知道工具来自进程内还是另一个进程。
//!
//! 只实现最小必要面：
//! - `initialize` -> `notifications/initialized` -> `tools/list` -> `tools/call`
//! - 工具名前缀 `mcp__<server>__<tool>`，避免与内置工具撞名
//! - 分页（`nextCursor`）、resources、prompts、sampling 一律不做
//!
//! 协议依据：MCP 规范的 stdio 传输用**换行分隔**的 JSON-RPC 消息，
//! 不是 LSP 的 `Content-Length` 头。

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use core_llm::ToolSchema;
use core_tools::{Registry, Tool, ToolCtx, ToolError};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

/// 客户端声称支持的协议版本。服务器若回别的版本，按规范以其回值为准。
const PROTOCOL_VERSION: &str = "2025-06-18";

/// 单行 JSON-RPC 消息的字节上限。服务器若吐出一条无换行的超长行，
/// 不设限就等于把内存交给对端控制；超限行直接丢弃。
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("启动 MCP 服务器失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("MCP 服务器 `{server}` 未提供 stdin/stdout")]
    NoPipes { server: String },
    #[error("MCP 服务器 `{server}` 提前关闭了连接")]
    Closed { server: String },
    #[error("MCP 请求 `{method}` 超时")]
    Timeout { method: String },
    #[error("MCP 返回错误：{0}")]
    Rpc(String),
    #[error("配置文件解析失败：{0}")]
    Config(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// 本地名字，只用于工具名前缀。
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

impl McpConfig {
    pub fn load(path: &std::path::Path) -> Result<Self, McpError> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| McpError::Config(format!("读不到 {}：{err}", path.display())))?;
        serde_json::from_str(&text).map_err(|err| McpError::Config(err.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct McpClient {
    server: String,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    // 持有 child 只为 kill_on_drop 生效。
    _child: Mutex<Child>,
    next_id: AtomicU64,
    call_timeout: Duration,
}

impl McpClient {
    pub async fn connect(cfg: &McpServerConfig) -> Result<Arc<Self>, McpError> {
        let mut command = Command::new(&cfg.command);
        command
            .args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // 服务器日志走 stderr，直接透传，不污染协议通道。
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (key, value) in &cfg.env {
            command.env(key, value);
        }

        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::NoPipes {
            server: cfg.name.clone(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::NoPipes {
            server: cfg.name.clone(),
        })?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let server = cfg.name.clone();
        let server_for_reader = server.clone();

        tokio::spawn(async move {
            let mut reader = BufReader::with_capacity(64 * 1024, stdout);
            let mut line: Vec<u8> = Vec::with_capacity(16 * 1024);
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if line.len() > MAX_LINE_BYTES {
                    // 超长行已在缓冲里，下一轮会覆盖它；这里只丢弃。
                    line.clear();
                    line.shrink_to(16 * 1024);
                    continue;
                }
                let Ok(text) = std::str::from_utf8(&line) else {
                    continue;
                };
                let Ok(message) = serde_json::from_str::<Value>(text) else {
                    continue; // 非 JSON 的噪声行直接丢弃
                };
                let Some(id) = message.get("id").and_then(Value::as_u64) else {
                    continue; // 通知，没有 id
                };
                if let Some(tx) = reader_pending.lock().await.remove(&id) {
                    let outcome = match message.get("error") {
                        Some(err) => Err(err.to_string()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(outcome);
                }
            }
            // 连接断了：把所有等待中的请求放掉，避免调用方挂死。
            let mut waiting = reader_pending.lock().await;
            for (_, tx) in waiting.drain() {
                let _ = tx.send(Err(format!("MCP 服务器 `{server_for_reader}` 连接已关闭")));
            }
        });

        Ok(Arc::new(Self {
            server,
            stdin: Mutex::new(stdin),
            pending,
            _child: Mutex::new(child),
            next_id: AtomicU64::new(1),
            call_timeout: Duration::from_secs(60),
        }))
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    async fn write_message(&self, message: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_vec(message)
            .map_err(|err| McpError::Rpc(format!("请求序列化失败：{err}")))?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(err) = self.write_message(&message).await {
            // 写不出去就把表项摘掉，别留一个永远不会被唤醒的 sender。
            self.pending.lock().await.remove(&id);
            return Err(err);
        }

        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(reason))) => Err(McpError::Rpc(reason)),
            Ok(Err(_)) => Err(McpError::Closed {
                server: self.server.clone(),
            }),
            Err(_) => {
                // 超时的表项必须自己清理：否则每个慢请求都会在 HashMap 里留下一具尸体。
                self.pending.lock().await.remove(&id);
                Err(McpError::Timeout {
                    method: method.to_string(),
                })
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.write_message(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// 握手：initialize -> notifications/initialized。
    pub async fn initialize(&self) -> Result<Value, McpError> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "dsh-mini", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(result)
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let result = self.request("tools/list", json!({})).await?;
        let tools: &[Value] = result
            .get("tools")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        Ok(tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?.to_string();
                Some(McpToolDef {
                    name,
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("（MCP 工具，无描述）")
                        .to_string(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                })
            })
            .collect())
    }

    /// 返回 (文本内容, 是否被服务器标记为错误)。
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
    ) -> Result<(String, bool), McpError> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;

        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut parts: Vec<String> = Vec::new();
        if let Some(blocks) = result.get("content").and_then(Value::as_array) {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            parts.push(text.to_string());
                        }
                    }
                    // 图片/资源等块类型 Mini 不支持，保留可读占位而不是静默丢。
                    Some(other) => parts.push(format!("[不支持的 MCP 内容块：{other}]")),
                    None => {}
                }
            }
        }
        if parts.is_empty() {
            parts.push(serde_json::to_string(&result).unwrap_or_default());
        }
        Ok((parts.join("\n"), is_error))
    }
}

/// 把一个远端 MCP 工具包装成本地 [`Tool`]，模型侧看不出区别。
pub struct McpTool {
    client: Arc<McpClient>,
    server: String,
    def: McpToolDef,
}

impl McpTool {
    pub fn qualified_name(server: &str, tool: &str) -> String {
        format!("mcp__{server}__{tool}")
    }
}

impl Tool for McpTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: Self::qualified_name(&self.server, &self.def.name),
            description: format!("[MCP:{}] {}", self.server, self.def.description),
            parameters: self.def.input_schema.clone(),
        }
    }

    fn run<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a ToolCtx,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>>
    {
        Box::pin(async move {
            let (text, is_error) = self
                .client
                .call_tool(&self.def.name, args)
                .await
                .map_err(|err| ToolError::Failed(err.to_string()))?;
            if is_error {
                return Err(ToolError::Failed(text));
            }
            Ok(text)
        })
    }
}

/// 连接一个服务器并把它的全部工具注册进注册表。返回注册数量。
pub async fn register_server(
    registry: &mut Registry,
    cfg: &McpServerConfig,
) -> Result<usize, McpError> {
    let client = McpClient::connect(cfg).await?;
    client.initialize().await?;
    let tools = client.list_tools().await?;
    let count = tools.len();
    for def in tools {
        registry.register_arc(Arc::new(McpTool {
            client: Arc::clone(&client),
            server: cfg.name.clone(),
            def,
        }));
    }
    Ok(count)
}
