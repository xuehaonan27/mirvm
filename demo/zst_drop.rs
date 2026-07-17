// C 维 corpus c_rustpython_mini 实锤回归探针（jit_compile analyze_frame 0 字节帧
// ZST 取址，FrameMap force 档）：自定义 Drop 的 ZST 经 mem::drop 传参时，glue 调用
// 实参 = AddrOf(Local(0))——fsz=0 无处落锚，旧实现 frame_ss 缺席、断言「取址 offset
// 必落帧」炸（JIT 线程 stderr 污染）。修复后该函数可编译，逢调即编维与 native 一致。
struct G;
impl Drop for G {
    fn drop(&mut self) {
        // 空体：glue 只需要 &_1 的地址本身（不读字段）
    }
}

#[inline(never)]
fn cycle() -> u64 {
    drop(G);
    1
}

fn main() {
    let mut n = 0u64;
    for _ in 0..8 {
        n += cycle();
    }
    println!("n={n}");
}
