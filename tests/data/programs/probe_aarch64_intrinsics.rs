#[cfg(target_arch = "aarch64")]
mod probe {
    use std::arch::aarch64::*;
    use std::hint::black_box;

    macro_rules! widening {
        ($op:ident, $narrow:ty, $wide:ty, $lanes:expr) => {{
            #[inline(never)]
            unsafe fn execute(input: [$narrow; $lanes]) -> [$wide; $lanes / 2] {
                unsafe { std::mem::transmute($op(std::mem::transmute(input))) }
            }
            let values = [
                <$narrow>::MAX,
                <$narrow>::MAX,
                <$narrow>::MIN,
                <$narrow>::MIN,
                <$narrow>::MAX,
                <$narrow>::MIN,
                0,
                !0,
            ];
            for offset in 0..values.len() {
                let input = black_box(std::array::from_fn(|i| values[(i + offset) % values.len()]));
                let actual = unsafe { execute(input) };
                let expected: [$wide; $lanes / 2] =
                    std::array::from_fn(|i| (input[2 * i] as $wide) + (input[2 * i + 1] as $wide));
                assert_eq!(actual, expected, "{} offset={offset}", stringify!($op));
            }
            println!("{} ok", stringify!($op));
        }};
    }

    macro_rules! pairwise {
        ($op:ident, $lane:ty, $lanes:expr) => {{
            #[inline(never)]
            unsafe fn execute(a: [$lane; $lanes], b: [$lane; $lanes]) -> [$lane; $lanes] {
                unsafe { std::mem::transmute($op(std::mem::transmute(a), std::mem::transmute(b))) }
            }
            for offset in 0..8 {
                let a = black_box(std::array::from_fn(|i| {
                    <$lane>::MAX - (i + offset) as $lane
                }));
                let b = black_box(std::array::from_fn(|i| (i * 7 + offset) as $lane));
                let actual = unsafe { execute(a, b) };
                let expected: [$lane; $lanes] = std::array::from_fn(|i| {
                    let src = if i < $lanes / 2 { a } else { b };
                    let j = (i % ($lanes / 2)) * 2;
                    src[j].wrapping_add(src[j + 1])
                });
                assert_eq!(actual, expected, "{} offset={offset}", stringify!($op));
            }
            println!("{} ok", stringify!($op));
        }};
    }

    #[inline(never)]
    #[target_feature(enable = "crc")]
    unsafe fn crc_results(state: u32, value: u64) -> [u32; 8] {
        [
            __crc32b(state, value as u8),
            __crc32h(state, value as u16),
            __crc32w(state, value as u32),
            __crc32d(state, value),
            __crc32cb(state, value as u8),
            __crc32ch(state, value as u16),
            __crc32cw(state, value as u32),
            __crc32cd(state, value),
        ]
    }

    fn crc_reference(mut state: u32, value: u64, bytes: usize, polynomial: u32) -> u32 {
        for byte in &value.to_le_bytes()[..bytes] {
            state ^= u32::from(*byte);
            for _ in 0..8 {
                state = (state >> 1) ^ if state & 1 != 0 { polynomial } else { 0 };
            }
        }
        state
    }

    pub fn run() {
        widening!(vpaddl_u8, u8, u16, 8);
        widening!(vpaddlq_u8, u8, u16, 16);
        widening!(vpaddl_u16, u16, u32, 4);
        widening!(vpaddlq_u16, u16, u32, 8);
        widening!(vpaddl_u32, u32, u64, 2);
        widening!(vpaddlq_u32, u32, u64, 4);
        widening!(vpaddl_s8, i8, i16, 8);
        widening!(vpaddlq_s8, i8, i16, 16);
        widening!(vpaddl_s16, i16, i32, 4);
        widening!(vpaddlq_s16, i16, i32, 8);
        widening!(vpaddl_s32, i32, i64, 2);
        widening!(vpaddlq_s32, i32, i64, 4);
        pairwise!(vpadd_u8, u8, 8);
        pairwise!(vpaddq_u8, u8, 16);
        pairwise!(vpadd_u16, u16, 4);
        pairwise!(vpaddq_u16, u16, 8);
        pairwise!(vpadd_u32, u32, 2);
        pairwise!(vpaddq_u32, u32, 4);
        pairwise!(vpaddq_u64, u64, 2);

        if std::arch::is_aarch64_feature_detected!("crc") {
            let mut checksum = 0u32;
            for state in [0, 1, u32::MAX, 0x1234_5678] {
                for value in [0, 1, u64::MAX, 0xfedc_ba98_7654_3210] {
                    let actual = unsafe { crc_results(black_box(state), black_box(value)) };
                    for (i, result) in actual.into_iter().enumerate() {
                        let polynomial = if i < 4 { 0xedb8_8320 } else { 0x82f6_3b78 };
                        assert_eq!(
                            result,
                            crc_reference(state, value, 1 << (i % 4), polynomial)
                        );
                        checksum = checksum.rotate_left(1) ^ result;
                    }
                }
            }
            println!("crc={checksum:08x}");
        } else {
            println!("crc=unavailable");
        }
    }
}

fn main() {
    #[cfg(target_arch = "aarch64")]
    probe::run();
    #[cfg(not(target_arch = "aarch64"))]
    println!("aarch64 unavailable");
}
