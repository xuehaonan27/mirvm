# mirvm

一个拥有自己执行引擎的 Rust runtime（工作代号）：rustc 真前端 + 自研 MIR 解释器
（后续：可开关的 Cranelift 热点 JIT，HotSpot `-Xint`/`-Xmixed` 风格）。
跳过 codegen 与链接，目标是"改完即跑"的开发内循环、LLM/Agent 的 Rust 脚本执行，
以及（后置的）真状态持久化 REPL。

架构、决策与里程碑见 [DESIGN.md](DESIGN.md)。

## 状态

M0（工具链打通）：驱动 rustc 前端，定位 entry fn 并 dump MIR。解释器从 M1 开始。

## 快速开始

```bash
rustup toolchain install nightly --component rustc-dev rust-src llvm-tools
cargo build
cargo run -- run demo/fib.rs --dump-mir
```
