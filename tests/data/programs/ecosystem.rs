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
    // serde_json: parse + modify + serialize
    let json = r#"{"id":7,"title":"ship mirvm M2","tags":["rust","vm"],"done":false}"#;
    let mut task: Task = serde_json::from_str(json).expect("parse");
    task.done = true;
    task.tags.push("interpreted".into());
    println!("serde: {}", serde_json::to_string(&task).unwrap());

    // regex: extract
    let re = Regex::new(r"(\w+)@(\w+)\.(\w+)").unwrap();
    let text = "Contact: alice@example.com, bob@test.org";
    let emails: Vec<String> = re.captures_iter(text).map(|c| format!("{}@{}", &c[1], &c[2])).collect();
    println!("regex: {emails:?}");

    // rand: range random number (value not compared, only verify it runs + falls in range)
    let mut rng = rand::rng();
    let n: u32 = rng.random_range(10..20);
    println!("rand in range: {}", (10..20).contains(&n));

    // combo: JSON array stats
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
