#![cfg(target_os = "linux")]

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use portproxy::types::Route;
use std::collections::HashSet;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(8);

struct ManagedWrapper {
    child: Child,
}

impl ManagedWrapper {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().is_ok_and(|status| status.is_none())
    }
}

impl Drop for ManagedWrapper {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|status| status.is_some()) {
            return;
        }

        let _ = kill(Pid::from_raw(self.child.id() as i32), Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.child.try_wait().is_ok_and(|status| status.is_some()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn unused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    ready()
}

fn process_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.chars().next()
}

fn child_pids(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|value| value.parse().ok())
        .collect()
}

fn route_owners(state: &Path) -> HashSet<u32> {
    std::fs::read_to_string(state.join("routes.json"))
        .ok()
        .and_then(|data| serde_json::from_str::<Vec<Route>>(&data).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|route| route.pid)
        .collect()
}

fn proxy_pid(state: &Path) -> Option<u32> {
    std::fs::read_to_string(state.join("proxy.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn wrapper_command(state: &Path, listen_port: u16, name: &str, app_port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_portproxy"));
    command
        .args([
            "run",
            "--name",
            name,
            "--app-port",
            &app_port.to_string(),
            "sleep",
            "30",
        ])
        .env("PORTPROXY_STATE_DIR", state)
        .env("PORTPROXY_LISTEN", format!("127.0.0.1:{listen_port}"))
        .env_remove("PORTPROXY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn gated_wrapper(
    state: &Path,
    listen_port: u16,
    name: &str,
    app_port: u16,
) -> (ManagedWrapper, ChildStdin) {
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "IFS= read -r _; exec \"$1\" run --name \"$2\" --app-port \"$3\" sleep 30",
            "sh",
            env!("CARGO_BIN_EXE_portproxy"),
            name,
            &app_port.to_string(),
        ])
        .env("PORTPROXY_STATE_DIR", state)
        .env("PORTPROXY_LISTEN", format!("127.0.0.1:{listen_port}"))
        .env_remove("PORTPROXY")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    (ManagedWrapper { child }, stdin)
}

#[test]
fn exited_proxy_is_reaped_while_wrapper_stays_alive() {
    let state = tempfile::tempdir().unwrap();
    let listen_port = unused_port();
    let mut wrapper = ManagedWrapper {
        child: wrapper_command(state.path(), listen_port, "reaper-test", unused_port())
            .spawn()
            .unwrap(),
    };
    let wrapper_pid = wrapper.pid();

    assert!(
        wait_until(PROCESS_TIMEOUT, || {
            route_owners(state.path()).contains(&wrapper_pid) && proxy_pid(state.path()).is_some()
        }),
        "wrapper did not register its route and start a proxy"
    );
    let proxy_pid = proxy_pid(state.path()).unwrap();

    kill(Pid::from_raw(proxy_pid as i32), Signal::SIGTERM).unwrap();

    assert!(
        wait_until(Duration::from_secs(3), || process_state(proxy_pid)
            .is_none()),
        "proxy PID {proxy_pid} was not reaped; state={:?}",
        process_state(proxy_pid)
    );
    assert!(
        wrapper.is_running(),
        "wrapper exited instead of reaping its proxy child"
    );
}

#[test]
fn concurrent_wrappers_share_one_proxy_without_zombies_or_bind_failures() {
    let state = tempfile::tempdir().unwrap();
    let listen_port = unused_port();
    let mut wrappers = Vec::new();
    let mut gates = Vec::new();

    for index in 0..4u16 {
        let (wrapper, gate) = gated_wrapper(
            state.path(),
            listen_port,
            &format!("concurrent-{index}"),
            30_000 + index,
        );
        wrappers.push(wrapper);
        gates.push(gate);
    }

    let wrapper_pids: HashSet<u32> = wrappers.iter().map(ManagedWrapper::pid).collect();
    for mut gate in gates {
        gate.write_all(b"go\n").unwrap();
    }

    assert!(
        wait_until(PROCESS_TIMEOUT, || {
            let owners = route_owners(state.path());
            wrapper_pids.iter().all(|pid| owners.contains(pid))
        }),
        "not every concurrent wrapper registered a route"
    );

    std::thread::sleep(Duration::from_millis(300));
    let zombie_children: Vec<(u32, u32)> = wrappers
        .iter()
        .flat_map(|wrapper| {
            child_pids(wrapper.pid())
                .into_iter()
                .filter(|child| process_state(*child) == Some('Z'))
                .map(|child| (wrapper.pid(), child))
                .collect::<Vec<_>>()
        })
        .collect();
    let log = std::fs::read_to_string(state.path().join("proxy.log")).unwrap_or_default();

    assert!(
        zombie_children.is_empty(),
        "concurrent startup left zombie children: {zombie_children:?}"
    );
    assert!(
        !log.contains("Address already in use"),
        "concurrent startup raced on the proxy listen socket:\n{log}"
    );
    let shared_proxy_state = proxy_pid(state.path()).and_then(process_state);
    assert!(
        shared_proxy_state.is_some_and(|state| state != 'Z'),
        "the shared proxy should remain live, state={shared_proxy_state:?}"
    );
}
