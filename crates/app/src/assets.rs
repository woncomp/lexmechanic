use gpui_kit::{AssetSource, SharedString};
use std::borrow::Cow;

pub struct Assets;

pub fn agent_icon(agent: &str) -> Option<&'static str> {
    match agent {
        "copilot-cli" => Some("agents/copilot-cli.svg"),
        "copilot-app" => Some("agents/copilot-app.png"),
        "vscode" => Some("agents/vscode.svg"),
        "vscode-agents-copilot" => Some("agents/vscode-agents-copilot.svg"),
        "vscode-agents-claude" => Some("agents/vscode-agents-claude.svg"),
        "vscode-agents-codex" => Some("agents/vscode-agents-codex.svg"),
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
            "agents/vscode.svg" => Some(include_bytes!("../../../assets/agents/vscode.svg")),
            "agents/vscode-agents-copilot.svg" => Some(include_bytes!(
                "../../../assets/agents/vscode-agents-copilot.svg"
            )),
            "agents/vscode-agents-claude.svg" => Some(include_bytes!(
                "../../../assets/agents/vscode-agents-claude.svg"
            )),
            "agents/vscode-agents-codex.svg" => Some(include_bytes!(
                "../../../assets/agents/vscode-agents-codex.svg"
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
                "agents/vscode.svg",
                "agents/vscode-agents-copilot.svg",
                "agents/vscode-agents-claude.svg",
                "agents/vscode-agents-codex.svg",
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
            "vscode",
            "vscode-agents-copilot",
            "vscode-agents-claude",
            "vscode-agents-codex",
        ] {
            let path = agent_icon(agent).unwrap();
            let bytes = Assets.load(path).unwrap().unwrap();
            if path.ends_with(".png") {
                assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
            } else {
                let svg = std::str::from_utf8(&bytes).unwrap();
                assert!(svg.contains("<svg"));
                // Vector-only: the app must not depend on embedded bitmaps for these icons.
                if agent == "copilot-cli" || agent.starts_with("vscode-agents-") {
                    assert!(svg.contains("<path"));
                    assert!(!svg.contains("<image"));
                    assert!(!svg.contains("data:image/"));
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
        assert!(Assets.load("icons/check.svg").unwrap().is_some());
    }
}
