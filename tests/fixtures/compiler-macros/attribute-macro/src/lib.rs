use proc_macro::{TokenStream, TokenTree};

#[proc_macro_attribute]
pub fn branching(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let generated_name = match attribute.into_iter().collect::<Vec<_>>().as_slice() {
        [TokenTree::Ident(identifier)] => identifier.to_string(),
        _ => {
            return "compile_error!(\"branching expects one generated function name\");"
                .parse()
                .expect("fixed compile_error token stream");
        }
    };
    let generated = format!(
        "fn {generated_name}(value: bool) -> bool {{ if value {{ true }} else {{ false }} }}"
    )
    .parse::<TokenStream>()
    .expect("generated fixture function is valid Rust");
    item.into_iter().chain(generated).collect()
}
