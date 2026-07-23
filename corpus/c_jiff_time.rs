#!/usr/bin/env mirvm
---
[dependencies]
# default-features = false + tzdb-bundle-always：tz 数据全部由 jiff-tzdb
# 以字节表形式编进二进制（IANA 大表，运行期 TZif 解析），不读宿主机
# /usr/share/zoneinfo（tz-system off）——两侧吃的是同一份确定性大表，
# 也顺带排除宿主机 tzdata 版本漂移。std 保留（TimeZone::get 需要 alloc+std）。
# 钉 =0.2.32（2026-07-23 实锤上游破洞）：jiff 0.2.33/0.2.34（07-19 发布）起
# 依赖当日新建的 jiff-core 0.1.0——其 from_nanosecond 越界路径先触
# new_unchecked 的 debug_assert（Timestamp::MAX.as_nanosecond()+1 实锤：
# debug 构建 panic、release 构建静默构造越界值），driver 的越界→Err 断言
# 被上游 panic 顶穿。0.2.32 = 换依赖前最后一版；native/mirvm 同文复现，
# 非分叉。恢复条件 = jiff-core 修该断言后升钉（GitHub 操作暂停，上游
# 报告暂缓）。
jiff = { version = "=0.2.32", default-features = false, features = ["std", "tzdb-bundle-always"] }
---
// jiff 0.2（BurntSushi datetime，内置 IANA tz 数据库）差分。
// 覆盖：① 固定 Unix 时间戳集（epoch/负值/两个闰秒边界前后/N.Y. 两次 DST
// 切换周）× 4 时区（UTC/New_York/Berlin/Tokyo）→ civil 时间 + UTC offset；
// ② 日历算术：±P3M 跨闰年（含 2/29 诞生与月末钳制）、跨 DST 的 P1D vs
// PT24H vs PT25H 对照、±25h 的 Spanned 差（hour 单位 vs day 单位）；
// ③ Parse/Display/ISO8601 roundtrip 谱系：Timestamp（负年/纳秒）/DateTime/
// Date/Time/Zoned（含 gap/fold compat 平移、offset 与 zone 不符错误）/
// ISOWeekDate/Span；④ 星期与 ISO 周推算（W53/W52/W01 跨年角点）；
// ⑤ Timestamp 四则（duration_since/checked_add/checked_sub/until）与
// clamp 夹逼、MIN/MAX 边界与越界错误路径、i128 纳秒位宽。
// 确定性：只打印 civil 时间戳、offset、span、星期、i64/i128 整数与布尔；
// 不调 now()/system tz；错误路径打印 jiff 静态格式化文本（无地址/路径）。
use jiff::civil::{Date, DateTime, ISOWeekDate, Time};
use jiff::tz::TimeZone;
use jiff::{Span, SpanFieldwise, Timestamp, Unit, Zoned};

fn main() {
    // ---- ① 时区装载 + 固定时间戳集 × 4 时区 ----
    let zones = [
        ("UTC", TimeZone::UTC),
        ("New_York", TimeZone::get("America/New_York").unwrap()),
        ("Berlin", TimeZone::get("Europe/Berlin").unwrap()),
        ("Tokyo", TimeZone::get("Asia/Tokyo").unwrap()),
    ];
    for (label, tz) in &zones {
        println!("zone {label}: iana={:?}", tz.iana_name());
    }
    // POSIX TZ 规则串（显式 posix 构造；DST 规则全编码在串里）
    match TimeZone::posix("EST5EDT,M3.2.0/2,M11.1.0/2") {
        Ok(tz) => {
            let z = Timestamp::from_second(1710072000).unwrap().to_zoned(tz);
            println!("posix-tz: ok 2024-03-10T12:00Z => {z}");
        }
        Err(e) => println!("posix-tz: ERR {e}"),
    }

    // epoch / 负值 / 两个 UTC 闰秒边界（23:59:60 出现处）前后 /
    // N.Y. spring-forward 与 fall-back 所在周
    let stamps: [(i64, &str); 14] = [
        (0, "epoch"),
        (1, "epoch+1"),
        (-1, "epoch-1"),
        (-2208988800, "1900-01-01"),
        (78796799, "leap72-before"), // 1972-06-30T23:59:59Z
        (78796800, "leap72-after"),  // 1972-07-01T00:00:00Z
        (1483228799, "leap16-before"), // 2016-12-31T23:59:59Z
        (1483228800, "leap16-after"),  // 2017-01-01T00:00:00Z
        (1709942400, "spring-sat"), // 2024-03-09T00:00:00Z
        (1710028800, "spring-sun"), // 2024-03-10T00:00:00Z
        (1710072000, "spring-noon"), // 2024-03-10T12:00:00Z
        (1730592000, "fall-sun"),   // 2024-11-03T00:00:00Z
        (1730606400, "fall-04z"),   // 2024-11-03T04:00:00Z
        (1730620800, "fall-08z"),   // 2024-11-03T08:00:00Z
    ];
    for (sec, tag) in stamps {
        let ts = Timestamp::from_second(sec).unwrap();
        println!("ts {tag}: sec={sec} ts={ts} nanos={}", ts.as_nanosecond());
        for (label, tz) in &zones {
            let z = ts.to_zoned(tz.clone());
            println!("  {label}: civil={} offset={}", z.datetime(), z.offset());
        }
    }
    // 往回取 Timestamp（Zoned → Timestamp 逆映射）
    let z0: Zoned = "2024-03-10T08:30:00[America/New_York]".parse().unwrap();
    println!("zoned->ts: {} sec={}", z0, z0.timestamp().as_second());

    // ---- ② 日历算术：±P3M 跨闰年、DST 下 P1D vs PT24H/PT25H ----
    for (d, span, tag) in [
        ("2023-11-30", "P3M", "nov23+3m"),  // → 2024-02-29（闰日诞生）
        ("2024-02-29", "-P3M", "feb24-3m"), // → 2023-11-29/30?
        ("2024-01-31", "P1M", "jan31+1m"),  // 月末钳制 → 2024-02-29
        ("2024-03-31", "-P1M", "mar31-1m"), // → 2024-02-29
        ("2024-02-29", "P12M", "leap+12m"), // 闰日钳制 → 2025-02-28
        ("2025-02-28", "-P12M", "feb25-12m"),
    ] {
        let d: Date = d.parse().unwrap();
        let s: Span = span.parse().unwrap();
        let got = d.checked_add(s).unwrap();
        println!("cal {tag}: {d} + {span} => {got}");
    }
    // 跨 DST：calendar day vs 24h
    let zs: Zoned = "2024-03-09T12:00:00[America/New_York]".parse().unwrap();
    let z_p1d = zs.checked_add(Span::new().days(1)).unwrap();
    let z_24h = zs.checked_add(Span::new().hours(24)).unwrap();
    let z_25h = zs.checked_add(Span::new().hours(25)).unwrap();
    println!("spring +P1D   => {z_p1d}");
    println!("spring +PT24H => {z_24h}");
    println!("spring +PT25H => {z_25h}");
    println!(
        "spring spanned: until-hour={} until-day={}",
        z_25h.since((Unit::Hour, &zs)).unwrap(),
        z_25h.since((Unit::Day, &zs)).unwrap()
    );
    let zf: Zoned = "2024-11-02T12:00:00[America/New_York]".parse().unwrap();
    let zf_p1d = zf.checked_add(Span::new().days(1)).unwrap();
    let zf_25h = zf.checked_add(Span::new().hours(25)).unwrap();
    println!("fall +P1D   => {zf_p1d}");
    println!("fall +PT25H => {zf_25h}");
    println!(
        "fall spanned: P1D-real-hours={} 25h-as-days={}",
        zf_p1d.since((Unit::Hour, &zf)).unwrap(),
        zf_25h.since((Unit::Day, &zf)).unwrap()
    );
    // gap/fold compat 平移
    let gap: Zoned = "2024-03-10T02:30:00[America/New_York]".parse().unwrap();
    let fold: Zoned = "2024-11-03T01:30:00[America/New_York]".parse().unwrap();
    println!("gap  02:30 => {gap}");
    println!("fold 01:30 => {fold}");

    // ---- ③ Parse/Display/ISO8601 roundtrip 谱系 ----
    // 幂等式：Display 输出 re-parse 须逐字节稳定且值相等
    for s in [
        "1970-01-01T00:00:00Z",
        "1900-01-01T00:00:00Z",
        "2024-06-01T12:34:56.789012345Z",
        "-009900-06-15T08:00:00Z",
        "9999-01-01T00:00:00Z",
    ] {
        let t: Timestamp = s.parse().unwrap();
        let s2 = t.to_string();
        let t2: Timestamp = s2.parse().unwrap();
        println!("rt ts {s} => {s2} ok={}", t == t2 && s2 == t2.to_string());
    }
    for s in [
        "2024-02-29T23:59:59.5",
        "1900-01-01T00:00:00",
        "-009900-01-01T00:00:00",
        "9999-12-31T23:59:59.999999999",
    ] {
        let dt: DateTime = s.parse().unwrap();
        let s2 = dt.to_string();
        println!("rt datetime {s} => {s2} ok={}", s2.parse::<DateTime>().unwrap() == dt);
    }
    for s in ["1970-01-01", "2024-02-29", "9999-12-31", "-009999-01-01"] {
        let d: Date = s.parse().unwrap();
        let s2 = d.to_string();
        println!("rt date {s} => {s2} ok={}", s2.parse::<Date>().unwrap() == d);
    }
    for s in ["00:00:00", "23:59:59.999999999", "12:34:56.5"] {
        let t: Time = s.parse().unwrap();
        let s2 = t.to_string();
        println!("rt time {s} => {s2} ok={}", s2.parse::<Time>().unwrap() == t);
    }
    for s in [
        "2024-03-10T03:30:00-04:00[America/New_York]",
        "1970-01-01T00:00:00+00:00[UTC]",
        "2024-07-01T09:00:00+09:00[Asia/Tokyo]",
    ] {
        let z: Zoned = s.parse().unwrap();
        let s2 = z.to_string();
        println!("rt zoned {s} => {s2} ok={}", s2.parse::<Zoned>().unwrap() == z);
    }
    for s in ["2015-W53-5", "2016-W52-7", "2020-W01-1", "2024-W09-4"] {
        let w: ISOWeekDate = s.parse().unwrap();
        let s2 = w.to_string();
        println!("rt isoweek {s} => {s2} ok={}", s2.parse::<ISOWeekDate>().unwrap() == w);
        // ISOWeekDate → Date 换算
        println!("  isoweek->date {} => {}", w, w.date());
    }
    for s in ["P1Y2M3DT4H5M6.789012345S", "PT25H", "-P3M", "P0D", "PT0.000000001S"] {
        let sp: Span = s.parse().unwrap();
        let s2 = sp.to_string();
        let ok = SpanFieldwise(s2.parse::<Span>().unwrap()) == SpanFieldwise(sp);
        println!("rt span {s} => {s2} ok={ok}");
    }
    // 错误路径（jiff 错误文本为确定性静态格式）
    // 注：闰秒 60 不是错误——jiff 宽容解析并钳制到 :59（leap-second tolerant）
    let leap: Timestamp = "2016-12-31T23:59:60Z".parse().unwrap();
    println!("leap60 clamped => {leap}");
    match "2024-13-01".parse::<Date>() {
        Ok(v) => println!("err date-13: unexpected {v}"),
        Err(e) => println!("err date-13: {e}"),
    }
    match "2023-02-29".parse::<Date>() {
        Ok(v) => println!("err date-feb29: unexpected {v}"),
        Err(e) => println!("err date-feb29: {e}"),
    }
    match "garbage".parse::<Timestamp>() {
        Ok(v) => println!("err garbage: unexpected {v}"),
        Err(e) => println!("err garbage: {e}"),
    }
    match "2024-01-01T00:00:00Z[Mars/Olympus_Mons]".parse::<Zoned>() {
        Ok(v) => println!("err badzone: unexpected {v}"),
        Err(e) => println!("err badzone: {e}"),
    }
    // offset 与 zone 实际 offset 不一致 → 拒绝
    match "2024-03-10T03:30:00-05:00[America/New_York]".parse::<Zoned>() {
        Ok(v) => println!("err offmismatch: unexpected {v}"),
        Err(e) => println!("err offmismatch: {e}"),
    }
    match "PQ".parse::<Span>() {
        Ok(v) => println!("err span: unexpected {v}"),
        Err(e) => println!("err span: {e}"),
    }

    // ---- ④ 星期 / ISO 周推算（跨年角点 + 闰日 + 范围极值）----
    for s in [
        "1900-01-01", // 周一
        "1970-01-01", // 周四
        "2000-01-01", // 周六
        "2015-12-31", // 2015-W53-4
        "2016-01-01", // 2015-W53-5
        "2017-01-01", // 2016-W52-7
        "2019-12-30", // 2020-W01-1
        "2024-02-29", // 闰日
        "2024-12-31", // 2025-W01-2
        "9999-12-31",
    ] {
        let d: Date = s.parse().unwrap();
        let w = d.iso_week_date();
        println!(
            "wk {s}: weekday={:?} ordinal={} iso={} (y{} w{} {:?})",
            d.weekday(),
            d.day_of_year(),
            w,
            w.year(),
            w.week(),
            w.weekday()
        );
    }

    // ---- ⑤ Timestamp 四则、夹逼与边界 ----
    let a = Timestamp::from_second(1_234_567_890).unwrap();
    let b = Timestamp::from_second(-987_654_321).unwrap();
    let diff = a.duration_since(b);
    println!("arith: a={a} b={b}");
    println!("arith: a-b secs={} nanos={}", diff.as_secs(), diff.as_nanos());
    println!("arith: b+(a-b)==a {}", b.checked_add(diff).unwrap() == a);
    println!(
        "arith: a.until(b)={} b.since(a)={}",
        a.until((Unit::Second, b)).unwrap(),
        b.since((Unit::Second, a)).unwrap()
    );
    let a2 = a.checked_add(Span::new().hours(100)).unwrap();
    println!("arith: a+PT100H={} back-ok={}", a2, a2.checked_sub(Span::new().hours(100)).unwrap() == a);
    // 夹逼（std Ord::clamp）
    let lo = Timestamp::UNIX_EPOCH;
    let hi = Timestamp::from_second(1_000_000).unwrap();
    for v in [-5i64, 500, 2_000_000] {
        let t = Timestamp::from_second(v).unwrap();
        println!("clamp[{lo},{hi}] {v} => {}", t.clamp(lo, hi).as_second());
    }
    // MIN/MAX 与越界错误路径；i128 纳秒位宽
    println!("bound: min={} nanos={}", Timestamp::MIN, Timestamp::MIN.as_nanosecond());
    println!("bound: max={} nanos={}", Timestamp::MAX, Timestamp::MAX.as_nanosecond());
    // MIN/MAX 的 Display→parse roundtrip（边界值本身动态往返，不写死字符串）
    let (smin, smax) = (Timestamp::MIN.to_string(), Timestamp::MAX.to_string());
    println!(
        "bound: min-rt={} max-rt={}",
        smin.parse::<Timestamp>().unwrap() == Timestamp::MIN,
        smax.parse::<Timestamp>().unwrap() == Timestamp::MAX
    );
    match Timestamp::from_second(i64::MAX) {
        Ok(v) => println!("bound: from_second(MAX) unexpected {v}"),
        Err(e) => println!("bound: from_second(MAX) ERR {e}"),
    }
    match Timestamp::from_second(i64::MIN) {
        Ok(v) => println!("bound: from_second(MIN) unexpected {v}"),
        Err(e) => println!("bound: from_second(MIN) ERR {e}"),
    }
    match Timestamp::MAX.checked_add(Span::new().seconds(1)) {
        Ok(v) => println!("bound: max+1s unexpected {v}"),
        Err(e) => println!("bound: max+1s ERR {e}"),
    }
    match Timestamp::MIN.checked_sub(Span::new().seconds(1)) {
        Ok(v) => println!("bound: min-1s unexpected {v}"),
        Err(e) => println!("bound: min-1s ERR {e}"),
    }
    match Timestamp::from_nanosecond(Timestamp::MAX.as_nanosecond() + 1) {
        Ok(v) => println!("bound: from_ns(max+1) unexpected {v}"),
        Err(e) => println!("bound: from_ns(max+1) ERR {e}"),
    }
    let en = Timestamp::from_nanosecond(-987_654_321_000_000_001i128).unwrap();
    println!("bound: from_ns(neg)={} nanos={}", en, en.as_nanosecond());
}
