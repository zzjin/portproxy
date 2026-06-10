use crate::utils::{MAX_APP_PORT, MIN_APP_PORT};
use rand::Rng;

pub fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

pub fn find_free_port() -> Option<u16> {
    let mut rng = rand::thread_rng();
    for _ in 0..50 {
        let p = rng.gen_range(MIN_APP_PORT..=MAX_APP_PORT);
        if port_free(p) {
            return Some(p);
        }
    }
    (MIN_APP_PORT..=MAX_APP_PORT).find(|&p| port_free(p))
}

/// Tool basename after skipping package runners (npx, pnpx, bunx, `pnpm dlx`,
/// `yarn dlx`, `npm exec`).
fn detect_tool(cmd: &[String]) -> Option<String> {
    let mut i = 0;
    while i < cmd.len() {
        let base = std::path::Path::new(&cmd[i])
            .file_name()?
            .to_string_lossy()
            .to_string();
        match base.as_str() {
            "npx" | "pnpx" | "bunx" => i += 1,
            "pnpm" | "yarn" if cmd.get(i + 1).map(String::as_str) == Some("dlx") => i += 2,
            "npm" if cmd.get(i + 1).map(String::as_str) == Some("exec") => i += 2,
            _ => return Some(base),
        }
    }
    None
}

/// Append --port/--host flags for tools that ignore $PORT.
pub fn inject_framework_flags(cmd: &[String], port: u16) -> Vec<String> {
    let mut out = cmd.to_vec();
    let Some(tool) = detect_tool(cmd) else {
        return out;
    };
    let p = port.to_string();
    match tool.as_str() {
        "vite" | "react-router" | "rsbuild" => {
            out.extend([
                "--port".into(),
                p,
                "--strictPort".into(),
                "--host".into(),
                "127.0.0.1".into(),
            ]);
        }
        "astro" | "ng" => {
            out.extend(["--port".into(), p, "--host".into(), "127.0.0.1".into()]);
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_free_port_in_range() {
        let p = find_free_port().unwrap();
        assert!((MIN_APP_PORT..=MAX_APP_PORT).contains(&p));
        assert!(port_free(p));
    }

    #[test]
    fn vite_gets_port_strictport_host() {
        let out = inject_framework_flags(&v(&["vite"]), 4123);
        assert_eq!(
            out,
            v(&[
                "vite",
                "--port",
                "4123",
                "--strictPort",
                "--host",
                "127.0.0.1"
            ])
        );
    }

    #[test]
    fn npx_runner_skipped_for_detection() {
        let out = inject_framework_flags(&v(&["npx", "astro", "dev"]), 4001);
        assert_eq!(
            out,
            v(&[
                "npx",
                "astro",
                "dev",
                "--port",
                "4001",
                "--host",
                "127.0.0.1"
            ])
        );
    }

    #[test]
    fn pnpm_dlx_runner_skipped() {
        let out = inject_framework_flags(&v(&["pnpm", "dlx", "vite"]), 4002);
        assert_eq!(
            out,
            v(&[
                "pnpm",
                "dlx",
                "vite",
                "--port",
                "4002",
                "--strictPort",
                "--host",
                "127.0.0.1"
            ])
        );
    }

    #[test]
    fn unknown_tool_untouched() {
        let cmd = v(&["next", "dev"]);
        assert_eq!(inject_framework_flags(&cmd, 4001), cmd);
    }
}
