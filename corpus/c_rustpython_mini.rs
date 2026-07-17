#!/usr/bin/env mirvm
---
[dependencies]
rustpython-vm = "=0.5.0"
---
// c_rustpython_mini — rustpython-vm 大物试探：嵌入式裸解释器（without_stdlib）
// 执行 5 个定值 Python 程序，结果经 globals 中的 RESULT 回收为 str 后由宿主打印。
//
// 覆盖清单：
//   P1 arith    : 整数算术（* + // % **，负数 floor 语义，abs）
//   P2 listdict : dict 插入/覆盖、sorted 定序、列表推导、str.join / repr
//   P3 closure  : 工厂函数+闭包 cell 状态、lambda、map 式 apply、列表切片
//   P4 exc      : try/except 捕获 ZeroDivisionError / raise ValueError / KeyError，
//                 输出 type(e).__name__ 与 str(e)/repr(str(e)) 异常文本
//   P5 strfmt   : strip/upper/split、dict comprehension、字符串反转、f-string 拼接
//
// 确定性说明：
//   - 全部程序只用整数与 ASCII 字符串；无浮点、无 import（裸 vm 无 importlib/stdlib）
//   - 无真随机/壁钟/线程；所有 dict 宕序输出一律先 sorted() 显式定序
//   - Python 侧 str()/repr() 只作用于 int/str 标量与容器，不 repr 函数/对象（无裸地址）
//   - 宿主侧只把 RESULT 的 utf8 文本逐字节搬到 stdout；stderr 真空，exit 恒 0
//   - 宿主防御分支（COMPILE_ERR/MISSING/NOT_STR/UNCAUGHT）为定值哨兵，绿时不触发
//
// 钉版本记录：rustpython-vm "=0.5.0"（crates.io 最新稳定，2026-03-31 发布）。
//   依赖证据：锁定树 230 包；本机 8 核 nightly-2026-07-02 debug 冷构建 1m08s（远低于
//   15min 预算）；libffi-sys 4.2.0 vendored 走 cc、psm 0.1.31 内置汇编用 cc，均过；
//   无 bindgen/nasm/clang 需求。
//
// 官方开关绕行（先例同 curve25519 serial backend 一类）：
//   Settings.install_signal_handlers = false（rustpython-vm 官方 embedding API，
//   src/vm/setting.rs:50，默认 true@188）。
//   诊断链（已实证）：
//   ① 首跑 A 维 exit=70，stderr = "guest handler for synchronous fault signal 7"
//      = mirvm D8l/open-issues R1 对 sync 故障信号 guest handler 的响亮拒绝；
//   ② rustpython-vm `_signal` 内建模块初始化（stdlib/_signal.rs:164
//      init_signal_handlers）在 install_signal_handlers=true 时对 1..NSIG 全部信号
//      做 `libc::signal(n, SIG_IGN)` 查询 + `libc::signal(n, handler)` 原样重装；
//   ③ 其中 SIGBUS(7) 的"原 handler"是 guest 侧 Rust std 运行时启动时装入的
//      stack_overflow::imp::signal_handler（LD_PRELOAD shim + addr2line 实证，
//      调用栈 lang_start_internal → stack_overflow::imp::init）——它是 guest fn，
//      在 mirvm 里经 thunk 落进 sync 故障拒绝面 → exit 70；
//   ④ 该安装只服务 CLI REPL 的 SIGINT/default_int_handler 接管与 signal 模块
//      handler 登记表；本 driver 不用 signal 模块、无 REPL，关掉不改变被测
//      Python 语义（native 同设置复跑，五程序输出逐字节不变）。
//
// C 维（JIT_THRESHOLD=1）= expected-red，红属类别③（引擎语义 bug，非 driver）：
//   症状：A/B 二维三维绿（stdout 逐字节一致、stderr 真空、exit 全 0）；
//        C 维 stdout 逐字节正确、exit 0，但 stderr 被 mirvm-jit 线程 panic 污染：
//        "thread 'mirvm-jit' (<tid 变>) panicked at src/vm/engine/jit_compile.rs:1276:14:
//         取址 offset 必落帧（analyze_frame 全集）"（4/4 跑稳定复现）→ 三维红。
//   肇事函数（MIRVM_JIT_DEBUG=1 最后一条"收到"实证）：
//        core::mem::drop::<rustpython_vm::object::core::weakref_lock::WeakrefLockGuard>
//        ——JIT 编译该 drop glue 时 analyze_frame 未把某取址 offset 提升落帧，
//        addr_of_local 断言即爆。jit.rs:57 设计=「线程死静默维持解释」，故语义
//        不受损、exit 恒 0，仅 stderr 污染。
//   最小复现（/tmp/rp_bisect2.rs，非本仓文件）：
//        fn main() { let mut s = Settings::default();
//            s.install_signal_handlers = false;
//            let i = Interpreter::without_stdlib(s);
//            i.enter(|_| println!("in_enter")); }
//        → MIRVM_JIT_THRESHOLD=1 下 genesis 即引爆同一 1276 panic（f7742 同一函数）。
//   族谱：批8 starlark_eval 同款断言族雷「analyze_frame 帧末 ZST 取址必爆」已由
//        dc6e30c 修过一次；本例 = 同族新形态（drop glue@weakref_lock）。
//   接线建议：red_code = 0（exit 不红，勿以退出码判）；
//        red_pattern = "取址 offset 必落帧（analyze_frame 全集）"（或
//        "panicked at src/vm/engine/jit_compile.rs:1276"），仅出现在 C 维 stderr，
//        tid 数字随行变、匹配时须剔除。
use rustpython_vm::builtins::PyStr;
use rustpython_vm::compiler::Mode;
use rustpython_vm::scope::Scope;
use rustpython_vm::{Interpreter, Settings, VirtualMachine};

const PROGRAMS: &[(&str, &str)] = &[
    (
        "P1_arith",
        r#"
a = 2 + 3 * 4
b = (a * 10) // 7
c = (2 ** 10) % 97
d = (-15) // 4
e = (-15) % 4
f = abs(-9) * (17 // -3)
RESULT = repr([a, b, c, d, e, f])
"#,
    ),
    (
        "P2_listdict",
        r#"
d = {}
for k in ["delta", "alpha", "charlie", "bravo"]:
    d[k] = len(k) * 7
keys = sorted(d.keys())
pairs = [k + ":" + str(d[k]) for k in keys]
nums = [x * x for x in range(12) if x % 3 == 0]
d["alpha"] += 100
RESULT = ",".join(pairs) + "|" + repr(nums) + "|" + repr(d["alpha"])
"#,
    ),
    (
        "P3_closure",
        r#"
def make_counter(step):
    total = [0]
    def bump():
        total[0] += step
        return total[0]
    return bump

c1 = make_counter(3)
c2 = make_counter(10)
vals = [c1(), c1(), c2(), c1(), c2()]

def apply(f, xs):
    return [f(x) for x in xs]

sq = apply(lambda x: x * x, vals[:3])
RESULT = "->".join(str(v) for v in vals) + "//" + ",".join(str(x) for x in sq)
"#,
    ),
    (
        "P4_exc",
        r#"
out = []

def explode(x):
    return 100 // x

try:
    explode(2)
    explode(0)
    out.append("noexc")
except ZeroDivisionError as e:
    out.append("caught:" + type(e).__name__ + ":" + str(e))

try:
    raise ValueError("explicit-" + str(6 * 7))
except ValueError as e:
    out.append(str(e).upper())

xs = {"k": 1}
try:
    xs["missing"]
except BaseException as e:
    out.append(type(e).__name__ + ":" + repr(str(e)))

RESULT = "|".join(out)
"#,
    ),
    (
        "P5_strfmt",
        r#"
s = " hello,mirvm "
parts = (s.strip() + " X7").upper().split(" ")
nums = [str(len(p)) for p in parts]
tbl = {p: p[::-1] for p in parts if p}
ks = sorted(tbl)
RESULT = f"{len(s)}:{'-'.join(nums)}:" + "|".join(f"{k}={tbl[k]}" for k in ks)
"#,
    ),
];

fn run_one(vm: &VirtualMachine, tag: &str, src: &str) {
    let globals = vm.ctx.new_dict();
    let scope = Scope::with_builtins(None, globals.clone(), vm);
    let captured = match vm.compile(src, Mode::Exec, format!("<{tag}>")) {
        Err(_) => "COMPILE_ERR".to_owned(),
        Ok(code) => match vm.run_code_obj(code, scope) {
            Err(_) => "UNCAUGHT".to_owned(),
            Ok(_) => match globals.get_item("RESULT", vm) {
                Err(_) => "MISSING".to_owned(),
                Ok(obj) => match obj.downcast::<PyStr>() {
                    Err(_) => "NOT_STR".to_owned(),
                    Ok(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                },
            },
        },
    };
    println!("{tag}={captured}");
}

fn main() {
    let mut settings = Settings::default();
    settings.install_signal_handlers = false;
    let interp = Interpreter::without_stdlib(settings);
    interp.enter(|vm| {
        for (tag, src) in PROGRAMS {
            run_one(vm, tag, src);
        }
    });
}
