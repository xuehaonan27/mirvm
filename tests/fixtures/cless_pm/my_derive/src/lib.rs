//! Differential fixture: a path proc-macro crate with zero external
//! dependencies (it uses only the built-in proc_macro crate, and its closure
//! has no build.rs). `#[derive(Hello)]` generates a `hello()` associated fn.
use proc_macro::TokenStream;

#[proc_macro_derive(Hello)]
pub fn derive_hello(input: TokenStream) -> TokenStream {
    // The fixture only handles the simple shape and avoids syn: take the first identifier after "struct ".
    let text = input.to_string();
    let after = text.split("struct ").nth(1).expect("derive(Hello) only accepts structs");
    let name: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    assert!(!name.is_empty(), "derive(Hello) could not extract the struct name: {text}");
    format!("impl {name} {{ pub fn hello() -> &'static str {{ \"hello-from-derive\" }} }}")
        .parse()
        .unwrap()
}
