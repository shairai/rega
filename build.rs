fn main() {
    println!("cargo:rerun-if-changed=assets/rega.ico");
    println!("cargo:rerun-if-changed=assets/rega.rc");
    // Give rega.exe its Explorer / taskbar icon.
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
        embed_resource::compile("assets/rega.rc", embed_resource::NONE).manifest_required().unwrap();
    }
}
