#!/usr/bin/env mirvm
---
[dependencies]
boa_engine = "0.20"
---
// boa_engine 0.20, a pure-Rust JS engine (boa_parser -> bytecompiler -> its own VM): one
// Context evals a fixed sequence of JS snippets and Rust prints each JsValue's Display and
// numeric bit pattern (pinned with to_bits). Covers arithmetic and number boundaries, string
// templates, array map/filter/reduce, closure counters, objects and prototype chains, nested
// JSON roundtrips, try/catch exception text, regex replace (the regress engine), Map/Set
// iteration order, BigInt, Unicode case mapping and TypedArrays. Fully deterministic: no
// Date/Math.random/crypto; key order is integer keys ascending, then insertion order.
use boa_engine::{Context, Source};

/// Eval one JS snippet and print a structured line: the full Display text plus numeric bits.
fn eval_one(ctx: &mut Context, tag: &str, src: &str) {
    match ctx.eval(Source::from_bytes(src)) {
        Ok(v) => {
            let extra = if let Some(n) = v.as_number() {
                format!(" bits={:016x}", n.to_bits())
            } else {
                String::new()
            };
            println!("{tag} => {}{extra}", v.display());
        }
        Err(e) => println!("{tag} !! {e}"),
    }
}

fn main() {
    let mut ctx = Context::default();

    // ① Arithmetic: i32 fast path / f64 family / boundary bit patterns
    eval_one(&mut ctx, "arith.int", "(1 + 2 * 3 - 4 / 2) * 7 % 3");
    eval_one(&mut ctx, "arith.pow", "2 ** 10");
    eval_one(&mut ctx, "arith.float", "0.1 + 0.2");
    eval_one(&mut ctx, "arith.div", "7 / 2");
    eval_one(&mut ctx, "arith.negzero", "-0");
    eval_one(&mut ctx, "arith.inf", "(1 / 0) + '|' + (-1 / 0)");
    eval_one(&mut ctx, "arith.nan", "0 / 0 === 0 / 0");
    eval_one(&mut ctx, "arith.safeint", "Number.MAX_SAFE_INTEGER + 1");
    eval_one(&mut ctx, "arith.epsilon", "Number.EPSILON");
    eval_one(&mut ctx, "arith.hex", "(255).toString(16) + '|' + parseInt('0x1f') + '|' + parseFloat('2.5e3')");

    // ② String templates and string methods
    eval_one(&mut ctx, "tpl.basic", "`sum=${1 + 2} prod=${3 * 4}`");
    eval_one(&mut ctx, "tpl.nested", "`${`inner ${[1, 2].map(x => x * x).join('+')}`}`");
    eval_one(&mut ctx, "str.methods", "'abc'.repeat(2) + '|' + 'xy'.padStart(5, '0') + '|' + 'Hello'.slice(1, 3)");
    eval_one(&mut ctx, "str.unicode", "'汉字abc'.length + '|' + 'ß'.toUpperCase() + '|' + String.fromCodePoint(0x1F600)");

    // ③ Array higher-order functions
    eval_one(
        &mut ctx,
        "arr.mrf",
        "[...Array(10).keys()].map(x => x * x).filter(x => x % 2 === 0).reduce((a, b) => a + b, 0)",
    );
    eval_one(&mut ctx, "arr.chain", "[3, 1, 2].sort((a, b) => a - b).concat([9, 8]).slice(1, 4).join(',')");
    eval_one(&mut ctx, "arr.flat", "[[1, 2], [3, [4]]].flat(2).reduceRight((a, b) => a + '-' + b)");
    eval_one(&mut ctx, "arr.find", "[5, 12, 8, 130].find(x => x > 10) + '|' + [1, 2, 3].every(x => x > 0) + '|' + [1, 2, 3].some(x => x > 2)");

    // ④ Closure counters: two independent instances, plus a parameterized adder
    eval_one(
        &mut ctx,
        "closure.counter",
        "(() => { const mk = () => { let n = 0; return () => ++n; }; \
         const a = mk(), b = mk(); a(); a(); b(); return a() + '|' + b(); })()",
    );
    eval_one(
        &mut ctx,
        "closure.adder",
        "(() => { const add = x => y => x + y; return add(10)(1) + '|' + add('a')('b'); })()",
    );

    // ⑤ Objects and prototype chains
    eval_one(
        &mut ctx,
        "proto.chain",
        "(() => { function Animal(name) { this.name = name; } \
         Animal.prototype.speak = function() { return this.name + ' speaks'; }; \
         function Dog(name) { Animal.call(this, name); } \
         Dog.prototype = Object.create(Animal.prototype); \
         Dog.prototype.constructor = Dog; \
         Dog.prototype.speak = function() { return this.name + ' barks'; }; \
         const d = new Dog('rex'); \
         return d.speak() + '|' + (d instanceof Dog) + '|' + (d instanceof Animal) + '|' \
           + d.hasOwnProperty('name') + '|' + d.hasOwnProperty('speak'); })()",
    );
    eval_one(
        &mut ctx,
        "proto.keys",
        "(() => { const o = { z: 1, a: 2, 10: 'x', 2: 'y' }; delete o.z; o.m = 3; \
         return Object.keys(o).join(',') + '|' + ('a' in o) + '|' + (Object.getPrototypeOf(o) === Object.prototype); })()",
    );
    eval_one(
        &mut ctx,
        "proto.entries",
        "Object.entries({ b: 1, a: 'x', c: true }).map(([k, v]) => k + '=' + v).join(';')",
    );

    // ⑥ JSON: nested stringify / key order / parse roundtrip / replacer
    eval_one(
        &mut ctx,
        "json.nested",
        "JSON.stringify({ name: 'boa', list: [1, 2.5, null, true], deep: { a: [{ b: '汉字' }] } })",
    );
    eval_one(
        &mut ctx,
        "json.keyorder",
        "JSON.stringify({ b: 1, 10: 'x', 2: 'y', a: 2 })",
    );
    eval_one(
        &mut ctx,
        "json.roundtrip",
        "(() => { const s = JSON.stringify({ n: 9007199254740991, u: 'é汉', arr: [1e21, -0.5] }); \
         const back = JSON.parse(s); return s + '|' + (back.n === 9007199254740991) + '|' + back.arr[0]; })()",
    );
    eval_one(
        &mut ctx,
        "json.replacer",
        "JSON.stringify({ a: 1, skip: undefined, f: function(){}, sym: Symbol('s'), keep: [1, 2] })",
    );

    // ⑦ try-catch exception families: name|message extracted in JS (deterministic text)
    eval_one(
        &mut ctx,
        "err.typeerror",
        "(() => { try { null.x; } catch (e) { return e.constructor.name + '|' + e.message; } })()",
    );
    eval_one(
        &mut ctx,
        "err.reference",
        "(() => { try { return nosuchvar; } catch (e) { return e.constructor.name + '|' + e.message; } })()",
    );
    eval_one(
        &mut ctx,
        "err.range",
        "(() => { try { new Array(-1); } catch (e) { return e.constructor.name + '|' + e.message; } })()",
    );
    eval_one(
        &mut ctx,
        "err.syntax",
        "(() => { try { JSON.parse('{bad'); } catch (e) { return e.constructor.name + '|' + e.message; } })()",
    );
    eval_one(
        &mut ctx,
        "err.custom",
        "(() => { try { throw new TypeError('custom boom'); } catch (e) { return e.name + '|' + e.message; } })()",
    );
    eval_one(
        &mut ctx,
        "err.finally",
        "(() => { const log = []; try { throw 'str-throw'; } catch (e) { log.push('caught:' + e); } \
         finally { log.push('fin'); } return log.join('>'); })()",
    );
    // Uncaught exception: exercises the Rust-side JsError Display path
    eval_one(&mut ctx, "err.uncaught", "throw new Error('uncaught boom')");

    // ⑧ Regex (regress engine): replace / capture groups / split / match / test
    eval_one(&mut ctx, "re.global", "'hello world'.replace(/o/g, '0')");
    eval_one(&mut ctx, "re.groups", "'2024-01-02'.replace(/(\\d+)-(\\d+)-(\\d+)/, '$3/$2/$1')");
    eval_one(&mut ctx, "re.split", "'a1b22c333d'.split(/\\d+/).join('|')");
    eval_one(&mut ctx, "re.match", "'The quick brown fox'.match(/\\b\\w{5}\\b/g).join(',')");
    eval_one(&mut ctx, "re.test", "/^\\p{Script=Han}+$/u.test('汉字') + '|' + /\\d+/.test('abc')");
    eval_one(&mut ctx, "re.func", "'abc def'.replace(/\\w+/g, w => w.length)");

    // ⑨ Map/Set order: insertion order, re-set keeps its place, delete+re-add to tail, SameValueZero
    eval_one(
        &mut ctx,
        "map.order",
        "(() => { const m = new Map(); m.set('b', 1); m.set('a', 2); m.set(10, 'x'); m.set(2, 'y'); \
         m.set('b', 11); m.delete('a'); m.set('a', 22); \
         return [...m.entries()].map(([k, v]) => k + '=' + v).join(';') + '|' + m.size + '|' + m.has(2); })()",
    );
    eval_one(
        &mut ctx,
        "set.order",
        "(() => { const s = new Set([3, 1, 3, 'x', NaN, NaN, 1]); s.add(2); s.delete('x'); \
         return [...s].map(v => typeof v === 'number' && Number.isNaN(v) ? 'NaN' : String(v)).join(',') \
           + '|' + s.size + '|' + s.has(NaN); })()",
    );

    // ⑩ BigInt / TypedArray / bit operations to close
    eval_one(&mut ctx, "bigint.mul", "12345678901234567890n * 98765432109876543210n");
    eval_one(&mut ctx, "bigint.asint", "BigInt.asIntN(8, 255n) + '|' + BigInt.asUintN(8, -1n)");
    eval_one(
        &mut ctx,
        "typed.wrap",
        "(() => { const u = new Uint8Array([255, 1, 2]); u[0] = u[0] + 1; \
         const i = new Int32Array([3, -1, 2]); i.sort(); \
         return [...u].join(',') + '|' + [...i].join(',') + '|' + u.BYTES_PER_ELEMENT + ',' + i.byteLength; })()",
    );
    eval_one(
        &mut ctx,
        "typed.dataview",
        "(() => { const b = new ArrayBuffer(4); const v = new DataView(b); \
         v.setUint16(0, 0x1234, false); v.setUint16(2, 0x1234, true); \
         return [...new Uint8Array(b)].map(x => x.toString(16).padStart(2, '0')).join(''); })()",
    );

    // ⑪ Rust API surface: ToString coercion / ToBoolean / type predicates
    let v = ctx.eval(Source::from_bytes("[1, [2, 3], 'x']")).unwrap();
    let s = v.to_string(&mut ctx).unwrap();
    println!("api.toString => {}", s.to_std_string_escaped());
    println!("api.toBoolean => {}", v.to_boolean());
    let v2 = ctx.eval(Source::from_bytes("'42'")).unwrap();
    println!("api.is => str={} num={} null={}", v2.is_string(), v2.is_number(), v2.is_null_or_undefined());
}
