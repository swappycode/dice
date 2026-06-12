// Hide the console window in release builds (Windows).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    dice_desktop_lib::run();
}
