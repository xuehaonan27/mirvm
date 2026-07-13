# mirvm

mirvm 是一个以 rustc 为前端、自建执行引擎的 Rust 抽象机器运行实现。它复用 rustc 完成解析、
宏、类型检查、trait 求解和 MIR 生成，在加载相把可达程序降低为 tcx-free typed bytecode，随后由
自己的运行时执行；长期目标是在保持 RAM 可观察语义的前提下加入方法级 Cranelift JIT。

> 项目仍处于开发阶段，不是完整 Rust 语义的成品。当前状态、已知缺口和下一步以
> [docs/current-status.md](docs/current-status.md) 为准；文档权威与历史替代关系见
> [docs/README.md](docs/README.md)。

## 当前状态

- **M4 完成**：自研 typed bytecode、tree-walking interpreter、tcx-free 执行相、真实地址内存、
  unwind、libffi FFI、native→guest thunk、1:1 OS 线程与 guest TLS。
- **M5.0 完成并复审**：有限 x86_64 inline asm 可在加载相物化为 GAS wrapper 共享库并由解释器调用。
- **M5.1 完成**：addcarry/subborrow 已使 numbigint 转绿，
  xgetbv 已与 native 差分，pshufb/SHA stdarch helpers 也已使 sha2 转绿；guest 静态归档
  的受约束 Linux/ELF 装载使 blake3 转绿，SIMD 补面也已使 ecosystem 转绿。signal guest
  handler 与 guest backtrace/frame-IP 映射仍是两个原因锁定的独立 XFAIL，不属于
  M5.1 已完成语义；当期验收的 diff_cargo 3/3、25 个 Rust tests 等数字按历史施工口径保留。
- **真实项目 TDD 本轮完成并继续扩面**：当前已有 35/35 个 Rust tests、diff 20/20、diff_cargo 5/5；
  gate-truth 最终 12/12，real-project harness 最终 65/65，最终 full gate5 为
  40 PASS / 2 XFAIL / 0 SKIP / 0 FAIL，fmt、Rust tests、Clippy 与 release build 均最终通过。workspace-local ripgrep/tokei
  已推动 direct dyn 尾 alignment、u128 `SwitchInt`、`track_caller` Reify shim 与宽 volatile
  修复；Cargo runner 额外 rustc warning summary 也已用结构化诊断 filter 最小化收口，且
  保留完整 compiler 收尾与 guest 同文 stderr。ripgrep 与 tokei 当前有十一个 workspace-local
  workload correctness PASS，其中三个已用同一 release mirvm sha256 `7b064b3f…` 完成
  warmup 1 / samples 3 benchmark。单 case 已使用
  Case/Check/Bench/Evidence 分层内容身份（含 Git/controller 字节）、不可变对象与原子 current view；
  schema-3/4 consumer 还会重推分层身份、PASS/XFAIL 语义、exact-check provenance，并从 samples
  复算 benchmark summary；schema-2/3 历史对象保留独立验证路径。harness 已按 `AGENTS.md` 冻结。
  它们仍是 Git-ignored workspace evidence，不能提前视作远程持续 gate。Cargo shim 对项目自定义 rustc/workspace wrapper 当前 fail-closed，不支持
  wrapper composition；聚合质量门与隔离边界见 `docs/current-status.md`。
- **生产 JIT 尚未实现**：Cranelift 目前只用于冻结的 Spike 5，不在产品执行路径中。

当前开发基线是 Linux/ELF/x86_64，工具链锁定在 `nightly-2026-07-02`。根设计契约见
[DESIGN.md](DESIGN.md)，frame/vmctx 等可逆架构决策及旧模型完整保留在
[docs/decision-history.md](docs/decision-history.md)。

## 快速开始

```bash
cargo build --release --locked

# 纯单文件
./target/release/mirvm run demo/fib.rs

# 带 cargo-script frontmatter 的单文件或 Cargo 项目
./target/release/mirvm run demo/ecosystem.rs
./target/release/mirvm run path/to/project -- arg1 arg2

# 当前基础回归；语义完整性仍以 current-status 中的诚实边界为准
MIRVM="$PWD/target/release/mirvm" bash tests/diff.sh
bash tests/m4_gate0.sh
bash tests/m4_gate1.sh
bash tests/m4_gate2.sh
bash tests/m4_gate4.sh
bash tests/m4_gate5.sh

# 真实项目 harness 自身回归（纯本地 fixture，不访问网络）
bash tests/real_projects_regression.sh
```

真实 Cargo 项目的 `prepare` / `check` / `bench` case 格式、Git-ignored workspace artifacts、
隔离边界和分层实证见 [docs/real-projects.md](docs/real-projects.md)。目前不能宣称支持
“任意 Rust 程序”。远程仓库和 GitHub Issues/PRD/PR 操作当前暂停，维护者明确恢复前不要执行。

单文件可使用 cargo script / RFC 3424 风格 frontmatter 声明依赖：

```rust
#!/usr/bin/env mirvm
---
[dependencies]
serde_json = "1"
---
fn main() { /* ... */ }
```

构建和首次运行会生成较大的 nightly/rustc 与 sysroot 缓存；开发和性能测量应使用 release 版本。
