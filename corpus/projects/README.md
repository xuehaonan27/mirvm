# corpus/projects/ —— 真 cargo 项目对拍

此目录收编**真实世界的 cargo 项目**（编译产出二进制的那种），vendor 进仓，
在 tests/corpus.manifest 以 `mode=diff` 登记：tests/gate.sh 会把
`mirvm run <项目>` 与 `cargo run`（native）的 stdout/stderr/退出码三维
逐字节对拍（另有 L2 warm 复跑防缓存回放）。判绿口径与夹具路径占位符
（{ROOT}）见 tests/corpus.manifest 头注与 tests/lib.sh。

## 收编清单与 provenance 钉

| 项目 | 版本 | .crate 包 sha256 | 来源 |
|---|---|---|---|
| hexyl | 0.17.0 | e0df0c7b9afa020673c0942ae257a938b257d79e6395d8f1b8b45898f3391c5e | https://static.crates.io/crates/hexyl/hexyl-0.17.0.crate |
| tokei | 14.0.0 | e4de7875c0c312f30e090edb0da5df9f6586033132d36d94c8f9133e82826797 | https://static.crates.io/crates/tokei/tokei-14.0.0.crate |

每个项目目录 = 对应 .crate 包**完整解包**（含 Cargo.lock、LICENSE、
Cargo.toml.orig、.cargo_vcs_info.json 等；未增删任何文件），可用
`sha256sum` 校验下载包后 `tar xzf` 重建。许可证在各目录内
（hexyl：MIT/Apache-2.0；tokei：MIT/Apache-2.0）。

## 为什么选它们

- hexyl（小）：纯 Rust hex 查看器，anyhow/clap/termcolor 依赖树——
  最小真实项目形态，先把 diff 对拍管线跑通。
- tokei（中）：真实 build.rs（tera 模板生成 language_type.rs）+
  grep-searcher/ignore/dashmap/crossbeam 等真实依赖树——
  专门压 cargo_shim 的 build.rs 调度与中型项目装载路径。

## 升级/新增纪律

1. 新版本：下载 .crate → 校验 sha256 → 全量替换目录 → 更新上表 →
   跑 `CORPUS_PROGS=<名> bash tests/gate.sh` 验证三维一致。
2. 新项目：先确认输出对固定输入逐字节确定（不定时不收编；
   不得用文本规范化掩盖非确定性）→ vendor → manifest 登记
   `mode=diff`（tier 先 full，撞红按欠账流程处理）→ 本表补行。
3. 运行期联网、依赖机侧绝对路径前缀的项目不收编（needs= 只覆盖
   既有手工批条目，不扩编）。
