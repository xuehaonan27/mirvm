#!/usr/bin/env mirvm
---
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rand = "0.9"
regex = "1"
---

use rand::Rng;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
struct Task {
    id: u32,
    title: String,
    tags: Vec<String>,
    done: bool,
}

fn main() {
    // serde_json：解析 + 修改 + 序列化
    let json = r#"{"id":7,"title":"ship mirvm M2","tags":["rust","vm"],"done":false}"#;
    let mut task: Task = serde_json::from_str(json).expect("parse");
    task.done = true;
    task.tags.push("interpreted".into());
    println!("serde: {}", serde_json::to_string(&task).unwrap());

    // regex：提取
    let re = Regex::new(r"(\w+)@(\w+)\.(\w+)").unwrap();
    let text = "联系: alice@example.com, bob@test.org";
    let emails: Vec<String> = re.captures_iter(text).map(|c| format!("{}@{}", &c[1], &c[2])).collect();
    println!("regex: {emails:?}");

    // rand：区间随机数（值不比对，只验证可运行 + 落在区间内）
    let mut rng = rand::rng();
    let n: u32 = rng.random_range(10..20);
    println!("rand in range: {}", (10..20).contains(&n));

    // 组合：JSON 数组统计
    let data: Vec<Task> = serde_json::from_str(
        r#"[{"id":1,"title":"a","tags":[],"done":true},
            {"id":2,"title":"b","tags":["x"],"done":false},
            {"id":3,"title":"c","tags":["y","z"],"done":true}]"#,
    )
    .unwrap();
    let done = data.iter().filter(|t| t.done).count();
    let tags: usize = data.iter().map(|t| t.tags.len()).sum();
    println!("stats: {done}/{} done, {tags} tags", data.len());
}
