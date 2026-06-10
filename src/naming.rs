use crate::config::ProjectConfig;
use crate::utils::sanitize_label;
use std::path::Path;

/// Inference chain, mirroring Vercel portless exactly:
/// 1. portproxy.json `name` (cwd only)
/// 2. package.json `portproxy` key (cwd only; string shorthand or `{name}`)
/// 3. package.json `name` (walk up directories, `@scope/` stripped)
/// 4. git repo root basename (`git rev-parse --show-toplevel`, filesystem
///    fallback: walk up looking for `.git`)
/// 5. cwd basename
/// A source whose value sanitizes to empty falls through to the next one.
pub fn infer_name(cwd: &Path) -> String {
    if let Some(pc) = ProjectConfig::load(cwd) {
        if let Some(s) = pc.name.as_deref().map(sanitize_label) {
            if !s.is_empty() {
                return s;
            }
        }
    }
    if let Some(s) = package_json_portproxy_key(cwd)
        .as_deref()
        .map(sanitize_label)
    {
        if !s.is_empty() {
            return s;
        }
    }
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if let Some(s) = package_json_name(d).as_deref().map(sanitize_label) {
            if !s.is_empty() {
                return s;
            }
        }
        dir = d.parent();
    }
    if let Some(s) = git_root_name(cwd).as_deref().map(sanitize_label) {
        if !s.is_empty() {
            return s;
        }
    }
    sanitize_label(
        &cwd.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
    )
}

fn read_package_json(dir: &Path) -> Option<serde_json::Value> {
    let data = std::fs::read_to_string(dir.join("package.json")).ok()?;
    serde_json::from_str(&data).ok()
}

/// `portproxy` key in cwd's package.json: `"portproxy": "name"` shorthand or
/// `"portproxy": { "name": "..." }`.
fn package_json_portproxy_key(dir: &Path) -> Option<String> {
    match read_package_json(dir)?.get("portproxy")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(o) => match o.get("name") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// `name` field of the nearest package.json walking up, `@scope/` stripped.
fn package_json_name(dir: &Path) -> Option<String> {
    let v = read_package_json(dir)?;
    let name = v.get("name")?.as_str()?;
    Some(name.rsplit('/').next().unwrap_or(name).to_string())
}

fn git_root_name(cwd: &Path) -> Option<String> {
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
    {
        if out.status.success() {
            let top = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            if let Some(name) = top.file_name() {
                return Some(name.to_string_lossy().to_string());
            }
        }
    }
    // git CLI unavailable: walk up looking for a .git entry
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(d.file_name()?.to_string_lossy().to_string());
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn portproxy_json_wins() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("portproxy.json"), r#"{"name":"custom"}"#).unwrap();
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
    fn package_json_portproxy_object_form() {
        let d = tempdir().unwrap();
        std::fs::write(
            d.path().join("package.json"),
            r#"{"name":"@org/pkg","portproxy":{"name":"objform"}}"#,
        )
        .unwrap();
        assert_eq!(infer_name(d.path()), "objform");
    }

    #[test]
    fn portproxy_key_is_cwd_only_but_name_walks_up() {
        let d = tempdir().unwrap();
        // parent has both a portproxy key and a name; child has neither
        std::fs::write(
            d.path().join("package.json"),
            r#"{"name":"@org/myapp","portproxy":"parent-override"}"#,
        )
        .unwrap();
        let sub = d.path().join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        // from the subdir, the portproxy key must NOT apply (cwd only);
        // the walked-up package.json "name" wins instead
        assert_eq!(infer_name(&sub), "myapp");
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
    fn git_root_name_via_dotgit_walk() {
        let d = tempdir().unwrap();
        let repo = d.path().join("my-repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let sub = repo.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(git_root_name(&sub).as_deref(), Some("my-repo"));
    }

    #[test]
    fn empty_sanitized_source_falls_through() {
        let d = tempdir().unwrap();
        let app = d.path().join("realname");
        std::fs::create_dir(&app).unwrap();
        // name sanitizes to "" -> must fall through to directory basename
        std::fs::write(app.join("package.json"), r#"{"name":"___"}"#).unwrap();
        assert_eq!(infer_name(&app), "realname");
    }

    #[test]
    fn falls_back_to_dir_name() {
        let d = tempdir().unwrap();
        let app = d.path().join("My Cool App");
        std::fs::create_dir(&app).unwrap();
        assert_eq!(infer_name(&app), "my-cool-app");
    }
}
