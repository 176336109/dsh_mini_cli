#!/usr/bin/env python3
"""dsh-mini 评测跑批器（只用标准库）。

设计原则：每条用例都是**可自动判定**的——给定 fixture，跑一次 agent，
再对最终工作区/进程输出做断言。不靠人看，不靠模型自评。

    python evals/run_evals.py --dry-run          # 只校验用例与 fixture
    python evals/run_evals.py -k B1              # 只跑某条（子串匹配 id）
    python evals/run_evals.py --category edit    # 按类跑
    python evals/run_evals.py --repeat 3         # 稳定性：同一条跑三遍

退出码：全通过 0，有失败 1，环境问题 2。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
RUNS = HERE / ".runs"
PLATFORM = "windows" if os.name == "nt" else "linux"


def _search_binary(name: str) -> Path:
    """在 target/{debug,release} 里找可执行文件。

    依次看 CARGO_TARGET_DIR（WSL 下按 scripts/build.sh 的做法产物在 Linux 盘上）
    和仓库自带的 target/。Windows 与 WSL 的产物格式不同，两者的 target 不能混用，
    所以这里允许外部指定。
    """
    exe = f"{name}.exe" if os.name == "nt" else name
    bases: list[Path] = []
    if os.environ.get("CARGO_TARGET_DIR"):
        bases.append(Path(os.environ["CARGO_TARGET_DIR"]))
    bases.append(ROOT / "target")
    for base in bases:
        for profile in ("debug", "release"):
            candidate = base / profile / exe
            if candidate.exists():
                return candidate
    raise SystemExit(
        f"找不到 {exe}，先跑 `cargo build --bins`。"
        "用 CARGO_TARGET_DIR 构建时，请在该环境里导出同名变量再跑评测。"
    )


def find_binary() -> Path:
    override = os.environ.get("DSH_BIN")
    if override:
        path = Path(override)
        if path.exists():
            return path
        raise SystemExit(f"DSH_BIN 指向的路径不存在：{path}")
    return _search_binary("dsh-mini")


def mock_mcp_binary() -> Path:
    return _search_binary("mock-mcp-server")


def load_cases(selected: list[str] | None) -> list[dict]:
    files = sorted((HERE / "cases").glob("*.json"))
    if not files:
        raise SystemExit("evals/cases/ 下没有用例文件")
    cases: list[dict] = []
    for path in files:
        data = json.loads(path.read_text(encoding="utf-8"))
        cases.extend(data.get("cases", []))
    if selected:
        cases = [c for c in cases if any(key in c["id"] or key in c["category"] for key in selected)]
    return cases


def materialize(case: dict, run_dir: Path) -> None:
    """每次都铺到**全新目录**，绝不清空旧目录。

    早先的版本用 shutil.rmtree 复用目录，一次批量删除超过 50 个文件就会撞上
    运行环境的删除保护，进程被直接掐掉（现象是跑批跑到一半没输出、没有汇总）。
    """
    run_dir.mkdir(parents=True, exist_ok=False)
    for rel, content in case.get("fixture", {}).get("files", {}).items():
        target = run_dir / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")


def sh_cmd(script: str) -> str:
    """把 {python} / {sh} 占位换成当前环境里真实可执行的解释器。"""
    script = script.replace("{python}", f'"{sys.executable}"')
    shell = shutil.which("sh") or ("cmd" if os.name == "nt" else "/bin/sh")
    return script.replace("{sh}", shell)


def read_text(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except FileNotFoundError:
        return None


def check(spec: dict, run_dir: Path, stdout: str) -> list[str]:
    """返回失败原因列表；空列表 = 通过。"""
    if "all_of" in spec:
        reasons: list[str] = []
        for sub in spec["all_of"]:
            reasons.extend(check(sub, run_dir, stdout))
        return reasons

    kind = spec["type"]

    if kind == "command":
        command = sh_cmd(spec["cmd"])
        try:
            proc = subprocess.run(
                command,
                shell=True,
                cwd=run_dir,
                capture_output=True,
                text=True,
                timeout=spec.get("timeout", 60),
                encoding="utf-8",
                errors="replace",
            )
        except subprocess.TimeoutExpired:
            return [f"验证命令超时：{command}"]
        expect = spec.get("expect_exit", 0)
        if proc.returncode != expect:
            return [f"验证命令退出码 {proc.returncode} != {expect}：{command}\n{proc.stderr[-400:]}"]
        needle = spec.get("stdout_contains")
        if needle and needle not in proc.stdout:
            return [f"验证命令输出里没有 {needle!r}：{proc.stdout[-300:]}"]
        return []

    if kind == "stdout_regex":
        return [] if __import__("re").search(spec["pattern"], stdout) else [
            f"agent 输出未匹配 /{spec['pattern']}/"
        ]

    if kind == "stdout_contains":
        return [] if spec["text"] in stdout else [f"agent 输出里没有 {spec['text']!r}"]

    path = run_dir / spec["path"]
    text = read_text(path)

    if kind == "file_exists":
        return [] if text is not None else [f"文件不存在：{spec['path']}"]
    if kind == "file_not_exists":
        return [] if not path.exists() else [f"文件不该存在：{spec['path']}"]
    if text is None:
        return [f"文件不存在：{spec['path']}"]

    if kind == "file_equals":
        return [] if text.strip() == spec["text"].strip() else [
            f"{spec['path']} 内容不符，实际：\n{text.strip()[:400]}"
        ]
    if kind == "file_contains":
        return [] if spec["text"] in text else [f"{spec['path']} 里没有 {spec['text']!r}"]
    if kind == "file_not_contains":
        return [] if spec["text"] not in text else [f"{spec['path']} 里仍有 {spec['text']!r}"]
    if kind == "file_regex":
        return [] if __import__("re").search(spec["pattern"], text) else [
            f"{spec['path']} 未匹配 /{spec['pattern']}/"
        ]
    return [f"未知断言类型：{kind}"]


def session_stats(run_dir: Path) -> dict:
    logs = sorted((run_dir / ".home" / "sessions").glob("*.jsonl"))
    steps = tool_calls = tool_errors = turns = 0
    tokens_in = tokens_out = 0
    for log in logs:
        for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
            if not line.strip():
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = event.get("type", "")
            data = event.get("data", {})
            if kind == "step/start":
                steps += 1
            elif kind == "turn/start":
                turns += 1
            elif kind == "tool/call":
                tool_calls += 1
            elif kind == "tool/result" and data.get("isError"):
                tool_errors += 1
            elif kind == "assistant/message":
                usage = data.get("usage") or {}
                tokens_in += int(usage.get("inputTokens") or 0)
                tokens_out += int(usage.get("outputTokens") or 0)
    return {
        "turns": turns,
        "steps": steps,
        "tool_calls": tool_calls,
        "tool_errors": tool_errors,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
    }


def write_mcp_config(case: dict, run_dir: Path) -> Path | None:
    if not case.get("mcp_demo"):
        return None
    config = {"servers": [{"name": "demo", "command": str(mock_mcp_binary())}]}
    path = run_dir / "mcp.json"
    path.write_text(json.dumps(config, ensure_ascii=False, indent=2), encoding="utf-8")
    return path


def run_case(case: dict, args, binary: Path, run_root: Path, attempt: int = 1) -> dict:
    name = case["id"] if attempt == 1 else f"{case['id']}-r{attempt}"
    run_dir = run_root / name
    materialize(case, run_dir)
    mcp_path = write_mcp_config(case, run_dir)

    command = [
        str(binary),
        "--cwd", str(run_dir),
        "--home", str(run_dir / ".home"),
        "--model", args.model,
        "--max-steps", str(case.get("max_steps", 16)),
    ]
    if mcp_path:
        command += ["--mcp", str(mcp_path)]
    command.append(case["prompt"])

    env = dict(os.environ)
    started = time.time()
    try:
        proc = subprocess.run(
            command,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=case.get("timeout", args.timeout),
            env=env,
        )
        stdout, stderr, code = proc.stdout, proc.stderr, proc.returncode
        timed_out = False
    except subprocess.TimeoutExpired as exc:
        stdout = (exc.stdout or "") if isinstance(exc.stdout, str) else ""
        stderr = "TIMEOUT"
        code = -1
        timed_out = True

    elapsed = time.time() - started
    reasons = ["超时"] if timed_out else check(case["verify"], run_dir, stdout)
    stats = session_stats(run_dir)

    if code not in (0,) and not timed_out and not reasons:
        reasons.append(f"进程退出码 {code}")

    return {
        "id": case["id"],
        "category": case["category"],
        "difficulty": case.get("difficulty", 1),
        "pass": not reasons,
        "reasons": reasons,
        "seconds": round(elapsed, 1),
        "exit_code": code,
        **stats,
        "run_dir": str(run_dir),
        "stdout_tail": stdout[-600:],
        "stderr_tail": stderr[-300:],
    }


def self_test() -> int:
    """验证断言器本身：每条断言都要在该通过时通过、该失败时失败。

    不做这一步，"全部通过"可能只是断言写得太松。
    """
    work = RUNS / f"_selftest-{int(time.time())}"
    materialize({"fixture": {"files": {"a.txt": "hello\n", "dir/b.txt": "x\n"}}}, work)

    probes = [
        ({"type": "file_exists", "path": "a.txt"}, True, "文件存在"),
        ({"type": "file_exists", "path": "missing.txt"}, False, "文件不存在"),
        ({"type": "file_not_exists", "path": "missing.txt"}, True, "断言不存在的文件"),
        ({"type": "file_equals", "path": "a.txt", "text": "hello"}, True, "内容相等（忽略尾部空白）"),
        ({"type": "file_equals", "path": "a.txt", "text": "hellO"}, False, "内容不相等"),
        ({"type": "file_contains", "path": "dir/b.txt", "text": "x"}, True, "子目录内包含"),
        ({"type": "file_not_contains", "path": "a.txt", "text": "ell"}, False, "包含却断言不包含"),
        ({"type": "file_regex", "path": "a.txt", "pattern": "^h.*o$"}, True, "正则匹配"),
        ({"type": "command", "cmd": "{python} -c \"print(42)\"", "stdout_contains": "42"}, True, "命令输出包含"),
        ({"type": "command", "cmd": "{python} -c \"raise SystemExit(3)\"", "expect_exit": 0}, False, "非零退出码要被判失败"),
        ({"type": "stdout_contains", "text": "zzz"}, False, "缺失的输出"),
        ({"type": "all_of", "all_of": [
            {"type": "file_exists", "path": "a.txt"},
            {"type": "file_exists", "path": "nope.txt"},
        ]}, False, "组合断言有一条失败即失败"),
    ]

    bad = 0
    for spec, want_pass, label in probes:
        reasons = check(spec, work, stdout="hello 42\n")
        got_pass = not reasons
        mark = "ok  " if got_pass == want_pass else "BAD "
        if got_pass != want_pass:
            bad += 1
        print(f"[{mark}] {label:28s} 期望{'通过' if want_pass else '失败'} 实际{'通过' if got_pass else '失败'}")
    print(f"\n断言器自检：{len(probes) - bad}/{len(probes)} 符合预期")
    return 0 if bad == 0 else 1


def smoke(binary: Path) -> int:
    """不需要 API Key 的管道自检：用 mock 模型的一次真实工具调用跑完整条链路
    （进程 → 会话日志 → 工具执行 → 断言 → 报告），确认 PASS 路径也是通的。"""
    case = {
        "id": "_smoke",
        "category": "selftest",
        "difficulty": 0,
        "prompt": '!bash {"command":"echo smoke>answer.txt"}',
        "fixture": {"files": {"placeholder.txt": "x\n"}},
        "verify": {
            "type": "all_of",
            "all_of": [
                {"type": "file_exists", "path": "answer.txt"},
                {"type": "file_contains", "path": "answer.txt", "text": "smoke"},
            ],
        },
        "max_steps": 6,
    }
    args = argparse.Namespace(model="mock", timeout=60)
    run_root = RUNS / f"smoke-{int(time.time())}"
    result = run_case(case, args, binary, run_root)
    flag = "PASS" if result["pass"] else "FAIL"
    print(f"[{flag}] 管道自检 · steps={result['steps']} tools={result['tool_calls']} exit={result['exit_code']}")
    for reason in result["reasons"]:
        print(f"       └─ {reason.splitlines()[0][:160]}")
    return 0 if result["pass"] else 1


def main() -> int:
    parser = argparse.ArgumentParser(description="dsh-mini 评测跑批")
    parser.add_argument("-k", "--case", nargs="*", help="按 id 或 category 子串过滤")
    parser.add_argument("--category", help="只跑某一类")
    parser.add_argument("--model", default="mock", help="传给 --model 的值（真实模型由适配器决定）")
    parser.add_argument("--repeat", type=int, default=1, help="每条重复次数，用于看稳定性")
    parser.add_argument("--timeout", type=int, default=180, help="单条默认超时（秒）")
    parser.add_argument("--dry-run", action="store_true", help="只校验用例与 fixture，不跑 agent")
    parser.add_argument("--self-test", action="store_true", help="自检断言器：该通过的通过、该失败的失败")
    parser.add_argument("--smoke", action="store_true", help="不带 API Key 跑一次管道自检（mock 模型）")
    parser.add_argument("--keep", action="store_true", help="保留 run 目录（默认保留，失败时可查）")
    args = parser.parse_args()

    if args.self_test:
        RUNS.mkdir(parents=True, exist_ok=True)
        return self_test()

    if args.smoke:
        RUNS.mkdir(parents=True, exist_ok=True)
        return smoke(find_binary())

    cases = load_cases(args.case)
    if args.category:
        cases = [c for c in cases if c["category"] == args.category]
    if not cases:
        print("没有匹配的用例", file=sys.stderr)
        return 2

    if args.dry_run:
        RUNS.mkdir(parents=True, exist_ok=True)
        dry_root = RUNS / f"dry-{int(time.time())}"
        for case in cases:
            materialize(case, dry_root / case["id"])
            missing = [k for k in ("id", "category", "prompt", "verify") if k not in case]
            status = "OK" if not missing else f"缺字段 {missing}"
            print(f"[dry] {case['id']:6s} {case['category']:9s} {status}")
        print(f"\n共 {len(cases)} 条用例，fixture 全部落盘于 {dry_root}")
        return 0

    binary = find_binary()
    if any(c.get("mcp_demo") for c in cases):
        mock_mcp_binary()

    RUNS.mkdir(parents=True, exist_ok=True)
    run_root = RUNS / time.strftime("%Y%m%d-%H%M%S")
    run_root.mkdir(parents=True, exist_ok=False)
    print(f"工作目录：{run_root}\n")

    results: list[dict] = []
    for case in cases:
        if case.get("platform") and case["platform"] != PLATFORM:
            print(f"[skip] {case['id']:6s} 仅 {case['platform']}")
            continue
        for attempt in range(args.repeat):
            result = run_case(case, args, binary, run_root, attempt + 1)
            result["attempt"] = attempt + 1
            results.append(result)
            flag = "PASS" if result["pass"] else "FAIL"
            print(
                f"[{flag}] {result['id']:6s} {result['category']:9s} "
                f"{result['seconds']:5.1f}s steps={result['steps']:2d} "
                f"tools={result['tool_calls']:2d} err={result['tool_errors']} "
                f"tok={result['tokens_in']}/{result['tokens_out']}"
            )
            for reason in result["reasons"]:
                print(f"       └─ {reason.splitlines()[0][:160]}")

    passed = sum(1 for r in results if r["pass"])
    total = len(results)
    print(f"\n总计 {passed}/{total} 通过")

    by_category: dict[str, list[bool]] = {}
    for result in results:
        by_category.setdefault(result["category"], []).append(result["pass"])
    for category, values in sorted(by_category.items()):
        ok = sum(values)
        print(f"  {category:9s} {ok}/{len(values)}")

    if results:
        report = run_root / "report.json"
        payload = {
            "platform": PLATFORM,
            "model": args.model,
            "passed": passed,
            "total": total,
            "results": results,
        }
        report.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"\n报告：{report}")

    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
