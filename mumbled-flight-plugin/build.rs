fn main() {
    let ts = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    println!("cargo:rustc-env=BUILD_TIMESTAMP={ts}");
}
