use serde::Deserialize;
use std::path::Path;

/// Global config at `<state_dir>/config.toml`. All fields optional.
/// `base_domain`/`scheme` are only used to print URLs (`get`/`list`/run banner);
/// the proxy itself is domain-agnostic.
///
/// `listen` accepts a single address or a list. The default is dual-stack
/// loopback: `.localhost` resolves to `::1` (RFC 6761) while Caddy and probes
/// typically use `127.0.0.1` — an IPv4-only listener silently breaks the
/// former.
#[derive(Debug)]
pub struct GlobalConfig {
    pub listen: Vec<String>,
    pub base_domain: Option<String>,
    pub scheme: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawGlobalConfig {
    listen: Option<OneOrMany>,
    base_domain: Option<String>,
    scheme: Option<String>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            listen: crate::utils::DEFAULT_LISTEN
                .iter()
                .map(|s| s.to_string())
                .collect(),
            base_domain: None,
            scheme: "https".into(),
        }
    }
}

impl GlobalConfig {
    pub fn load(state_dir: &Path) -> Self {
        let raw: RawGlobalConfig = std::fs::read_to_string(state_dir.join("config.toml"))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        let mut cfg = GlobalConfig::default();
        match raw.listen {
            Some(OneOrMany::One(l)) if !l.is_empty() => cfg.listen = vec![l],
            Some(OneOrMany::Many(ls)) if !ls.is_empty() => cfg.listen = ls,
            _ => {}
        }
        cfg.base_domain = raw.base_domain;
        if let Some(s) = raw.scheme {
            cfg.scheme = s;
        }
        // env beats config file; comma-separated for multiple addresses
        if let Ok(l) = std::env::var("PORTPROXY_LISTEN") {
            let addrs: Vec<String> = l
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            if !addrs.is_empty() {
                cfg.listen = addrs;
            }
        }
        cfg
    }

    pub fn url_for(&self, label: &str) -> Option<String> {
        self.base_domain
            .as_ref()
            .map(|d| format!("{}://{label}.{d}", self.scheme))
    }
}

/// Per-project config (cwd only, no walk-up — mirrors Vercel portless):
/// `portproxy.json`, falling back to the package.json `portproxy` key
/// (string shorthand sets `name`). At a workspace root, `apps` overrides
/// member packages by root-relative path:
/// `{ "name": "example", "apps": { "packages/web": { "name": "frontend" } } }`.
#[derive(Debug, Deserialize, Default)]
pub struct ProjectConfig {
    pub name: Option<String>,
    /// package.json script to run when no command is given (default "dev").
    pub script: Option<String>,
    /// Fixed backend port instead of random 4000-4999.
    #[serde(alias = "appPort")]
    pub app_port: Option<u16>,
    /// false = run without proxy/route; true = always route (skips
    /// build-command auto-detection); absent = auto.
    pub proxy: Option<bool>,
    #[serde(default)]
    pub apps: std::collections::HashMap<String, AppOverride>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AppOverride {
    pub name: Option<String>,
    pub script: Option<String>,
    #[serde(alias = "appPort")]
    pub app_port: Option<u16>,
    pub proxy: Option<bool>,
}

impl ProjectConfig {
    pub fn load(dir: &Path) -> Option<Self> {
        let pj = dir.join("portproxy.json");
        if pj.exists() {
            let s = std::fs::read_to_string(&pj).ok()?;
            return serde_json::from_str(&s).ok();
        }
        let data = std::fs::read_to_string(dir.join("package.json")).ok()?;
        let v: serde_json::Value = serde_json::from_str(&data).ok()?;
        match v.get("portproxy")? {
            serde_json::Value::String(s) => Some(ProjectConfig {
                name: Some(s.clone()),
                ..Default::default()
            }),
            obj @ serde_json::Value::Object(_) => serde_json::from_value(obj.clone()).ok(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // GlobalConfig::load reads PORTPROXY_LISTEN; serialize tests touching it
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn defaults_when_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        let c = GlobalConfig::load(d.path());
        // dual-stack loopback: .localhost resolves to ::1 per RFC 6761
        assert_eq!(c.listen, vec!["127.0.0.1:1355", "[::1]:1355"]);
        assert_eq!(c.scheme, "https");
        assert!(c.url_for("app").is_none());
    }

    #[test]
    fn listen_accepts_string_or_array() {
        let _g = ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "listen = \"[::]:1355\"").unwrap();
        assert_eq!(GlobalConfig::load(d.path()).listen, vec!["[::]:1355"]);
        std::fs::write(
            d.path().join("config.toml"),
            "listen = [\"0.0.0.0:1355\", \"[::]:1355\"]",
        )
        .unwrap();
        assert_eq!(
            GlobalConfig::load(d.path()).listen,
            vec!["0.0.0.0:1355", "[::]:1355"]
        );
    }

    #[test]
    fn project_config_full_keys() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("portproxy.json"),
            r#"{"name":"x","script":"start","appPort":4500,"proxy":false,
                "apps":{"packages/web":{"name":"w","script":"serve","appPort":4501,"proxy":true}}}"#,
        )
        .unwrap();
        let c = ProjectConfig::load(d.path()).unwrap();
        assert_eq!(c.script.as_deref(), Some("start"));
        assert_eq!(c.app_port, Some(4500));
        assert_eq!(c.proxy, Some(false));
        let w = &c.apps["packages/web"];
        assert_eq!(w.script.as_deref(), Some("serve"));
        assert_eq!(w.app_port, Some(4501));
        assert_eq!(w.proxy, Some(true));
    }

    #[test]
    fn project_config_package_json_fallback() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("package.json"),
            r#"{"name":"pkg","portproxy":{"name":"obj","script":"dev:app","appPort":4400}}"#,
        )
        .unwrap();
        let c = ProjectConfig::load(d.path()).unwrap();
        assert_eq!(c.name.as_deref(), Some("obj"));
        assert_eq!(c.script.as_deref(), Some("dev:app"));
        assert_eq!(c.app_port, Some(4400));
        // string shorthand
        std::fs::write(d.path().join("package.json"), r#"{"portproxy":"short"}"#).unwrap();
        let c = ProjectConfig::load(d.path()).unwrap();
        assert_eq!(c.name.as_deref(), Some("short"));
        // portproxy.json beats package.json key
        std::fs::write(d.path().join("portproxy.json"), r#"{"name":"file"}"#).unwrap();
        let c = ProjectConfig::load(d.path()).unwrap();
        assert_eq!(c.name.as_deref(), Some("file"));
    }

    #[test]
    fn listen_env_overrides_config() {
        let _g = ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "listen = \"127.0.0.1:9999\"").unwrap();
        std::env::set_var("PORTPROXY_LISTEN", "0.0.0.0:7777, [::1]:7777");
        let c = GlobalConfig::load(d.path());
        std::env::remove_var("PORTPROXY_LISTEN");
        assert_eq!(c.listen, vec!["0.0.0.0:7777", "[::1]:7777"]);
    }

    #[test]
    fn url_built_from_base_domain() {
        let _g = ENV_LOCK.lock().unwrap();
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            "base_domain = \"dev.example.test\"\nlisten = \"0.0.0.0:1355\"",
        )
        .unwrap();
        let c = GlobalConfig::load(d.path());
        assert_eq!(c.listen, vec!["0.0.0.0:1355"]);
        assert_eq!(
            c.url_for("sample-web").as_deref(),
            Some("https://sample-web.dev.example.test")
        );
    }
}
