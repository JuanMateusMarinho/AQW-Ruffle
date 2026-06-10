fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let mut res = winres::WindowsResource::new();
        res.set_icon_with_id("icon.ico", "1");
        res.compile()
            .expect("Falha ao compilar recursos do Windows");
    }
}
