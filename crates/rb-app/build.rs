// O rust-i18n embute locales/ na compilação, mas o cargo não sabe: sem isto, editar só o TOML não recompila.
fn main() {
    println!("cargo:rerun-if-changed=locales");
}
