#!/usr/bin/env mirvm
---
[dependencies]
redb = "2"
---
// redb 2.6：纯 Rust 嵌入式 KV（B-tree 页存储 + xxh3 页 checksum + flock 文件锁，
// 无 mmap、零依赖）。覆盖：temp_dir 建库；单 write txn 跨双表插入（u64→&[u8] 与
// &str→u64 复合）；read txn 点查（命中/未命中）+ range 正/反向扫描（BTree 确定序）
// + first/last/len/iter 计数锚点；同 txn 更新（insert 覆盖回旧值）与删除（remove
// 回旧值 / 未命中 None / pop_last）；显式 abort() 与 drop 隐式 abort 的不可见性；
// MVCC 快照隔离（commit 前开启的 read txn 看不到新提交）；同 txn 重复 open_table
// 的 TableAlreadyOpen 与不存在表的 TableDoesNotExist 错误路径；drop 后重开的
// 持久性校验（含文件长度锚点）；1MB 大 value 写入/读回/二次重开持久性
// （len+FNV-1a+eq）。结尾 remove_file 清理（开头也清一次，多跑不累加）。
//
// 确定性：数据全部由定种 xorshift64* 生成；输出只有计数/长度/排序键列/FNV 哈希/
// 布尔断言与 redb 错误 Display（TableDoesNotExist 仅含表名；TableAlreadyOpen 含
// 首个 open 的 Location——两维共用同一 materialized src/main.rs 与同一 redb 源
// 路径，行/列逐字节一致）。不打印路径/时间/地址；stderr 为空。
// 页 checksum 的 xxh3 在 >240B 页上走 AVX2 路径，但本 nightly 的 core_arch 把它
// 实现为通用 simd_*（非 llvm.x86.* 专用降级），mirvm Simd IR 直接覆盖，与 native
// 按 xxh3 算法定义逐位一致——无 FRONTIER。
//
// 注：AccessGuard 带 Drop，借用延续到作用域结束——取值一律块内提取后立即释放。
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};

const T_NUM: TableDefinition<u64, &[u8]> = TableDefinition::new("num_kv");
const T_STR: TableDefinition<&str, u64> = TableDefinition::new("str_kv");
const T_NOPE: TableDefinition<u64, u64> = TableDefinition::new("nope");

/// 定种 xorshift64* PRNG（native/mirvm 同序列）。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(n);
        out
    }
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 第 i 行的确定性值（长度 8..=55B 变化）。
fn row_val(i: u64) -> Vec<u8> {
    let len = 8 + (i * 13) % 48;
    Rng(0x5EED_0000 + i).bytes(len as usize)
}

fn main() {
    let path = std::env::temp_dir().join("mirvm_c_redb_kv.redb");
    let _ = std::fs::remove_file(&path); // 起点清空，多跑不累加

    // ---- ① 建库 + 单 write txn 跨双表插入 ----
    let db = Database::create(&path).unwrap();
    let w = db.begin_write().unwrap();
    {
        let mut tn = w.open_table(T_NUM).unwrap();
        for i in 0..40u64 {
            tn.insert(i, row_val(i).as_slice()).unwrap();
        }
        let mut ts = w.open_table(T_STR).unwrap();
        for i in 0..25u64 {
            ts.insert(format!("user:{i:03}").as_str(), i * i).unwrap();
        }
    }
    w.commit().unwrap();
    println!("[1] committed num=40 str=25");

    // ---- ② read txn：点查 + range 扫描（确定序）----
    let r = db.begin_read().unwrap();
    let tn = r.open_table(T_NUM).unwrap();
    let ts = r.open_table(T_STR).unwrap();
    println!(
        "[2] len num={} str={} empty={}",
        tn.len().unwrap(),
        ts.len().unwrap(),
        tn.is_empty().unwrap()
    );
    let (g7_len, g7_fnv, g7_eq) = {
        let g = tn.get(7u64).unwrap().unwrap();
        (g.value().len(), fnv1a(g.value()), g.value() == row_val(7).as_slice())
    };
    println!("[2] get k=7 len={} fnv={:016x} expect_eq={}", g7_len, g7_fnv, g7_eq);
    println!("[2] get k=1000 none={}", tn.get(1000u64).unwrap().is_none());
    println!(
        "[2] get user:007 v={} miss999_none={}",
        ts.get("user:007").unwrap().unwrap().value(),
        ts.get("user:999").unwrap().is_none()
    );
    let (fkv, fvl, lkv, lvl) = {
        let (fk, fv) = tn.first().unwrap().unwrap();
        let (lk, lv) = tn.last().unwrap().unwrap();
        (fk.value(), fv.value().len(), lk.value(), lv.value().len())
    };
    println!("[2] first k={} vlen={} last k={} vlen={}", fkv, fvl, lkv, lvl);
    let mut keys = Vec::new();
    let mut lens = Vec::new();
    for item in tn.range(5u64..15u64).unwrap() {
        let (k, v) = item.unwrap();
        keys.push(k.value().to_string());
        lens.push(v.value().len().to_string());
    }
    println!("[2] range 5..15 keys={} lens={}", keys.join(","), lens.join(","));
    let mut rkeys = Vec::new();
    for item in tn.range(30u64..40u64).unwrap().rev() {
        let (k, _v) = item.unwrap();
        rkeys.push(k.value().to_string());
    }
    println!("[2] range 30..40 rev keys={}", rkeys.join(","));
    let mut pairs = Vec::new();
    for item in ts.range("user:010"..="user:014").unwrap() {
        let (k, v) = item.unwrap();
        pairs.push(format!("{}={}", k.value(), v.value()));
    }
    println!("[2] str range [user:010,user:014] {}", pairs.join(","));
    let mut n = 0u64;
    let mut acc = 0u64;
    for item in tn.iter().unwrap() {
        let (k, v) = item.unwrap();
        n += 1;
        acc = acc.wrapping_add(k.value()).wrapping_add(v.value()[0] as u64);
    }
    println!("[2] iter n={} acc={}", n, acc);
    drop(ts);
    drop(tn);
    drop(r);

    // ---- ③ 同 txn 更新 + 删除（旧值回传）----
    let w = db.begin_write().unwrap();
    {
        let mut tn = w.open_table(T_NUM).unwrap();
        let mut ts = w.open_table(T_STR).unwrap();
        let new7 = Rng(0xAAAA_0007).bytes(100);
        let (ol, ofnv, oeq) = {
            let old = tn.insert(7u64, new7.as_slice()).unwrap().unwrap();
            (old.value().len(), fnv1a(old.value()), old.value() == row_val(7).as_slice())
        };
        println!("[3] overwrite k=7 old_len={} old_fnv={:016x} old_expect={}", ol, ofnv, oeq);
        println!(
            "[3] insert fresh k=900 old_none={}",
            tn.insert(900u64, row_val(900).as_slice()).unwrap().is_none()
        );
        let rm8_len = {
            let rm8 = tn.remove(8u64).unwrap().unwrap();
            rm8.value().len()
        };
        println!(
            "[3] remove k=8 old_len={} again_none={}",
            rm8_len,
            tn.remove(8u64).unwrap().is_none()
        );
        let rms_v = {
            let rms = ts.remove("user:003").unwrap().unwrap();
            rms.value()
        };
        println!("[3] remove user:003 old_v={}", rms_v);
        let (pkv, pvv) = {
            let (pk, pv) = ts.pop_last().unwrap().unwrap();
            (pk.value().to_string(), pv.value())
        };
        println!("[3] str pop_last k={} v={}", pkv, pvv);
    }
    w.commit().unwrap();
    println!("[3] committed file_len={}", std::fs::metadata(&path).unwrap().len());
    let r = db.begin_read().unwrap();
    let tn = r.open_table(T_NUM).unwrap();
    let ts = r.open_table(T_STR).unwrap();
    let (p7_len, p7_fnv) = {
        let g = tn.get(7u64).unwrap().unwrap();
        (g.value().len(), fnv1a(g.value()))
    };
    println!("[3] post k=7 len={} fnv={:016x}", p7_len, p7_fnv);
    println!(
        "[3] post none k=8={} user:003={} user:024={} has900={}",
        tn.get(8u64).unwrap().is_none(),
        ts.get("user:003").unwrap().is_none(),
        ts.get("user:024").unwrap().is_none(),
        tn.get(900u64).unwrap().is_some()
    );
    drop(ts);
    drop(tn);
    drop(r);

    // ---- ④ abort 语义：显式 abort / drop 隐式 abort / abort 撤销删除 ----
    let a1 = db.begin_write().unwrap();
    {
        let mut tn = a1.open_table(T_NUM).unwrap();
        tn.insert(1001u64, row_val(1001).as_slice()).unwrap();
        let mut ts = a1.open_table(T_STR).unwrap();
        ts.insert("temp:001", 777u64).unwrap();
    }
    a1.abort().unwrap();
    let a2 = db.begin_write().unwrap();
    {
        let mut tn = a2.open_table(T_NUM).unwrap();
        tn.insert(1002u64, row_val(1002).as_slice()).unwrap();
    }
    drop(a2); // 未 commit 直接 drop = 隐式 abort
    let a3 = db.begin_write().unwrap();
    {
        let mut tn = a3.open_table(T_NUM).unwrap();
        tn.remove(9u64).unwrap();
    }
    a3.abort().unwrap();
    let r = db.begin_read().unwrap();
    let tn = r.open_table(T_NUM).unwrap();
    let ts = r.open_table(T_STR).unwrap();
    println!(
        "[4] aborted none k1001={} k1002={} temp:001={} k9_alive={}",
        tn.get(1001u64).unwrap().is_none(),
        tn.get(1002u64).unwrap().is_none(),
        ts.get("temp:001").unwrap().is_none(),
        tn.get(9u64).unwrap().is_some()
    );
    drop(ts);
    drop(tn);
    drop(r);

    // ---- ⑤ MVCC：commit 前开启的 read txn 看不到新提交 ----
    let r0 = db.begin_read().unwrap();
    let w = db.begin_write().unwrap();
    {
        let mut tn = w.open_table(T_NUM).unwrap();
        tn.insert(555u64, row_val(555).as_slice()).unwrap();
    }
    w.commit().unwrap();
    let t0 = r0.open_table(T_NUM).unwrap();
    let r1 = db.begin_read().unwrap();
    let t1 = r1.open_table(T_NUM).unwrap();
    println!(
        "[5] snapshot old_none={} new_some={}",
        t0.get(555u64).unwrap().is_none(),
        t1.get(555u64).unwrap().is_some()
    );
    drop(t0);
    drop(t1);
    drop(r0);
    drop(r1);

    // ---- ⑥ 错误路径：不存在表 / 同 txn 重复 open_table ----
    let r = db.begin_read().unwrap();
    match r.open_table(T_NOPE) {
        Ok(_) => println!("[6] open nope unexpected ok"),
        Err(e) => println!("[6] open nope err: {e}"),
    }
    drop(r);
    let w = db.begin_write().unwrap();
    {
        let _t = w.open_table(T_NUM).unwrap();
        match w.open_table(T_NUM) {
            Ok(_) => println!("[6] double-open unexpected ok"),
            Err(e) => println!("[6] double-open err: {e}"),
        }
    }
    w.abort().unwrap();

    // ---- ⑦ drop 后重开：持久性校验 ----
    drop(db);
    let db = Database::create(&path).unwrap();
    let r = db.begin_read().unwrap();
    let tn = r.open_table(T_NUM).unwrap();
    let ts = r.open_table(T_STR).unwrap();
    println!("[7] reopen len num={} str={}", tn.len().unwrap(), ts.len().unwrap());
    let (r7_len, r7_fnv) = {
        let g = tn.get(7u64).unwrap().unwrap();
        (g.value().len(), fnv1a(g.value()))
    };
    println!(
        "[7] k=7 len={} fnv={:016x} has555={} has900={} none1001={}",
        r7_len,
        r7_fnv,
        tn.get(555u64).unwrap().is_some(),
        tn.get(900u64).unwrap().is_some(),
        tn.get(1001u64).unwrap().is_none()
    );
    println!(
        "[7] user:007 v={} none user:003={}",
        ts.get("user:007").unwrap().unwrap().value(),
        ts.get("user:003").unwrap().is_none()
    );
    drop(ts);
    drop(tn);
    drop(r);

    // ---- ⑧ 1MB 大 value：写入 / 读回 / 二次重开持久性 ----
    let big = Rng(0xB16B_00B5).bytes(1 << 20);
    println!("[8] big gen len={} fnv={:016x}", big.len(), fnv1a(&big));
    let w = db.begin_write().unwrap();
    {
        let mut tn = w.open_table(T_NUM).unwrap();
        tn.insert(u64::MAX, big.as_slice()).unwrap();
    }
    w.commit().unwrap();
    println!("[8] committed file_len={}", std::fs::metadata(&path).unwrap().len());
    let (bl, bfnv, beq) = {
        let r = db.begin_read().unwrap();
        let tn = r.open_table(T_NUM).unwrap();
        let v = tn.get(u64::MAX).unwrap().unwrap();
        (v.value().len(), fnv1a(v.value()), v.value() == big.as_slice())
    };
    println!("[8] readback len={} fnv={:016x} eq={}", bl, bfnv, beq);
    drop(db);
    let db = Database::create(&path).unwrap();
    let (rl, rfnv, req, rfkv, rlast_max) = {
        let r = db.begin_read().unwrap();
        let tn = r.open_table(T_NUM).unwrap();
        let v = tn.get(u64::MAX).unwrap().unwrap();
        let f = tn.first().unwrap().unwrap();
        let l = tn.last().unwrap().unwrap();
        (
            v.value().len(),
            fnv1a(v.value()),
            v.value() == big.as_slice(),
            f.0.value(),
            l.0.value() == u64::MAX,
        )
    };
    println!("[8] reopened len={} fnv={:016x} eq={}", rl, rfnv, req);
    println!("[8] reopened first k={} last_is_max={}", rfkv, rlast_max);
    drop(db);

    // ---- ⑨ 清理临时库文件 ----
    std::fs::remove_file(&path).unwrap();
    println!("[9] removed={}", !path.exists());
    println!("redb_kv ok");
}
