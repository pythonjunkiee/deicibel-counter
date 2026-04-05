// Suppresses the console window that would normally appear on Windows in
// release builds — keeps the overlay clean without a terminal behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    decibel_counter_lib::run();
}
