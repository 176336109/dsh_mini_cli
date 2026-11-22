# 产品愿景

- 让嵌入式Linux能化尽量少的内存、CPU占用DSH级别的AGENT
- 可以通CLI方式，灵活指定提示词、Agents.md、Skill目录、mcp配置、日志位置及上限、超时时间、DeepSeek系列模型名、DeepSeek模型Key、网关代理
- 可以使用API能力：https://api-docs.deepseek.com/zh-cn/guides/thinking_mode

# 规则

## 编码时

- 多参考原DSH的代码
- 目标平台是Linux
- Windows下使用WSL测试

## DEBUG时

- 先考虑代码结构上面的臭味道，从结构考虑根因
- 偶发问题加log，拿到入参、log、结果的闭环，然后e2e

## 完成修改时

- 提交Git，并写清楚Commit说明
- 推送前，检查修改内容，自动修改Readme
- 问用户是否触发一次用例评估。

## Agnet能力评估

- 跑测试测试用例，并形成报告。手动触发。
