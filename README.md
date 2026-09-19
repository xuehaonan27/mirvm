# mirvm

mirvm 目标：
1. 提升 Rust 项目开发效率和进度，快速迭代，减少浪费在等待编译和测试的时间。
2. 一个方便快捷的 cargo script 脚本执行器。

mirvm 是一个以 rustc 为前端、自建执行引擎的 Rust 抽象机器运行实现。它复用 rustc 完成解析、
宏、类型检查、trait 求解和 MIR 生成，在加载相把可达程序降低为 tcx-free typed bytecode，随后由
自己的运行时执行；执行引擎 = tree-walking 解释器 + 方法级 Cranelift JIT（M5.0–M5.5 全收，
JIT 默认开启）。

> 项目仍处于开发阶段，不是完整 Rust 语义的成品。当前状态与已验证边界以
> [docs/current-status.md](docs/current-status.md) 为准，未解决债务见
> [docs/open-issues.md](docs/open-issues.md)。

## 当前状态（2026-08-13 快照）

- **M4 完成**：自研 typed bytecode、tree-walking interpreter、tcx-free 执行相、真实地址内存、
  unwind、libffi FFI、native→guest thunk、1:1 OS 线程与 guest TLS。
- **M5 全收（M5.0–M5.5）**：asm-stub 工厂、llvm.x86 intrinsic 补面、M5.2 语义补全
  （signal/backtrace 两历史 XFAIL 转绿）；方法级 Cranelift JIT = M5.3 骨架 + M5.4a–d
  全覆盖（ABI 全形态、五调用助手、unwind 产品化双 CIE 全覆 LSDA、stmt/rvalue/terminator
  准入三表穷尽）+ M5.5 vmctx 终裁（T 骨架生产定稿）——JIT 默认开启（`--jit off` 回退），
  多帧 unwind 于 2026-08-07 复核并修正为完整 `.eh_frame` 一次注册。
- **M6 冷启动完成**：S1 小件包、S2 依赖剪 codegen、S4 std 预降底座（脚本纯冷 385→104ms）、
  S3′b A2 纯化聚合 deps-image（eco 冷 924→热 66ms）。
- **地址模型 P1/P2 已进入多 Engine 形态**：P1 是“交给原生代码调用的 guest 函数入口”。
  包里只保存逻辑地址和重建配方；每个 Engine 在实例化时得到独有的可执行入口，关闭后旧地址
  也不复用给新 Engine，因此不会把陈旧函数指针误认成新实例。GOT 间接槽同样在每个实例启动时
  重填，不再把某次运行的宿主地址烤进包。
- **corpus manifest 当前 166 项（139 full + 24 smoke + 3 manual）**，含 vendored
  真 cargo 项目（hexyl/tokei）：
  创建时完成 mirvm/native/逢调即编三维逐字节验收；持续门 =
  `tests/suites/corpus/cases.manifest` 唯一真源（exit/oracle/diff 三口径，
  提升项见 [open-issues.md G7](docs/open-issues.md)）；`./tests/run.sh gate`
  2026-07-23 历史全 gate 为 **179 PASS / 0 XFAIL / 0 FAIL**；当前现场复验数字见
  [current-status.md §3](docs/current-status.md)。
- **`.mirvm` 格式 v4**：`Package` 是可重复实例化的程序映像，不是已经运行的 Engine。
  `Package::load` 一次复制并校验文件，之后源文件被改写或删除也不影响该对象；每次
  `instantiate` 都建立独立 frozen/TLS、原生映像、MC 机器码映像和 P1 入口。同一对象可并发
  创建多个 Engine。函数体仍按独立索引惰性驻留并记录真实访问顺序，但为了完整语义验证，
  load 时仍会逐函数临时解码一次。格式尚未冻结，也不承诺跨 build/target 兼容。
- **`mirvm test` Cargo 合同**：默认 self 路径无需 Cargo，支持 lib/bin/integration/
  example/bench、自定义 harness、开发依赖、根 proc-macro、resolver 1/2/3 工作区、复杂
  成员 glob、workspace lints、package spec、常用选择和 libtest 参数。固定
  Cargo/compat/self 的单包 test/bench/doctest 合同 34/34、工作区合同 31/31；doctest
  仍由 rustdoc 前端提取和裁判，但统一由 `mirvm test` 调度。
- **真实嵌入生命周期已接通**：Engine 用执行租约记录正在运行的调用，用延迟持有记录
  “原生系统已经收下回调、但回调尚未开始或撤销”的空窗。`close` 先禁止新入口，再等调用、
  pthread 启动回调和线程私有析构回调退出，最后运行每实例原生析构并释放 Shared、frozen、
  guest TLS 与各宿主线程的 `Ctx`。`Ctx` 是每个宿主线程进入 Engine 时使用的执行上下文；
  即使该宿主线程长期不退出，Engine 关闭也会清空它持有的重资源。`wait_closed` 可等待完成，
  在本线程仍位于该 Engine 调用链中时则明确返回错误，避免等自己退出而死锁。原生构造函数
  的受控异常转成创建失败并完成关闭；原生析构是不可展开的拆除边界，任何异常逃出都
  固定诊断后终止进程，不会把 Engine 留在 `Closing`。
- **传统异步信号已接入多 Engine 和真线程生命周期**：每次 guest handler 注册都会生成固定
  22 字节可执行桩。内核信号帧只做原子登记，不在任意中断点执行 guest、libffi、加锁或展开。
  进程定向事件进入注册 Engine 的 inbox（待处理信号箱）；`pthread_kill` 和真实 libc `raise`
  产生的 `SI_TKILL`（内核的“发给指定 pthread”来源码）进入目标 pthread 按本次注册代际建立的
  稳定槽，之后只能由该线程在安全点或退出收口时执行。未阻塞、经 MIRVM 桥调用的 `raise`
  在返回前跑完
  handler；已阻塞的信号留在内核，`sigwaitinfo` 仍能看到真实 `SI_TKILL`。close 等待已经接收的
  目标线程事件，不能换一条线程代跑；线程退出把 pthread 全局最后一轮 TSD 析构和 signal
  槽一起收口，按原始 key 号顺序处理后再阻塞信号、复查并关槽。当前线程若还有自己的
  目标事件，`wait_closed` 会返回 `ActiveOnCurrentThread`，不会睡住等待自己。
  `signal`/`sigaction` 查询与 `oldact` 仍返回 guest 原地址，多个 Engine 非 LIFO 关闭仍只摘本
  owner。桩、registration（一次 handler 安装的登记对象）和线程槽保留到进程结束；若原生代码
  在 owner 关闭后回装旧桩，裸内核投递以 `_exit(70)` 终止，经 MIRVM 桥调用的 `raise` 返回
  `EngineFault(70)`，
  不会挂起或误投。同步故障信号、realtime 和
  `SA_SIGINFO/SA_ONSTACK/SA_NODEFER/SA_RESETHAND` 仍明确拒绝；进程定向外部信号只承诺在 owner
  Engine 下一安全点派送，不承诺原生 handler 级即时延迟。
- **公开面仍有明确的 `unsafe` 边界**：安全入口是 `Package::load` 和对既有 Engine 的
  `run_main`/状态/关闭操作；`Package::instantiate` 是 `unsafe`，因为容器校验无法证明包内
  原生库、宿主符号和 FFI 签名真实相符。手工 IR 入口与无类型的两机器字导出调用也在
  `vm::engine::raw` 下保持 `unsafe`；`Shared` 不对外公开。这还不是完整的 safe typed API。
  真实 `lang_start` 标记与每次 `run_main` 状态栈仍保证正常返回 101、main panic 和引擎错误
  可区分；C++ typed exception 也可原样穿出整个 `C-unwind` Engine。

已知缺口、响亮拒绝边界与全部未解决债务集中登记在
[docs/open-issues.md](docs/open-issues.md)；目前不能宣称支持"任意 Rust 程序"。
当前开发基线是 Linux/ELF/x86_64，工具链锁定在 `nightly-2026-07-02`。根设计契约见
[DESIGN.md](DESIGN.md)，frame/vmctx 等架构决策与现行语义合同见
[docs/designs/](docs/designs/)。
FFI 的普通 C 与 C-unwind 已按源 ABI 分治：前者维持终止边界，后者允许 Rust panic
或 C++ 异常穿过并执行 Drop。引擎只为 guest panic/`EngineFault` 增加自有异常身份，
继续复用 Rust personality；各帧按实际异常对象分类，不用线程级标记决定 cleanup，也不把
C++ 异常转换成 Rust panic。完整合同见
[C-unwind 跨语言异常合同](docs/designs/c-unwind-contract.md)。
已发布的 P1/普通回调 closure、JIT 机器码及其展开表、已提交的 MC/原生映像仍保留到进程
结束：原生代码可能保存这些地址，当前没有普遍可用的撤销协议。关闭后它们只保留稳定的
“原 Engine 已关闭”身份，不再保留整份 Shared；已知有完成事件的 pthread 线程私有数据
（TSD）回调则由上述
延迟持有等待并清账。少见依赖来源余面、safe typed 嵌入 API、正式沙箱/资源治理、daemon、
格式冻结与跨平台尚未完成，逐条状态见
[docs/open-issues.md](docs/open-issues.md) 的 D/E/G 分区。

## 快速开始

```bash
cargo build --release --locked

# 纯单文件
./target/release/mirvm run tests/scripts/fib.rs

# 带 cargo-script frontmatter 的单文件或 Cargo 项目
./target/release/mirvm run tests/scripts/ecosystem.rs
./target/release/mirvm run path/to/project -- arg1 arg2

# 单包或 resolver 1/2/3 工作区测试；-- 后参数逐字传给 libtest
./target/release/mirvm test path/to/project -- --nocapture
./target/release/mirvm test path/to/workspace --workspace --all-features

# 打成自包含 .mirvm 包并运行（格式 v4 当前不定死，随开发可变动）
./target/release/mirvm pack path/to/project -o app.mirvm
./target/release/mirvm run app.mirvm

# 标准测试入口；完整套件说明见 tests/README.md
make test                              # 每日提交检查（fast）
make smoke                             # 加上小型真实负载与运行时语义
make gate                              # 完整终局门（严格 corpus、deps image、性能上限）
make suite S=corpus.run ARGS="--tier smoke"
make list                              # 全部 suite id
```

`make` 是对外接口，`tests/run.sh` 是它转发的实现；两者之外不要直接调用任何测试脚本。
测试素材全部在 `tests/` 下：guest 程序一个目录（`tests/scripts/`，按前缀分 `c_` corpus driver、
`vmcall_` 导出入口探针、其余差分程序），真实 Cargo 项目一个目录（`tests/projects/`，全部是
pin 到上游某个 commit 的 submodule，不 vendor 进本仓，先 `make projects` 拉取），夹具/预言值
在 `tests/fixtures/`，TSan crate 在 `tests/tsan/`。

真实 Cargo 项目的对拍走 `tests/projects/` + manifest `mode=diff` 三维逐字节；未初始化
submodule 时该条目记 SKIP 而不是 FAIL（pin 与 provenance 见其 README）。目前不能宣称支持
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
