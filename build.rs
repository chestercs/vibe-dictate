fn main() {
    #[cfg(target_os = "windows")]
    {
        let icon_path = std::path::Path::new("assets/icon.ico");
        if icon_path.exists() {
            let mut res = winres::WindowsResource::new();
            res.set_icon("assets/icon.ico");
            if let Err(e) = res.compile() {
                println!("cargo:warning=winres compile failed: {e}");
            }
        } else {
            println!(
                "cargo:warning=assets/icon.ico not found — skipping embedded icon"
            );
        }
    }
}
