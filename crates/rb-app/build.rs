// rust-i18n embeds locales/ at compile time, but cargo doesn't know it: without this, editing only the TOML doesn't rebuild.
fn main() {
    println!("cargo:rerun-if-changed=locales");
}
