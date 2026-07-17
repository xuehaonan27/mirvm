#!/usr/bin/env mirvm
---
[dependencies]
# mlua 0.10.5（0.10 线最新 patch；上游已发 0.11/0.12，不占本槽位授权）；
# 本 crate 无 default feature，显式 default-features=false 剥净辅助件
# （不开 async/send/serialize/macros——闭包最小化）。features = vendored +
# lua54：mlua-sys 0.6.8 经 lua-src 547.0.0（Lua 5.4.7 C 源码）走 cc 静态构建
# 出 .a，由 mirvm 的 native-archive（.a→.so 闭包 + crate 图动态库链接行）通道
# 加载——重 FFI（全 lua_* C API 面）+ VM-in-VM（C Lua VM 嵌在 Rust MIR VM 里）
# 双重压力，即批8 波1 rocksdb 候补位（librocksdb-sys 因 libclang 缺席判不可）
# 的本职负载。物化闭包 39 crate（mlua/mlua-sys/lua-src/cc/bstr/either/
# num-traits/parking_lot 系/rustc-hash/rustversion 等）。
mlua = { version = "=0.10.5", default-features = false, features = ["vendored", "lua54"] }
---
// mlua 0.10（vendored Lua 5.4）三维差分：计算主体一半在 native C（Lua VM 执行
// chunk、table/string 库）一半在被解释/JIT 的 Rust 绑定层（值转换、回调、错误包
// 装）。FFI 签名全标量/指针（lua_State* / c_int / double / fn-ptr），不撞
// debt-map §9 按值聚合墙。回调形态：create_function → mlua 存 CallbackUpvalue
// userdata（带 __gc，lua_close 时 native→guest 销毁回调）+ lua_pushcclosure
// 推入 monomorphic `unsafe extern "C-unwind" fn call_callback` 显式 fn-ptr——
// P1 可执行化（decision-history §7.6）覆盖的 thunk 面。错误面前沿（本 driver
// 刻意压到最后一节）：Rust 回调 Err → callback_error_ext 包 WrappedFailure
// userdata → ffi::lua_error（C 侧 longjmp）跨 thunk/解释帧跳回 lua_pcall 的
// setjmp——宿主 longjmp 遗弃中途 mirvm 解释调用的存活试探针。
//
// 测试面清单：
//   ① _VERSION 锚（"Lua 5.4"）+ chunk1 算术与表：整数平方累加 / 10! /
//      1<<40 六十四位整算 / 数组回读（eval::<Table> 按名取，五个 assert_eq）。
//   ② chunk2 迭代与字符串库：gmatch 分割 / ipairs 拼接 table.concat /
//      string.gsub 带计数 / reverse / string.format("%05d:%s:%.3f")。
//   ③ chunk3 echo 与 pcall 错误面（纯 Lua 侧）：vararg echo 计数与
//      tostring 拼接；pcall 捕获三类错误——普通串 error、结构化表错误
//      {code=8317,msg}（Lua 侧拆字段拼回串，避开 userdata/table 地址）、
//      nil 索引运行时错误（带 chunk 名与行号，同名同源两侧一致）。
//   ④ Rust 侧注册函数喂 Lua 调：rust_feed（Rc<Cell> 跨调用计数 12 次 +
//      逐次参数断言 s=="arg#"..i，返回值吃上次计数制造跨调用状态）与
//      rust_add；Lua 循环调 12+6 次聚合回总值，Rust 侧回读 counter==12。
//   ⑤ Lua 返回 table（stats 键刻意乱序 + tags 数组）经 Rust 端
//      BTreeMap 字典序打印——避开 next 序（见确定性段）。
//   ⑥ 长字符串往返 >1KB：Rust 建 seg00..seg63 定长段表（19B×64）设入
//      globals，Lua table.concat（"|" 连接 = 1216+63 = 1279B）并算
//      31-bit rolling hash（mod 1000000007，i64 精确），回 Rust 打
//      len/lua_crc/fnv1a(hex)/头尾 16B。
//   ⑦ 错码面（callback_error_ext→lua_error 跨 thunk longjmp 探针，压最后）：
//      rust_boom(5) Err(RuntimeError "boom#5")——(a) Lua pcall 侧：
//      ok=false + type(err)=="userdata"（不打 userdata 本体，含地址）；
//      (b) 无 pcall 直传 Rust：exec 返回 Err(CallbackError)，打 Display
//      首行（根因串 "runtime error: boom#5"，traceback 后续行不入输出）。
//
// 确定性：全常量源串与硬编码期望值；Lua 侧 string 哈希种子（g->seed 由
// time/地址混合）使 pairs 序跨进程不稳——本 driver 一律不用无序迭代：Lua 内
// 只走 ipairs/数组，string 键表回 Rust 入 BTreeMap 再打印；不调
// math.random/os.clock/os.time/print；地址经 tostring 的值一律不入输出；
// B 维 native 实测 stderr 真空、exit 0。
//
// 三维实测（2026-07-17，全绿）：A/C/B 三进程 stdout 逐字节一致（17 行 769B，
// 含 [string \"chunk3\"]:11/:19 错误行号锚与 chunk6 fnv=3658152a23cb81fb），
// stderr 全真空、exit 全 0。首次 A 维即通，无 FRONTIER、无绕行、无产品 bug。
// 节⑦的 callback_error_ext→lua_error 跨 thunk longjmp 面（pcall 内截获
// userdata + 直传 Rust 的 CallbackError Display 首行）三维同样逐字节一致。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_mlua_lua.rs
//   B: cd "$(grep -l 'name = "c_mlua_lua"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_mlua_lua.rs
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use mlua::{Lua, Table};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const CHUNK1: &str = r##"
local t = {}
local acc = 0
for i = 1, 20 do
  t[i] = i * i - i
  acc = acc + t[i]
end
local fact = 1
for i = 1, 10 do fact = fact * i end
local big = (1 << 40) + ((1 << 3) * 7) - 5
return { sum = acc, fact10 = fact, big = big, third = t[3], n = #t }
"##;

const CHUNK2: &str = r##"
local parts = {}
for w in string.gmatch("alpha,beta;gamma,delta;epsilon", "[^,;]+") do
  parts[#parts + 1] = w
end
local up = {}
for i, w in ipairs(parts) do
  up[i] = w:upper() .. "#" .. i
end
local joined = table.concat(up, "|")
local sub, nsub = string.gsub("abc abc abd", "ab(%a)", "%1.")
local rev = ("mirror"):reverse()
local fmt = string.format("%05d:%s:%.3f", 42, "seven", 1 / 8)
return joined, sub, nsub, rev, fmt
"##;

const CHUNK3: &str = r##"
local function echo(...)
  local n = select("#", ...)
  local acc = {}
  for i = 1, n do
    acc[i] = tostring(select(i, ...))
  end
  return n, table.concat(acc, ";")
end
local n1, s1 = echo("x", 7, true, false)
local ok1, e1 = pcall(function() error("plain failure") end)
local ok2, e2 = pcall(function() error({code = 8317, msg = "structured failure"}) end)
local err2s = "none"
if not ok2 and type(e2) == "table" then
  err2s = tostring(e2.code) .. ":" .. tostring(e2.msg)
end
local ok3, e3 = pcall(function()
  local x = nil
  return x.field
end)
return {
  echo_n = n1, echo_s = s1,
  ok1 = ok1, err1 = tostring(e1),
  ok2 = ok2, err2 = err2s,
  ok3 = ok3, err3 = tostring(e3),
}
"##;

const CHUNK4: &str = r##"
local total = 0
for i = 1, 12 do
  total = total + rust_feed(i, "arg#" .. i)
end
local addsum = 0
for i = 1, 6 do
  addsum = addsum + rust_add(i, i * 2)
end
return { total = total, addsum = addsum }
"##;

const CHUNK5: &str = r##"
return {
  stats = { gamma = 256, alpha = 11, beta = -7 },
  tags = { "x", "yy", "zzz" },
}
"##;

const CHUNK6: &str = r##"
local joined = table.concat(PARTS, "|")
local h = 0
for i = 1, #joined do
  h = (h * 131 + string.byte(joined, i)) % 1000000007
end
return joined, #joined, h
"##;

const CHUNK7A: &str = r##"
local ok, err = pcall(rust_boom, 5)
local ok2, out2 = pcall(rust_boom, 9)
return { ok = ok, errtype = type(err), ok2 = ok2, out2 = tostring(out2) }
"##;

const CHUNK7B: &str = "return rust_boom(5)";

fn main() -> mlua::Result<()> {
    let lua = Lua::new();
    let globals = lua.globals();

    // ---- ⓪ 版本锚 ----
    let ver: String = lua.load("return _VERSION").set_name("chunk0").eval()?;
    println!("version = {ver}");

    // ---- ① chunk1：算术与表 ----
    let t1: Table = lua.load(CHUNK1).set_name("chunk1").eval()?;
    let (sum, fact10, big, third, n) = (
        t1.get::<i64>("sum")?,
        t1.get::<i64>("fact10")?,
        t1.get::<i64>("big")?,
        t1.get::<i64>("third")?,
        t1.get::<i64>("n")?,
    );
    assert_eq!(sum, 2660);
    assert_eq!(fact10, 3628800);
    assert_eq!(big, 1099511627827);
    assert_eq!(third, 6);
    assert_eq!(n, 20);
    println!("chunk1 sum={sum} fact10={fact10} big={big} third={third} n={n}");

    // ---- ② chunk2：迭代与字符串库 ----
    let (joined, sub, nsub, rev, fmt): (String, String, i64, String, String) =
        lua.load(CHUNK2).set_name("chunk2").eval()?;
    assert_eq!(joined, "ALPHA#1|BETA#2|GAMMA#3|DELTA#4|EPSILON#5");
    assert_eq!(sub, "c. c. d.");
    assert_eq!(nsub, 3);
    assert_eq!(rev, "rorrim");
    assert_eq!(fmt, "00042:seven:0.125");
    println!("chunk2 joined = {joined}");
    println!("chunk2 sub={sub} nsub={nsub} rev={rev} fmt={fmt}");

    // ---- ③ chunk3：echo 与 pcall 错误面（纯 Lua 侧）----
    let t3: Table = lua.load(CHUNK3).set_name("chunk3").eval()?;
    let echo_n: i64 = t3.get("echo_n")?;
    let echo_s: String = t3.get("echo_s")?;
    assert_eq!((echo_n, echo_s.as_str()), (4, "x;7;true;false"));
    println!("chunk3 echo n={echo_n} s={echo_s}");
    let ok1: bool = t3.get("ok1")?;
    let err1: String = t3.get("err1")?;
    assert!(!ok1 && err1.ends_with(": plain failure"));
    println!("chunk3 pcall1 ok={ok1} err1={err1}");
    let ok2: bool = t3.get("ok2")?;
    let err2: String = t3.get("err2")?;
    assert!(!ok2 && err2 == "8317:structured failure");
    println!("chunk3 pcall2 ok={ok2} err2={err2}");
    let ok3: bool = t3.get("ok3")?;
    let err3: String = t3.get("err3")?;
    assert!(!ok3 && err3.contains("attempt to index a nil value"));
    println!("chunk3 pcall3 ok={ok3} err3={err3}");

    // ---- ④ Rust 注册回调：计数 + 参数断言 ----
    let counter = Rc::new(Cell::new(0i64));
    let counter_in_cb = Rc::clone(&counter);
    let feed = lua.create_function(move |_lua, (i, s): (i64, String)| {
        let prev = counter_in_cb.get();
        assert_eq!(s, format!("arg#{i}"));
        counter_in_cb.set(prev + 1);
        Ok(i * 3 + prev)
    })?;
    globals.set("rust_feed", feed)?;
    let add = lua.create_function(|_lua, (a, b): (i64, i64)| Ok(a + b + 1))?;
    globals.set("rust_add", add)?;
    let t4: Table = lua.load(CHUNK4).set_name("chunk4").eval()?;
    let total: i64 = t4.get("total")?;
    let addsum: i64 = t4.get("addsum")?;
    assert_eq!(total, 300);
    assert_eq!(addsum, 69);
    assert_eq!(counter.get(), 12);
    println!("chunk4 total={total} addsum={addsum} counter={}", counter.get());

    // ---- ⑤ Lua 返回 table 经 Rust 端 BTreeMap 序打印 ----
    let t5: Table = lua.load(CHUNK5).set_name("chunk5").eval()?;
    let stats: Table = t5.get("stats")?;
    let mut map = BTreeMap::new();
    for kv in stats.pairs::<String, i64>() {
        let (k, v) = kv?;
        map.insert(k, v);
    }
    assert_eq!(map.len(), 3);
    for (k, v) in &map {
        println!("chunk5 stat {k}={v}");
    }
    let tags: Table = t5.get("tags")?;
    let tags: Vec<String> = (1..=3i64).map(|i| tags.get::<String>(i)).collect::<Result<_, _>>()?;
    assert_eq!(tags, ["x", "yy", "zzz"]);
    println!("chunk5 tags = {}", tags.join(","));

    // ---- ⑥ 长字符串 >1KB 往返 ----
    let parts = lua.create_table()?;
    for i in 0..64i64 {
        let payload: String = std::iter::repeat((b'A' + (i as u8 % 13)) as char)
            .take(13)
            .collect();
        parts.set(i + 1, format!("seg{i:02}:{payload}"))?;
    }
    globals.set("PARTS", parts)?;
    let (joined6, len6, crc6): (String, i64, i64) = lua.load(CHUNK6).set_name("chunk6").eval()?;
    assert_eq!(len6, 1279);
    assert_eq!(joined6.len() as i64, len6);
    assert!(crc6 > 0);
    println!(
        "chunk6 len={len6} lua_crc={crc6} fnv={:016x} head={} tail={}",
        fnv1a(joined6.as_bytes()),
        &joined6[..16],
        &joined6[joined6.len() - 16..]
    );

    // ---- ⑦ 错码面：Rust 回调错误（跨 thunk longjmp 探针，压最后）----
    let boom = lua.create_function(|_lua, n: i64| -> mlua::Result<i64> {
        if n == 5 {
            Err(mlua::Error::RuntimeError("boom#5".to_string()))
        } else {
            Ok(n)
        }
    })?;
    globals.set("rust_boom", boom)?;
    let t7: Table = lua.load(CHUNK7A).set_name("chunk7a").eval()?;
    let ok7: bool = t7.get("ok")?;
    let errtype: String = t7.get("errtype")?;
    let ok72: bool = t7.get("ok2")?;
    let out2: String = t7.get("out2")?;
    assert!(!ok7 && errtype == "userdata" && ok72 && out2 == "9");
    println!("chunk7a ok={ok7} errtype={errtype} ok2={ok72} out2={out2}");
    match lua.load(CHUNK7B).set_name("chunk7b").exec() {
        Ok(()) => println!("chunk7b unexpected ok"),
        Err(e) => {
            let first = e.to_string().lines().next().unwrap_or("?").to_string();
            let is_cb = matches!(e, mlua::Error::CallbackError { .. });
            assert_eq!(first, "runtime error: boom#5");
            assert!(is_cb);
            println!("chunk7b bubbled={first} is_callback={is_cb}");
        }
    }

    println!("done");
    Ok(())
}
