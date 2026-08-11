# 真实 Cargo 项目 correctness 与 benchmark

> **停放声明（2026-07-23）**：本文描述的 `tests/real_projects*.sh` harness 已整体
> 挪入 `tests/parked/` 休眠——仓内零 case（case 与源码镜像在 git-ignored
> `artifacts/real-projects/` 且本机已不存在）、不进 CI、其"真项目对拍"职责已由
> 更轻的 `corpus/projects/<名>/`（`tests/suites/corpus/cases.manifest` `mode=diff`）接替。
> 休眠原因与复活条件见 `tests/parked/README.md`。以下为历史合同存档，仅供参考。

> 状态日期：2026-07-13。本文描述 `tests/real_projects.sh` 当前已经实现的合同。
> 它是测试基础设施说明，不扩大 [current-status.md](current-status.md) 中的产品语义承诺。

## 1. 结论先行

目前不能宣称 mirvm 已经能运行“任意 Rust 程序”。真实项目 harness 已能用固定 revision、
固定 `Cargo.lock` 和 native oracle 判断一个具体 workload 是 PASS、精确 XFAIL 还是 FAIL，
并且只为 PASS workload 计时；它证明的是这些被执行的切片，不是对整个 Rust 生态的外推。

当前工作集固定为 ripgrep 与 tokei，source、case、suite cache 和结果位于 Git-ignored 的
`artifacts/real-projects/{sources,cases,suite,results}`。它比 `/tmp` clone 更适合跨开发轮次 TDD，
但仍不是已提交的持续 gate，并可能被 `git clean -X/-x` 删除。按当前维护要求，远程仓库、
GitHub Issues 与其他 GitHub 操作全部暂停，恢复前只使用已有的本地来源。

## 2. 命令

```bash
# 当前 workspace-local working set
export PROJECT_SUITE_ROOT="$PWD/artifacts/real-projects/suite"
export PROJECT_SUITE_ARTIFACTS="$PWD/artifacts/real-projects/results"
export PROJECT_SUITE_MIRVM_CACHE="$PWD/artifacts/real-projects/suite/mirvm-xdg"

bash tests/real_projects.sh prepare CASE.toml
bash tests/real_projects.sh check   CASE.toml
bash tests/real_projects.sh bench   CASE.toml

# harness 自身的纯本地回归，不访问网络
bash tests/real_projects_regression.sh
```

依赖 Linux、Git、Python 3.11+、bubblewrap、锁定的 Cargo/rustc，以及 release mirvm。
`PROJECT_SUITE_ROOT` 可覆盖 mirror/cache/run 根目录，`PROJECT_SUITE_ARTIFACTS` 可覆盖证据目录，
`PROJECT_SUITE_MIRVM_CACHE` 可指定已准备的 MIR-rich sysroot cache。由于隔离器会挂载私有 `/run`
tmpfs，控制器启动时会 realpath 检查 workspace、suite、toolchain 与 cache；任何必要宿主路径位于
`/run` 或其子目录都会以基础设施错误 fail-fast，而不是运行到中途才因路径被遮蔽失败。

## 3. Case 格式

```toml
name = "example"
repo = "artifacts/real-projects/sources/example"
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

`name`、`repo`、完整 40-hex `rev` 和 `Cargo.lock` 的 64-hex SHA-256 必填。脚本先切到仓库根，
因此本地 `repo` 可写成仓库根相对路径，也可使用绝对路径；远程地址虽然属于 schema 能力，当前
按维护要求暂停使用。未知顶层字段、
未知 `[bench]`/`[xfail]` 字段、路径逃逸、保留退出码和 harness 控制环境变量都会被拒绝。
`normalizers` 当前只能是空数组：一旦输出包含非确定性内容，应先把 workload 设计成确定性，
不能用宽泛文本替换掩盖差异。

调用者不填写任何 digest。`name` 只是人类可读的结果 namespace，不进入 CaseID；因此同名但
args/env/provenance 不同的 case 不会串用 correctness 证据。`[bench]` 也不进入 CaseID，而是在
正确性身份确定后单独派生 BenchID；只改 warmup/samples 不会伪造一个新 correctness case。

`[env]` 只进入离线项目 build 和被测 workload，不进入 dependency fetch、全局 sysroot 准备或
benchmark 计时 Python。stdin 未在 schema 中声明，因此 check/bench 两侧都固定为 EOF。

## 4. 三个阶段的合同

`prepare` 以 repo 身份、revision 和 lock hash 共同建立 provenance。它先让 Cargo 对 host target
执行锁定 fetch，再在断网 sandbox 中以只读共享依赖 cache 执行 `cargo build --locked`；build.rs
只能写临时 checkout，不能写宿主工作区或共享 Cargo cache。随后准备持久 MIR-rich sysroot。

`check` 从 mirror 建立两个独立 checkout。native 和 mirvm 使用同一逻辑工作目录、同一 host
target、干净环境、EOF stdin 和断网 namespace；只开放各自运行目录写权限，并各用私有 runtime
cache。每次运行的 bubblewrap namespace 在私有 `/run` tmpfs 中建立
`/run/mirvm-project-side`，把物理 side root 映射到相同逻辑路径；rustc diagnostics 再 remap 到
`/mirvm-project`。harness 通过 RUSTC proxy **追加**自己的 diagnostic remap，而不是设置
`CARGO_ENCODED_RUSTFLAGS` 覆盖 Cargo 决策，因此项目 `.cargo/config.toml` 的 rustflags 保持生效。
native 必须先满足 `expected_exit`，然后逐字节比较 exit/stdout/stderr。运行后 tracked source
必须保持不变。

这不等于可以嵌套任意项目 rustc wrapper。产品 Cargo shim 在接管 Cargo 前会拒绝非空
`RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER`，也会查询并拒绝有效的
`build.rustc-wrapper` / `build.rustc-workspace-wrapper`。当前策略是 fail-closed；wrapper
composition 尚未实现。

产品 runner 还要处理 Cargo 与 human rustc 的诊断协议差异：Cargo 已消费 target crate 的 JSON
diagnostics，而 runner 重建的 human session 会在最终收尾额外打印 `N warnings emitted`。callback
现在只 lower tcx-free `Module`，随后安装结构化 `TRACK_DIAGNOSTIC` filter；它只抑制无
lint/code/span/children/suggestions 的 `ForceWarning` count-summary，仍委托 rustc 原 hook。
`run_compiler` 完整完成 `tcx.finish`、诊断和 compiler drop、恢复 hook，并且只有 compiler success
才在 callback 外执行 VM。因此 guest `process::exit` 也不能截断 rustc 收尾；guest 自己写出的同文
stderr 不会被过滤。`tests/fixtures/cargo_warning_return.rs` 同时锁住这两个方向。

`[xfail]` 不是模糊子串白名单。mirvm 必须得到指定退出码，stderr 中必须恰好出现一条以
`mirvm` 开头的 canonical 诊断，而且整行等于 manifest；普通 Cargo trailer 可以存在。失败消失
会成为 XPASS 并使 gate 失败，诊断或调用点漂移也会失败。XFAIL 不具备 benchmark 准入资格。

`bench` 会先重新执行完整 correctness。只有普通 PASS 才建立新的两侧 checkout，先 warmup，
再按 native-first / mirvm-first 交替采样。每一次 warmup 和 sample 的 exit/stdout/stderr 都必须
等于 correctness oracle，每轮还会复查 tracked source；隔离器失败、旧文件复用和 workload
漂移都不会生成样本。成功时写出 `samples.jsonl` 与 `summary.json`，summary 包含 repo/rev/lock、
args/env、host target、warmup/sample 数和 Cargo/rustc/mirvm 内容哈希，以及 median/p95。
发布与消费时都会严格解析每个 sample 的 case/index/order/ns，核对配置样本数，并从原始样本重新
计算 native/mirvm median 与 p95；只改 summary 再重算 EvidenceID 不能伪造有效 benchmark。
当前计时语义是预热 cache 后的 **end-to-end Cargo 调度 + rustc lowering/finalization + VM 执行**，
不是纯 VM execution-only 吞吐。samples=3 时 p95 实际等同于最大样本；这些数字用于可追溯快照，
不作为稳定性能门。

### 身份、并发与发布合同

外部接口仍是 `prepare|check|bench CASE.toml`，内部按四层身份分开：

- **CaseID**：绑定 repo/revision/lock、subdir、args/env、expected exit、timeout、EOF stdin 与精确
  XFAIL；排除人类 `name` 和 benchmark schedule。
- **CheckID**：绑定 CaseID、host target，以及 Cargo/rustc/rustdoc/mirvm/bwrap/Python/Git、RUSTC proxy、
  harness、evidence validator 的内容哈希和 MIRVM sysroot marker 哈希。Git 运行在清空全局/system
  config 并固定 `core.autocrlf=false` 的 controller 环境中。物理路径只写 provenance，
  不代替内容身份。Python 哈希不宣称覆盖其动态库闭包；sysroot marker 代表 cache 协议声明的
  build identity，不宣称重新 Merkle-hash 整个 sysroot。
- **BenchID**：绑定 CaseID、CheckID、**exact Check EvidenceID**、warmup/samples、交替顺序、时钟
  与单位。benchmark 不能脱离它实际消费的 correctness oracle。
- **EvidenceID**：分别由 check/bench 的 schema-3/4 `result.json` envelope 和 payload 字节内容派生。
  consumer 会从 metadata 重推 CaseID/CheckID/BenchID，重算每个 payload 的长度/hash、去掉
  EvidenceID 后重算 envelope 地址，并校验 `COMMITTED`、精确文件集合与目录身份。check consumer
  还会重新执行 PASS 的 exit/stdout/stderr 等值关系和 XFAIL 的 exit/canonical diagnostic 规则；bench
  consumer 会读取 exact linked check evidence、交叉核对其 execution provenance，再执行上述样本/
  统计复算。

当前 writer 对没有外部 workload tool 的 case 发布 schema 3；显式声明 `workload_tools` 时发布
schema 4，并将实际执行工具与 `SYSTEM_PATH` 身份带入 check 和 benchmark。validator 对已存在的
schema-2/3 object 保留独立历史 dispatch，使旧证据仍按旧字段合同可验证；它不会用新 schema 的
新增事实改写旧 schema 当时具备的保证。pre-identity 扁平快照则不属于任何内容寻址 schema。

发布布局如下：

```text
results/<name>/
  .staging/...                         # 仅运行中，非 evidence
  objects/check/<check-id>/<evidence-id>/{payload...,result.json,COMMITTED}
  objects/bench/<bench-id>/<evidence-id>/{samples.jsonl,summary.json,result.json,COMMITTED}
  check -> objects/check/<check-id>/<evidence-id>
  bench -> objects/bench/<bench-id>/<evidence-id>
  legacy/...
```

`check`/`bench` 只是原子替换的 current view，不是历史本体。有效 case 完成解析、取得 name/cache
locks 并进入 evidence transaction 后，新 check 先撤销 bench、再撤销 check，随后才执行
identity/prepared/run preflight；失败后 current 保持缺失，但不可变历史对象仍保留。旧扁平目录第一次遇到新 harness
时移入 `legacy/*.pre-identity.*`，不会冒充 schema 2/3 evidence。XFAIL 可以发布带精确状态的 check
evidence，但没有 benchmark 资格。

若 `results/current/` 这种顶层扁平快照仍存在，它也是 pre-identity 历史材料，不是 current API；
只有 `results/<name>/check` 与 `results/<name>/bench` 的相对 symlink 才是当前视图。

每个 `name` 有独占、非阻塞 workflow lock；冲突以退出码 75 和 `evidence_busy` fail-fast。另有
suite-global cache lock：prepare 独占，check/bench 共享，所以不同名称的只读 check 可以并发，
同时不会与共享 Cargo/MIRVM cache 的写入竞争。固定加锁顺序是 name → cache。Python/Git controller、
命令替换与 runner grouping 子 shell、bubblewrap/workload 都显式关闭两个锁 FD；确定性 hard-kill
回归证明主 workflow 消失后可立即重试。prepare/check/bench 都会回收全局 `.staging`、phase-ID
目录下的隐藏 `.staging.*` 以及崩溃遗留的临时 current symlink，同时 prepare 不撤销有效 current。
workload 又处在 PID namespace 中，不能用后台后代继续改写 payload 或拖住 workflow lock。

新对象先在全局 staging 生成并作 stage 校验，再以仍可写状态移入目标 CheckID/BenchID 目录的隐藏
staging；在那里完整封存后，才在**同一父目录**原子 rename 为 EvidenceID。这样最终 object 路径
一出现就已经是完整、只读、内容校验过的对象。benchmark 完成采样后还会在发布前再次验证 exact
check evidence，关闭初次准入与最终发布之间的 TOCTOU 窗口。object rename 后、current 发布前的
crash 仍可能留下完整但不可见的孤儿 object，这是安全历史而非 current。

发布对象只含 regular files，文件封为 `0444`、目录为 `0555`；文件、object parent 与 current view
所在目录均执行 `fsync`。这提供同一文件系统上的原子可见性和本地耐久性步骤，不是断电恢复证明，
也不抵抗 owner/root 主动改回权限；完整性仍以 consumer 重算为准。object rename 与 current view
是两个提交点：两者之间崩溃可留下完整但不可见的孤儿对象，这是安全状态。

## 5. 2026-07-13 本地证据

### 当前 workspace working set

| 项目切片 | 固定 provenance | 当前证据层级 |
|---|---|---|
| ripgrep，匹配 `Cargo.toml` 的 package name | rev `d5b85d44057ff729a89be9c6549958c45d95aa99`；lock `7d0fc6b674662a91a640e1ba17027873ea109e63bdc192beb8413d7d4a8879cf` | correctness **PASS**；warmup 1、samples 3。native median 52,103,549 ns（52.10 ms）、p95 56,295,738 ns；mirvm median 3,686,678,338 ns（3.687 s）、p95 3,706,007,559 ns |
| tokei 原始切片，输出 `Cargo.toml` JSON 统计 | rev `fa44e5194060305576514d59b850353643afbfc8`；lock `20691d68bf9c3862a80e5dbb6aae0729469fa5d86eff884b682f028877389e95` | correctness **PASS**；warmup 1、samples 3。native median 129,798,756 ns（129.80 ms）、p95 130,363,928 ns；mirvm median 3,795,783,699 ns（3.796 s）、p95 3,828,973,362 ns |
| `tokei_languages`，`--columns 160 --compact tests/data` | 同一 tokei rev/lock；输入覆盖 **206** 个 tracked fixtures | correctness **PASS**；stdout 两侧均为 **205 行**，SHA-256 均为 `0f5058901e0042629c9ff02714134206ca417235ebf6f892aeccec03a34d64c4`。warmup 1、samples 3；native median 148,099,847 ns（148.10 ms）、p95 176,679,111 ns；mirvm median 4,662,846,996 ns（4.663 s）、p95 4,731,264,584 ns |
| `ripgrep_gzip`，`--search-zip` 搜索固定 gzip fixture | 同一 ripgrep rev/lock；外部 `gzip` 身份进入 schema-4 evidence | correctness **PASS**；尚未 benchmark |
| `ripgrep_parallel_nomatch`，4 线程递归无匹配 | 同一 ripgrep rev/lock | correctness **PASS**，双方预期退出码均为 1；尚未 benchmark |
| `ripgrep_mmap_binary`，`--mmap` 搜索含 NUL 的 fixture | 同一 ripgrep rev/lock | correctness **PASS**；双方 stdout 均为 58 bytes，SHA-256 均为 `3f44831849e7d64e89db7dcebf69ceb3d6769f9b30e5e4cbb32113e83b974775`；尚未 benchmark |
| `tokei_sort_code`，compact 全 fixture 并按 code 排序 | 同一 tokei rev/lock | correctness **PASS**；双方 stdout 均为 34,084 bytes，SHA-256 均为 `2f28dd4a8dd670095f5529367ca9a5d053da5782b06159fa5b7d9e086112ebbe`；尚未 benchmark |
| `tokei_streaming_json`，单 Rust fixture 的 streaming JSON | 同一 tokei rev/lock | correctness **PASS**；双方 stdout 均为 221 bytes，SHA-256 均为 `ff5228f0bca37cd6e88a3b7282cd8fe6377b8a772e595706e4d04c12ed8f2ed6`；尚未 benchmark |
| `ripgrep_parallel_match`，4 线程遍历源码树并唯一命中 | 同一 ripgrep rev/lock | correctness **PASS**；双方 stdout 均为 121 bytes，SHA-256 均为 `2e0e6afc717e9444e1adddf1add963717b7cd8cd0b48631527c5ed5028fe61c5`；尚未 benchmark |
| `ripgrep_multiline_replace`，跨行捕获并 replacement | 同一 ripgrep rev/lock | correctness **PASS**；双方 stdout 均为 `2:ripgrep@15.1.0\n`（17 bytes），SHA-256 均为 `436f74d86dd8587cfdc3ccb85f9ae8d4b8f833e0ef03e6aca402a75371b9e2c7`；尚未 benchmark |
| `tokei_rust_files`，Rust 类型过滤、per-file 报告并按 code 排序 | 同一 tokei rev/lock | correctness **PASS**；双方 stdout 均为 3,705 bytes，SHA-256 均为 `8b08061b7b931f7a609306733022e1b9d83c334e0f29b8232e8a8e705613d27c`；尚未 benchmark |

三个最终 benchmark 共同使用 release mirvm sha256
`7b064b3f8861e39cfe07dd7df67583d73c0ec32bcd6108e12fb8e217c89da16e`。每次 bench 都重新通过
correctness gate，并在每轮 warmup/sample 复查 exit/stdout/stderr oracle；当前 case 不再携带旧 XFAIL。
三个已 benchmark workload 的 check/bench current views 均是上述 schema-3 内容寻址 evidence；其 harness SHA-256 为
`5192e8bff86748c9becaecc0d93825b3735ea4899394d068761e5841777bd0e1`，validator SHA-256 为
`aacf6362dcd9c18fcbb659f8658c7188d934b5a4216d7ebcd923fd01724cb35b`。
最终 Rust tests 为 **35/35**，harness 自身回归为 **65/65**，gate-truth 为 **12/12**；full gate5
为 **40 PASS / 2 XFAIL / 0 SKIP / 0 FAIL**（load 443 ms、rayon 927 ms），fmt、Clippy 与 release
build 也最终通过。这些仍是 Git-ignored workspace evidence，而不是远程入库的持续 gate。

原始 tokei case 对单个 `Cargo.toml` 的 JSON 输出仍是稳定切片。扩大到 `tests/data` 时，JSON
reports 会受并行收集次序影响；tokei 的 `--sort` 在 JSON early exit 之后才应用，不能稳定该输出。
因此广覆盖 case 选择可复现的 compact aggregate，而没有添加 normalizer。这是 workload/oracle
选择，不是声称 JSON 次序语义已经修复。

### 本轮 TDD 推进记录

1. ripgrep 从 `memchr::memmem::Searcher::find` 的零间接目标，最小化到 direct `dyn` 尾字段的
   运行期 alignment：sized prefix 后的字段 offset 必须按 vtable alignment 重新向上取整。
   `tests/fixtures/real_ripgrep_regex.rs` 已进入 `diff_cargo.sh`。
2. tokei 先暴露 i128/u128 `SwitchInt` discriminator 不能经 u64 scalar 截断，随后暴露
   `#[track_caller]` function item 转 fn pointer 必须使用 rustc Reify shim，最后暴露
   136-byte `MaybeUninit<ignore::walk::Message>` volatile store。三个最小回归分别是
   `demo/u128_switch.rs`、`demo/track_caller_fn_ptr.rs` 和 `demo/volatile_wide.rs`。
3. tokei 的第二个真实 workload 把输入扩大到 `tests/data` 的 206 个 tracked fixtures；不稳定的
   并行 JSON reports 没有用 normalizer 掩盖，而是改成上述稳定 compact aggregate oracle。
4. Cargo runner 正常返回曾在 rustc driver 收尾泄漏额外 warning summary；中间方案在 callback
   内直接退出又会跳过 `tcx.finish` 与迟发诊断，已被撤回。最终方案先完整完成 compiler 收尾，
   只结构化过滤 count-summary，再执行 VM；`cargo_warning_return` 还证明 guest 同文 stderr 被保留。
5. 最终 `diff.sh` 为 20 个 native differential cases，`diff_cargo.sh` 为 5 个；ripgrep 与
   tokei 的两个完整项目/三个 workload 均已经过本节所述最新 harness 的 correctness-gated benchmark。
### 更早的临时证据

hexyl v0.17.0（rev `8eb6d4771ce1ec7af65d06bd335457783b77d557`）曾从 `/tmp` clone
correctness PASS，3 样本 native median 约 51.0 ms、mirvm 约 1.570 s。它只保留为早期 harness
建设证据，不属于当前 ripgrep/tokei workspace working set。

当前 workspace artifacts 同样不能替代入库 manifest。未来恢复远程项目工作时，应先评审来源、
revision、lock hash、许可/维护策略与届时真实 PASS/XFAIL，再接入持续 gate。

## 6. 仍需诚实说明的限制

- harness 和产品当前都以 Linux/ELF/x86_64 为开发基线。
- bubblewrap 隔离用于限制写面和 check/build 网络，不是机密性边界；build/check 会 unshare network，
  所有 guest sandbox 还会 unshare PID，但**没有** unshare IPC，且仍可读取宿主的只读路径。它只适用于受信任且固定 provenance
  的 source，不是对抗性安全边界。dependency fetch 阶段允许网络，只有显式执行 `prepare` 才会发生。
- 两侧 checkout、HOME、TMP、XDG 和 target 的宿主物理目录仍彼此隔离，但 namespace 内统一映射到
  `/run/mirvm-project-side`，诊断路径统一 remap 到 `/mirvm-project`。这解决的是 harness 自造的
  随机路径噪声，不是通用路径 normalizer；程序主动读取宿主 `/proc` 等平台细节仍可能暴露差异。
- harness 自有 rustc flags 只能经 proxy 追加；不得改回覆盖 `CARGO_ENCODED_RUSTFLAGS`，否则会吞掉
  项目 `.cargo/config.toml` rustflags 并制造错误的 native baseline。2026-08-10 起
  `MIRVM_ENCODED_RUSTFLAGS_APPEND` 按内容分 Cargo target store，暖缓存改值不会复用旧
  fake binary；标准差分套件已覆盖该变化。
- 构建期会安装记录的 rustc 环境，执行 guest 前恢复调用者的 cwd 与完整运行环境；这已解决
  build env 污染 guest 的产品缺陷，但不代表 bubblewrap 两侧具备不同的权限模型。正式权限与资源
  隔离仍归 OS worker。
- 自定义 Cargo rustc/workspace wrapper 当前会 fail-closed；wrapper composition 是待实现兼容面，
  不能把拒绝写成 harness 或产品已支持 wrapper 链。
- rustc runner 的诊断 hook 仍是进程全局设施，但 compiler session 已由全局 guard 串行保护；
  这保证当前多 Engine 基础不会并发改写诊断槽。稳定嵌入 API 若要并行编译，仍需 rustc 提供更小的
  隔离边界或把编译移到独立进程。
- 所有当前 case manifest 仍位于 Git-ignored artifacts；新增 workload 没有把它们升级成入库持续
  gate。现有 harness 已冻结；suite inventory/集合身份不是当前下一步，除非未来某个真实产品 RED
  证明缺少它会使失败无法复现或结果无法判定。
- signal guest handler、guest backtrace/frame-IP、未支持 intrinsic/asm/链接形态及平台差异仍会
  按 [current-status.md](current-status.md) 的边界失败，不能由 harness 的存在推导为已支持。
