#!/usr/bin/env mirvm
---
[dependencies]
whatlang = "0.18"
---
// whatlang 0.18 语言检测差分：无外部数据文件的纯静态模型——字母模型
// （每语言字母串 → LazyLock 反查表 → 字符打分）+ 三元模型（hashbrown
// SwissTable 计 trigram 频次 → (count,trigram) 全序排序转位次表 → 与静态
// 语言档案算 Manhattan 距离），combined 按字母权重合并两路 f64 分数，
// confidence 为双曲函数。全部 f64 计数->分数->confidence 链是 MIR 逐条
// 浮点运算 + 128 位/向量无关的标量路径；输出一律 conf.to_bits() 锁位型。
// 覆盖：detect/detect_lang/detect_script 自由函数三件套；Info 的
// lang/script/confidence/is_reliable；Detector::{new,with_allowlist,
// with_denylist,detect,detect_lang,detect_script}（含名单过滤出 None 的
// 边界、Mandarin/Japanese 判别的 allow/deny 两支）；Lang::{all,from_code,
// code,name,eng_name} 全量 roundtrip + FromStr 错误路径；Script::{all,name,
// langs} + FromStr 错误路径；Info::new 手工重建与 PartialEq/Debug。
// 样本：en/fr/de/ru/zh/ja/ko/es/ar/hi 十语长句 + 多语混排 + 四类短串 +
// 四类单字 + 空/纯空格/纯标点/纯数字/纯空白/纯 emoji 六条边界。
// 确定性：静态表与 Vec 序全固定；hashbrown 迭代只喂 (count,trigram) 全序
// sort 与 u32 交换律求和，输出与哈希序无关；无时间/线程/地址输出。
use whatlang::{Detector, Info, Lang, Script, detect, detect_lang, detect_script};

/// 单样本全量 dump：detect 全信息 + detect_lang 对照 + detect_script。
fn dump(tag: &str, label: &str, text: &str) {
    println!(
        "[{tag}/{label}] bytes={} chars={}",
        text.len(),
        text.chars().count()
    );
    let info = detect(text);
    match &info {
        Some(i) => {
            let l = i.lang();
            println!(
                "[{tag}/{label}] detect: lang={l:?} code={} name={} eng_name={} \
                 script={:?}/{} conf={:#018x} reliable={}",
                l.code(),
                l.name(),
                l.eng_name(),
                i.script(),
                i.script().name(),
                i.confidence().to_bits(),
                i.is_reliable()
            );
        }
        None => println!("[{tag}/{label}] detect: none"),
    }
    let dl = detect_lang(text);
    let dl_match = dl == info.as_ref().map(Info::lang);
    match dl {
        Some(l) => println!("[{tag}/{label}] detect_lang: {l:?} match={dl_match}"),
        None => println!("[{tag}/{label}] detect_lang: none match={dl_match}"),
    }
    match detect_script(text) {
        Some(s) => println!("[{tag}/{label}] script: {s:?}/{}", s.name()),
        None => println!("[{tag}/{label}] script: none"),
    }
}

/// Detector 维度：detect + detect_lang 两行。
fn dump_det(tag: &str, det: &Detector, text: &str) {
    let info = det.detect(text);
    match &info {
        Some(i) => println!(
            "[{tag}] detect: lang={:?} script={:?} conf={:#018x} reliable={}",
            i.lang(),
            i.script(),
            i.confidence().to_bits(),
            i.is_reliable()
        ),
        None => println!("[{tag}] detect: none"),
    }
    match det.detect_lang(text) {
        Some(l) => println!("[{tag}] detect_lang: {l:?}"),
        None => println!("[{tag}] detect_lang: none"),
    }
}

fn main() {
    // ① 十语长句（同义「棕狐」句）+ 混排：detect 全信息 + 三件套对照。
    let longs: &[(&str, &str)] = &[
        ("en", "The quick brown fox jumps over the lazy dog near the river bank every single morning."),
        ("fr", "Le renard brun rapide saute par-dessus le chien paresseux près de la rive du fleuve chaque matin."),
        ("de", "Der schnelle braune Fuchs springt jeden Morgen am stillen Flussufer über den faulen Hund."),
        ("ru", "Быстрая коричневая лиса прыгает через ленивую собаку возле тихого берега реки каждое утро."),
        ("zh", "敏捷的棕色狐狸每天早晨都跳过河边那只懒惰的狗。"),
        ("ja", "素早い茶色のキツネは毎朝、静かな川岸で怠け者の犬を飛び越えます。"),
        ("ko", "빠른 갈색 여우는 매일 아침 조용한 강둑에서 게으른 개를 뛰어넘습니다."),
        ("es", "El rápido zorro marrón salta sobre el perro perezoso cerca de la orilla del río cada mañana."),
        ("ar", "الثعلب البني السريع يقفز فوق الكلب الكسول بالقرب من ضفة النهر الهادئة كل صباح."),
        ("hi", "तेज़ भूरी लोमड़ी हर सुबह शांत नदी के किनारे आलसी कुत्ते के ऊपर कूद जाती है।"),
        ("mixed", "Hello there, 今天天气很好, comment ça va? Мы идём в школу, とても楽しいですね!"),
    ];
    for (label, text) in longs {
        dump("long", label, text);
    }

    // ② 短串与单字：信心/脚本模型的退化路径。
    let shorts: &[(&str, &str)] = &[
        ("hello", "Hello!"),
        ("bonjour", "Bonjour"),
        ("danke", "Danke schön!"),
        ("da", "да"),
        ("shide", "是的"),
        ("hai", "はい"),
    ];
    for (label, text) in shorts {
        dump("short", label, text);
    }
    for (label, text) in [
        ("a", "a"),
        ("cyr-yu", "я"),
        ("han-ni", "你"),
        ("han-ri", "日"),
        ("hangul-han", "한"),
        ("hira-no", "の"),
    ] {
        dump("char", label, text);
    }

    // ③ 边界：空/空格/ASCII 标点/CJK 标点/数字/空白控制符/纯 emoji →
    //    脚本计数全零 → detect/detect_lang/detect_script 全 None（期望）。
    for (label, text) in [
        ("empty", ""),
        ("spaces", "   "),
        ("punct-ascii", "!?.,;:—"),
        ("punct-cjk", "，。！？、…"),
        ("digits", "123 456 789"),
        ("controls", "\n \t \r\n"),
        ("emoji", "\u{1f600}\u{1f389}\u{1f680}"),
    ] {
        dump("edge", label, text);
    }

    // ④ Detector：allowlist / denylist / 全过滤 → None / Mandarin 特殊两支。
    let en_txt = longs[0].1;
    let fr_txt = longs[1].1;
    let ru_txt = longs[3].1;
    let allow = Detector::with_allowlist(vec![Lang::Eng, Lang::Fra, Lang::Deu]);
    dump_det("allow/eng", &allow, en_txt);
    dump_det("allow/fra", &allow, fr_txt);
    // 俄文落到 Cyrillic 脚本组，三语言全不在组内 → 全过滤 → None。
    dump_det("allow/rus", &allow, ru_txt);
    let deny = Detector::with_denylist(vec![Lang::Eng, Lang::Ita]);
    dump_det("deny/eng", &deny, en_txt);
    dump_det("deny/fra", &deny, fr_txt);
    // Mandarin↔Japanese 判别被 allow/deny 支配的两支（单汉字）。
    dump_det("han/allow-jpn", &Detector::with_allowlist(vec![Lang::Jpn]), "水");
    dump_det("han/deny-jpn", &Detector::with_denylist(vec![Lang::Jpn]), "水");
    // 希伯来脚本两语言全 deny → None（脚本组内零候选）。
    let hebrew = "האקדמיה ללשון העברית";
    dump_det(
        "heb/deny-all",
        &Detector::with_denylist(vec![Lang::Heb, Lang::Yid]),
        hebrew,
    );
    dump("heb", "baseline", hebrew);
    // Detector::new 裸路径 + detect_script 方法面（对照自由函数）。
    let bare = Detector::new();
    dump_det("bare/deu", &bare, longs[2].1);
    match bare.detect_script("Кириллица") {
        Some(s) => println!("[bare] detect_script(Кириллица): {s:?}"),
        None => println!("[bare] detect_script(Кириллица): none"),
    }

    // ⑤ Lang 静态面：all() 全量 code roundtrip + 点查 + from_code/FromStr。
    let all = Lang::all();
    let ok = all
        .iter()
        .filter(|l| Lang::from_code(l.code()) == Some(**l))
        .count();
    println!("lang: all={} code-roundtrip ok={ok} bad={}", all.len(), all.len() - ok);
    for l in [Lang::Eng, Lang::Rus, Lang::Cmn, Lang::Jpn, Lang::Ara, Lang::Hin] {
        println!(
            "lang[{l:?}]: code={} name={} eng_name={}",
            l.code(),
            l.name(),
            l.eng_name()
        );
    }
    println!("from_code(ukr)={:?} from_code(xxx)={:?}", Lang::from_code("ukr"), Lang::from_code("xxx"));
    match "fra".parse::<Lang>() {
        Ok(l) => println!("parse(fra) ok code={}", l.code()),
        Err(e) => println!("parse(fra) err {e:?}"),
    }
    match "zzz".parse::<Lang>() {
        Ok(l) => println!("parse(zzz) ok {l:?}"),
        Err(e) => println!("parse(zzz) err {e:?}"),
    }

    // ⑥ Script 静态面：all()/name()/langs() + FromStr。
    let scripts = Script::all();
    let names: Vec<&str> = scripts.iter().map(|s| s.name()).collect();
    println!("script: all={} names={}", scripts.len(), names.join(","));
    for (label, s) in [("latin", Script::Latin), ("cyrillic", Script::Cyrillic), ("hangul", Script::Hangul)] {
        let ls = s.langs();
        let codes: Vec<&str> = ls.iter().map(|l| l.code()).collect();
        println!(
            "script[{label}].langs: n={} first={} last={}",
            ls.len(),
            codes.first().copied().unwrap_or("-"),
            codes.last().copied().unwrap_or("-")
        );
    }
    match "latin".parse::<Script>() {
        Ok(s) => println!("parse(latin) ok {s:?}"),
        Err(e) => println!("parse(latin) err {e:?}"),
    }
    match "klingon".parse::<Script>() {
        Ok(s) => println!("parse(klingon) ok {s:?}"),
        Err(e) => println!("parse(klingon) err {e:?}"),
    }

    // ⑦ Info::new 手工重建 + PartialEq + Debug（与 detect 结果逐字段同值）。
    let info = detect(en_txt).unwrap();
    let rebuilt = Info::new(info.script(), info.lang(), info.confidence());
    println!("info: rebuilt-eq={} debug={rebuilt:?}", rebuilt == info);
}
