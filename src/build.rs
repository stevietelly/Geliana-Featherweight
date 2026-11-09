
fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico"); // Path to your 256x256 .ico file
        res.compile().unwrap();
    }
}
