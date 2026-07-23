# corpus 驱动补全与三维差分扩编

> 本文是 corpus（真实生态 crate 最小驱动）的总台账。
> **§0–§4 = tier-0 时代（M2.5，2026-07-05）调研票据归档**——五张设计票据已全部兑现，
> 逐 crate 过程细节见 git 历史（2026-07-18 文档精简时压缩为一屏）；
> **§5 = 三维逐字节差分时代的当前活台账**（批1–11；条目唯一真源 =
> `tests/corpus.manifest`，164 脚本驱动 + 2 vendored 真 cargo 项目）；
> **§6–§7 = 已投放清单与后续候选池**。
> 当前可信边界见 [current-status.md](current-status.md)；撞出的未解决债务全部登记在
> [open-issues.md](open-issues.md)。

## 0. corpus 是什么

corpus = 一组**真实生态 crate 的最小驱动程序**（`corpus/c_*.rs`）外加
**vendored 真 cargo 项目**（`corpus/projects/<名>/`，批11 起），用来发现真实
代码对抽象机 / VM 边界提出的要求。**条目唯一真源 = `tests/corpus.manifest`**
（tier/timeout/mode/env/needs 全在此；新增 driver 必须先登记）。跑法：
`bash tests/corpus.sh`（全量三层）、`bash tests/corpus.sh --tier smoke`（按层）、
`bash tests/corpus.sh <name>...`（按名子集）；release 二进制，stdout/stderr 落
`/tmp/corpus-out/<name>.{out,err}`。三维差分纪律与验收食谱见 §5 头注与
[agents/onboarding.md](agents/onboarding.md)；gate 内判绿三口径
（exit / oracle / diff）见 manifest 头注。

## 1–4. tier-0 时代票据（M2.5 归档，2026-07-05）

tier-0（rustc `InterpCx` + 协作调度）上跑了五批 22 crate + 3 定向探针（14 通过），
逼出五张设计票据。**五张已全部兑现**（tier-0 已于 2026-07-09 删除）：

| 票据 | 一句话 | 兑现处 |
|---|---|---|
| §2.1 协作调度 vs 真阻塞 syscall | 阻塞释放依赖另一 guest 线程即死（net_echo_threaded 挂死实锤）；阻塞式线程服务器是主流模式 | M4.4 真 1:1 线程 |
| §2.2 inline asm 三面孔（cpuid / rustix 裸 syscall / num-bigint 算术原语） | asm 本身就是机器码，直接 JIT（B 决策）；虚拟 CPU = 真宿主 CPU | M5.0 asm-stub 工厂 + M5.1/M5.2 补面 |
| §2.3 dlsym 直通边界 | 默认直通 + denylist 形状可行；**rustix 裸 syscall 无符号可拦，与 os:: 收口的张力至今成立**（→ [open-issues.md](open-issues.md) E19） | M4 denylist；signal/回调经 M4.4 thunk 转正 |
| §2.4 协作调度性能（rayon 28s） | 协作只能作对拍基底，性能要真线程 + JIT | M5.3 方法级 JIT |
| §2.5 InterpCx 检查器 overlay 误报（walkdir/process/mmap 三实例） | native 真地址指针算术被判 UB——tier-0 第一号阻塞 | M4 clean-slate 甩掉 AllocId overlay |
| §2.6 fork/clone 处置 | denylist 非围栏（asm-JIT 落地后可绕）；终局 = 三分支持（fork+exec / 单线程 / 多线程） | M5.2 D8f 单线程放行；多线程仍拒（open-issues R2） |

架构确认：async-stackless 成立（tokio current_thread/multi_thread 跑通）；真 OS 原语
直通可用（epoll/eventfd/socket/文件 IO 与 native 逐字节一致）；unsafe/布局/别名纯计算
正确。选型过程与逐 crate 过程细节完整保留在 git 历史（2026-07-18 之前的本文版本）。

## 5. 真实项目三维差分扩编（2026-07-15；批1 13 个 + 批2 14 个）

> 背景：M5.4b 收尾期一个错值级 miscompile（analyze_frame 只记基址漏区间）在合成
> 门禁全绿下潜伏了整片 M5.4a，最终由真实项目（regex capture drop 链）炸出——
> 用户据此裁定 corpus 从"exit-code 冒烟"升级为 **三维逐字节差分**：
> **mirvm 默认 / native cargo run / MIRVM_JIT_THRESHOLD=1**，stdout/stderr/exit
> 全部逐字节一致才算绿（driver 确定性纪律：定种、BTree 序、浮点 to_bits、stderr 真空）。
> 复跑法：每个 driver 在 `corpus/c_*.rs`，三维命令见其文件头注释与
> [history/m5-log.md](history/m5-log.md) M5.4b 节。

### 批1（13 个；10 绿 / 2 FRONTIER / 1 路径探针）

- **绿**：serde_json（Pair 返回密集迭代器）、serde_yaml（unsafe-libyaml 纯 Rust
  移植）、rand_det（rand 0.9 改名 API）、flate2（miniz_oxide raw deflate；
  gz/zlib 原生 API 撞下述 FRONTIER 改手工容器等价覆盖；**psad.bw/pclmulqdq 已内建，
  原生 API 已于 2026-07-18 回摘回归（open-issues G4 关闭，§7.9）**）、brotli、argon2
  （内存硬）、ed25519（dalek u128 域算术；用官方 serial backend 绕下述
  avx512ifma）、p256（RFC6979 定向量锚点）、syn_parse（递归类型 + drop glue 重）、
  hickory（DNS codec；ring 撞 bug② 后的替换项）、unicode_tables（大表四件套）。
- **FRONTIER（锁定 expected-red）**：c_aes_gcm（`llvm.x86.aesni.*`/`pclmulqdq.*`
  未内建，aes/ghash 运行期探测无 force-soft 退路）、c_png_round
  （`llvm.x86.avx2.psad.bw` 未内建，simd-adler32/fdeflate 处处必经）。
- **c_serde_json**：三维对拍机制的路径探针（materialize → script_dir → cargo run -q）。

### 批2（14 个；全绿，2 个 FRONTIER 绕行记录）

- wasmi（**VM-in-VM**：wat 模块调用/memory/global/宿主回调/trap 四类）；
  boa_js（纯 Rust JS 引擎大物：45 片段全语义面；JIT 队列 5562 函数/发布 1862，
  输出仍逐字节一致——语义零依赖 JIT 的实证）；
  tiny_skia（标量路径 2D 光栅化 32 轮，像素 FNV hash 三路一致——浮点重场景
  JIT 与解释器无分歧）；zip_arch（Stored+deflate；crc32fast **≥128B 单块**必撞
  pclmulqdq，64B 分块合法绕行；实证 zip deflate 走 raw 不碰 simd-adler32）；
  rust_decimal（96 位定点）；rustfft（标量路径全绿；默认 avx 撞
  `llvm.x86.avx2.gather.q.pd.256` = FRONTIER，且运行期探测致两路径 1-ulp 分叉
  对拍本无意义，钉 default-features=false）；roaring / bitvec（指针打包别名边界）/
  compact_str（niche 24B 内联临界）；nom_parse / comrak_md（全扩展 CommonMark）/
  fst_build（自动机）；jieba_cut（钉 =0.10.0：0.10.2 的 bytecount 依赖撞
  `llvm.x86.sse2.psad.bw`；另避 jieba-macros 0.10.1 semver 破洞）；
  spade_delaunay（robust 精确谓词：共圆精确零 / 1e-13 近共线 / 1ulp 扰动
  全逐比特一致）。

### 扩编撞出的两个产品 bug（均已修复）

- **bug① 缓存污染**（`718dac5`）：A2 split 的 fn_addrs 按【值域】分拆，S4 补建
  条目（底座 fn 在 deps 降低期于 image 冻结区补建 fn 条目）被留在建者 delta——
  消费方装载同一 image 后其静态烘焙的补建地址在运行期反查表无登记 → 间接调用
  abort「不是已知 fn 条目」（三个 driver 独立撞见；负对照 edit_rand v2-v6 五连崩
  同址 0x6a0000001630/core::fmt::write）。**修复 = 按【地址域】分拆**；负对照
  45 跑 5 崩 → 修复后 45 跑 0 崩。
- **bug② ring 整 crate lower panic**（`cb09b5b`）：fn 体内 extern fn item 作
  fn 指针实参 → 取址路径不判 `is_foreign_item` 直取 optimized_mir → rustc query
  panic。**修复 = foreign_fn_entry_addr（native 链接器语义真符号地址）+
  elfsym.rs（.symtab 兜底，ring 的 -fvisibility=hidden 归档符号）**。ring
  SHA-256 三向量与 native 逐字节一致。

### M5.x intrinsic 内建欠账队列（**七族已于 2026-07-15 全清**，`2b4766b`）

> 核销记录：psad.bw 家族（sse2/avx2 + fallout maddubs/madd 四兄弟）、pclmulqdq、
> aesni（aeskeygenassist 为软件 S-box 模型）、sse42.crc32 四宽、avx2.permd、
> avx2.gather（q.pd.256 + d.pd.256，mask fault-suppression 单测）、avx512
> vpmadd52l/h 六宽（软件 u128 模型）——26 符号内建，六个 expected-red 全部
> XPASS 翻绿（png_round/gix_pure/calamine_xlsx/aes_gcm/lz4_snap + snow 摘
> RUSTFLAGS），gate5 89 pass/1 expected-red（仅剩 rusqlite 的 libm 闭包非
> intrinsic 红）。cargo test 63/63（+9 个 helper 单测）。剩余未内建兄弟
> （pclmulqdq.256/.512、vaes、其余 gather 形态、avx512.pmadd 系）遇 workload
> 按同四触点法补。下表为原始欠账（留档）：


| intrinsic | 撞它的真实 crate |
|---|---|
| `llvm.x86.avx2.psad.bw` / `llvm.x86.sse2.psad.bw`（`_mm(256)_sad_epu8`） | png/fdeflate、simd-adler32（→flate2 gz/zlib）、jieba-rs 0.10.2 bytecount |
| `llvm.x86.pclmulqdq.*` | crc32fast ≥128B 单块（→zip/flate2 gz）、aes-gcm 的 ghash/polyval |
| `llvm.x86.aesni.*` | aes/aes-gcm（运行期探测无 force-soft） |
| `llvm.x86.avx512.vpmadd52*`（IFMA） | curve25519-dalek 默认 simd backend |
| `llvm.x86.avx2.gather.*` | rustfft 默认 avx（GoodThomas/Rader 路径） |

共性机理：guest cpuid 直通宿主 → 运行期派发选中硬件路径 → 未内建 intrinsic 降
Trap。处理口径：内建进 lower 的 llvm.x86 内建表（M5.2 的 pshufb/sha256 先例；
属 M5.4d SIMD 或独立 M5.x 片）。内建一个解锁一片真实 crate（png/jieba-0.10.2/
flate2 原生容器/crc32fast 整块/aes-gcm/dalek 默认路径/rustfft-avx）。

### gate 接线（2026-07-15）

- gate5 corpus 段扩到 57 个程序（新增 24 绿 + aes_gcm/png_round 双 expected-red
  ——red_pattern 锁定诊断，内建后 XPASS 强制转绿；ed25519 段内注入官方
  serial-backend env）。jieba_cut 全绿但单跑 77-89s 贴 timeout，留 corpus.sh。
- corpus.sh 默认清单同步扩编（timeout 600 容纳 jieba）。
- 三维逐字节差分在 driver 创建时强制执行；gate 内为 exit-code + oracle 级
  （native 逐字节维的冷构建成本不进 gate）。

### 批3（14 个；8 绿 / 4 FRONTIER / 2 产品 bug 实锤）

- **绿**：smoltcp_tcp（纯 Rust TCP/IP 栈：loopback echo 全状态机 + 手工时钟 +
  codec 畸形 13 条）、snow_noise（Noise_XX/NKpsk0 ChaChaPoly 全 transcript 锚定；
  poly1305 avx2 撞 `llvm.x86.avx2.permd` 用官方 `--cfg poly1305_force_soft` 绕——
  与 ed25519 serial env 同型先例；curve25519-dalek 默认 simd backend 全程安然）、
  statrs_stats（0.18 分布族/检验全 bits 锚）、rkyv_zero（零拷贝 + bytecheck
  校验路径；validator 错误文案内嵌裸地址，归类打印）、qr_round（qrcode+rqrr
  闭环，RS 纠错 5×5 翻转仍解出）、fatfs_img（钉 =0.3.6——0.4 从未发布；
  chrono 壁钟炸弹用 default-features=false + 固定 TimeProvider 拆）、
  geo_ops（0.29 robust/i_overlay 谓词全 bits 一致）、rhai_script（meta 解释器：
  闭包/宿主注册/11 种错误变体/资源上限；ahash runtime-rng 不打印哈希序集合）。
- **FRONTIER（锁定 expected-red）**：gix_pure（git loose object 必经 zlib →
  simd-adler32 `sse2.psad.bw`；三条 zlib 路线排查全记录）、lz4_snap（snap frame
  层 crc32c 撞 `llvm.x86.sse42.crc32.*` 新族；lz4_flex 全线 + snap raw 绿）、
  calamine_xlsx（rust_xlsxwriter/calamine 双 crate 经 zip entry CRC 撞
  crc32fast ≥128B 单块 pclmulqdq，块长封死在 crate 内部不可绕）、
  rusqlite_db（native_archive 闭包策略：libsqlite3.a 的 FTS5 引 libm `log`，
  `-z defs` 整档链接拒——闭包检查未计 libm；exit 101，red_code 机制因此
  从写死 70 扩为按程序可配）。**已修（本轮）**：LINK_SUFFIX 纳入 std 经
  `#[link]` 恒给 guest 最终链接的系统库集（m/dl/pthread/rt/util/gcc_s，
  落 DT_NEEDED 由宿主解析），rusqlite_db 三维翻绿——gate5 103 pass/
  **0 expected-red**。
- **产品 bug 实锤**：
  - **track_caller fn_span**（`7dc3b31` 已修）：方法调用点 Location 取
    整个调用表达式 span（lo=接收者）而非 rustc 的 fn_span（被调名段）——
    redb TableAlreadyOpen 错误串行列分叉实锤（269:18 vs 269:20；链式多行
    连行号都偏）。凡方法调用点 unwrap/expect 的 panic 头全偏。修复后
    redb_kv 三维转绿并入 gate。
  - **zstd 静默换库**（已修，待主线收录提交）：hidden-visibility 归档（.dynsym
    空）符号解析 dlsym(RTLD_DEFAULT) 优先于归档 .symtab 兜底 → guest zstd 被
    绑到宿主 libLLVM 内嵌 zstd（dfast 策略 level 3/4 输出不同，len=915 vs 918）；
    reject_symbol_ambiguity 用 nm --dynamic 对空导出表失效。**native 链接器
    语义：静态归档成员的定义在链接期绑定，guest 自己的库永远赢过全局
    命名空间**——修法 = 兜底表只收 .symtab−.dynsym 的 hidden 符号并在三处
    解析点（FfiState::resolve / fn-ptr 取址 / extern static）先于 dlsym 全域
    查询；dynsym 可见面维持原序（物化期碰撞拒绝仍兜底）。修复后 c_zstd_stream
    三维逐字节转绿，ffi_zlib/blake3/ring 回归无损。
- **另发现（欠账类，未立项）**：①**thunk 盲区**（**已根治：P1 条目可执行化
  `4202317`，decision-history §7.6**）——flate2 的 C-libz 后端把
  Rust allocator fn-ptr（zalloc/zfree）嵌进 z_stream **结构体**传给 libz，
  libz 回调时宿主跳进 guest 数据地址静默 SIGSEGV 无诊断（thunk 机制只覆盖
  显式 fn-ptr 实参，结构体内嵌回调是盲区；LD_PRELOAD 实锤 si_addr==rip 落在
  delta 冻结域 rw 非可执行）。②**rusqlite 的 libm 闭包缺口**（见上
  FRONTIER——闭包检查应纳入 -lm 或白名单系统库进 DT_NEEDED）。

**M5.x intrinsic 欠账队列追加**（按批3 证据）：`llvm.x86.sse42.crc32.*`
（snap frame、任意 crc32c 用户——实现成本低，单指令语义）、
`llvm.x86.avx2.permd`（poly1305 avx2；一行 shuffle 语义）。

### 批4（13 个；全三维绿；新内建首战 + 大物偏门）

- **新内建首战**：crc32fast（pclmulqdq 整块谱系 1B-1MB，cbf43926 对拍）、
  chacha_poly（XChaCha20/ChaCha20-Poly1305 定向量+篡改失败例；poly1305 avx2
  走新内建 permd 无需 env）。
- **大数 crypto**：k256_ecdsa（secp256k1 RFC6979 定向量+ECDH）、rsa_pss（固定
  p/q/n/d 组件重建 RsaPrivateKey 绕生成 rng；PKCS1v15 定向量+加解密往返；
  JIT=1 维 335s = 大数 u128 JIT 压力实测）。
- **VM-in-VM 第三弹**：revm_evm（EVM 解释器：PUSH/ADD/SSTORE/LOG 固定字节码
  合约执行+gas/存储变更锚定）。
- **TLS 无网络面**：rustls_cert（内嵌 PEM：解析+config 构建+verifier 正反例）。
- **格式/几何/DS**：libflate_zlib（jieba 词典同款）、lyon_tess（贝塞尔/圆弧/
  自交 path 镶嵌顶点 bits 谱）、midly_midi（SMF 闭环+变长量）、qoi_img
  （QOI 逐像素）、kdl_doc（KDL 树往返）、jaq_jq（纯 Rust jq 查询族）、
  ds_obscure（hyperloglog+succinct）。
- **rpgp_sign 弃**：OpenPGP API 高阻（pgp 0.14 改名+密钥格式折腾，swarm 中断
  后主线限时一击未成）；下批可以 packet 解析面（从 armored 公钥打印字段）重试。
- 接线：gate5 102 pass/1 expected-red（rsa_pss tmo=400 记大数 JIT 压力；
  revm/rustls 冷构建宽限）。

### 批5（13 个；12 绿 / 1 DIFF 实锤（已修）/ 1 FRONTIER）

- **绿**：pgp_packet（OpenPGP packet 解析面；armor 体层错误绕开 M4.2 dyn 上溯欠账）、
  zopfli_deep（重计算 JIT 压力；尺寸按 8 分钟预算缩减链记录）、simd_json（显式
  SIMD：pshufb 族+通用 simd 已覆盖，两维同选 avx2 实现——cpuid 派发锚定行作
  假绿警报）、symphonia_wav（hound→symphonia PCM 逐样本+f64 sin 合成逐位一致）、
  pdf_pair（printpdf+lopdf 闭环；壁钟头/假随机 xorshift/profile 敏感三坑记录）、
  deunicode_slug（Inflector 0.11 怪癖锚点 fish→fishes/data→Daum）、malachite_big
  （256/512/1024-bit 大数谱系）、arkworks_ff（BLS12-381：域塔/Fq12/pairing
  576B 指纹）、im_persistent（10k 深共享 drop glue）、zxcvbn_pass（**上游
  exact-tie nondeterminism 实证**：scoring.rs 对 u64::MAX 饱和并列取 HashMap
  迭代序——native 自对拍都不稳，三维恰三进程同种子纯属侥幸，已离饱和区）、
  barcoders_gen（9 种 1D 条码）、bzip2_pure（0.6 纯 Rust libbz2-rs-sys 后端，
  与系统 C bzip2 逐字节一致）。
- **DIFF 实锤（已修 fffd462）**：c_fixed_point——16 字节 niche tag 截断 W64
  判别（NonZero<u128> niche_start=0 时 lo=0 的合法大值误判进 niche 返回
  None）→ fixed U64F64::sqrt 整数输入（lo 恒 0）静默产 0。修复 = 16 字节
  niche tag 一律 u128 全宽 wrapping 判别（cg_ssa operand.rs 同构）。同 driver
  另压出三个 128 位族 Trap 欠账（Cmp/cast/Neg/saturating，见批5 修记）。
- **FRONTIER（既定：`17665dc` 已修转绿）**：c_openssl_evp——rlib 元数据 -l 传播缺口
  两撞同源：①cargo 把 openssl-sys build.rs 的 rustc-link-lib=ssl/crypto 只写
  元数据，bin rustc 命令行无 -l/-L，mirvm 的 dlopen 候选只读 sess.opts.libs
  → 库从未进全域（driver 层显式 dlopen 合法绕行）；②绕行后 lower 期
  EVP_EncryptInit_ex 按值取址烘焙仍 miss（运行期预载够不着 lower 期 dlsym）。
  **已修**：排干 worklist 前收集全图 tcx.native_libraries 的系统动态链接类
  （Dylib/RawDylib/Unspecified/Static{bundle:false}），soname_candidates
  （dev 符号链+ldconfig -p 版本项）RTLD_GLOBAL 尽力预载 + 同清单移交
  module.native_libs。cargo 的机制实证：-sys build.rs 的 cargo:rustc-link-lib
  只进该 crate 自己的 rustc 行（-l ssl -l crypto），经 rlib 元数据由 final
  link 全图收集——bin 的 rustc 行恒无 -l。
- **闭包欠账新形态**：bzip2-sys——vendored BZ_NO_STDIO 的断言桩 bz_internal_error
  定义在其 Rust rlib（#[no_mangle]）里：native_archive 闭包检查覆盖不到
  「符号在 rlib」形态（记档；driver 走 0.6 纯 Rust 后端）。**已闭合
  （2026-07-18，C2 救援链，decision-history §7.11）**——c_bzip2_csys（同
  vendored C 后端）三维转绿入 gate。

### 批6（14 个；13 绿 / 1 实锤→fb0b204 修；2026-07-16；`3062184`）

- **绿**：tokenizers_hf（BPE/Unigram 手工 vocab，fancy-regex 绕行上游 onig 破洞）、
  jiff_time（内置 tz 大表）、exr_image（OpenEXR 八档压缩 roundtrip 含 PXR24/B44
  有损面逐 channel fnv 锚；half 钉 =2.2.1 绕 F16C 运行期探测未内建 vcvtps2ph；
  **vcvtps2ph 已内建，钉已于 2026-07-18 摘除（open-issues G4 关闭，§7.9）**）、
  nalgebra_la（LU/QR/SVD/Cholesky/eigenvalues 全 bits）、h3_hex（Uber H3 全 API）、
  faer_lu（**2026-07-23 起复原默认特性**：pulp V3 LD_ST 544 例程真走汇编路径，
  C4 闭合（decision-history §7.23——dep 编译期 global_asm 抽取 + bin 同通道装载，
  native/默认/SYNC 三维逐字节一致；曾 default-features=false 绕行））、
  stemmers_multi、whatlang_detect、gluesql_db（17 类型/join/聚合/错误路径；
  钉 bigdecimal =0.4.5 绕上游破洞）、plotters_chart（SVG 全文+bitmap FNV）、
  fastfloat_ryu（2.2250738585072011e-308 等经典边界 bits）、comfy_table_render。
- **实锤（已修 fb0b204）**：c_polars_frame——guest 侧 psm（stacker）dynsym 导出
  `rust_psm_on_stack` 与宿主 librustc_driver 内嵌 psm 撞车，被
  reject_symbol_ambiguity 按设计拒（此前 hidden 类已由 056b212 修；本次 = dynsym
  可见碰撞的 native 链接期绑定语义决议）：修 = **dynsym 归档句柄优先于
  RTLD_DEFAULT 解析**（guest 链进的对象恒胜宿主同名库）+ InvalidEnumConstruction
  assert(u128)。修复即三维转绿入册。

### 批7（24 个三波激进扩编；23 全绿可用 / 1 FRONTIER 记档；修出 2 只产品 bug；2026-07-17）

- **波1（12，纯 Rust 轻中型；全绿）**：wat_parse（wat/wast/wasmprinter 三件套
  往返）、jsonschema（default-features=false 避 aws-lc-sys 198 闭包）、
  html5ever（0.29.2 yank 实锤改 0.29.1）、xml_rs、markdown_it（syntect 裁、
  linkify+全插件）、logos_lex、chumsky_parse（psm 分段栈无撞）、ndarray
  （matrixmultiply x86 微内核位级闭合，风险面核销）、smartcore、rune
  （0.13.4 宿主回调；JIT 8830/2415 过队仍一致）、koto（0.15.3 拷贝捕获
  语义锚）、opencc（**FFI 条目非纯 Rust**——原清单分类误差；机器侧
  /tmp/opencc-local 前缀，三维带 env 全绿，留 corpus.sh 手工批有 gating）。
- **波2（10，中型/FFI/边界；7 绿 + 3 红分诊）**——绿：parquet2_rw（thrift 系
  本就零依赖实锤）、syntect_fancy（默认语法集无 TOML→YAML 同角色替代）、
  phonenumber（2.2MB 元数据 postcard 热面）、orgmode（organic 0.1.16；
  org-rs 未发布）、oxc_parse（0.140.0 大物槽 62 闭包温跑 1s）、rsa_4096
  （固定 PEM+v1.5 确定性签名）、ed25519_default（**不设 env 默认 simd
  backend 直跑通过——批1 serial 绕行核销**）。
- **修出两只产品 bug（均当日修复入册）**：
  ① **native-archive 链接行缺 crate 图动态库**（`867b3de`）——`-sys` 的
  cargo:rustc-link-lib 只写 rlib 元数据，libgit2.a 的 CRC32/deflate 等 17 处
  undefined（libz-sys.stock-zlib 动态模式）。修 = system_dylibs(tcx) 统一收集
  （与 lower RTLD_GLOBAL 预载同名单）+ `-l<name>` 入链接行与缓存键——c_libgit2
  红转绿。
  ② **custom #[global_allocator] 致 `__rust_*` 跨堆撕裂**（decision-history
  §7.7）——分配按 lower 会话路由（base/deps image 烘 CallBuiltin→引擎堆，
  delta/image 走 AST 展开器 guest shim→用户分配器），两堆互穿 free =
  mimalloc 元数据 SIGSEGV（c_mimalloc 两镜像实例实锤：退出段 stdout 缓冲、
  Vec<String> 6144B 末档；另有 shim FuncId 漏 A2 rebase 的实现自伤一记）。
  修 = kind=Global 时登记 shim 四件套，interp CallBuiltin(Rust*) 臂运行期
  统一路由——c_mimalloc 三维转绿（线程相位 53110 calls 无分歧）。
- **FRONTIER（**已转正（2026-07-18，C1 闭合 `见 decision-history §7.10`）**）**：c_tree_sitter——FFI **按值聚合**封送
  （TSInput/TSNode/TSPoint）系统性缺席：ffi_kind_of 只收标量的既定边界，
  转正需 Aggregate 类 + System V 拆分 + thunk 方向按值读写，按 workload
  优先级立项面；driver 头注/B 维 15 行 oracle 已固定。
- **波3（加测；全绿）**：rustls_shake（rcgen 定种子 Ed25519 自签 +
  rustls 0.23 ring TLS1.3 双手真握手，证书 DER FNV 硬锚）、zstd_long
  （zstdmt 真线程面，16MiB 混合数据 L1/9/19+MT 各档指纹，ZSTDMT 行跨
  进程稳定）。
- gate5 128→**139**，corpus 段批7 共 +22（opencc 留手工批；tree_sitter
  未接线待按值聚合转正）。三维铁律全程零例外放行。

### 批8 波1（2026-07-17，重型 FFI/C 5 个；全绿可用：4 直接绿 / aws_lc 撞出已修复锁系列）

- **绿**：mlua_lua（vendored lua54：跨 thunk longjmp 存活、lua_pushcclosure
  与 __gc 销毁回调全通道——rocksdb 因 bindgen/libclang 缺席判不可的补位）、
  tantivy（0.26.1 mmap 全特性：同时压出 **lddqu 两符号欠账（已修 a3a8d7e）**，
  FAST 列面原绕行 InvertedIndexRangeQuery）、sequoia_pgp（2.4.1 crypto-rust
  238 闭包；手工拼无盐 v4 签绕上游签名注记随机器）、sqlx_sqlite（sqlx+tokio
  异步执行器 worker 线程通道 + bundled C sqlite 8.4.6）、aws_lc（1.17.1：
  SHA/HMAC/HKDF/GCM/Ed25519/RSA 六族定向量）。
- **撞出并当日修复的锁系列（decision-history §7.8）**：① constructor 分治
  （lifecycle 全拒收窄为仅拒旧式 `.init`/`.fini` 裸注入段——aws-lc 的
  do_library_init 与 mimalloc 的 mi_process_attach 两实锤后，DT_INIT 语义
  判与 native constructor 同构）；② `#[link_name = "\u{1}..."]` LLVM 的
  `\x01` verbatim 前缀统一剥除（aws-lc-sys BORINGSSL_PREFIX 全符号家族）；
  ③ P2 GOT 键名去重盲点同剥（带前缀家族的 fn-ptr 常量原掉回烤 Imm——
  跨进程腐旧地址 Heisenberg，EVP_AEAD 入口实锤）。全部修复后 c_aws_lc
  冷/热×3 逐字节一致。
- gate5 139→**144**（aws_lc/mlua_lua/tantivy/sequoia_pgp/sqlx_sqlite 入册；
  tmo 对 aws_lc/tantivy/sequoia_pgp 放宽 300）。

### 批8 波2（2026-07-17，VM/语言机/大物 5 个；4 绿 / 1 expected-red 记档；修出 1 JIT bug）

- **绿**：swc_parse（oxc 姊妹压强：swc 41.x 手写递归下降 + serde JSON
  census + 错误模型差异记录）、miden_exec（0.25.5 **execute-only**：绕开
  umbrella 内嵌的 prover——直用 miden-assembly+processor；MASM 三程序
  含 dyncall 摘要注入的递归逃逸形态；trace 矩阵 ≠ 证明）、polodb
  （polodb_core 3.5.2：明确勘破上游 base update 泄漏事务语义并以打印
  立据）、starlark_eval（0.13.0——**撞出 JIT analyze_frame 帧末 ZST 取址
  必爆之雷**（→ `dc6e30c` 修复，C 维由红转绿）；钉 allocative=0.3.4 绕
  上游 hashbrown semver 破洞）。
- **expected-red 记档（债 [open-issues.md](open-issues.md) C2/C3）**：c_wasmtime_wat——双层欠账：
  ① libwasmtime-helpers 蹦床调 `#[export_name]` Rust 符号（native
  final-link 从 rlib 集符；`-z defs` 单闭包够不着——批5 bzip2-sys 同族
  的第二实例）；② trap 上抛 inline asm `noreturn`（探针实锤主体路径：
  cranelift 在讲解进程里发机器码并执行——模块编译/实例化/内存表宿主
  回调/rayon 全通零分歧）。**双层均已闭合（2026-07-18）**：层① C2 救援链
  （§7.11）、层② noreturn 两面孔物化（§7.12，含一次自我反转）——
  **driver 由 expected-red 转绿**（全 trap 面三维确定性绿）。
- gate5 144→**148**（swc_parse/miden_exec/polodb/starlark_eval 入册）。

### 批9（8 个偏门小中型；8/8 全绿；修出 1 JIT bug；2026-07-18）

- **绿**：zune_jpeg（zune-jpeg 0.4.21 解码 + image/jpeg-encoder 0.7 编码矩阵：
  baseline 4:2:2×2、progressive 4:2:0 RI=99/4:4:4、sequential RI=5 真实重启；
  **zune-jpeg 0.4.21 progressive+重启触发的上游破洞**以 RI=99 绕行并洞面如实
  打印记档，0.5.15 同款正常；observed 上游解码线 0.4↔0.5 在 4:2:0 帧差 48
  像素的语义数据点非红）、candle_mlp（candle-core 0.9.2 mini MLP 硬编码
  weights；CPU matmul 无条件走 gemm→pulp 标量面，faer LD_ST 类 global_asm
  风险面实证不存在——复红定因参照在头注）、pest（2.8.7 双 grammar：JSON 子集
  递归 + 计算器；先序遍历节点序/求值/三错误位置 line:col+expected 锚定）、
  scraper_dom（scraper 0.27：多档选择器命中序/text 串联/attrs/classes 排序
  语义/outer-html FNV/三选择器错误路径）、arrow_rs（arrow 59.1 大物：五类型
  RecordBatch（含 List\<i32\>/Struct/null 三面）+ filter/take/sort_to_indices/
  sum-min-max 内核 + IPC stream 写读闭环 batch_eq=true）、fontdue（0.9.3 +
  `/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf` 绝对路径双维同文件：
  392 glyph metrics/bitmap FNV；CJK miss 探测如实打印；simd feature 关走官方
  simd_core 标量后备——tiny-skia 同族官方后端开关先例）、unicode_rs
  （unicode-segmentation 1.13.3 + normalization 0.1.25，Unicode 17 表：
  grapheme 16 串/word 双遍 9 串/sentence 6 串/四形式 13 串/流式等价三面/
  未分配码点专测）、rustpython_mini（rustpython-vm 0.5.0 嵌入式裸解释器五
  程序；**install_signal_handlers=False 官方开关**绕 R1 故障信号面（_signal
  模块 genesis 期对 SIGBUS 重装撞 guest std stack_overflow handler），头注
  全链；构建 230 包 1m08s 冷，预算内）。
- **修出 1 个 JIT bug（当日修复 `68829ad`）**：analyze_frame **0 字节帧
  ZST 取址**——mem::drop::<ZST 自定义 Drop> 的 glue 实参 = `AddrOf(Local(0))`，
  fsz=0 处批8 `dc6e30c` 帧末锚 `checked_sub(1)` 无处落、frame_ss 缺席 →
  「取址 offset 必落帧」断言爆（C 维 stderr 污染，语义由解释兜底未受损）。
  修复 = FrameMap 增 force 档 + define_fast 以 1 字节尺寸物化 0 字节帧
  （批8 starlark 帧末 ZST 同族第三形态）；demo/zst_drop.rs 回归入册，
  c_rustpython_mini 三维复绿。
- gate5 148→**156**（8 driver 全绿入册；arrow_rs/rustpython_mini tmo=300、
  candle_mlp tmo=180）。

### 批10 波1（2026-07-18，重/大物 4；1 直接绿 / 3 红→当日同修转正全绿）

- **直接绿**：c_miden_prove（miden prover 面：定值 MASM 出证明 + verify 布尔，
  批8 execute 的接棒；gate 接线随批10 波2 总收口）。
- **红三同修（当日修复转正，三维逐字节）**：
  ① c_risc0_run（risc0 zkVM execute-only，journal fnv=fbb47fc52544af18）——
  撞**跨归档 weak/COMDAT 碰撞**：`reject_symbol_ambiguity` 原判重名即拒，
  修 = 纳入 nm posix 类型字母（W/w/V/v/u 首件胜出全放行、恰一 strong 放行、
  双 strong 仍拒；GNU unique `_ZGVZ*` 族与 weak object `V` 在覆盖），
  单测 `duplicate_weak_symbols_follow_native_link_semantics`。
  ② c_typst_pdf（typst 编译 3 页 PDF，bytes=24770 fnv=a07292af73881d72）——
  撞 **asm-stub 缺 xmm 16B 槽**（`__m128i` 按值过 asm ins/outs）：修 =
  `AsmIoVal`/`AsmIoDst`（Scalar + VecBytes(PlaceExpr,u32)），
  Terminator::InlineAsm ins/outs 改型，lower/interp/jit analyze_frame 四面
  同步。同 driver 又撞 C5（③）双供养。
  ③ c_datafusion_sql（datafusion SessionContext 内存表 + SQL 七查询 +
  plan，95 行逐字节）——撞 **C5 dyn 上溯 vtable 变换**（open-issues C5，
  M4.2 欠账转正闭合，decision-history §7.13）：`Arc<dyn Source> →
  Arc<dyn Any+Send+Sync>` 类 principal 变换走 `dyn_unsize_tails` 统一递归
  判据（含解引用落点 lockstep 再判——Arc→NonNull→Pat→*const ArcInner→data
  全链）+ `supertrait_vtable_slot` chase；合成探针 demo/dyn_upcast_probe.rs
  两形入 diff.sh。
- gate5 159→**162**（risc0_run/typst_pdf tmo=300、datafusion_sql tmo=400）。

### 批10 波2（2026-07-18，中轻 5；4 直接绿 / 1 RED-bug 当日修复转绿；修出 1 JIT 误编译）

- **绿**：ethers_evm（alloy 系 consensus 2.2.0/primitives 1.6.1/sol-types
  1.6.1 全 `=` 钉：ABI 编解矩阵（嵌套 tuple+tuple 数组/packed/截断错型负
  路径/transfer 选择子）、RLP 双型交易签名（RFC6979 定签 + recover==地址
  回环 + EIP-155/type-2 全 hex 锚）、主网 genesis 头 RLP+hash_slow 公开
  哈希外部锚；零 C/FFI/零汇编，96 包编译图）、polars_lazy（0.44.2 与批6
  同版同基座：优化器计划文本开/关对照 + 三 pushdown、表达式电池 ×4 帧、
  group_by 含 skew/kurtosis、over 窗口、inner/semi/anti join、CSE 共享
  with_columns 基座 self-join（caches=2——elim_cmn_subplans 只对恒等子
  计划去重，经 5 组 native 对照实验锁定形态）、sort+limit、错误路径；
  351 crate 闭包）、xlsxwriter_rw（rust_xlsxwriter 0.96.0 写 + calamine
  0.36.0+picture 读闭环：七表内存（类型矩阵/公式四型缓存+数组公式
  `_xlfn.IFS` 转义/Grid+autofilter+冻结/命名表/隐藏表/媒体+Url 转义+富
  文本/Empty 边界）+ workbook 级属性；读侧全格 dump 逐格对拍 41 项期望
  零 mismatch；压出 calamine 0.26→0.36 上游语义变更三条，native/mirvm
  同文实证非分叉）、ugrep_bin（ugrep 7.8.2 源码自建 /tmp/ugrep-local，
  sha256 双钉：9 case 子进程面（模式/计数/上下文组分隔/递归/大小写/反选/
  无命中 exit 1/词匹配）；`-J1` 官方可复现开关 + `--sort=name` 钉输出序
  （30 跑 md5 单一实证）；照 opencc 先例留 corpus.sh 手工批有 gating）。
- **RED-bug→当日修复（resvg_svg）**：usvg 0.47 全文本面升级（fontdb 17
  内嵌字体/塑形/bidi/deco/fallback；resvg 本体 simd 默认特性照批6 退路
  直挂 usvg+tiny-skia 0.12 标量）。A/B 维 141 行逐字节绿，C 维 JIT=1 撞
  tiny-skia hairline_aa `slope` 断言（exit 101，381/473 两站漂移、编译序
  敏感）→ 实锤 **JIT signed 窄宽 Div/Rem 漏 sext**：槽不变量「I64 零扩
  到宽」下负操作数被当大正数除（`fdot16::fast_div` 负差遭殃）；
  `int_bin` 修 = signed Div/Rem 先 sext 双操作数（同文件 Shr/IntCmp 已有
  同款步骤的孤点补漏）、MIN/-1 特判 `a`→`ineg(x)`
  （`wrapping_div(x,-1) = -x`）、Bin128 signed 同族并修（含 rem 零常量
  I64→I128 型别错）。探针 demo/signed_div_probe.rs（热循环锤
  i8/i16/i32/i64 负值除余 + fast_div 同形 + W64 b=−1 支）入 diff.sh
  37→38；修复后 MRE/全 driver C 维与解释器逐字节。
- gate5 163→**167**（ethers_evm tmo=90 默认；polars_lazy/xlsxwriter_rw/
  resvg_svg tmo=180；ugrep_bin 依赖 /tmp/ugrep-local 留 corpus.sh 手工批）。

### 批11（2026-07-23，测试管线整顿：manifest 唯一真源 + 孤儿接线 + 真 cargo 项目形态）

- **双名单漂移实锤（整顿的直接动机）**：旧 gate5 与 corpus.sh 各内联一份
  corpus 名单且已分叉——**13 个批6 条目（comfy_table_render/exr_image/faer_lu/
  fastfloat_ryu/gluesql_db/h3_hex/jiff_time/nalgebra_la/plotters_chart/
  polars_frame/stemmers_multi/tokenizers_hf/whatlang_detect）两边都没接线**
  （创建时三维验收过却从不进任何 gate），zstd_stream 只在 corpus.sh 侧。
  治理 = `tests/corpus.manifest` 唯一真源（tier/timeout/mode/env/needs/xfail
  六列，gate 与跑批共读），164 脚本驱动全接线：137 full + 24 smoke + 3 manual。
- **判绿三口径**（gate 内）：`exit`（退出码，创建时三维验收的既定口径）/
  `oracle:<名>`（stdout 逐字节等于 tests/fixtures/oracles/<名>.txt，原内联
  六 oracle 抽离）/ `diff`（真项目三维对拍，下条）。manual 层 = jieba_cut
  （单跑 77-89s 贴 timeout）、opencc/ugrep_bin（依赖 /tmp 机侧前缀，
  needs 缺席记 SKIP）。
- **真 cargo 项目形态（hexyl 0.17.0 + tokei 14.0.0 vendor 进
  `corpus/projects/`，.crate 全量解包 + sha256 钉，见其 README）**：
  `mode=diff` = mirvm run 与 native cargo run 三维逐字节对拍。
  实锤两个机制点：① **cargo 会回放缓存告警**（warm 构建 stderr 仍含依赖
  告警，对拍被噪音炸）→ 两侧同帽 `--cap-lints allow`（manifest env 列，
  `%20` 编码空格），stderr 只承载程序自身输出；② **mirvm 项目模式
  guest cwd=项目目录**（cargo run 从不 chdir，二者语义分叉）→ 夹具路径
  经 `{ROOT}` 占位绝对化（lib.sh::parse_args），产品侧对齐与否记
  [open-issues.md E36](open-issues.md) 待裁定。L2 warm 复跑判定 = warm
  三维 == native 三维 + warm stdout == cold stdout（冷跑 stderr 含构建
  告警属构建事件，仅断言 exit 0）。
- **接线即立功（jiff_time）**：全量 gate 首跑 178/179 绿，唯一红 = c_jiff_time——
  孤儿期间上游漂移：jiff 0.2.33/0.2.34（2026-07-19 发布，验收后三天）起依赖
  当日新建的 jiff-core 0.1.0，其 `from_nanosecond` 越界路径先触 `new_unchecked`
  的 debug_assert（driver 的越界→Err 断言被上游 panic 顶穿；release 构建静默
  构造越界值更糟）。**native 同文 panic（非 mirvm 分叉）**——按上游破洞先例
  钉 `=0.2.32`（jiff-core 换血前最后一版）并在 driver 头注记恢复条件，
  钉后三维逐字节复绿。此例正是"孤儿条目不接线就会悄悄烂掉"的实证。
- **附带收编**：旧 real_projects 重型 harness（ripgrep/tokei 工作集，
  71K+118K+32K，仓内零 case、不进 CI）挪 tests/parked/ 休眠——
  「真项目对拍」职责由本形态以更轻基建接替。

## 6. 批7/批8 候选清单（已全部投放，2026-07-17）

批7（24 个三波激进扩编）与批8（重型 10 个）候选已全部投放，过程与结果见 §5 对应批次。
工具链环境实勘（批8 定稿时核）：cmake/g++/perl/make 在场；**nasm/clang/go 缺席**；
磁盘 148G。

**已判不可（记档，勿重试）**：rocksdb（bindgen 需 libclang）、ravif/av1（nasm 缺席）、
z3/ONNXRuntime、solana 族（构建预算超批量级）、typst 直接版（字体确定性要先做独立
设计评估）。

## 7. 后续候选池（未投放）

**批10（9 个两波）已全部投放收口（2026-07-18；波1/波2 账见 §5）**，原定稿
条目保留备查：

<details><summary>批10 定稿原文（2026-07-18，9 个两波；磁盘 115G 余量已核）</summary>

**波1（重/大物 4）**：

- **c_datafusion_sql**：datafusion SQL 引擎大物——SessionContext 内存表 +
  SQL（select/聚合/join/窗口/子查询），结果位级锚定（arrow 已通的接棒）。
- **c_risc0_run**：risc0 zkVM **execute-only**（guest 程序执行器面，不证明；
  miden_exec 同型先例）。预算铁律：构建爆则判不可并给依赖尺寸证据。
- **c_typst_pdf**：typst 编译小段文档为 PDF（typst-assets 内嵌字体，不依赖
  系统字体；输出字节 FNV）。预算铁律同上。
- **c_miden_prove**：miden prover 面——miden-prover 对定值 MASM 出证明 +
  verify 布尔（批8 execute 已通的证明侧接棒）。预算铁律同上。

**波2（中轻 5）**：

- **c_ethers_evm**：ethers-core/alloy 无网面——ABI encode/decode 矩阵、
  交易 RLP 签名（k256 已通）、区块头解析与哈希指纹。
- **c_polars_lazy**：polars lazy frame——查询优化器/表达式/lazy plan 执行
  （批6 eager 已通的计划侧接棒），内存帧无 IO。
- **c_xlsxwriter_rw**：rust_xlsxwriter 写 + calamine 读闭环——内存写
  xlsx（多 sheet/格式/公式），calamine 读回逐格对拍。
- **c_resvg_svg**：resvg/usvg 完整 SVG 渲染——路径/渐变/文本（内嵌
  fontdb 定值字体）光栅像素 FNV（tiny-skia/fontdue 的上层接棒）。
- **c_ugrep_bin**：ugrep 真二进制——driver 生成定值 fixture 目录文本，
  子进程跑 ugrep 做模式/计数/上下文（输出非 TTY 纯文本确定）。

</details>

池内尚余（缓，记档）：stringsext（ugrep 同族边际低）、wasi 深嵌套（形态未清）、
kerberos/dbus/usb（环境依赖非语义面）、qemu 类、risc0 证明面（先行 execute）、
typst 字体独立设计片（如 c_typst_pdf 撞字体不确定性再升级）、rustls 更多
握手形态、polars 其余延伸、zune-jpeg 0.5 线（0.4.21 破洞面复测）、
datafusion 更大面（以 c_datafusion_sql 结果定）、rustpython 完整形态（mini
已绿；stdlib 满装载按实需）。
