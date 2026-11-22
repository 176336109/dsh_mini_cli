//! 一个最小的 MCP 服务器，只为两件事存在：
//! 1. 作为 `dsh-mini` 的 MCP 客户端联调/测试对端（不依赖 npx、不联网）。
//! 2. 当写自己 MCP 服务器的最小可读样板。
//!
//! 传输：stdout 上逐行输出 JSON-RPC 2.0；stderr 留给日志。
//! 提供工具：`echo` —— 原样返回 `text` 参数。

use std::io::{BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let response = match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-mcp-server", "version": env!("CARGO_PKG_VERSION")}
            })),
            "tools/list" => Ok(json!({
                "tools": [{
                    "name": "echo",
                    "description": "原样回显 text 参数，用于验证 MCP 通道是否通。",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                        "required": ["text"]
                    }
                }]
            })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                if name != "echo" {
                    Err(format!("未知工具 `{name}`"))
                } else {
                    let text = params
                        .get("arguments")
                        .and_then(|a| a.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    Ok(json!({"content": [{"type": "text", "text": text}]}))
                }
            }
            // 通知没有 id，不需要回复。
            "" => continue,
            other => Err(format!("未知方法 `{other}`")),
        };

        let Some(id) = id else { continue };
        if id.is_null() {
            continue;
        }

        let message = match response {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(message) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": message}
            }),
        };

        writeln!(stdout, "{message}").ok();
        stdout.flush().ok();
    }
}
