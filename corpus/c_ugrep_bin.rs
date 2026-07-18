#!/usr/bin/env mirvm
---
[dependencies]
---
// c_ugrep_bin —— ugrep 真二进制（C++，非 crate）差分：driver 生成定值 fixture
// 目录文本，guest 内 std::process::Command 子进程跑 ugrep 做模式/计数/上下文，
// 捕获其 stdout 逐字节锚定（批10 波2；子进程 passthrough 的 c_process 同族，
// 机器侧 /tmp 前缀的 c_opencc 同族）。
//
// 版本钉（相容组合证据）：
//   * 本 driver 零 crate 依赖（frontmatter 空）；被测物 = ugrep C++ 真二进制。
//   * 系统无 ugrep（`which ugrep` 空；crates.io 无同名 crate，API 404 实证），
//     按 c_opencc 先例源码构建机器侧前缀：GitHub Genivia/ugrep tag v7.8.2
//     （2026-05-17 最新 release），2026-07-18 本机 g++ 11.4.0、configure 全
//     默认 + make install 装至 /tmp/ugrep-local。哈希钉（实测）：
//       源码 tarball /tmp/ugrep-7.8.2.tar.gz
//         sha256 = f991cc6c61dbc5af5a3b3939083e917df4113509549670fb400d121f639f69f6
//       二进制 /tmp/ugrep-local/bin/ugrep
//         sha256 = 5740c878b8a2664662a6c96c54fd7c842c3104657135cdd70bc147eef52efa24
//     动态链接仅系统 liblzma/libz/libstdc++/libgcc_s/libc/libm；三维共用同一
//     绝对路径二进制，版本面锁死。case 0 打印 `ugrep --version` 全文（4 行，
//     无机器路径），二进制被替换/升级立即可见。
//
// 确定性说明（非 TTY 纯文本可复现的证据链）：
//   * fixture 三文件全文硬编码 ASCII（alpha.txt 11 行 / beta.txt 6 行 /
//     sub/gamma.txt 3 行），每维开头 remove_dir_all + 重建，内容与目录项
//     创建序恒定；二进制与 fixture 均走固定绝对路径，与三维各自 cwd 无关
//     （A/C 在仓库根、B 在 script dir，输出里嵌的路径完全一致）。
//   * 子进程 env_clear() 只设 LC_ALL=C（大小写折叠/词字符类局域锁死）；
//     命令行恒带 --color=never --no-config（管道 stdout 本就非 TTY 无色，
//     双保险防 .ugrep 配置文件/HOME 环境面）。
//   * 并发序面（实测实锤）：ugrep 默认多线程搜索，输出序不稳——无干预时
//     `-r` 递归面 30 跑出现 4 种 md5；官方 help 明文 "-J1 may be specified
//     to produce replicable results"。故全用例恒带 -J1，递归面再加
//     --sort=name（文件序按路径名排序，与 readdir/重建序无关：10 次拆建
//     fixture 复跑全同）。加锁后完整用例集 30 跑 md5 单一。
//   * 每用例打印 argv/status/stdout len+FNV-1a/64 + stdout 全文（ASCII，
//     from_utf8_lossy 零替换）+ stderr len（实测恒 0；若非空则全文照打，
//     确定性同样成立）；exit code 以数值打印（0=命中，1=无命中，case 7
//     专测 1）。无壁钟/随机/裸地址/HashMap 序。
//
// 复红定因参照：
//   * 无已知 FRONTIER/欠账面。若 mirvm 的子进程 spawn/env_clear/argv 传递/
//     stdout 捕获/exit code 回传链路（c_process 压过的同一面）日后回归，
//     本 driver 即探针。
//
// 覆盖清单：
//   case 0  --version 全文（版本钉锚）。
//   case 1  模式：-n needle 单文件行号命中（5 行）。
//   case 2  计数：-c needle 两文件（argv 序输出，5/2）。
//   case 3  上下文：-n -C2 'needle[0-9]' 正则 + 双命中分组 + GNU 式 "--"
//           组分隔行（line 3 与 line 10 两组）。
//   case 4  递归：-r --sort=name -n needle 整 fixture（alpha/beta/sub/gamma
//           路径序 9 行）。
//   case 5  大小写：-i -n needle（NEEDLE 行并入，6 行）。
//   case 6  反选：-v -n needle beta.txt（4 行）。
//   case 7  无命中：-n zzz_no_such_word → status=1、stdout 空。
//   case 8  词匹配：-w -n needle（needle123/needle9 排除，3 行）。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_ugrep_bin.rs
//   B: d=$(grep -l 'name = "c_ugrep_bin"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && cargo +nightly-2026-07-02 run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_ugrep_bin.rs
//
// 三维实测（2026-07-18，全绿）：A/B/C 三进程 stdout 逐字节一致（3061 字节 /
// 115 行 / 9 case），stderr 全真空（0 字节）、exit 全 0；diff A-B、A-C 均
// 空。关键锚：case0 version fnv=1cd2b98914827be4（292B）；case1 模式 5 行
// fnv=786db0131bdcfcbf；case2 计数 5/2 fnv=b28125633f24c804；case3 双分组
// 上下文（含 "--" 分隔行）fnv=f559779048766a77；case4 递归 9 行
// fnv=4c4f4d14f9963518；case7 无命中 status=1、stdout 空（fnv 恰为初值
// cbf29ce484222325）；case8 词匹配 3 行 fnv=da3f4213b86b57c7。时长（deps
// 树 0 crate、缓存热）：A 0.411s / B 0.289s / C（JIT=1）0.151s。无 FRONTIER、
// 无引擎 bug 信号。

use std::fs;
use std::path::Path;
use std::process::Command;

/// 机器侧 ugrep 真二进制（版本钉见头注）。
const UGREP: &str = "/tmp/ugrep-local/bin/ugrep";
/// 定值 fixture 根（绝对路径，三维同路径）。
const FIXTURE: &str = "/tmp/mirvm_corpus_ugrep_fixture";
/// fixture 文件绝对路径常量（argv 打印与 ugrep 输出同款字面值）。
const ALPHA_PATH: &str = "/tmp/mirvm_corpus_ugrep_fixture/alpha.txt";
const BETA_PATH: &str = "/tmp/mirvm_corpus_ugrep_fixture/beta.txt";

const ALPHA: &str = "needle at top\n\
                     hay line one\n\
                     needle123 tail\n\
                     NEEDLE upper\n\
                     no match here\n\
                     needle! punct\n\
                     last needle\n\
                     pure hay\n\
                     more hay lines\n\
                     needle9 far away\n\
                     tail hay\n";

const BETA: &str = "beta line\n\
                    needle mid\n\
                    more hay\n\
                    Needle mixed\n\
                    plain text here\n\
                    final needle again\n";

const GAMMA: &str = "sub needle deep\n\
                     sub hay\n\
                     needle again in sub\n";

/// FNV-1a 64：子进程 stdout 指纹（无外部依赖，位确定）。
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 每维重建 fixture：先清后建，内容与目录项创建序恒定。
fn setup_fixture() {
    let root = Path::new(FIXTURE);
    if root.exists() {
        fs::remove_dir_all(root).expect("清 fixture 失败");
    }
    fs::create_dir_all(root.join("sub")).expect("建 fixture 目录失败");
    fs::write(root.join("alpha.txt"), ALPHA).unwrap();
    fs::write(root.join("beta.txt"), BETA).unwrap();
    fs::write(root.join("sub").join("gamma.txt"), GAMMA).unwrap();
}

/// 跑一个 ugrep 用例并锚定打印。flag 前缀恒 -J1 --color=never --no-config
/// （确定性论证见头注），子进程 env 只含 LC_ALL=C。
fn run_case(idx: usize, desc: &str, args: &[&str]) {
    let out = Command::new(UGREP)
        .args(["-J1", "--color=never", "--no-config"])
        .args(args)
        .env_clear()
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("spawn ugrep case {idx} 失败（{UGREP} 在场？）: {e}"));

    println!("== case {idx}: {desc} ==");
    println!("argv: {}", args.join(" "));
    match out.status.code() {
        Some(c) => println!("status: {c}"),
        None => println!("status: signal"),
    }
    println!("stdout: len={} fnv={:016x}", out.stdout.len(), fnv1a(&out.stdout));
    println!("--- stdout begin ---");
    print!("{}", String::from_utf8_lossy(&out.stdout));
    println!("--- stdout end ---");
    println!("stderr: len={}", out.stderr.len());
    if !out.stderr.is_empty() {
        print!("{}", String::from_utf8_lossy(&out.stderr));
    }
    println!();
}

fn main() {
    assert!(Path::new(UGREP).exists(), "缺 ugrep 二进制 {UGREP}（见头注版本钉）");
    setup_fixture();

    run_case(0, "version", &["--version"]);
    run_case(1, "pattern -n single", &["-n", "needle", ALPHA_PATH]);
    run_case(2, "count -c multi", &["-c", "needle", ALPHA_PATH, BETA_PATH]);
    run_case(3, "context -C2 regex", &["-n", "-C2", "needle[0-9]", ALPHA_PATH]);
    run_case(4, "recursive -r sort", &["-r", "--sort=name", "-n", "needle", FIXTURE]);
    run_case(5, "ignore-case -i", &["-i", "-n", "needle", ALPHA_PATH]);
    run_case(6, "invert -v", &["-v", "-n", "needle", BETA_PATH]);
    run_case(7, "no-match exit 1", &["-n", "zzz_no_such_word", ALPHA_PATH]);
    run_case(8, "word -w", &["-w", "-n", "needle", ALPHA_PATH]);
}
