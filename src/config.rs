use serde::Deserialize;
use std::path::Path;

/// Global config at `<state_dir>/config.toml`. All fields optional.
/// `base_domain`/`scheme` are only used to print URLs (`get`/`list`/run banner);
/// the proxy itself is domain-agnostic.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub listen: String,
    pub base_domain: Option<String>,
    pub scheme: String,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            listen: crate::utils::DEFAULT_LISTEN.into(),
            base_domain: None,
            scheme: "https".into(),
        }
    }
}

impl GlobalConfig {
    pub fn load(state_dir: &Path) -> Self {
        std::fs::read_to_string(state_dir.join("config.toml"))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn url_for(&self, label: &str) -> Option<String> {
        self.base_domain
            .as_ref()
            .map(|d| format!("{}://{label}.{d}", self.scheme))
    }
}

/// Per-project config: `portproxy.toml` in the working directory.
#[derive(Debug, Deserialize, Default)]
pub struct ProjectConfig {
    pub name: Option<String>,
}

impl ProjectConfig {
    pub fn load(dir: &Path) -> Option<Self> {
        let s = std::fs::read_to_string(dir.join("portproxy.toml")).ok()?;
        toml::from_str(&s).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_when_missing() {
        let d = tempdir().unwrap();
        let c = GlobalConfig::load(d.path());
        assert_eq!(c.listen, "127.0.0.1:1355");
        assert_eq!(c.scheme, "https");
        assert!(c.url_for("app").is_none());
    }

    #[test]
    fn url_built_from_base_domain() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            "base_domain = \"dev.example.test\"\nlisten = \"0.0.0.0:1355\"",
        )
        .unwrap();
        let c = GlobalConfig::load(d.path());
        assert_eq!(c.listen, "0.0.0.0:1355");
        assert_eq!(
            c.url_for("sample-web").as_deref(),
            Some("https://sample-web.dev.example.test")
        );
    }
}
