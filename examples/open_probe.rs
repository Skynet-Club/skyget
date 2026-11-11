//! 诊断用：单独验「打开目录」到底能不能调起文件管理器。
//!
//!     cargo run --example open_probe --features gui -- ~/Downloads/x.iso

fn main() {
    let path = std::env::args().nth(1).expect("用法：open_probe <路径>");
    match skyfetch::gui::open_in_file_manager(&path) {
        Ok(()) => println!("OK  已调起文件管理器：{path}"),
        Err(e) => println!("ERR {e}"),
    }
}
