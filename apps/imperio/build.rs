//! Gives the programs of this package the Imperio icon (see `crates/ui/src/logo.rs`).

#[path = "../../crates/ui/src/logo.rs"]
mod logo;

fn main() {
    logo::embed_in_programs();
}
