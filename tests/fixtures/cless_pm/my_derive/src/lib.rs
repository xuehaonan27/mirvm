//! D15 P2 切② 对拍夹具：零外部依赖的 path proc-macro crate（只用编译器
//! 内建 proc_macro crate，闭包无 build.rs）。`#[derive(Hello)]` 给 struct
//! 生成 `hello()` 关联函数。
use proc_macro::TokenStream;

#[proc_macro_derive(Hello)]
pub fn derive_hello(input: TokenStream) -> TokenStream {
    // 夹具只服务简单形态，不引 syn：从 input 里找 "struct " 后第一个标识符
    let text = input.to_string();
    let after = text.split("struct ").nth(1).expect("derive(Hello) 只接 struct");
    let name: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    assert!(!name.is_empty(), "derive(Hello) 抠不出 struct 名: {text}");
    format!("impl {name} {{ pub fn hello() -> &'static str {{ \"hello-from-derive\" }} }}")
        .parse()
        .unwrap()
}
