//! 端到端：客户端 ←stdio JSON-RPC→ 我们的 mock MCP 服务器。
//!
//! 这个测试的价值在于它是**真的跨进程**：spawn 一个二进制、说 JSON-RPC、
//! 把结果塞进工具注册表再执行一次。MCP 通道里最常见的失败（换行分帧、
//! 握手顺序、id 关联、连接提前关闭）都会在这里暴露。

use core_llm::ToolCall;
use core_tools::{AllowAll, Registry, ToolCtx};
use providers::{register_server, McpServerConfig};

fn demo_server() -> McpServerConfig {
    McpServerConfig {
        name: "demo".into(),
        command: env!("CARGO_BIN_EXE_mock-mcp-server").into(),
        args: Vec::new(),
        env: Default::default(),
    }
}

#[tokio::test]
async fn registers_remote_tools_with_prefixed_names() {
    let mut registry = Registry::new();
    let count = register_server(&mut registry, &demo_server())
        .await
        .expect("握手 + tools/list 应当成功");
    assert_eq!(count, 1);
    assert_eq!(registry.names(), vec!["mcp__demo__echo"]);

    let schema = &registry.schemas()[0];
    assert!(schema.description.contains("[MCP:demo]"));
    assert_eq!(schema.parameters["required"][0], "text");
}

#[tokio::test]
async fn calls_a_remote_tool_and_returns_its_text() {
    let mut registry = Registry::new();
    register_server(&mut registry, &demo_server())
        .await
        .unwrap();

    let call = ToolCall {
        id: "c1".into(),
        name: "mcp__demo__echo".into(),
        arguments: serde_json::json!({"text": "hello mcp"}),
    };
    let outcome = registry.execute(&call, &ToolCtx::new("."), &AllowAll).await;
    assert!(!outcome.is_error, "{}", outcome.content);
    assert_eq!(outcome.content, "hello mcp");
}

#[tokio::test]
async fn missing_server_binary_is_a_clean_error() {
    let cfg = McpServerConfig {
        name: "ghost".into(),
        command: "definitely-not-a-real-binary-xyz".into(),
        args: Vec::new(),
        env: Default::default(),
    };
    let mut registry = Registry::new();
    let result = register_server(&mut registry, &cfg).await;
    assert!(result.is_err());
    assert!(registry.is_empty(), "失败时不能留下半个工具");
}
