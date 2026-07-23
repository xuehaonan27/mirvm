# tests/parked/ —— 休眠基建存档

此处存放**当前不进任何 gate、也不进 CI** 的测试基建，保留备将来复用；
不是可执行文档，引用前先读本说明。

## real_projects.sh + real_projects_regression.sh + project_suite_evidence.py

2026-07-13 建的"真实 Cargo 项目 correctness/benchmark"重型 harness
（ripgrep/tokei 工作集，bwrap 隔离 + 内容寻址证据链）。合同文档：
[docs/real-projects.md](../../docs/real-projects.md)。

**休眠原因（2026-07-23 测试管线整顿实锤）**：

- case 文件与源码镜像位于 git-ignore 的 `artifacts/real-projects/`，仓内无一例；
  本机 artifacts 早已不存在，harness 实际零执行。
- 不进 CI（CI 只跑 gate_truth + gate.sh），自身却有 118K 的自回归脚本——
  基建超过产品，违反 AGENTS.md 基建预算纪律。
- 其"真项目对拍"职责已由更轻的 `corpus/projects/<名>/`（manifest mode=diff，
  native cargo run 三维对拍）接替。

**复活条件**：需要带 provenance 钉版 + 沙箱执行的真实项目证据链时（例如
对外发布兼容性声明），回此处；注意三脚本内的相对路径假设是 `tests/` 根，
复活时要么挪回要么改路径。彼时请先重读基建预算纪律：能跑当前真实负载并
产出可信判绿即冻结。

## 历史

- `tests/project_suite_rustc_proxy.sh` 不是休眠件——它是 diff_cargo.sh 的
  活动依赖，2026-07-23 挪至 `tests/fixtures/rustc_proxy.sh`。
