# 真实 Cargo 项目 correctness 与 benchmark

> 状态日期：2026-07-13。本文描述 `tests/real_projects.sh` 当前已经实现的合同。
> 它是测试基础设施说明，不扩大 [current-status.md](current-status.md) 中的产品语义承诺。

## 1. 结论先行

目前不能宣称 mirvm 已经能运行“任意 Rust 程序”。真实项目 harness 已能用固定 revision、
固定 `Cargo.lock` 和 native oracle 判断一个具体 workload 是 PASS、精确 XFAIL 还是 FAIL，
并且只为 PASS workload 计时；它证明的是这些被执行的切片，不是对整个 Rust 生态的外推。

当前仓库没有提交远程项目 case。2026-07-13 的 hexyl、ripgrep、tokei 结果来自已有的本地
`/tmp` clone；它们是本轮开发证据，不是可长期复现的入库 gate。按当前维护要求，远程仓库和
GitHub Issues 暂不操作。

## 2. 命令

```bash
bash tests/real_projects.sh prepare CASE.toml
bash tests/real_projects.sh check   CASE.toml
bash tests/real_projects.sh bench   CASE.toml

# harness 自身的纯本地回归，不访问网络
bash tests/real_projects_regression.sh
```

依赖 Linux、Git、Python 3.11+、bubblewrap、锁定的 Cargo/rustc，以及 release mirvm。
`PROJECT_SUITE_ROOT` 可覆盖 mirror/cache/run 根目录，`PROJECT_SUITE_ARTIFACTS` 可覆盖证据目录，
`PROJECT_SUITE_MIRVM_CACHE` 可指定已准备的 MIR-rich sysroot cache。

## 3. Case 格式

```toml
name = "example"
repo = "/absolute/path/or-approved-remote"
rev = "0000000000000000000000000000000000000000"
lock_sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
subdir = "."
args = ["--version"]
expected_exit = 0
timeout_seconds = 60
normalizers = []

[env]
EXAMPLE_MODE = "deterministic"

[bench]
warmup = 1
samples = 5

# 只在已知 mirvm 前沿失败时使用；与普通 PASS case 二选一。
# [xfail]
# mirvm_exit = 70
# diagnostic = "mirvm[engine]: 完整、单行、调用点精确的诊断"
```

`name`、`repo`、完整 40-hex `rev` 和 `Cargo.lock` 的 64-hex SHA-256 必填。未知顶层字段、
未知 `[bench]`/`[xfail]` 字段、路径逃逸、保留退出码和 harness 控制环境变量都会被拒绝。
`normalizers` 当前只能是空数组：一旦输出包含非确定性内容，应先把 workload 设计成确定性，
不能用宽泛文本替换掩盖差异。

`[env]` 只进入离线项目 build 和被测 workload，不进入 dependency fetch、全局 sysroot 准备或
benchmark 计时 Python。stdin 未在 schema 中声明，因此 check/bench 两侧都固定为 EOF。

## 4. 三个阶段的合同

`prepare` 以 repo 身份、revision 和 lock hash 共同建立 provenance。它先让 Cargo 对 host target
执行锁定 fetch，再在断网 sandbox 中以只读共享依赖 cache 执行 `cargo build --locked`；build.rs
只能写临时 checkout，不能写宿主工作区或共享 Cargo cache。随后准备持久 MIR-rich sysroot。

`check` 从 mirror 建立两个独立 checkout。native 和 mirvm 使用同一逻辑工作目录、同一 host
target、干净环境、EOF stdin 和断网 namespace；只开放各自运行目录写权限，并各用私有 runtime
cache。native 必须先满足 `expected_exit`，然后逐字节比较 exit/stdout/stderr。运行后 tracked
source 必须保持不变。

`[xfail]` 不是模糊子串白名单。mirvm 必须得到指定退出码，stderr 中必须恰好出现一条以
`mirvm` 开头的 canonical 诊断，而且整行等于 manifest；普通 Cargo trailer 可以存在。失败消失
会成为 XPASS 并使 gate 失败，诊断或调用点漂移也会失败。XFAIL 不具备 benchmark 准入资格。

`bench` 会先重新执行完整 correctness。只有普通 PASS 才建立新的两侧 checkout，先 warmup，
再按 native-first / mirvm-first 交替采样。每一次 warmup 和 sample 的 exit/stdout/stderr 都必须
等于 correctness oracle，每轮还会复查 tracked source；隔离器失败、旧文件复用和 workload
漂移都不会生成样本。成功时写出 `samples.jsonl` 与 `summary.json`，summary 包含 repo/rev/lock、
args/env、host target、warmup/sample 数和 Cargo/rustc/mirvm 内容哈希，以及 median/p95。

## 5. 2026-07-13 本地证据

| 项目切片 | 固定 revision | 结果 |
|---|---|---|
| hexyl v0.17.0，读取 64 bytes 的 `Cargo.toml` | `8eb6d4771ce1ec7af65d06bd335457783b77d557` | correctness PASS；3 样本 native median 约 51.0 ms，mirvm median 约 1.570 s |
| ripgrep，匹配 `Cargo.toml` 的 package name | `d5b85d44057ff729a89be9c6549958c45d95aa99` | 精确 XFAIL：`memchr::memmem::Searcher::find` 的间接调用目标前沿 |
| tokei，输出 `Cargo.toml` JSON 统计 | `fa44e5194060305576514d59b850353643afbfc8` | 精确 XFAIL：`log::max_level` 中 `u128` 聚合非标量前沿 |

这些 clone 和 case 文件位于临时目录，不能替代入库 manifest。未来恢复远程项目工作时，应先把
来源、revision、lock hash、许可/维护策略和精确 XFAIL 一并评审，再把它们接入持续 gate。

## 6. 仍需诚实说明的限制

- harness 和产品当前都以 Linux/ELF/x86_64 为开发基线。
- bubblewrap 隔离用于限制写面和 check/build 网络，不是机密性边界；sandbox 仍可读取宿主的
  只读路径。dependency fetch 阶段允许网络，只有显式执行 `prepare` 才会发生。
- 两侧逻辑 cwd 相同，但 checkout、HOME、TMP、target 的物理绝对路径不同。会观察绝对路径的
  程序可能得到差异；当前没有 normalizer 来隐藏它。
- 同名 case 的 artifacts 还没有并发锁；不要并行运行相同 `name`。
- signal guest handler、guest backtrace/frame-IP、未支持 intrinsic/asm/链接形态及平台差异仍会
  按 [current-status.md](current-status.md) 的边界失败，不能由 harness 的存在推导为已支持。
