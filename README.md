# mirvm

一个拥有自己执行引擎的 Rust runtime（工作代号）：rustc 真前端 + 自研 MIR 解释器
（后续：可开关的 Cranelift 热点 JIT，HotSpot `-Xint`/`-Xmixed` 风格）。
跳过 codegen 与链接，目标是"改完即跑"的开发内循环、LLM/Agent 的 Rust 脚本执行，
以及（后置的）真状态持久化 REPL。

架构、决策与里程碑见 [DESIGN.md](DESIGN.md)。

## 状态

**M2 完成**：真实生态可用——cargo 依赖图、proc-macro、frontmatter 单文件脚本、
真实 args/env、时间/文件 shims。serde_json + rand + regex 与 native `cargo run`
输出逐字节一致（tests/diff_cargo.sh），M1 corpus 回归 7/7（tests/diff.sh）。
热启动：纯 std 脚本 ~0.24s，带 serde_json 的项目 ~0.26s（依赖构建一次全局缓存）。

## 快速开始

```bash
cargo build --release          # mirvm 自身务必 release（debug 慢 ~7×）
alias mirvm=$PWD/target/release/mirvm

mirvm run demo/fib.rs                    # 单文件（零 cargo 快路径）
mirvm run demo/ecosystem.rs              # 带 frontmatter 依赖的脚本（自动物化 cargo 项目）
mirvm run path/to/project -- arg1 arg2   # cargo 项目 + 程序参数
./tests/diff.sh && ./tests/diff_cargo.sh # 差分对拍
```

单文件脚本声明依赖（cargo script / RFC 3424 语法）：

```rust
#!/usr/bin/env mirvm
---
[dependencies]
serde_json = "1"
---
fn main() { /* ... */ }
```
