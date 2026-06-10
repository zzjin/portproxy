use crate::config::ProjectConfig;
use crate::utils::sanitize_label;
use std::path::Path;

/// Inference chain: portproxy.toml (cwd only) -> walk up [package.json
/// "portproxy" key -> package.json "name" (scope stripped) -> Cargo.toml
/// package.name] -> git main-repo root basename -> cwd basename.
pub fn infer_name(cwd: &Path) -> String {
    if let Some(pc) = ProjectConfig::load(cwd) {
        if let Some(n) = pc.name {
            return sanitize_label(&n);
        }
    }
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if let Some(n) = package_json_name(d) {
            return sanitize_label(&n);
        }
        if let Some(n) = cargo_toml_name(d) {
            return sanitize_label(&n);
        }
        dir = d.parent();
    }
    if let Some(n) = git_root_name(cwd) {
        return sanitize_label(&n);
    }
    sanitize_label(
        &cwd.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
    )
}

fn package_json_name(dir: &Path) -> Option<String> {
    let data = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    // dedicated key beats the package name
    match v.get("portproxy") {
        Some(serde_json::Value::String(s)) => return Some(s.clone()),
        Some(serde_json::Value::Object(o)) => {
            if let Some(serde_json::Value::String(s)) = o.get("name") {
                return Some(s.clone());
            }
        }
        _ => {}
    }
    let name = v.get("name")?.as_str()?;
    // strip @scope/
    Some(name.rsplit('/').next().unwrap_or(name).to_string())
}

fn cargo_toml_name(dir: &Path) -> Option<String> {
    let data = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let v: toml::Value = toml::from_str(&data).ok()?;
    Some(v.get("package")?.get("name")?.as_str()?.to_string())
}

fn git_root_name(cwd: &Path) -> Option<String> {
    // main repo root even from a linked worktree: parent of --git-common-dir
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Some(common.parent()?.file_name()?.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn portproxy_toml_wins() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("portproxy.toml"), "name = \"custom\"").unwrap();
        std::fs::write(d.path().join("package.json"), r#"{"name":"@org/pkg"}"#).unwrap();
        assert_eq!(infer_name(d.path()), "custom");
    }

    #[test]
    fn package_json_portproxy_key_beats_name() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("package.json"),
            r#"{"name":"@org/pkg","portproxy":"override"}"#,
        )
        .unwrap();
        assert_eq!(infer_name(d.path()), "override");
    }

    #[test]
    fn package_json_name_strips_scope_walks_up() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("package.json"), r#"{"name":"@org/myapp"}"#).unwrap();
        let sub = d.path().join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(infer_name(&sub), "myapp");
    }

    #[test]
    fn cargo_toml_name_used() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("Cargo.toml"),
            "[package]\nname = \"rusty_app\"",
        )
        .unwrap();
        assert_eq!(infer_name(d.path()), "rusty-app");
    }

    #[test]
    fn falls_back_to_dir_name() {
        let d = tempdir().unwrap();
        let app = d.path().join("My Cool App");
        std::fs::create_dir(&app).unwrap();
        assert_eq!(infer_name(&app), "my-cool-app");
    }
}
