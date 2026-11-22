//! 工具注册表：启动期注册、运行期只读查找。

use std::sync::Arc;

use core_llm::{ToolCall, ToolSchema};

use crate::text::truncate_owned;
use crate::{Guard, Tool, ToolCtx};

/// 内置工具数量的量级，用来一次性预留注册表容量。
const DEFAULT_CAPACITY: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub call_id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
}

/// 三个平行数组按名字升序排列：
/// - `schemas` 连续存放，`schemas()` 直接借出切片给模型请求，零构造；
/// - `tools` 与 `schemas` 同下标，执行时二分查找。
///
/// 老实现每次 `schemas()` 都调用 `Tool::schema()` 重建一遍（含 `format!` 和
/// `json!` 克隆）—— 那是每个 step 一次的固定分配，已消除。
#[derive(Default)]
pub struct Registry {
    names: Vec<String>,
    schemas: Vec<ToolSchema>,
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            names: Vec::with_capacity(DEFAULT_CAPACITY),
            schemas: Vec::with_capacity(DEFAULT_CAPACITY),
            tools: Vec::with_capacity(DEFAULT_CAPACITY),
        }
    }

    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.register(tool);
        self
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.register_arc(Arc::new(tool));
    }

    /// 注册一个已经共享的工具。MCP 这类"一个客户端挂多个工具"的场景用它。
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        // schema 只在注册时求值一次。
        let schema = tool.schema();
        let slot = match self.names.binary_search(&schema.name) {
            Ok(slot) => {
                self.schemas[slot] = schema;
                self.tools[slot] = tool;
                return;
            }
            Err(slot) => slot,
        };
        self.names.insert(slot, schema.name.clone());
        self.schemas.insert(slot, schema);
        self.tools.insert(slot, tool);
    }

    pub fn names(&self) -> Vec<&str> {
        self.names.iter().map(String::as_str).collect()
    }

    /// 名字数组的借用视图，供系统提示拼装等只读场景使用（零分配）。
    pub fn name_slice(&self) -> &[String] {
        &self.names
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// 交给模型的 schema 列表。顺序稳定（按名字升序），便于缓存与断言。
    pub fn schemas(&self) -> &[ToolSchema] {
        &self.schemas
    }

    /// 完整的执行管线。**任何**失败都必须变成 `is_error` 的 tool/result，
    /// 不能向上抛：模型需要看到失败原因才能自我纠正。
    pub async fn execute(&self, call: &ToolCall, ctx: &ToolCtx, guard: &dyn Guard) -> ToolOutcome {
        let fail = |content: String| ToolOutcome {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content,
            is_error: true,
        };

        let Some(slot) = self.lookup(&call.name) else {
            let mut content = format!("工具不存在：{}。可用工具：", call.name);
            for (idx, name) in self.names.iter().enumerate() {
                if idx > 0 {
                    content.push_str(", ");
                }
                content.push_str(name);
            }
            return fail(content);
        };

        if let Err(reason) = guard.check(&call.name, &call.arguments) {
            return fail(format!("策略拒绝执行：{reason}"));
        }

        let fut = self.tools[slot].run(&call.arguments, ctx);
        match tokio::time::timeout(ctx.timeout, fut).await {
            Ok(Ok(content)) => ToolOutcome {
                call_id: call.id.clone(),
                name: call.name.clone(),
                content: truncate_owned(content),
                is_error: false,
            },
            Ok(Err(err)) => fail(err.to_string()),
            Err(_) => fail(format!("工具执行超时（{:?}）", ctx.timeout)),
        }
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.names
            .binary_search_by(|candidate| candidate.as_str().cmp(name))
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AllowAll, ToolError};
    use core_llm::ToolSchema;
    use serde_json::json;
    use std::future::Future;
    use std::pin::Pin;

    struct Echo;

    impl Tool for Echo {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "echo".into(),
                description: "回显".into(),
                parameters: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            }
        }

        fn run<'a>(
            &'a self,
            args: &'a serde_json::Value,
            _ctx: &'a ToolCtx,
        ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
            Box::pin(async move {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidArgs("缺少 text".into()))?;
                Ok(text.to_string())
            })
        }
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn executes_a_registered_tool() {
        let registry = Registry::new().with(Echo);
        let ctx = ToolCtx::new(".");
        let out = registry
            .execute(&call("echo", json!({"text": "hi"})), &ctx, &AllowAll)
            .await;
        assert!(!out.is_error);
        assert_eq!(out.content, "hi");
    }

    #[tokio::test]
    async fn unknown_tool_becomes_an_error_result_not_a_panic() {
        let registry = Registry::new();
        let ctx = ToolCtx::new(".");
        let out = registry
            .execute(&call("nope", json!({})), &ctx, &AllowAll)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("工具不存在"));
    }

    #[tokio::test]
    async fn guard_rejection_short_circuits() {
        struct Deny;
        impl Guard for Deny {
            fn check(&self, _t: &str, _a: &serde_json::Value) -> Result<(), String> {
                Err("越界".into())
            }
        }
        let registry = Registry::new().with(Echo);
        let ctx = ToolCtx::new(".");
        let out = registry
            .execute(&call("echo", json!({"text": "hi"})), &ctx, &Deny)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("策略拒绝"));
    }

    #[test]
    fn schemas_stay_sorted_and_are_borrowed_from_the_cache() {
        struct Named(&'static str);
        impl Tool for Named {
            fn schema(&self) -> ToolSchema {
                ToolSchema {
                    name: self.0.into(),
                    description: String::new(),
                    parameters: json!({"type": "object"}),
                }
            }
            fn run<'a>(
                &'a self,
                _args: &'a serde_json::Value,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
                Box::pin(async { Ok(String::new()) })
            }
        }

        let registry = Registry::new()
            .with(Named("b"))
            .with(Named("a"))
            .with(Named("c"));
        let names: Vec<&str> = registry.schemas().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(registry.names(), names);
        assert_eq!(registry.schemas().as_ptr(), registry.schemas().as_ptr());
    }

    #[test]
    fn re_registering_the_same_name_replaces_in_place() {
        struct Named(&'static str, &'static str);
        impl Tool for Named {
            fn schema(&self) -> ToolSchema {
                ToolSchema {
                    name: self.0.into(),
                    description: self.1.into(),
                    parameters: json!({"type": "object"}),
                }
            }
            fn run<'a>(
                &'a self,
                _args: &'a serde_json::Value,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
                Box::pin(async { Ok(String::new()) })
            }
        }

        let mut registry = Registry::new().with(Named("a", "第一版"));
        registry.register(Named("a", "第二版"));
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.schemas()[0].description, "第二版");
    }
}
