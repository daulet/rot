extern crate proc_macro;

use proc_macro::TokenStream;

#[proc_macro]
pub fn target_off(input: TokenStream) -> TokenStream {
    input
}
