fn main() {
    println!("cargo:rerun-if-env-changed=BEAKR_TEST_PANIC_ON_STARTUP");
    tauri_build::build()
}
