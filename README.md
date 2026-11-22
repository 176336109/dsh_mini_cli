# dsh-mini

把 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)（dsh）的核心裁成一个**几 MB 的 Rust 单文件二进制**，让嵌入式 Linux、车机、以及任何跑不起 Node 运行时的设备，也能拥有一个真正能干活（读代码、改文件、跑命令、连 MCP 工具）的编码 Agent。

> **English** — dsh-mini is a lightweight Rust re-implementation of the DeepSeek Harness core (agent loop + tool pipeline + MCP client + session log), shipped as a single static binary with a Landlock sandbox. It targets embedded Linux and automotive head units, where Node.js is too heavy and "an agent that cannot be sandboxed" is not acceptable.

```
                  ┌──────────────────────────────────────────────┐
  你的终端 / 车机  │  dsh-mini  ~3 MB  ·  单文件  ·  无运行时依赖  │
                  └──────────────────────────────────────────────┘
```

<p>
<img alt="license" src="https://img.shields.io/badge/license-MIT-blue">
<img alt="rust" src="https://img.shields.io/badge/rust-1.77%2B-orange">
<img alt="platform" src="https://img.shields.io/badge/platform-linux%20%7C%20wsl%20%7C%20windows-lightgrey">
<img alt="tests" src="https://img.shields.io/badge/tests-95%20passing-brightgreen">
</p>

---

## 目录

- [为什么需要它](#为什么需要它)
- [特性](#特性)
- [快速开始](#快速开始)
- [跑在车机上](#跑在车机上)
- [架构](#架构)
- [与 dsh 的差异](#与-dsh-的差异)
- [工具集](#工具集)
- [项目约定与技能](#项目约定与技能)
- [安全模型](#安全模型)
- [性能与内存](#性能与内存)
- [评测](#评测)
- [CLI 参考](#cli-参考)
- [会话日志格式](#会话日志格式)
- [已知边界](#已知边界)
- [贡献](#贡献)
- [License](#license)

---

## 为什么需要它

dsh 本体是 TypeScript：338 个包、主干约 1.6 万行核心代码、依赖 Node 运行时。这在开发机上毫无问题，但放到车机上就是三重负担：

| | dsh（Node） | dsh-mini（Rust） |
|---|---|---|
| 交付形态 | 数百个包 + node_modules | 单个二进制 |
| 冷启动 | 需要 Node 引导 | 直接 exec |
| 常驻内存 | V8 堆 + 模块图 | 与对话量线性相关 |
| 沙箱 | 依赖 bwrap / 容器 | Linux 内核 Landlock，非特权、进程内生效 |
| 交叉编译到 ARM64 | 几乎不可行 | `TARGET=aarch64-unknown-linux-gnu ./scripts/build.sh` |

裁剪的原则只有一条：**只删接缝的实现，不删接缝的形状**。核心主干（loop / session / tools / prompt）一比一移植，provider 每类只保留一个。

---

## 特性

- **一个 turn = 若干 step 的确定性循环**：模型要工具就再问一次，不要就收尾；`max_steps` 兜住死循环。
- **七个内置工具**：`bash`、`read_file`、`write_file`、`str_replace`、`list_dir`、`grep`、`glob`。搜索不依赖系统命令，车机上没有 GNU 工具也能用。
- **MCP 客户端**：stdio + 换行分帧的 JSON-RPC 2.0，远端工具注册后与内置工具完全同形（`mcp__<server>__<tool>`）。
- **Landlock 沙箱**：只接管写类权限，可写范围钉死在 `--cwd`（外加 `TMPDIR` 与 `/dev/null`）；内核不支持就**拒绝启动**，而不是悄悄降级。
- **追加式会话日志**：v4 信封子集，`--resume` 原样续跑；读侧遇到不认识的事件必须拒绝重建。
- **逐事件落盘**：崩溃只丢当前那一条事件，不是整个 turn。
- **流式输出**：模型 token 边生成边打印，工具输出边跑边截断。
- **不联网也能自检**：`--model mock` 走假模型，把整条链路（循环 → 工具 → 日志 → 断言）跑通。

---

## 快速开始

### 构建

```sh
git clone https://github.com/176336109/dsh_mini_cli.git
cd dsh_mini_cli
./scripts/build.sh          # 等价于 cargo build --release，并处理 WSL 上的 9p 慢盘
./scripts/build.sh debug    # 顺带跑一遍 cargo test
```

只需要 Rust 1.77+，没有系统库依赖（TLS 用 rustls，不碰 OpenSSL）。

### 配置

API Key **只从环境变量或 `.env` 读**，不接受命令行参数（避免出现在进程列表和 shell 历史里）：

```sh
# 以下三处任选其一，优先级从高到低
export DEEPSEEK_API_KEY=sk-...
echo 'DEEPSEEK_API_KEY=sk-...' > ./.dsh-mini/.env    # 跟随数据目录
echo 'DEEPSEEK_API_KEY=sk-...' > ~/.dsh-mini/.env    # 全局
```

`.env` 支持 `export` 前缀、单双引号与 `#` 注释，和 `/bin/sh` 的写法一致，可以直接复用 `EnvironmentFile`。
也支持在 `.env` 里覆盖 `DEEPSEEK_BASE_URL`（私有网关 / 离线部署）。

走网关或反向代理时，把 `--base-url` 指向它就够了：

```sh
dsh-mini --base-url http://10.0.0.5:8000/v1 "改个 bug"
```

密钥也不一定要写在 `.env` 里：`--key-file` 会读文件的第一行非空内容，专给 systemd `LoadCredential=` / Docker secrets 这类"裸文件"场景（**Key 永远不会出现在 argv 里**）：

```sh
dsh-mini --key-file /run/credentials/dsh-mini.service/deepseek-key "..."
```

### 跑起来

```sh
# 一次性任务
./target/release/dsh-mini "把 src/ 下所有 unwrap 换成 expect 并说明理由"

# 交互模式（省略 prompt 参数即可）
./target/release/dsh-mini

# 续跑上一次会话
./target/release/dsh-mini --resume latest "接着改 tests/"

# 不联网自检（假模型，会真的执行工具）
./target/release/dsh-mini --model mock '!bash {"command":"echo hello"}'

# 挂 MCP 服务器
./target/release/dsh-mini --mcp mcp.demo.json "用 demo 服务器回显 hi"
```

---

## 跑在车机上

车机上的三个现实约束：CPU 弱、内存小、文件系统可能是只读的。

```sh
# 1. 在 WSL / Linux 构建机上交叉编译（ARM64 车机）
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
TARGET=aarch64-unknown-linux-gnu ./scripts/build.sh

# 2. 想彻底摆脱 glibc 版本问题就编 musl 静态版本
rustup target add aarch64-unknown-linux-musl
TARGET=aarch64-unknown-linux-musl ./scripts/build.sh

# 3. 拷到车机
scp target/.../dsh-mini root@car:/data/bin/
```

部署时建议：

- 用 `--home /data/dsh-mini` 把会话日志指到可写分区（默认是 `<cwd>/.dsh-mini`，车机上往往只读）。
- 用 `--sandbox landlock` 强制沙箱；内核 < 5.13 会在启动阶段就报错，而不是跑起来才发现没有隔离。
- 内存紧张时把 `--history-budget` 调小（例如 `8` MB→`1`），模型可见历史会自动压缩最旧的工具输出。
- 并发跑多个实例时，给每个实例独立的 `--home`，会话文件不共享写。

WSL 上的两个坑，`scripts/build.sh` 已经处理：**别在 `/mnt/*` 上编译**（9p 文件系统比原生盘慢一个数量级，脚本会自动把 `CARGO_TARGET_DIR` 挪到 `$HOME`），以及**别和 Windows 共用 `target/`**（产物格式不同，会反复重编）。

---

## 架构

```
crates/
├── core-llm/        消息与流式词汇 + LlmAdapter 接缝（服务定义）
├── core-session/    事件词汇表 + 追加式日志 + 历史投影 + 落盘接缝
├── core-tools/      工具注册表 + 受守卫的执行管线 + 沙箱（含 Landlock）
├── core-loop/       turn/step 驱动器 + 系统提示拼装
├── providers/       接缝的 Rust 原生实现（DeepSeek / mock / JSONL / 5 个工具 / MCP）
└── cli/             命令行入口（装配 + 终端呈现）
```

一次 turn 的事件流：

```
turn/start
  └─ step/start
       ├─ 组装 system + 历史投影 + 工具 schema  →  LlmAdapter::complete()  (SSE 流式)
       ├─ assistant/message (+ usage)
       ├─ tool/call → guard.check → tool.run → tool/result
       └─ step/end
  仍欠工具请求？→ 下一个 step : turn/end
```

两条**不能省**的语义（与 dsh 逐条对齐）：

1. **一个 turn 可以包含 0 个 step**：首个输入为空或被拒时，turn 开完就关。
2. **step 的推进条件不是"有 tool_calls 就再问一次"**，而是"工具还欠一次请求"。

接缝一览（trait 在 `core-*`，实现在 `providers`）：

| 接缝 | 定义 | 实现 |
|---|---|---|
| `ctx.llm` | `core_llm::LlmAdapter` | `providers::DeepSeekAdapter`、`MockAdapter` |
| `ctx.sessionPersistence` | `core_session::SessionSink` | `providers::Appender`（JSONL） |
| `ctx.tools` | `core_tools::{Tool, Registry}` | `providers::tools::*`、`McpTool` |
| `ctx.sandbox` | `core_tools::sandbox::Sandbox` | `LandlockSandbox`、`NoSandbox` |
| `ctx.mcpClient` | — | `providers::McpClient`（工厂：连一个服务器注册一批工具） |

MCP 的用法和别的接缝不同：它不是一个可替换的 provider，而是 **provider 的工厂** —— 每连一个服务器就往注册表里塞若干工具，循环侧完全无感。

---

## 与 dsh 的差异

**保留**：事件信封形状（`{type, seq, time, data, surfaceOp?}`）、读侧故障关闭规则、turn/step 语义、工具结果归一化（超时 + 截断）、系统提示的分节结构、MCP 工具命名。

**删除**（对应产品能力本次不做）：subagent、多模型路由、compaction 的 `replace` 表面操作、ACP、team/goal/schedule 事件族、`run_code` / PTC 执行器、批量并行调度、presentation 渲染。

**新增**（为嵌入式目标）：Landlock 沙箱、进程组整体回收、历史字节预算、单句柄增量落盘、`--model mock` 离线自检。

---

## 工具集

| 工具 | 说明 | 兜底 |
|---|---|---|
| `bash` | 执行一条 shell 命令（Linux `sh -c`，Windows `cmd /C`） | 单流输出 ≤ 64 KiB（超限杀进程）、单次 ≤ `--tool-timeout` |
| `read_file` | 带行号读取，支持 `offset`/`limit` | 流式读只保留窗口；末尾状态（是否到 EOF、有没有换行）随结果返回，模型不必再用 `od`/`certutil` 探字节 |
| `write_file` | 整体覆盖写入，自动建目录 | 路径必须在 `--cwd` 内 |
| `str_replace` | 唯一匹配的字符串替换 | 唯一性检查、CRLF 容错、路径约束 |
| `list_dir` | 列目录，目录以 `/` 结尾 | 单次 ≤ 1000 项 |
| `grep` | 按内容搜索，返回 `路径:行号: 内容`；支持 `include` glob 与 ASCII 忽略大小写 | 遍历 ≤ 2 万项、单文件 ≤ 2 MiB、≤ 200 条匹配、跳过二进制与 `.git`/`target`/`node_modules` |
| `glob` | 按名字/路径查找文件（`*` 不跨目录、`**` 跨目录） | 遍历同上，≤ 500 条结果 |

阶段产出统一经过 `Registry` 的归一化管线：**任何失败都变成 `is_error` 的 `tool/result`，绝不向上抛** —— 模型必须看到失败原因才能自我纠正。

---

## 项目约定与技能

两个"外部知识"入口，都在启动期一次性读取，不参与循环热路径。

### 项目约定（Agents.md）

默认自动发现 `<cwd>/Agents.md`（或 `AGENTS.md`），也可以用 `--agents-file` 指定。内容作为独立分节注入系统提示，并标注来源路径与截断情况（上限 16000 字符）：

```sh
dsh-mini --agents-file docs/AGENTS.md "按项目约定改一下 CI"
```

注入结果**写进会话日志里的 `system/message` 事件**，所以 `--resume` 沿用的是当时那份约定 —— 之后改了 `Agents.md` 也不会让正在续跑的会话上下文漂移。

### 附加指令（本次运行）

`--system-prompt` / `--system-file` 把一段指令追加到系统提示的**最后一节**，优先级高于内置规则与项目约定：

```sh
dsh-mini --system-prompt "只改 Rust 文件，不要碰 Cargo.lock" "修一下 lint"
dsh-mini --system-file ./ci/reviewer-rules.md "按评审规则过一遍"
```

它和 `Agents.md` 的区别是作用域：`Agents.md` 跟着仓库走（谁跑都生效），附加指令跟着这一次调用走（CI 里给某个 job 临时加约束）。两者都写进会话日志的系统消息。

### 技能（SKILL.md）

`--skills-dir <DIR>` 扫描 `<DIR>/*/SKILL.md`，只把 `name` / `description` 收进提示，正文留给模型按需 `read_file`：

```text
skills/
└── dsh-mini-eval/
    └── SKILL.md      # 开头可选 frontmatter：name / description
```

```sh
dsh-mini --skills-dir .claude/skills "跑一遍回归"
```

三个设计点：

- **按需付费**：技能正文动辄几千字，全塞进提示会把工作上下文挤掉；只给"名字 + 一句话 + 路径"。
- **只读根**：技能目录会自动加入文件工具的只读根，因此放在工作目录之外也能被 `read_file` 读到；**写**依然只能在 `--cwd` 内。
- **上限**：最多 32 个技能、单条描述 200 字符，超出截断并告警。

启动行会把注入情况打出来，避免"以为注入了其实没有"：

```
约定 · /wks/repo/Agents.md（647 字节）
提示 · 约定=/wks/repo/Agents.md（647 字节） · 技能=2 个
```

---

## 安全模型

三道闸，按"便宜 → 昂贵"排列：

1. **路径约束（词法）**：四个文件工具收到的路径会做归一化并检查是否仍在 `--cwd` 内，`..` 逃逸与工作目录外的绝对路径直接拒绝。对还不存在的文件同样有效（不用 `canonicalize`）。
2. **资源上限**：子进程输出有硬上限、工具执行有超时、工具结果进上下文前截断到 3 万字符。超时或取消时**整个进程组**被 `SIGKILL`（`sh -c 'sleep 100 &'` 的后代也不留），正常结束则解除，不误杀用户故意留在后台的服务。
3. **内核沙箱（Landlock）**：只接管写类权限，可写范围 = `--cwd` + `TMPDIR` + `/dev/null`；规则集在**父进程**构造，子进程 `pre_exec` 里只做两个不分配内存的系统调用（`fork` 后调用 malloc 有死锁风险）。权限位按 ABI 版本（v1/v2/v3）逐级放开。

**故障关闭**是硬契约：

```sh
dsh-mini --sandbox landlock ...   # 内核不支持 → 启动即失败，exit 1
dsh-mini --sandbox auto ...       # Linux 上等价于 landlock；其他平台退化为 none 并告警
dsh-mini --sandbox none ...       # 显式关掉，自己承担风险
```

启动行会打印实际可写范围，避免"以为有隔离"：

```
dsh-mini · 模型=deepseek-flash · 会话=s1795... · 沙箱=landlock(abi=v3, 可写=/wks/repo:/tmp) · 工具=bash,list_dir,...
```

Landlock 只管文件系统（ABI v4 起兼管 TCP 端口），不等于容器：进程仍可联网、仍可 fork。它解决的是"模型手滑写坏系统文件"，不是"恶意代码提权"。

---

## 性能与内存

针对车机（弱 CPU、小内存、eMMC）做过一轮系统性的分配与泄漏清理：

| 位置 | 问题 | 处理 |
|---|---|---|
| 循环热路径 | 每个 step 重放全部事件并克隆整段历史 | 历史投影增量维护，`replay()` 只借出 `&[Message]`，请求结构体全借用 |
| 工具 schema | 每个 step 重建 `ToolSchema`（含 `format!`/`json!`） | 注册时求值一次，`schemas()` 借出 `&[ToolSchema]` |
| 文本截断 | 先建 `Vec<char>` 再拼回 `String`（30k 字符 ≈120 KiB/次） | 按 UTF-8 边界切片，零分配 |
| SSE 分帧 | 每行 `drain` 一次缓冲（O(n) 搬移 + 每行一次堆分配） | 读指针前进 + 一次性 drain，缓冲复用 |
| 会话落盘 | 每条事件 `create_dir_all` + `open` + 写两次 + `flush` | 一会话一句柄，序列化进复用缓冲后单次 `write_all` |
| 会话加载 | 整个文件读成 `String` 再解析 | `BufReader` 流式按行解析 |
| 工具输出 | `Command::output()` 全量缓冲 | 双流 `select!` 边读边限，超限杀进程 |
| MCP | 写失败/超时的请求留在 pending 表里 | 两条失败路径都主动摘除；单行 8 MiB 上限 |

配套的可调参数（都有默认值）：`--tool-timeout`（默认 120s）、`--history-budget`（默认 2 MB，超出后只压缩**投影**里最旧的工具输出，磁盘日志始终完整）。

产物大小：Windows x86_64 release（LTO + strip + panic=abort）实测 **3.15 MB**；Linux / ARM64 同数量级。

---

## 评测

评测不靠人看、不靠模型自评：给定 fixture，跑一次 agent，再对最终工作区做断言。

```sh
cargo build --bins
python evals/run_evals.py --self-test     # 断言器自检，必须 12/12
python evals/run_evals.py --smoke         # 管道自检（mock，不花钱），必须 PASS
python evals/run_evals.py --model deepseek-flash   # 全量 51 条
```

- **51 条用例，9 类**：edit 8 / debug 7 / locate 6 / honesty 6 / delivery 6 / env 5 / refactor 5 / long 5 / mcp 3。
- 断言类型：`file_exists`、`file_equals`、`file_contains`、`file_not_contains`、`file_regex`、`command`、`stdout_contains`、`stdout_regex`、`all_of`。
- 支持 `-k <id>` 单跑、`--category` 分类跑、`--repeat N` 看稳定性。
- 报告与现场取证：`evals/.runs/<时间戳>/report.json` 与每个用例的工作区 + `.home/sessions/*.jsonl`。

三条评测卫生规矩：**不改断言迁就模型**、**不删历史批次**、**不把"是否交付"写进框架做启发式检测**（这类判断不可泛化，只能改提示词或加用例）。

当前基线（deepseek-flash，2026-11-23）：**51/51**，293 步 / 49.8 万输入 token / 2.5 万输出 token，单条均值约 9.8k 输入 token、5.7 步。

两处提示词与工具改进把"追加类"和"范围类"用例的步数砍掉了一半（`B7`/`P5` 8→4 步，`P1` 7→4 步）：`read_file` 现在直接报告文件末尾状态，提示词明确"改文件不用 shell 重定向""只处理用户指定的范围"。

模型没有随机种子，单次全绿不等于稳定；关键结论请用 `--repeat 3` 复核。

---

## CLI 参考

| 参数 | 默认 | 说明 |
|---|---|---|
| `[PROMPT]...` | — | 一次性任务；省略则进入交互模式 |
| `--cwd <DIR>` | `.` | 工作目录，同时也是路径约束的根 |
| `--home <DIR>` | `<cwd>/.dsh-mini` | 数据目录，会话日志在 `<home>/sessions/` |
| `--model <NAME>` | `mock` | `mock` 为离线假模型，其他值为 DeepSeek 模型名 |
| `--base-url <URL>` | `https://api.deepseek.com` | API 基址（`.env` 里的 `DEEPSEEK_BASE_URL` 优先） |
| `--thinking` | off | 打开思维链（更慢更贵） |
| `--max-steps <N>` | `24` | 单个 turn 内 step 上限 |
| `--resume [<ID>]` | — | 续跑会话，无参取最近一个 |
| `--mcp <FILE>` | — | MCP 服务器配置（JSON） |
| `--sandbox <MODE>` | `auto` | `auto` / `none` / `landlock` |
| `--agents-file <FILE>` | `<cwd>/Agents.md`（自动） | 项目约定，注入系统提示 |
| `--system-prompt <TEXT>` | — | 附加指令（内联），排在提示最后一节，优先级最高 |
| `--system-file <FILE>` | — | 附加指令（文件），与 `--system-prompt` 同时给时以它为准 |
| `--skills-dir <DIR>` | — | 技能目录，只注入索引；目录同时成为只读根 |
| `--key-file <FILE>` | — | 从文件读 API Key（取第一个非空行） |
| `--tool-timeout <SECS>` | `120` | 单次工具执行超时，超时整组 kill |
| `--history-budget <MB>` | `2` | 模型可见历史预算，`0` 关闭压缩 |

---

## 会话日志格式

`<home>/sessions/<id>.jsonl`，一行一个事件，只追加、`seq` 单调递增：

```json
{"type":"user/message","seq":3,"time":1795364413616,"data":{"content":[{"kind":"text","text":"..."}]},"surfaceOp":"append"}
```

读侧规则：**遇到不认识且没有 `ignorable: true` 的事件类型，必须拒绝重建会话** —— 静默跳过会重建出一个"看起来正常但缺了一块"的会话。

`--resume` 的语义是"回到这个项目上一次的会话"：`next_seq` 与 turn 编号都接着走，历史投影从磁盘重建（并套用当前 `--history-budget`，结果可复现）。

---

## 已知边界

- **不做的能力**：subagent、多模型路由、compaction 改写、ACP、HTTP+SSE 传输的 MCP、MCP 分页。
- **技能只做"索引 + 按需读取"**：没有自动触发、没有技能编排、没有脚本执行沙箱之外的额外约束。
- **Landlock 是文件系统沙箱**，不是容器；联网与 fork 不受限。
- **Windows 仅作开发机**：没有进程组回收的等价物（需要 Job Object），`--sandbox auto` 会退化为 `none` 并告警。
- **会话日志在内存中随对话增长**，只有投影受 `--history-budget` 约束；磁盘侧不做轮转。

路线图（欢迎 PR）：

- [ ] GitHub Actions：Linux 全量测试 + `aarch64` 交叉编译产物
- [ ] MCP HTTP 传输与分页
- [ ] Landlock 网络端口规则（ABI v4+）
- [ ] 显式的 HTTP(S) 代理参数

---

## 贡献

```sh
cargo test --workspace            # 95 项
cargo clippy --all-targets        # 零告警
cargo fmt --all --check
python evals/run_evals.py --self-test && python evals/run_evals.py --smoke
```

提交前请保证上面四行都是绿的。改动涉及 `core-loop` / `core-tools` / `providers` 时，另外跑一遍 `python evals/run_evals.py -k <受影响的类> --model deepseek-flash`。

代码风格：中文注释、每个模块头部写清"这个模块对应 dsh 的哪个包 / 为什么这么裁"。**不要为了通过评测去改断言**。

## License

[MIT](LICENSE) © 2026 dsh-mini contributors

本项目是 DeepSeek Harness 的独立重写实现，不包含其源码；dsh 本身同样以 MIT 协议开源。
