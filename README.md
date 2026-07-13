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
  M5.1 已完成语义。diff_cargo 3/3、六个 release tracer 脚本（七个独立子断言）、
  25 个 Rust tests、rustfmt 与 Clippy 均通过。
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

真实 Cargo 项目的 `prepare` / `check` / `bench` case 格式、隔离边界和当前本地实证见
[docs/real-projects.md](docs/real-projects.md)。目前不能宣称支持“任意 Rust 程序”。

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
