use crate::types::Route;
use crate::utils::pid_alive;
use anyhow::{bail, Result};
use std::path::PathBuf;
use std::time::Duration;

pub struct RouteStore {
    dir: PathBuf,
}

impl RouteStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn routes_path(&self) -> PathBuf {
        self.dir.join("routes.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("routes.lock")
    }

    /// Routes as on disk, including dead-PID entries (used by `prune`).
    pub fn load_raw(&self) -> Vec<Route> {
        let Ok(data) = std::fs::read_to_string(self.routes_path()) else {
            return vec![];
        };
        // corrupt file => self-heal as empty
        serde_json::from_str(&data).unwrap_or_default()
    }

    /// Live routes: aliases (pid 0) plus entries whose wrapper PID is alive.
    pub fn load(&self) -> Vec<Route> {
        self.load_raw()
            .into_iter()
            .filter(|r| r.pid == 0 || pid_alive(r.pid))
            .collect()
    }

    fn save(&self, routes: &[Route]) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(".routes.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(routes)?)?;
        std::fs::rename(&tmp, self.routes_path())?;
        Ok(())
    }

    fn with_lock<T>(&self, f: impl FnOnce(&mut Vec<Route>) -> Result<T>) -> Result<T> {
        std::fs::create_dir_all(&self.dir)?;
        let lock = self.lock_path();
        let mut waited = Duration::ZERO;
        loop {
            match std::fs::create_dir(&lock) {
                Ok(()) => break,
                Err(_) => {
                    // break stale locks older than 10s
                    if let Ok(meta) = std::fs::metadata(&lock) {
                        if meta
                            .modified()
                            .ok()
                            .and_then(|m| m.elapsed().ok())
                            .is_some_and(|e| e > Duration::from_secs(10))
                        {
                            let _ = std::fs::remove_dir(&lock);
                            continue;
                        }
                    }
                    if waited > Duration::from_secs(5) {
                        bail!("timed out waiting for route lock at {}", lock.display());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    waited += Duration::from_millis(50);
                }
            }
        }
        let result = (|| {
            // load() filters dead entries; saving below persists the cleanup
            let mut routes = self.load();
            let out = f(&mut routes)?;
            self.save(&routes)?;
            Ok(out)
        })();
        let _ = std::fs::remove_dir(&lock);
        result
    }

    pub fn add_route(&self, hostname: &str, port: u16, pid: u32, force: bool) -> Result<()> {
        let hostname = hostname.to_string();
        self.with_lock(|routes| {
            if let Some(existing) = routes.iter().find(|r| r.hostname == hostname) {
                let live = existing.pid == 0 || pid_alive(existing.pid);
                if live && !force {
                    bail!(
                        "\"{hostname}\" is already registered by a running process (PID {}). Use --force to override.",
                        existing.pid
                    );
                }
                if live && existing.pid != 0 {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(existing.pid as i32),
                        nix::sys::signal::Signal::SIGTERM,
                    );
                }
                routes.retain(|r| r.hostname != hostname);
            }
            routes.push(Route {
                hostname: hostname.clone(),
                port,
                pid,
            });
            Ok(())
        })
    }

    pub fn remove_route(&self, hostname: &str) -> Result<()> {
        self.with_lock(|routes| {
            routes.retain(|r| r.hostname != hostname);
            Ok(())
        })
    }

    /// Remove an entry from the raw on-disk list, keeping other dead entries
    /// intact (prune iterates raw entries one by one).
    pub fn remove_raw_entry(&self, hostname: &str) -> Result<()> {
        let mut raw = self.load_raw();
        raw.retain(|r| r.hostname != hostname);
        self.save(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> (RouteStore, tempfile::TempDir) {
        let d = tempdir().unwrap();
        (RouteStore::new(d.path().to_path_buf()), d)
    }

    #[test]
    fn add_and_load() {
        let (s, _d) = store();
        s.add_route("app", 4001, std::process::id(), false).unwrap();
        let r = s.load();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].port, 4001);
    }

    #[test]
    fn dead_pid_filtered_on_load_but_kept_raw() {
        let (s, d) = store();
        let dead = Route {
            hostname: "dead".into(),
            port: 4002,
            pid: 999_999_999, // far beyond pid_max => never alive
        };
        std::fs::write(
            d.path().join("routes.json"),
            serde_json::to_string(&vec![dead]).unwrap(),
        )
        .unwrap();
        assert!(s.load().is_empty());
        assert_eq!(s.load_raw().len(), 1);
    }

    #[test]
    fn conflict_live_pid_errors_without_force() {
        let (s, _d) = store();
        s.add_route("app", 4001, std::process::id(), false).unwrap();
        let e = s
            .add_route("app", 4002, std::process::id(), false)
            .unwrap_err();
        assert!(e.to_string().contains("already registered"));
    }

    #[test]
    fn force_replaces_route() {
        let (s, _d) = store();
        // alias owner (pid 0) so --force doesn't SIGTERM anything real
        s.add_route("app", 4001, 0, false).unwrap();
        s.add_route("app", 4002, 0, true).unwrap();
        let r = s.load();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].port, 4002);
    }

    #[test]
    fn alias_pid_zero_survives_load() {
        let (s, _d) = store();
        s.add_route("static", 8080, 0, false).unwrap();
        assert_eq!(s.load().len(), 1);
    }

    #[test]
    fn remove_route_works() {
        let (s, _d) = store();
        s.add_route("app", 4001, std::process::id(), false).unwrap();
        s.remove_route("app").unwrap();
        assert!(s.load().is_empty());
    }

    #[test]
    fn corrupt_file_self_heals() {
        let (s, d) = store();
        std::fs::write(d.path().join("routes.json"), "{not json").unwrap();
        assert!(s.load().is_empty());
        s.add_route("app", 4001, 0, false).unwrap();
        assert_eq!(s.load().len(), 1);
    }
}
