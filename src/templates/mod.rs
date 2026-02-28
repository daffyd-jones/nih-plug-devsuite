pub struct Templates;

impl Templates {
    pub const CARGO_TOML: &'static str = include_str!("cargo_toml.txt");
    pub const LIB_RS: &'static str = include_str!("lib_rs.txt");
    pub const PARAMS_RS: &'static str = include_str!("params_rs.txt");
    pub const EDITOR_RS: &'static str = include_str!("editor_rs.txt");
}
