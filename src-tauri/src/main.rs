// Prevents an extra console window on Windows in release. Nothing opens a
// terminal; everything runs in the background.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    backlog_lib::run()
}
