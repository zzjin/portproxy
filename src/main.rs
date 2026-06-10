use anyhow::{bail, Context, Result};
use colored::Colorize;
use portproxy::config::GlobalConfig;
use portproxy::routes::RouteStore;
use portproxy::{naming, ports, proxy, utils, worktree};
use std::path::Path;
use std::process::ExitCode;

const RESERVED: &[&str] = &[
    "run", "proxy", "list", "get", "alias", "prune", "clean", "help",
];

const USAGE: &str = "\
portproxy - stable named URLs for dev servers behind Caddy/Nginx

USAGE:
  portproxy run <cmd...>             run with auto-inferred name
  portproxy <name> <cmd...>          run with explicit name
  portproxy proxy start [--foreground] [-l ADDR]
  portproxy proxy stop
  portproxy list                     show active routes
  portproxy get <name>               print URL (needs base_domain in config)
  portproxy alias <name> <port> [--remove] [--force]
  portproxy prune [--force]          kill orphaned dev servers
  portproxy clean                    stop proxy and remove all state

RUN FLAGS:
  --name <n>      override inferred name (worktree suffix still applies)
  --force         take over a name registered by a live process
  --app-port <p>  fixed backend port instead of random 4000-4999

ENV:
  PORTPROXY=0           bypass portproxy, run the command directly
  PORTPROXY_STATE_DIR   state directory (default ~/.portproxy)

CONFIG (~/.portproxy/config.toml):
  listen = \"127.0.0.1:1355\"        proxy listen address
  base_domain = \"dev.example.test\"  for printed URLs only
  scheme = \"https\"                 for printed URLs only
";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(args).await {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("{} {e:#}", "portproxy:".red().bold());
            ExitCode::from(1)
        }
    }
}

async fn dispatch(args: Vec<String>) -> Result<i32> {
    let Some(first) = args.first().map(String::as_str) else {
        println!("{USAGE}");
        return Ok(0);
    };
    match first {
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(0)
        }
        "-V" | "--version" => {
            println!("portproxy {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "run" => {
            let (flags, cmd) = split_flags(&args[1..])?;
            cmd_run(flags, cmd).await
        }
        "proxy" => cmd_proxy(&args[1..]).await,
        "list" => cmd_list(),
        "get" => cmd_get(&args[1..]),
        "alias" => cmd_alias(&args[1..]),
        "prune" => cmd_prune(args.iter().any(|a| a == "--force")),
        "clean" => cmd_clean(),
        name if !name.starts_with('-') => {
            // explicit-name form: portproxy <name> <cmd...>
            if args.len() < 2 {
                bail!("missing command after name \"{name}\" (see `portproxy help`)");
            }
            let (mut flags, cmd) = split_flags(&args[1..])?;
            if flags.name.is_none() {
                flags.name = Some(name.to_string());
            }
            cmd_run(flags, cmd).await
        }
        _ => bail!("unknown option {first} (see `portproxy help`)"),
    }
}

#[derive(Default)]
struct RunFlags {
    name: Option<String>,
    force: bool,
    app_port: Option<u16>,
}

/// Split leading portproxy flags from the command to execute.
fn split_flags(args: &[String]) -> Result<(RunFlags, Vec<String>)> {
    let mut flags = RunFlags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                flags.name = Some(args.get(i + 1).context("--name requires a value")?.clone());
                i += 2;
            }
            "--force" => {
                flags.force = true;
                i += 1;
            }
            "--app-port" => {
                flags.app_port = Some(
                    args.get(i + 1)
                        .context("--app-port requires a value")?
                        .parse()
                        .context("--app-port must be a port number")?,
                );
                i += 2;
            }
            _ => break,
        }
    }
    let cmd = args[i..].to_vec();
    if cmd.is_empty() {
        bail!("no command given (see `portproxy help`)");
    }
    Ok((flags, cmd))
}

async fn cmd_run(flags: RunFlags, cmd: Vec<String>) -> Result<i32> {
    if std::env::var("PORTPROXY").is_ok_and(|v| v == "0" || v == "skip") {
        return exec_passthrough(&cmd).await;
    }
    let cwd = std::env::current_dir()?;
    let base = match &flags.name {
        Some(n) => utils::sanitize_label(n),
        None => naming::infer_name(&cwd),
    };
    let label = match worktree::worktree_suffix(&cwd) {
        Some(sfx) => utils::sanitize_label(&format!("{base}-{sfx}")),
        None => base,
    };
    if label.is_empty() {
        bail!("could not infer a usable name; pass --name");
    }
    if RESERVED.contains(&label.as_str()) {
        bail!("\"{label}\" is a reserved subcommand name; pass a different --name");
    }

    let state = utils::state_dir();
    let cfg = GlobalConfig::load(&state);
    ensure_proxy(&state, &cfg).await?;

    let port = match flags.app_port {
        Some(p) => p,
        None => ports::find_free_port().context("no free port in 4000-4999")?,
    };
    let store = RouteStore::new(state.clone());
    store.add_route(&label, port, std::process::id(), flags.force)?;

    let final_cmd = ports::inject_framework_flags(&cmd, port);
    let url = cfg.url_for(&label);
    eprintln!(
        "{} {} -> 127.0.0.1:{}{}",
        "portproxy:".green().bold(),
        label.bold(),
        port,
        url.as_deref()
            .map(|u| format!("  ({u})"))
            .unwrap_or_default()
    );

    let code = run_child(&final_cmd, port, &label, url.as_deref()).await;

    let _ = store.remove_route(&label);
    shutdown_proxy_if_idle(&state, &store);
    code
}

async fn exec_passthrough(cmd: &[String]) -> Result<i32> {
    let status = tokio::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .status()
        .await
        .with_context(|| format!("failed to run {:?}", cmd[0]))?;
    Ok(exit_code(status))
}

async fn run_child(cmd: &[String], port: u16, label: &str, url: Option<&str>) -> Result<i32> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut c = tokio::process::Command::new(&cmd[0]);
    c.args(&cmd[1..])
        .env("PORT", port.to_string())
        .env("HOST", "127.0.0.1")
        .env("PORTPROXY_NAME", label)
        .env("__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS", ".localhost");
    if let Some(u) = url {
        c.env("PORTPROXY_URL", u);
    }
    // own process group so signals reach the whole dev-server tree
    unsafe {
        c.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = c
        .spawn()
        .with_context(|| format!("failed to run {:?}", cmd[0]))?;
    let pgid = child.id().map(|p| p as i32);

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    loop {
        tokio::select! {
            status = child.wait() => {
                return Ok(exit_code(status?));
            }
            _ = sigint.recv() => forward(pgid, nix::sys::signal::Signal::SIGINT),
            _ = sigterm.recv() => forward(pgid, nix::sys::signal::Signal::SIGTERM),
        }
    }
}

fn forward(pgid: Option<i32>, sig: nix::sys::signal::Signal) {
    if let Some(p) = pgid {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(p), sig);
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

async fn ensure_proxy(state: &Path, cfg: &GlobalConfig) -> Result<()> {
    if utils::is_proxy_running(&cfg.listen) {
        return Ok(());
    }
    std::fs::create_dir_all(state)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join("proxy.log"))?;
    let exe = std::env::current_exe()?;
    let mut c = std::process::Command::new(exe);
    c.args(["proxy", "start", "--foreground", "--listen", &cfg.listen])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        use std::os::unix::process::CommandExt;
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    c.spawn()?;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if utils::is_proxy_running(&cfg.listen) {
            return Ok(());
        }
    }
    bail!(
        "proxy failed to start; see {}",
        state.join("proxy.log").display()
    );
}

fn shutdown_proxy_if_idle(state: &Path, store: &RouteStore) {
    if !store.load().is_empty() {
        return;
    }
    if let Ok(pid) = std::fs::read_to_string(state.join("proxy.pid")) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
    let _ = std::fs::remove_file(state.join("proxy.pid"));
    let _ = std::fs::remove_file(state.join("proxy.port"));
}

async fn cmd_proxy(args: &[String]) -> Result<i32> {
    let state = utils::state_dir();
    let cfg = GlobalConfig::load(&state);
    match args.first().map(String::as_str) {
        Some("start") => {
            let listen = args
                .iter()
                .position(|a| a == "--listen" || a == "-l")
                .and_then(|i| args.get(i + 1).cloned())
                .unwrap_or_else(|| cfg.listen.clone());
            if args.iter().any(|a| a == "--foreground") {
                let addr: std::net::SocketAddr = listen
                    .parse()
                    .with_context(|| format!("invalid listen address {listen:?}"))?;
                // bind BEFORE writing pid files: a second racing proxy must
                // die here without ever touching (and later deleting) the
                // healthy proxy's state files
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("failed to bind {addr}"))?;
                std::fs::create_dir_all(&state)?;
                std::fs::write(state.join("proxy.pid"), std::process::id().to_string())?;
                std::fs::write(state.join("proxy.port"), addr.port().to_string())?;
                let store = RouteStore::new(state.clone());
                eprintln!("portproxy proxy listening on {addr}");

                let result = {
                    use tokio::signal::unix::{signal, SignalKind};
                    let mut sigterm = signal(SignalKind::terminate())?;
                    let mut sigint = signal(SignalKind::interrupt())?;
                    tokio::select! {
                        r = proxy::run_proxy(store, listener, proxy::ProxyOptions::default()) => r,
                        _ = sigterm.recv() => Ok(()),
                        _ = sigint.recv() => Ok(()),
                    }
                };
                let _ = std::fs::remove_file(state.join("proxy.pid"));
                let _ = std::fs::remove_file(state.join("proxy.port"));
                result?;
                eprintln!("portproxy proxy exiting (idle or signalled)");
                Ok(0)
            } else {
                let cfg = GlobalConfig { listen, ..cfg };
                ensure_proxy(&state, &cfg).await?;
                println!("proxy running on {}", cfg.listen);
                Ok(0)
            }
        }
        Some("stop") => {
            let pid_file = state.join("proxy.pid");
            match std::fs::read_to_string(&pid_file) {
                Ok(pid) => {
                    if let Ok(pid) = pid.trim().parse::<i32>() {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid),
                            nix::sys::signal::Signal::SIGTERM,
                        );
                        println!("stopped proxy (PID {pid})");
                    }
                    let _ = std::fs::remove_file(&pid_file);
                    let _ = std::fs::remove_file(state.join("proxy.port"));
                    Ok(0)
                }
                Err(_) => {
                    println!("proxy not running");
                    Ok(0)
                }
            }
        }
        _ => bail!("usage: portproxy proxy start|stop [--foreground] [-l ADDR]"),
    }
}

fn cmd_list() -> Result<i32> {
    let state = utils::state_dir();
    let cfg = GlobalConfig::load(&state);
    let routes = RouteStore::new(state).load();
    if routes.is_empty() {
        println!("no active routes");
        return Ok(0);
    }
    for r in routes {
        let owner = if r.pid == 0 {
            "alias".dimmed().to_string()
        } else {
            format!("pid {}", r.pid).dimmed().to_string()
        };
        let url = cfg
            .url_for(&r.hostname)
            .map(|u| format!("  {}", u.cyan()))
            .unwrap_or_default();
        println!(
            "{}  127.0.0.1:{}  {}{}",
            r.hostname.bold(),
            r.port,
            owner,
            url
        );
    }
    Ok(0)
}

fn cmd_get(args: &[String]) -> Result<i32> {
    let name = args.first().context("usage: portproxy get <name>")?;
    let state = utils::state_dir();
    let cfg = GlobalConfig::load(&state);
    let routes = RouteStore::new(state.clone()).load();
    let route = routes
        .iter()
        .find(|r| &r.hostname == name)
        .with_context(|| format!("no active route named \"{name}\""))?;
    match cfg.url_for(&route.hostname) {
        Some(url) => println!("{url}"),
        None => bail!(
            "base_domain not set in {}; cannot build a URL (route is on 127.0.0.1:{})",
            state.join("config.toml").display(),
            route.port
        ),
    }
    Ok(0)
}

fn cmd_alias(args: &[String]) -> Result<i32> {
    let usage = "usage: portproxy alias <name> <port> [--remove] [--force]";
    let name = args.first().context(usage)?;
    let label = utils::sanitize_label(name);
    if label.is_empty() {
        bail!("invalid alias name {name:?}");
    }
    let store = RouteStore::new(utils::state_dir());
    if args.iter().any(|a| a == "--remove") {
        store.remove_route(&label)?;
        println!("removed alias {label}");
        return Ok(0);
    }
    let port: u16 = args
        .get(1)
        .context(usage)?
        .parse()
        .context("port must be a number")?;
    let force = args.iter().any(|a| a == "--force");
    store.add_route(&label, port, 0, force)?;
    println!("alias {label} -> 127.0.0.1:{port}");
    Ok(0)
}

fn cmd_prune(force: bool) -> Result<i32> {
    let store = RouteStore::new(utils::state_dir());
    let mut killed = 0usize;
    for r in store.load_raw() {
        if r.pid == 0 || utils::pid_alive(r.pid) {
            continue; // alias or healthy
        }
        // wrapper died; kill whatever still squats on its port
        let out = std::process::Command::new("lsof")
            .args(["-ti", &format!("tcp:{}", r.port)])
            .output();
        if let Ok(out) = out {
            for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                if let Ok(pid) = pid.parse::<i32>() {
                    let sig = if force {
                        nix::sys::signal::Signal::SIGKILL
                    } else {
                        nix::sys::signal::Signal::SIGTERM
                    };
                    if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), sig).is_ok() {
                        killed += 1;
                        println!(
                            "killed orphan PID {pid} (route {}, port {})",
                            r.hostname, r.port
                        );
                    }
                }
            }
        }
        store.remove_raw_entry(&r.hostname)?;
    }
    println!("pruned {killed} orphan process(es)");
    Ok(0)
}

fn cmd_clean() -> Result<i32> {
    let state = utils::state_dir();
    if let Ok(pid) = std::fs::read_to_string(state.join("proxy.pid")) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
    if state.exists() {
        std::fs::remove_dir_all(&state)
            .with_context(|| format!("failed to remove {}", state.display()))?;
    }
    println!("removed {}", state.display());
    Ok(0)
}
