use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("../../assets/app/app-icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", icon.display());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(icon.to_str().ok_or("Icon path is not valid Unicode")?)
        .set("ProductName", "Lexmechanic")
        .set("FileDescription", "Lexmechanic - local agent conversations")
        .set("OriginalFilename", "Lexmechanic.exe")
        .set("ProductVersion", env!("CARGO_PKG_VERSION"));
    resource.compile()?;
    Ok(())
}
