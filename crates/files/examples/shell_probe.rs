//! Diagnostic: what `imperio_files::shell` sees right now.
//!
//!   cargo run -p imperio-files --example shell_probe [-- <x> <y>]
//!
//! Prints the files selected in the Explorer window in front, and the folder
//! at a point of the screen (the pointer's position when none is given).

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

fn main() {
    let args: Vec<i32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let (x, y) = match args.as_slice() {
        [x, y] => (*x, *y),
        _ => {
            let mut p = POINT::default();
            // SAFETY: plain call with a valid out pointer.
            let _ = unsafe { GetCursorPos(&mut p) };
            (p.x, p.y)
        }
    };
    for line in imperio_files::shell::describe() {
        println!("view: {line}");
    }
    println!("selected in the window in front: {:?}", imperio_files::shell::selected_in_foreground());
    println!("folder at {x},{y}: {:?}", imperio_files::shell::folder_at(x, y));
}
