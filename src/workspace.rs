use crate::utils::sanitize_label;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Package {
    pub dir: PathBuf,
    /// Raw package.json `name` (may be scoped); dir basename when missing.
    pub name: String,
    /// Name with `@scope/` stripped.
    pub short: String,
    /// Contents of `scripts.dev`, when present.
    pub dev_script: Option<String>,
    /// Path relative to the workspace root, `/`-separated (apps-map key).
    pub rel: String,
}

#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub packages: Vec<Package>,
}

impl Workspace {
    /// Deepest package whose directory contains `path`.
    pub fn package_containing(&self, path: &Path) -> Option<&Package> {
        self.packages
            .iter()
            .filter(|p| path.starts_with(&p.dir))
            .max_by_key(|p| p.dir.components().count())
    }
}

/// Walk up from `cwd` looking for a workspace definition
/// (pnpm-workspace.yaml or package.json `workspaces`) with at least one
/// resolvable member package.
pub fn find_workspace(cwd: &Path) -> Option<Workspace> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if let Some(globs) = workspace_globs(d) {
            let packages = expand_globs(d, &globs);
            if !packages.is_empty() {
                return Some(Workspace {
                    root: d.to_path_buf(),
                    packages,
                });
            }
        }
        dir = d.parent();
    }
    None
}

fn workspace_globs(dir: &Path) -> Option<Vec<String>> {
    if let Ok(s) = std::fs::read_to_string(dir.join("pnpm-workspace.yaml")) {
        if let Some(globs) = parse_pnpm_workspace_yaml(&s) {
            return Some(globs);
        }
    }
    let data = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    let ws = v.get("workspaces")?;
    let arr = match ws {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(o) => o.get("packages")?.as_array()?,
        _ => return None,
    };
    let globs: Vec<String> = arr
        .iter()
        .filter_map(|x| x.as_str().map(String::from))
        .collect();
    if globs.is_empty() {
        None
    } else {
        Some(globs)
    }
}

/// Minimal hand-rolled parser for the `packages:` list (same approach as
/// Vercel portless — no YAML dependency).
fn parse_pnpm_workspace_yaml(s: &str) -> Option<Vec<String>> {
    let mut in_packages = false;
    let mut out = Vec::new();
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented && !trimmed.starts_with('-') {
            in_packages = trimmed == "packages:" || trimmed.starts_with("packages:");
            continue;
        }
        if in_packages && trimmed.starts_with('-') {
            let v = trimmed[1..].trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() && !v.starts_with('!') {
                out.push(v.to_string());
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Supports `dir/*` and `dir/**` (one level), plus exact paths.
fn expand_globs(root: &Path, globs: &[String]) -> Vec<Package> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for g in globs {
        let g = g.trim_end_matches('/');
        if let Some(prefix) = g.strip_suffix("/**").or_else(|| g.strip_suffix("/*")) {
            if let Ok(entries) = std::fs::read_dir(root.join(prefix)) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() && p.join("package.json").exists() {
                        dirs.push(p);
                    }
                }
            }
        } else {
            let p = root.join(g);
            if p.join("package.json").exists() {
                dirs.push(p);
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs.into_iter()
        .filter(|d| d != root)
        .filter_map(|d| load_package(root, d))
        .collect()
}

fn load_package(root: &Path, dir: PathBuf) -> Option<Package> {
    let data = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .map(String::from)
        .or_else(|| dir.file_name().map(|f| f.to_string_lossy().to_string()))?;
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
    let dev_script = v
        .get("scripts")
        .and_then(|s| s.get("dev"))
        .and_then(|d| d.as_str())
        .map(String::from);
    let rel = dir
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Some(Package {
        dir,
        name,
        short,
        dev_script,
        rel,
    })
}

/// Project name, Vercel-portless precedence:
/// 1. portproxy.json `name` at workspace root
/// 2. package.json `portproxy` key at workspace root
/// 3. most common npm scope across member packages (`@example/web` -> `example`)
/// 4. plain inference on the root (package.json name -> git root -> basename)
pub fn project_name(ws: &Workspace) -> String {
    if let Some(pc) = crate::config::ProjectConfig::load(&ws.root) {
        if let Some(s) = pc.name.as_deref().map(sanitize_label) {
            if !s.is_empty() {
                return s;
            }
        }
    }
    if let Some(s) = crate::naming::package_json_portproxy_key(&ws.root)
        .as_deref()
        .map(sanitize_label)
    {
        if !s.is_empty() {
            return s;
        }
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for p in &ws.packages {
        if let Some(scope) = p.name.strip_prefix('@').and_then(|r| r.split('/').next()) {
            *counts.entry(scope).or_default() += 1;
        }
    }
    if let Some((scope, _)) = counts
        .into_iter()
        .max_by_key(|(s, n)| (*n, std::cmp::Reverse(s.to_string())))
    {
        let s = sanitize_label(scope);
        if !s.is_empty() {
            return s;
        }
    }
    crate::naming::infer_name_plain(&ws.root)
}

/// Package manager from lockfiles, walking up from `dir`.
pub fn detect_package_manager(dir: &Path) -> &'static str {
    let mut d = Some(dir);
    while let Some(cur) = d {
        if cur.join("pnpm-lock.yaml").exists() {
            return "pnpm";
        }
        if cur.join("yarn.lock").exists() {
            return "yarn";
        }
        if cur.join("bun.lockb").exists() || cur.join("bun.lock").exists() {
            return "bun";
        }
        if cur.join("package-lock.json").exists() {
            return "npm";
        }
        d = cur.parent();
    }
    "npm"
}

/// Final label for a member package: `<project>-<pkgshort>`, or bare
/// `<project>` when the short name equals the project name (Vercel rule,
/// adapted to single-label routing).
pub fn package_label(project: &str, pkg: &Package) -> String {
    let short = sanitize_label(&pkg.short);
    if short.is_empty() || short == project {
        project.to_string()
    } else {
        sanitize_label(&format!("{project}-{short}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(p: &Path, content: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn pnpm_monorepo() -> tempfile::TempDir {
        let d = tempdir().unwrap();
        write(
            &d.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - apps/site\n",
        );
        write(&d.path().join("package.json"), r#"{"name":"root"}"#);
        write(
            &d.path().join("packages/web/package.json"),
            r#"{"name":"@example/web","scripts":{"dev":"vite"}}"#,
        );
        write(
            &d.path().join("packages/api/package.json"),
            r#"{"name":"@example/api","scripts":{"dev":"node server.js"}}"#,
        );
        write(
            &d.path().join("packages/tsconfig/package.json"),
            r#"{"name":"@other/tsconfig"}"#,
        );
        write(
            &d.path().join("apps/site/package.json"),
            r#"{"name":"site","scripts":{"dev":"next dev"}}"#,
        );
        d
    }

    #[test]
    fn pnpm_yaml_parsed_and_globs_expanded() {
        let d = pnpm_monorepo();
        let ws = find_workspace(&d.path().join("packages/web")).unwrap();
        assert_eq!(ws.root, d.path());
        let mut rels: Vec<&str> = ws.packages.iter().map(|p| p.rel.as_str()).collect();
        rels.sort();
        assert_eq!(
            rels,
            [
                "apps/site",
                "packages/api",
                "packages/tsconfig",
                "packages/web"
            ]
        );
    }

    #[test]
    fn npm_workspaces_field_works() {
        let d = tempdir().unwrap();
        write(
            &d.path().join("package.json"),
            r#"{"name":"root","workspaces":["pkgs/*"]}"#,
        );
        write(&d.path().join("pkgs/a/package.json"), r#"{"name":"a"}"#);
        let ws = find_workspace(d.path()).unwrap();
        assert_eq!(ws.packages.len(), 1);
        assert_eq!(ws.packages[0].short, "a");
    }

    #[test]
    fn project_name_from_scope_majority() {
        let d = pnpm_monorepo();
        let ws = find_workspace(d.path()).unwrap();
        // @example x2 beats @other x1
        assert_eq!(project_name(&ws), "example");
    }

    #[test]
    fn project_name_config_beats_scope() {
        let d = pnpm_monorepo();
        write(&d.path().join("portproxy.json"), r#"{"name":"custom"}"#);
        let ws = find_workspace(d.path()).unwrap();
        assert_eq!(project_name(&ws), "custom");
    }

    #[test]
    fn package_label_combines_or_collapses() {
        let d = pnpm_monorepo();
        let ws = find_workspace(d.path()).unwrap();
        let web = ws.packages.iter().find(|p| p.short == "web").unwrap();
        assert_eq!(package_label("example", web), "example-web");
        let same = Package {
            short: "example".into(),
            ..web.clone()
        };
        assert_eq!(package_label("example", &same), "example");
    }

    #[test]
    fn package_containing_picks_deepest() {
        let d = pnpm_monorepo();
        let ws = find_workspace(d.path()).unwrap();
        let deep = d.path().join("packages/web/src/components");
        assert_eq!(ws.package_containing(&deep).unwrap().short, "web");
        assert!(ws.package_containing(&d.path().join("elsewhere")).is_none());
    }

    #[test]
    fn no_workspace_files_means_none() {
        let d = tempdir().unwrap();
        write(&d.path().join("package.json"), r#"{"name":"plain"}"#);
        assert!(find_workspace(d.path()).is_none());
    }
}
