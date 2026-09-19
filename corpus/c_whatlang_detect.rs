#!/usr/bin/env mirvm
---
[dependencies]
whatlang = "0.18"
---
// whatlang 0.18 language detection differential: a purely static model with no
// external data files -- a letter model (per-language letter strings -> LazyLock
// reverse table -> per-character scoring) plus a trigram model (hashbrown
// SwissTable counts -> total (count,trigram) sort into a rank table -> Manhattan
// distance against static profiles). combined merges both f64 paths by letter
// weight; confidence is a hyperbolic function. The f64 count->score->confidence
// chain is per-instruction MIR math on a scalar path independent of 128-bit/vector
// lowering; output always locks conf.to_bits().
// Coverage: detect/detect_lang/detect_script; Info's lang/script/confidence/
// is_reliable; Detector::{new,with_allowlist,with_denylist,detect,detect_lang,
// detect_script}; Lang::{all,from_code,code,name,eng_name} roundtrip + FromStr
// error; Script::{all,name,langs} + FromStr error; Info::new with PartialEq/Debug.
// Samples: ten-language long sentences, mixed scripts, four short strings, four
// single characters, and six edge inputs (empty/whitespace/punctuation/emoji).
// Determinism: static tables and Vec order are fixed; hashbrown iteration only
// feeds a total sort and a commutative u32 sum, so output ignores hash order.
use whatlang::{Detector, Info, Lang, Script, detect, detect_lang, detect_script};

/// Full dump for one sample: all detect info + the detect_lang cross-check + detect_script.
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

/// The Detector dimension: detect + detect_lang, two lines.
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
    // ① ten-language long sentences (the "brown fox" equivalent) + mixed text: full info.
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

    // ② short strings and single characters: degenerate paths of the confidence/script model.
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

    // ③ edges: empty/spaces/ASCII punctuation/CJK punctuation/digits/blank controls/emoji ->
    //    all script counts zero -> detect/detect_lang/detect_script all None (expected).
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

    // ④ Detector: allowlist / denylist / everything filtered -> None / the two Mandarin cases.
    let en_txt = longs[0].1;
    let fr_txt = longs[1].1;
    let ru_txt = longs[3].1;
    let allow = Detector::with_allowlist(vec![Lang::Eng, Lang::Fra, Lang::Deu]);
    dump_det("allow/eng", &allow, en_txt);
    dump_det("allow/fra", &allow, fr_txt);
    // Russian falls in the Cyrillic script group while none of the three allowed languages does.
    dump_det("allow/rus", &allow, ru_txt);
    let deny = Detector::with_denylist(vec![Lang::Eng, Lang::Ita]);
    dump_det("deny/eng", &deny, en_txt);
    dump_det("deny/fra", &deny, fr_txt);
    // The two branches where allow/deny governs the Mandarin vs Japanese call (a single han char).
    dump_det("han/allow-jpn", &Detector::with_allowlist(vec![Lang::Jpn]), "水");
    dump_det("han/deny-jpn", &Detector::with_denylist(vec![Lang::Jpn]), "水");
    // Both Hebrew script languages denied -> None (zero candidates in the group).
    let hebrew = "האקדמיה ללשון העברית";
    dump_det(
        "heb/deny-all",
        &Detector::with_denylist(vec![Lang::Heb, Lang::Yid]),
        hebrew,
    );
    dump("heb", "baseline", hebrew);
    // The bare Detector::new path + the detect_script method surface (against the free function).
    let bare = Detector::new();
    dump_det("bare/deu", &bare, longs[2].1);
    match bare.detect_script("Кириллица") {
        Some(s) => println!("[bare] detect_script(Кириллица): {s:?}"),
        None => println!("[bare] detect_script(Кириллица): none"),
    }

    // ⑤ The static Lang surface: full code roundtrip over all() + point lookups + from_code/FromStr.
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

    // ⑥ The static Script surface: all()/name()/langs() + FromStr.
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

    // ⑦ Info::new rebuilt by hand + PartialEq + Debug (field-identical to the detect result).
    let info = detect(en_txt).unwrap();
    let rebuilt = Info::new(info.script(), info.lang(), info.confidence());
    println!("info: rebuilt-eq={} debug={rebuilt:?}", rebuilt == info);
}
