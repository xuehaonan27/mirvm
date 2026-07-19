//! 固定基址布局（P1/P2/S3′/S4 地址模型，decision-history §7.5b/§7.6/§7.5c/§7.3）：
//! 引擎可缓存性的地基常量层——冻结区三域（数据）与代码域三族（stub 码址）
//! 的全部固定基址/样条参数与白名单判据。frozen.rs/codearena.rs 的 arena 实现
//! 以此为准；ir 序列化白名单、baseimage/depsimage 装载校验、lower 装配共享同
//! 一套数值，禁止任何第二处字面量。
//!
//! 选址论证（Linux x86_64 虚拟地址空间知识，与 global_asm/asm-stub 的 x86_64
//! 硬门同前提）：PIE 映像/brk 随机化上界 ~0x66xx_xxxx_xxxx（mmap_rnd_bits=28），
//! mmap 自顶向下区在 0x7fxx_xxxx_xxxx 附近——0x68–0x6E 带落在两带之间的空洞；
//! 与影子 IP（FUNC_IP_BASE，非规范高位、从不映射）无交集。样条步距 16 GiB
//! 远大于各区容量（空洞供未来扩容），上界 1300 不触 0x7f。

// ---- 冻结区三域（数据：statics/常量池/fn-ptr 条目；S4 双域 + S3′ 依赖样条）----

/// 底座域（跨程序共享的 std 预降低模块）固定基址。
pub const BASE_IMAGE_FIXED_ADDR: usize = 0x6800_0000_0000;
/// delta 域（本程序模块；无底座时=全量模块）固定基址。
pub const DELTA_FIXED_ADDR: usize = 0x6900_0000_0000;

/// 依赖 image 域样条：每个 registry 依赖 image 占一固定域，起点 0x6A00、
/// 步距 16 GiB，k 由 lockfile 拓扑序分配。栈 = [底座][img_k…][delta]，
/// 各域绝对地址跨域互指全稳定（可缓存性判据①对每域成立）。
pub const IMAGE_SPLINE_BASE: usize = 0x6A00_0000_0000;
pub const IMAGE_SPLINE_STEP: usize = 1 << 34;
pub const IMAGE_SPLINE_COUNT: usize = 1300;

/// 第 k 个依赖 image 的固定域基址。
pub fn image_addr(k: usize) -> usize {
    assert!(k < IMAGE_SPLINE_COUNT, "image 样条越界: k={k}");
    IMAGE_SPLINE_BASE + k * IMAGE_SPLINE_STEP
}

/// 合法冻结域白名单：底座 / delta / 依赖 image 样条（对齐且在界内）。
/// restore 与 serde 反序列化都过它——防伪造快照把区放到任意地址（错基址=静默错值）。
pub fn is_valid_home(addr: usize) -> bool {
    addr == BASE_IMAGE_FIXED_ADDR
        || addr == DELTA_FIXED_ADDR
        || (addr >= IMAGE_SPLINE_BASE
            && (addr - IMAGE_SPLINE_BASE).is_multiple_of(IMAGE_SPLINE_STEP)
            && (addr - IMAGE_SPLINE_BASE) / IMAGE_SPLINE_STEP < IMAGE_SPLINE_COUNT)
}

// ---- 代码域三族（P1 条目 stub：fn-ptr 值可执行化；与冻结区三域同构互不相交）----

/// delta 代码域固定基址。
pub const DELTA_CODE_ADDR: usize = 0x6C00_0000_0000;
/// 底座代码域固定基址。
pub const BASE_CODE_ADDR: usize = 0x6D00_0000_0000;
/// 依赖 image 代码域样条（k 与冻结样条同一分配序）。
pub const IMAGE_CODE_SPLINE: usize = 0x6E00_0000_0000;
pub const IMAGE_CODE_STEP: usize = 1 << 34;
pub const IMAGE_CODE_COUNT: usize = 1300;

/// 第 k 个依赖 image 的代码域基址。
pub fn image_code_addr(k: usize) -> usize {
    assert!(k < IMAGE_CODE_COUNT, "image 代码样条越界: k={k}");
    IMAGE_CODE_SPLINE + k * IMAGE_CODE_STEP
}

/// 冻结域基址 → 本模块的代码域基址（同一 k 的不变量：delta↔delta、底座↔底座、
/// image_spline(k)↔image_code(k)）。非冻结白名单基址 ⇒ None（引擎不变量违规）。
pub fn code_home_for_frozen(home: usize) -> Option<usize> {
    match home {
        DELTA_FIXED_ADDR => Some(DELTA_CODE_ADDR),
        BASE_IMAGE_FIXED_ADDR => Some(BASE_CODE_ADDR),
        h if h >= IMAGE_SPLINE_BASE
            && (h - IMAGE_SPLINE_BASE).is_multiple_of(IMAGE_SPLINE_STEP)
            && (h - IMAGE_SPLINE_BASE) / IMAGE_SPLINE_STEP < IMAGE_SPLINE_COUNT =>
        {
            Some(image_code_addr((h - IMAGE_SPLINE_BASE) / IMAGE_SPLINE_STEP))
        }
        _ => None,
    }
}

/// 合法代码域白名单（serde 配方回放与装载防御双验证；伪造快照防线）。
pub fn is_valid_code_home(addr: usize) -> bool {
    addr == DELTA_CODE_ADDR
        || addr == BASE_CODE_ADDR
        || (addr >= IMAGE_CODE_SPLINE
            && (addr - IMAGE_CODE_SPLINE).is_multiple_of(IMAGE_CODE_STEP)
            && (addr - IMAGE_CODE_SPLINE) / IMAGE_CODE_STEP < IMAGE_CODE_COUNT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 域界与白名单() {
        assert!(is_valid_home(BASE_IMAGE_FIXED_ADDR));
        assert!(is_valid_home(DELTA_FIXED_ADDR));
        assert!(is_valid_home(image_addr(0)));
        assert!(is_valid_home(image_addr(IMAGE_SPLINE_COUNT - 1)));
        assert!(!is_valid_home(image_addr(IMAGE_SPLINE_COUNT - 1) + 0x1000));
        assert_eq!(code_home_for_frozen(DELTA_FIXED_ADDR), Some(DELTA_CODE_ADDR));
        assert_eq!(code_home_for_frozen(BASE_IMAGE_FIXED_ADDR), Some(BASE_CODE_ADDR));
        assert_eq!(
            code_home_for_frozen(image_addr(7)),
            Some(image_code_addr(7))
        );
        assert!(code_home_for_frozen(0x1234_5678_0000).is_none());
        assert!(is_valid_code_home(DELTA_CODE_ADDR));
        assert!(is_valid_code_home(image_code_addr(IMAGE_CODE_COUNT - 1)));
        assert!(!is_valid_code_home(DELTA_FIXED_ADDR));
    }
}
