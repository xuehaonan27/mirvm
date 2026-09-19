#!/usr/bin/env mirvm
---
[dependencies]
# mlua 0.10.5, the newest patch on the 0.10 line (0.11/0.12 exist upstream but are not used
# here). The crate has no default features, so default-features=false strips the helpers
# (no async/send/serialize/macros, keeping the closure minimal). features = vendored + lua54:
# mlua-sys 0.6.8 goes through lua-src 547.0.0 (the Lua 5.4.7 C sources), which cc builds
# statically into a .a that mirvm native-archive loads (the .a -> .so closure plus the crate
# graph dynamic-library link line). That gives heavy FFI over the whole lua_* C API surface
# plus VM-in-VM (a C Lua VM inside the Rust MIR VM). The materialised dependency closure is
# 39 crates (mlua/mlua-sys/lua-src/cc/bstr/either/num-traits/parking-lot/rustc-hash/
# rustversion and others).
mlua = { version = "=0.10.5", default-features = false, features = ["vendored", "lua54"] }
---
// mlua 0.10 (vendored Lua 5.4) three-way differential: half the computation lives in native
// C (the Lua VM executing chunks, the table/string libraries) and half in the interpreted/
// JIT-ed Rust binding layer (value conversion, callbacks, error wrapping). Every FFI
// signature is scalar or pointer (lua_State* / c_int / double / fn-ptr), so no by-value
// aggregate ABI is involved. Callback shape: create_function makes mlua store a
// CallbackUpvalue userdata (with __gc, so lua_close destroys it from native into guest), and
// lua_pushcclosure pushes the monomorphic explicit fn-ptr
// `unsafe extern "C-unwind" fn call_callback`. The error frontier (deliberately last): a Rust
// callback returning Err wraps WrappedFailure through callback_error_ext and calls
// ffi::lua_error (a longjmp on the C side) that jumps across thunk and interpreter frames
// back to lua_pcall's setjmp -- a probe for whether a host longjmp correctly abandons
// in-flight mirvm interpreter calls.
//
// Test surface:
//   ① _VERSION anchor ("Lua 5.4") plus chunk1 arithmetic and tables: integer square
//      accumulation, 10!, a 64-bit 1<<40 computation, and array read-back (eval::<Table>
//      reads by name; five assert_eq calls).
//   ② chunk2 iteration and the string library: gmatch splitting, ipairs joining through
//      table.concat, string.gsub with a count, reverse, and string.format("%05d:%s:%.3f").
//   ③ chunk3 echo and the pcall error surface (pure Lua): vararg echo counting plus
//      tostring joining; pcall catches a plain string error, a structured table error
//      {code=8317,msg} (Lua splits the fields back into a string so no userdata/table
//      address is exposed), and a nil-index error carrying the chunk name and line number.
//   ④ Rust-registered functions called from Lua: rust_feed (Rc<Cell> counting 12 calls,
//      per-call assertion s=="arg#"..i, return value consuming the previous count for
//      cross-call state) and rust_add; Lua loops 12+6 calls, Rust reads back counter==12.
//   ⑤ A Lua-returned table (stats keys out of order plus the tags array) is printed
//      through a Rust BTreeMap in key order, avoiding next order.
//   ⑥ A >1KB string roundtrip: Rust builds the fixed segment table seg00..seg63 (19B x 64)
//      in globals, Lua table.concat's it with "|" (1216+63 = 1279B) and computes a 31-bit
//      rolling hash (mod 1000000007); Rust prints len/lua_crc/fnv1a(hex) and head/tail 16B.
//   ⑦ The error surface (callback_error_ext -> lua_error longjmp across the thunk, last):
//      rust_boom(5) returns Err(RuntimeError "boom#5") -- under pcall, ok=false and
//      type(err)=="userdata" (the body is not printed); with no pcall the error crosses
//      into Rust as Err(CallbackError), printing only the first line "runtime error: boom#5".
//
// Determinism: sources are constant strings and expectations are hardcoded. Lua's string
// hash seed (g->seed mixes time and addresses) makes pairs order unstable across processes,
// so this fixture never iterates unordered tables: Lua uses only ipairs/arrays; string-keyed
// tables come back to Rust into a BTreeMap, values that stringify to an address never reach
// the output, math.random/os.clock/os.time/print are never called, and stderr is empty.
// No floating-point value is printed and the process exits 0.
// Error-location anchors: chunk3 reports the `[string "chunk3"]:11` and `:19` line numbers and
// chunk6 prints fnv=3658152a23cb81fb; all three runs must reproduce these byte-for-byte.
// Section ⑦'s cross-thunk longjmp surface is covered the same way: callback_error_ext ->
// lua_error, with the userdata caught inside pcall and the CallbackError Display line in Rust.
//
// Three-way rerun:
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

    // ---- ⓪ version anchor ----
    let ver: String = lua.load("return _VERSION").set_name("chunk0").eval()?;
    println!("version = {ver}");

    // ---- ① chunk1: arithmetic and tables ----
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

    // ---- ② chunk2: iteration and the string library ----
    let (joined, sub, nsub, rev, fmt): (String, String, i64, String, String) =
        lua.load(CHUNK2).set_name("chunk2").eval()?;
    assert_eq!(joined, "ALPHA#1|BETA#2|GAMMA#3|DELTA#4|EPSILON#5");
    assert_eq!(sub, "c. c. d.");
    assert_eq!(nsub, 3);
    assert_eq!(rev, "rorrim");
    assert_eq!(fmt, "00042:seven:0.125");
    println!("chunk2 joined = {joined}");
    println!("chunk2 sub={sub} nsub={nsub} rev={rev} fmt={fmt}");

    // ---- ③ chunk3: echo and the pcall error surface (pure Lua) ----
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

    // ---- ④ Rust-registered callbacks: counting plus parameter assertions ----
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

    // ---- ⑤ a Lua-returned table printed in Rust BTreeMap order ----
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

    // ---- ⑥ >1KB string roundtrip ----
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

    // ---- ⑦ error surface: Rust callback error (cross-thunk longjmp probe, last) ----
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
