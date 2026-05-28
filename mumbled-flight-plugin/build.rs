fn main() {
    // Bake a UTC build timestamp into the binary so the log always shows which build is loaded.
    let ts = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M:%S"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_TIMESTAMP={ts}");
}
