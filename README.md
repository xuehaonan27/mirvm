# mirvm

**Rust 抽象机器（Rust Abstract Machine）的一个事实标准实现，按 JVM 级系统软件构建。**

用真 rustc 做前端（RAM 的加载器/验证器：宏/typeck/trait/MIR，复用不重建），
自研执行引擎实现 RAM 的计算与并发（解释 tier → 生而并发的字节码 VM → Cranelift JIT）。
跳过 codegen 与链接，"改完即跑"。正确性 = 忠实实现 RAM；native codegen 是 RAM 的另一实现，
所以对拍 native 逐字节一致是同源的必然。

心智模型、抽象机器规格、VM 架构、决策账本见 [DESIGN.md](DESIGN.md)。
定位：Miri 是 RAM 的*检查*实现（宁慢勿漏 UB），mirvm 是 RAM 的*运行/标准*实现（假设合法、追求快）。

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
