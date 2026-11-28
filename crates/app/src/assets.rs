use gpui_kit::{
    App, Asset, AssetSource, DevicePixels, ImageCacheError, ImageSource, RenderImage, SharedString,
    SvgSize, size,
};
use std::{borrow::Cow, future::Future, sync::Arc};

pub struct Assets;

/// SVG icons rasterized at exactly the device-pixel size they are shown at.
///
/// `img()` rasterizes an SVG once at the size the file declares and lets the GPU shrink the
/// bitmap, which turns fine lines ragged when the declared size is far larger than the icon.
enum SharpSvg {}

impl Asset for SharpSvg {
    /// The asset path and the edge length in device pixels.
    type Source = (SharedString, i32);
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        (path, pixels): Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        let assets = cx.asset_source().clone();
        async move {
            let bytes = assets
                .load(&path)
                .map_err(|error| ImageCacheError::Other(Arc::new(error)))?
                .ok_or_else(|| ImageCacheError::Asset(format!("Missing icon {path}").into()))?;
            let svg = renderer.parse_svg(&bytes)?;
            let edge = DevicePixels(pixels);
            Ok(renderer.render_parsed(&svg, SvgSize::Size(size(edge, edge)))?)
        }
    }
}

/// An icon shown `logical` pixels square: SVGs are rasterized for the window's scale factor,
/// bitmaps are drawn as they are.
pub fn icon_source(path: &'static str, logical: f32) -> ImageSource {
    if !path.ends_with(".svg") {
        return path.into();
    }
    ImageSource::Custom(Arc::new(move |window, cx| {
        let pixels = (logical * window.scale_factor()).ceil() as i32;
        window.use_asset::<SharpSvg>(&(path.into(), pixels), cx)
    }))
}

/// The icon for an agent; a few have separate artwork for the dark theme.
pub fn agent_icon(agent: &str, dark: bool) -> Option<&'static str> {
    match agent {
        "copilot-cli" => Some("agents/copilot-cli.svg"),
        "copilot-app" => Some("agents/copilot-app.png"),
        "codex" if dark => Some("agents/codex-dark.svg"),
        "codex" => Some("agents/codex-light.svg"),
        "vscode" => Some("agents/vscode.svg"),
        "vscode-agents-copilot" => Some("agents/vscode-agents-copilot.svg"),
        "vscode-agents-claude" => Some("agents/vscode-agents-claude.svg"),
        "vscode-agents-codex" if dark => Some("agents/vscode-agents-codex-dark.svg"),
        "vscode-agents-codex" => Some("agents/vscode-agents-codex-light.svg"),
        _ => None,
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "agents/copilot-cli.svg" => {
                Some(include_bytes!("../../../assets/agents/copilot-cli.svg"))
            }
            "agents/copilot-app.png" => {
                Some(include_bytes!("../../../assets/agents/copilot-app.png"))
            }
            "agents/codex-light.svg" => {
                Some(include_bytes!("../../../assets/agents/codex-light.svg"))
            }
            "agents/codex-dark.svg" => {
                Some(include_bytes!("../../../assets/agents/codex-dark.svg"))
            }
            "agents/vscode.svg" => Some(include_bytes!("../../../assets/agents/vscode.svg")),
            "agents/vscode-agents-copilot.svg" => Some(include_bytes!(
                "../../../assets/agents/vscode-agents-copilot.svg"
            )),
            "agents/vscode-agents-claude.svg" => Some(include_bytes!(
                "../../../assets/agents/vscode-agents-claude.svg"
            )),
            "agents/vscode-agents-codex-light.svg" => Some(include_bytes!(
                "../../../assets/agents/vscode-agents-codex-light.svg"
            )),
            "agents/vscode-agents-codex-dark.svg" => Some(include_bytes!(
                "../../../assets/agents/vscode-agents-codex-dark.svg"
            )),
            _ => None,
        };
        match bytes {
            Some(bytes) => Ok(Some(Cow::Borrowed(bytes))),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(
            [
                "agents/copilot-cli.svg",
                "agents/copilot-app.png",
                "agents/codex-light.svg",
                "agents/codex-dark.svg",
                "agents/vscode.svg",
                "agents/vscode-agents-copilot.svg",
                "agents/vscode-agents-claude.svg",
                "agents/vscode-agents-codex-light.svg",
                "agents/vscode-agents-codex-dark.svg",
            ]
            .into_iter()
            .filter(|name| name.starts_with(path))
            .map(SharedString::from),
        );
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_icons_and_component_icons_are_bundled_without_runtime_file_paths() {
        for agent in [
            "copilot-cli",
            "copilot-app",
            "codex",
            "vscode",
            "vscode-agents-copilot",
            "vscode-agents-claude",
            "vscode-agents-codex",
        ] {
            for dark in [false, true] {
                let path = agent_icon(agent, dark).unwrap();
                let bytes = Assets.load(path).unwrap().unwrap();
                if path.ends_with(".png") {
                    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
                } else {
                    let svg = std::str::from_utf8(&bytes).unwrap();
                    assert!(svg.contains("<svg"));
                    // An embedded bitmap must be inline; nothing may point at a file on disk.
                    assert!(!svg.contains("file:"));
                    if svg.contains("<image") {
                        assert!(svg.contains("data:image/png;base64,"));
                    }
                    // Hand-drawn icons stay vector-only.
                    if matches!(
                        agent,
                        "copilot-cli" | "codex" | "vscode-agents-copilot" | "vscode-agents-claude"
                    ) {
                        assert!(svg.contains("<path"));
                        assert!(!svg.contains("<image"));
                    }
                }
                assert!(
                    Assets
                        .list("agents/")
                        .unwrap()
                        .iter()
                        .any(|entry| entry == path)
                );
            }
        }
        for agent in ["codex", "vscode-agents-codex"] {
            assert_ne!(agent_icon(agent, false), agent_icon(agent, true));
        }
        assert!(Assets.load("icons/check.svg").unwrap().is_some());
    }
}
