# mirvm

一个拥有自己执行引擎的 Rust runtime（工作代号）：rustc 真前端 + 自研 MIR 解释器
（后续：可开关的 Cranelift 热点 JIT，HotSpot `-Xint`/`-Xmixed` 风格）。
跳过 codegen 与链接，目标是"改完即跑"的开发内循环、LLM/Agent 的 Rust 脚本执行，
以及（后置的）真状态持久化 REPL。

架构、决策与里程碑见 [DESIGN.md](DESIGN.md)。

## 状态

**M1 完成**：std 程序端到端解释执行，差分测试 5/5 与原生一致
（递归/迭代器/Vec/String/HashMap/panic/catch_unwind）。
脚本热启动 ~0.24s（首次运行自动构建带 MIR 的 sysroot，约几分钟，缓存于 `~/.cache/mirvm`）。

## 快速开始

```bash
cargo build --release          # mirvm 自身务必 release（debug 慢 ~7×）
target/release/mirvm run demo/fib.rs
target/release/mirvm run demo/catch.rs
./tests/diff.sh                # 差分测试：native vs mirvm 对拍
target/release/mirvm run demo/fib.rs --dump-mir   # 只看 MIR 不执行
```
