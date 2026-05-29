fn main() {
    // Non-existent path → Cargo always re-runs this script, keeping the timestamp current.
    println!("cargo:rerun-if-changed=__force_rerun__");
    let ts = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    println!("cargo:rustc-env=BUILD_TIMESTAMP={ts}");
}
