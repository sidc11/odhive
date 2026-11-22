//! The OdHive background service: running it, starting it detached, opening
//! the browser on it, and asking it to quit.
//!
//! One service runs per user: it holds the lock of
//! [`odhive_core::service_lock`] and publishes its port next to it. The
//! launcher (`odhive` without a command) starts it detached when it is not
//! running and signs the browser in with a one-time code from the API.

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use odhive_core::config::AppPaths;
use odhive_core::manager::{Manager, ManagerOptions, Recovery};
use odhive_core::pidfile::RunDir;
use odhive_core::ports::{DEFAULT_PORT_ATTEMPTS, bind_loopback};
use odhive_core::service_lock::{self, Acquired, ServiceLock, ServiceState};
use odhive_core::token::{self, Token};
use odhive_core::workspace::bootstrap;
use odhive_server::Service;
use odhive_server::auth::LoginCode;
use tokio::signal::unix::{SignalKind, signal};

use crate::autostart;
use crate::client::Client;

/// How long running jobs get to clean up when the service ends.
const JOB_CANCEL_WAIT: Duration = Duration::from_secs(20);

/// How long a new service gets to answer.
const START_WAIT: Duration = Duration::from_secs(30);

/// How long `odhive quit` waits for the service to stop its instances.
const QUIT_WAIT: Duration = Duration::from_secs(60);

/// How long the browser opener gets to report a failure.
const OPENER_WAIT: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(100);

/// How [`run`] was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// In a terminal: prints a sign-in link, ends on Ctrl+C or a hang-up.
    Foreground,
    /// By the launcher, without a terminal: output goes to the service log,
    /// hang-ups are ignored.
    Detached,
}

/// Runs the service in this process until Ctrl+C, SIGTERM or `odhive quit`.
pub async fn run(port: u16, mode: Mode) -> anyhow::Result<ExitCode> {
    let paths = AppPaths::discover()?;
    let mut lock = match ServiceLock::acquire(&paths.data_dir)
        .context("could not take the OdHive service lock")?
    {
        Acquired::Locked(lock) => lock,
        Acquired::Running(state) => {
            eprintln!("{}", already_running(state.as_ref()));
            // Detached, the service is wanted and it runs: that is success,
            // and service managers must not restart this one over and over.
            return Ok(if mode == Mode::Detached {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
    };
    let boot = bootstrap(&paths).context("could not prepare the OdHive workspace")?;
    if boot.config_created {
        tracing::info!(path = %paths.config_file().display(), "created config");
    }
    for dir in &boot.created {
        tracing::info!(path = %dir.display(), "created folder");
    }
    tracing::info!(root = %boot.workspace.root().display(), "workspace ready");
    // Refuses a token file others could read: they could drive OdHive.
    let token = token::load_or_create(&paths.token_file())
        .context("could not read the OdHive API token")?;
    let auth = odhive_server::Auth::new(token);

    let listeners = bind_loopback(port, DEFAULT_PORT_ATTEMPTS)
        .await
        .context("could not bind the OdHive UI port")?;
    if listeners.port != port {
        tracing::warn!(
            requested = port,
            actual = listeners.port,
            "port {} is busy, using {}",
            port,
            listeners.port
        );
    }

    let manager = Manager::new(ManagerOptions {
        run_dir: Some(RunDir::new(paths.run_dir())),
        ..ManagerOptions::default()
    });
    log_recovery(&manager)?;
    let service = Service::new(paths.clone(), auth.clone(), boot.workspace, manager.clone());

    // Registered before anyone can find the service, so no signal is missed.
    let mut interrupt = signal(SignalKind::interrupt()).context("could not watch for Ctrl+C")?;
    let mut terminate = signal(SignalKind::terminate()).context("could not watch for SIGTERM")?;
    // A handler replaces the default action (exit): detached, hang-ups are
    // ignored; in a terminal, closing it ends the service cleanly.
    let mut hangup = signal(SignalKind::hangup()).context("could not watch for SIGHUP")?;

    lock.publish(&ServiceState {
        pid: std::process::id(),
        port: listeners.port,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
    .context("could not publish the service state")?;

    if mode == Mode::Foreground {
        let login = auth
            .issue_code()
            .context("could not create a sign-in code")?;
        // The URL holds a secret: printed for the user only, never logged.
        eprintln!(
            "Open OdHive (link valid for {} s, once): {}{}",
            login.expires_in,
            listeners.url(),
            login.path
        );
    }

    let quit = service.clone();
    let served = odhive_server::serve(listeners, service.clone(), async move {
        let why = loop {
            tokio::select! {
                _ = interrupt.recv() => break "interrupted",
                _ = terminate.recv() => break "terminated",
                () = quit.quit_requested() => break "asked to quit",
                _ = hangup.recv() => {
                    if mode == Mode::Foreground {
                        break "terminal closed";
                    }
                }
            }
        };
        tracing::info!("shutting down ({why})");
    })
    .await;
    // A half-done clone or setup is undone rather than left behind.
    service.cancel_jobs(JOB_CANCEL_WAIT).await;
    for (owner, result) in manager.shutdown().await {
        if let Err(e) = result {
            tracing::warn!(instance = %owner, "could not stop: {e}");
        }
    }
    // Released last: once the lock is free, the instances are stopped.
    drop(lock);
    served.context("server error")?;
    tracing::info!("stopped");
    Ok(ExitCode::SUCCESS)
}

fn log_recovery(manager: &Manager) -> anyhow::Result<()> {
    for r in manager.recover().context("could not read the PID files")? {
        match r {
            Recovery::Attached { owner, pid, .. } => {
                tracing::info!(instance = %owner, pid, "re-attached running instance");
            }
            Recovery::SupervisedElsewhere { owner, pid, by } => {
                tracing::info!(instance = %owner, pid, by, "instance runs in another OdHive");
            }
            Recovery::Stale { owner, pid, .. } => {
                tracing::info!(instance = %owner, pid, "removed stale PID file");
            }
            Recovery::Invalid { path, reason } => {
                tracing::warn!(path = %path.display(), %reason, "removed unusable PID file");
            }
        }
    }
    Ok(())
}

fn already_running(state: Option<&ServiceState>) -> String {
    match state {
        Some(s) => format!(
            "OdHive is already running at http://localhost:{} (pid {})",
            s.port, s.pid
        ),
        None => "OdHive is already running (it is starting up)".to_owned(),
    }
}

/// `odhive serve`: starts the service in the background.
pub async fn start(port: u16) -> anyhow::Result<ExitCode> {
    let paths = AppPaths::discover()?;
    if let Some(state) = service_lock::probe(&paths.data_dir)? {
        eprintln!("{}", already_running(state.as_ref()));
        return Ok(ExitCode::FAILURE);
    }
    let up = start_service(&paths, port).await?;
    eprintln!(
        "OdHive is running at http://localhost:{} (pid {}); `odhive quit` stops it",
        up.state.port, up.state.pid
    );
    Ok(ExitCode::SUCCESS)
}

/// `odhive` without a command: makes sure the service runs, then signs the
/// browser in (or prints the sign-in link).
pub async fn open(port: u16, browser: bool) -> anyhow::Result<ExitCode> {
    let paths = AppPaths::discover()?;
    let up = match service_lock::probe(&paths.data_dir)? {
        None => {
            let up = start_service(&paths, port).await?;
            eprintln!(
                "Started OdHive in the background (pid {}); `odhive quit` stops it",
                up.state.pid
            );
            up
        }
        Some(_) => wait_ready(&paths, None).await?,
    };
    if up.state.version != env!("CARGO_PKG_VERSION") {
        eprintln!(
            "note: the running service is OdHive {}, this is {}; run `odhive quit` and `odhive` again to switch",
            up.state.version,
            env!("CARGO_PKG_VERSION")
        );
    }
    let code: LoginCode = up.client.post("/api/v1/auth/codes").await?.json()?;
    let url = format!("http://localhost:{}{}", up.state.port, code.path);
    if browser && open_browser(&paths.data_dir, &url).await {
        eprintln!(
            "Opened OdHive in your browser: http://localhost:{}",
            up.state.port
        );
    } else {
        // The URL holds a secret: printed for the user only, never logged.
        eprintln!(
            "Open OdHive (link valid for {} s, once): {url}",
            code.expires_in
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `odhive quit`: asks the service to stop its instances and exit, and
/// waits until it has.
pub async fn quit() -> anyhow::Result<ExitCode> {
    let paths = AppPaths::discover()?;
    let Some(state) = service_lock::probe(&paths.data_dir)? else {
        eprintln!("OdHive is not running");
        return Ok(ExitCode::SUCCESS);
    };
    let Some(state) = state else {
        bail!("OdHive is starting up; try again in a moment");
    };
    let client = Client::new(state.port, &read_token(&paths)?);
    let reply = client.post("/api/v1/service/quit").await?;
    if !reply.status.is_success() {
        bail!("{}", reply.error_text());
    }
    eprintln!("Stopping OdHive (pid {})…", state.pid);
    let deadline = Instant::now() + QUIT_WAIT;
    while service_lock::probe(&paths.data_dir)?.is_some() {
        if Instant::now() >= deadline {
            bail!(
                "OdHive (pid {}) is still stopping after {} s",
                state.pid,
                QUIT_WAIT.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
    eprintln!("OdHive stopped");
    Ok(ExitCode::SUCCESS)
}

/// A service that answers.
struct Up {
    state: ServiceState,
    client: Client,
}

/// Starts the service: through the service manager when `odhive service
/// install` registered it there (it then owns the process and its port),
/// else by spawning it.
async fn start_service(paths: &AppPaths, port: u16) -> anyhow::Result<Up> {
    if autostart::installed(paths) {
        match autostart::start_installed().await {
            Ok(()) => {
                return wait_ready(paths, None)
                    .await
                    .with_context(|| format!("see {}", autostart::log_hint(paths)));
            }
            Err(e) => eprintln!("note: {e:#}; starting OdHive directly"),
        }
    }
    launch(paths, port).await
}

/// Waits until a service started elsewhere (by the service manager) answers.
pub async fn wait_started(paths: &AppPaths) -> anyhow::Result<ServiceState> {
    Ok(wait_ready(paths, None).await?.state)
}

/// Starts `odhive serve --foreground --detached` in its own process group,
/// with output going to the service log, and waits until it answers.
async fn launch(paths: &AppPaths, port: u16) -> anyhow::Result<Up> {
    let exe = std::env::current_exe().context("could not find the odhive executable")?;
    let log = service_lock::create_log(&paths.data_dir)?;
    let log_path = paths.data_dir.join(service_lock::LOG_FILE_NAME);
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--port")
        .arg(port.to_string())
        .args(["serve", "--foreground", "--detached"])
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // Away from the terminal's process group: Ctrl+C there doesn't reach it.
        .process_group(0);
    let child = cmd.spawn().context("could not start the OdHive service")?;
    wait_ready(paths, Some((child, log_path))).await
}

/// Waits until the service publishes its state and answers. With `child`,
/// gives up when that process ended and no other service took over.
async fn wait_ready(
    paths: &AppPaths,
    mut child: Option<(std::process::Child, PathBuf)>,
) -> anyhow::Result<Up> {
    let deadline = Instant::now() + START_WAIT;
    loop {
        match service_lock::probe(&paths.data_dir)? {
            Some(Some(state)) => {
                // The service creates the token before it publishes its state.
                let client = Client::new(state.port, &read_token(paths)?);
                if let Ok(reply) = client.get("/api/v1/health").await
                    && reply.status.is_success()
                {
                    return Ok(Up { state, client });
                }
            }
            Some(None) => {}
            None => {
                if let Some((proc, log)) = child.as_mut()
                    && let Some(status) = proc.try_wait()?
                {
                    bail!("the OdHive service exited ({status}){}", log_tail(log));
                }
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "the OdHive service did not answer within {} s",
                START_WAIT.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

fn read_token(paths: &AppPaths) -> anyhow::Result<Token> {
    token::load(&paths.token_file())
        .context("could not read the OdHive API token")?
        .context("the OdHive service has no API token yet")
}

/// The last lines of the service log, for an error message.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(5)..];
    if tail.is_empty() {
        String::new()
    } else {
        format!(":\n{}\n(log: {})", tail.join("\n"), log.display())
    }
}

/// Name of the page that sends the browser to the sign-in link.
const OPEN_PAGE: &str = "open.html";

/// Opens `url` in the user's browser. The browser gets a private local page
/// that redirects to it, not the URL itself: command lines are visible to
/// other users of the machine, and the URL signs in whoever opens it first.
/// Returns `false` when no browser could be started.
async fn open_browser(data_dir: &Path, url: &str) -> bool {
    let Some(opener) = opener() else {
        return false;
    };
    let page = data_dir.join(OPEN_PAGE);
    let Some(html) = redirect_page(url) else {
        return false;
    };
    if let Err(e) = odhive_core::fsutil::write_private_atomic(&page, html.as_bytes()) {
        tracing::warn!(path = %e.path.display(), "could not write the browser page: {}", e.source);
        return false;
    }
    let child = tokio::process::Command::new(&opener)
        .arg(&page)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(opener = %opener.display(), "could not open the browser: {e}");
            return false;
        }
    };
    match tokio::time::timeout(OPENER_WAIT, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(_)) => false,
        // Still running: it is the browser itself, so it started.
        Err(_) => true,
    }
}

/// The program that opens a file in the browser: the first entry of
/// `$BROWSER` (a program, run without a shell), else `open` on macOS and
/// `xdg-open` on Linux when there is a display.
fn opener() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let find = |name: &str| {
        let p = Path::new(name);
        if p.is_absolute() {
            p.is_file().then(|| p.to_path_buf())
        } else if name.contains('/') {
            None
        } else {
            odhive_core::python::find_executable(&path, name)
        }
    };
    if let Some(browser) = std::env::var("BROWSER").ok().filter(|b| !b.is_empty()) {
        let first = browser.split(':').next().unwrap_or_default().trim();
        return find(first);
    }
    if cfg!(target_os = "macos") {
        return find("open");
    }
    let display = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|d| !d.is_empty()));
    if display { find("xdg-open") } else { None }
}

/// A page that redirects to `url`; `None` unless `url` is a plain
/// `http://localhost` link (nothing to escape).
fn redirect_page(url: &str) -> Option<String> {
    let plain = url.starts_with("http://localhost:")
        && url
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b":/?=._-".contains(&b));
    plain.then(|| {
        format!(
            "<!doctype html>\n<meta charset=\"utf-8\">\n<meta name=\"referrer\" content=\"no-referrer\">\n<meta http-equiv=\"refresh\" content=\"0;url={url}\">\n<title>OdHive</title>\n<p><a href=\"{url}\">Open OdHive</a></p>\n"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_pages_only_take_plain_local_links() {
        let page = redirect_page("http://localhost:7069/login?t=ab12").expect("page");
        assert!(page.contains("content=\"0;url=http://localhost:7069/login?t=ab12\""));
        for bad in [
            "http://evil.com/login?t=a",
            "http://localhost:7069/\"><script>",
            "javascript:alert(1)",
            "http://localhost:7069/a b",
        ] {
            assert!(redirect_page(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn already_running_names_the_port_and_pid() {
        let s = ServiceState {
            pid: 42,
            port: 7070,
            version: "x".into(),
        };
        assert_eq!(
            already_running(Some(&s)),
            "OdHive is already running at http://localhost:7070 (pid 42)"
        );
        assert!(already_running(None).contains("starting"));
    }
}
