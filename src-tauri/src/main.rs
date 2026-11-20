// Prevents an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Started by ssh to answer a password prompt: answers and exits.
    ferry_files_lib::askpass();
    ferry_files_lib::run()
}
