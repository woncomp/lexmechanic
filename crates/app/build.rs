use std::{env, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }
    let path = PathBuf::from(env::var("OUT_DIR")?).join("AgentChatReader.ico");
    fs::write(&path, icon())?;
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(path.to_str().ok_or("Icon path is not valid Unicode")?)
        .set("ProductName", "AgentChatReader")
        .set(
            "FileDescription",
            "AgentChatReader - local agent conversations",
        )
        .set("OriginalFilename", "AgentChatReader.exe")
        .set("ProductVersion", env!("CARGO_PKG_VERSION"));
    resource.compile()?;
    Ok(())
}

fn icon() -> Vec<u8> {
    let pixels = 32 * 32 * 4;
    let image_bytes = 40 + pixels + 128;
    let mut bytes = Vec::new();
    for word in [0_u16, 1, 1] {
        bytes.extend(word.to_le_bytes());
    }
    bytes.extend([32, 32, 0, 0]);
    bytes.extend(1_u16.to_le_bytes());
    bytes.extend(32_u16.to_le_bytes());
    bytes.extend((image_bytes as u32).to_le_bytes());
    bytes.extend(22_u32.to_le_bytes());
    for value in [40_u32, 32, 64] {
        bytes.extend(value.to_le_bytes());
    }
    bytes.extend(1_u16.to_le_bytes());
    bytes.extend(32_u16.to_le_bytes());
    for value in [0_u32, pixels as u32, 0, 0, 0, 0] {
        bytes.extend(value.to_le_bytes());
    }
    for y in (0..32).rev() {
        for x in 0..32 {
            let (r, g, b) = if (6..26).contains(&x) && (6..25).contains(&y) {
                if (9..23).contains(&x) && [10, 11, 15, 16, 20].contains(&y) {
                    (220, 244, 241)
                } else {
                    (31, 136, 130)
                }
            } else {
                (26, 34, 46)
            };
            bytes.extend([b, g, r, 255]);
        }
    }
    bytes.extend([0; 128]);
    bytes
}
