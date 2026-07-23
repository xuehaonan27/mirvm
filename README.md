# mirvm

mirvm 是一个以 rustc 为前端、自建执行引擎的 Rust 抽象机器运行实现。它复用 rustc 完成解析、
宏、类型检查、trait 求解和 MIR 生成，在加载相把可达程序降低为 tcx-free typed bytecode，随后由
自己的运行时执行；执行引擎 = tree-walking 解释器 + 方法级 Cranelift JIT（M5.0–M5.5 全收，
JIT 默认开启）。

> 项目仍处于开发阶段，不是完整 Rust 语义的成品。当前状态、已知缺口和下一步以
> [docs/current-status.md](docs/current-status.md) 为准；文档权威与历史替代关系见
> [docs/README.md](docs/README.md)。

## 当前状态（2026-07-21 快照）

- **M4 完成**：自研 typed bytecode、tree-walking interpreter、tcx-free 执行相、真实地址内存、
  unwind、libffi FFI、native→guest thunk、1:1 OS 线程与 guest TLS。
- **M5 全收（M5.0–M5.5）**：asm-stub 工厂、llvm.x86 intrinsic 补面、M5.2 语义补全
  （signal/backtrace 两历史 XFAIL 转绿）；方法级 Cranelift JIT = M5.3 骨架 + M5.4a–d
  全覆盖（ABI 全形态、五调用助手、unwind 产品化双 CIE 全覆 LSDA、stmt/rvalue/terminator
  准入三表穷尽）+ M5.5 vmctx 终裁（T 骨架生产定稿）——JIT 默认开启（`--jit off` 回退），
  `tests/m5_gate6.sh` 收口全绿。
- **M6 冷启动完成**：S1 小件包、S2 依赖剪 codegen、S4 std 预降底座（脚本纯冷 385→104ms）、
  S3′b A2 纯化聚合 deps-image（eco 冷 924→热 66ms）。
- **地址模型 P1/P2 完成**（2026-07-17）：GOT 间接消除宿主地址烘焙 + extern fn 条目
  可执行化，thunk 盲区结构性根治。
- **corpus 批1–10 全量 129 个真实 crate driver**（创建时完成 mirvm/native/逢调即编
  三维逐字节验收；持续门 = 默认 mirvm 单跑 exit-code/oracle 级，提升项见
  [open-issues.md G7](docs/open-issues.md)）；gate5 **167 PASS / 0 XFAIL / 0 FAIL**，
  m5_gate6 4/4，cargo test 76/76，diff.sh 45/45（默认 + 阈值=1 双态 + MIRVM_JIT_SYNC
  同步发布），diff_cargo 5/5。

已知缺口、响亮拒绝边界与全部未解决债务集中登记在
[docs/open-issues.md](docs/open-issues.md)；目前不能宣称支持"任意 Rust 程序"。
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

# 打成 .mirvm 包并运行（mode B 片②；格式当前不定死，随开发可变动）
./target/release/mirvm pack path/to/project -o app.mirvm
./target/release/mirvm run app.mirvm

# 当前基础回归；语义完整性仍以 current-status 中的诚实边界为准
MIRVM="$PWD/target/release/mirvm" bash tests/diff.sh
bash tests/m4_gate0.sh
bash tests/m4_gate1.sh
bash tests/m4_gate2.sh
bash tests/m4_gate4.sh
bash tests/m4_gate5.sh
bash tests/m5_gate6.sh   # M5 收口门（内含 m4_gate5 全量）

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
