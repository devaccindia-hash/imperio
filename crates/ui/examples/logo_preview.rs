//! Writes the logo as PNG files and an .ico into a folder, to look at it.
//!
//!   cargo run -p imperio-ui --example logo_preview -- <folder>

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("folder to write into"));
    std::fs::create_dir_all(&dir).expect("create folder");
    for size in [16u32, 20, 24, 32, 48, 64, 256] {
        std::fs::write(dir.join(format!("logo-{size}.png")), imperio_ui::logo::png(size, imperio_ui::logo::RED)).expect("write");
    }
    std::fs::write(dir.join("logo-256-pro.png"), imperio_ui::logo::png(256, 0xE3B341)).expect("write");
    std::fs::write(dir.join("imperio.ico"), imperio_ui::logo::ico(imperio_ui::logo::RED)).expect("write");
    println!("written to {}", dir.display());
}
