//! `odhive service install|uninstall`: lets the system's service manager
//! start the OdHive service at login — a systemd user unit on Linux, a
//! launchd agent on macOS.
//!
//! The installed service runs `odhive serve --foreground --detached`, the
//! same command the launcher starts. Once installed, the launcher starts the
//! service through the service manager instead of spawning it itself, so
//! there is only ever one owner.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use anyhow::{Context, bail};
use odhive_core::config::AppPaths;
use odhive_core::platform::effective_uid;
use odhive_core::python::find_executable;
use odhive_core::service_lock;

/// Name of the systemd user unit.
pub const SYSTEMD_UNIT: &str = "odhive.service";

/// Label of the launchd agent.
pub const LAUNCHD_LABEL: &str = "io.github.sidc11.odhive";

/// How long the service manager gets to stop the service (instances stop
/// in parallel, 15 s each at most, and jobs get 20 s to clean up).
const STOP_TIMEOUT_SECS: u32 = 90;

/// How long one `systemctl` / `launchctl` call may take.
const MANAGER_TIMEOUT: Duration = Duration::from_secs(120);

/// The service manager of this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Launchd
        } else {
            Self::Systemd
        }
    }

    const fn program(self) -> &'static str {
        match self {
            Self::Systemd => "systemctl",
            Self::Launchd => "launchctl",
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Launchd => "launchd",
        }
    }

    /// Where the unit / agent file goes.
    pub fn file(self, paths: &AppPaths) -> PathBuf {
        match self {
            // `config_dir` is `$XDG_CONFIG_HOME/odhive`.
            Self::Systemd => paths
                .config_dir
                .parent()
                .map_or_else(|| paths.home_dir.join(".config"), Path::to_path_buf)
                .join("systemd/user")
                .join(SYSTEMD_UNIT),
            Self::Launchd => paths
                .home_dir
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
        }
    }
}

/// What the installed service runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Absolute path of the `odhive` executable.
    pub exe: String,
    pub port: u16,
    /// `PATH` for the service: the service manager's own one usually lacks
    /// the folders where uv, git or Python live.
    pub path_var: String,
    /// Where launchd writes the service's output (systemd uses the journal).
    pub log: String,
}

impl Spec {
    fn args(&self) -> Vec<String> {
        [
            self.exe.as_str(),
            "--port",
            &self.port.to_string(),
            "serve",
            "--foreground",
            "--detached",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
}

/// A value that goes into a unit or agent file: UTF-8 without control
/// characters, so nothing can start a new line or key.
fn plain(what: &str, value: &OsStr) -> anyhow::Result<String> {
    let Some(s) = value.to_str() else {
        bail!("{what} is not valid UTF-8: {}", value.display());
    };
    if s.chars().any(char::is_control) {
        bail!("{what} contains control characters: {s:?}");
    }
    Ok(s.to_owned())
}

/// Only the absolute entries of `path_var`: relative ones would resolve
/// against the service's working directory.
fn service_path(path_var: &OsStr) -> anyhow::Result<String> {
    let dirs: Vec<PathBuf> = std::env::split_paths(path_var)
        .filter(|d| d.is_absolute())
        .collect();
    let joined = std::env::join_paths(dirs).context("PATH has an entry with ':'")?;
    plain("PATH", &joined)
}

impl Spec {
    /// The spec for this executable and environment.
    pub fn for_current(paths: &AppPaths, port: u16) -> anyhow::Result<Self> {
        let exe = std::env::current_exe().context("could not find the odhive executable")?;
        let exe = std::fs::canonicalize(&exe)
            .with_context(|| format!("could not resolve {}", exe.display()))?;
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        Ok(Self {
            exe: plain("the odhive path", exe.as_os_str())?,
            port,
            path_var: service_path(&path_var)?,
            log: plain(
                "the data folder",
                paths.data_dir.join(service_lock::LOG_FILE_NAME).as_os_str(),
            )?,
        })
    }
}

/// One argument of a systemd `ExecStart=` line: quoted, with backslashes,
/// quotes, specifiers (`%`) and variable references (`$`) escaped.
fn systemd_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A quoted `Environment=` assignment (no `$` expansion there).
fn systemd_env(key: &str, value: &str) -> String {
    let mut out = format!("\"{key}=");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The systemd user unit.
pub fn systemd_unit(spec: &Spec) -> String {
    let exec = spec
        .args()
        .iter()
        .map(|a| systemd_arg(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "# Installed by `odhive service install`; remove with `odhive service uninstall`.\n\
         [Unit]\n\
         Description=OdHive: Odoo development servers\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         Environment={env}\n\
         WorkingDirectory=/\n\
         UMask=0077\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # SIGTERM to OdHive only: it stops its Odoo instances itself.\n\
         KillMode=mixed\n\
         TimeoutStopSec={STOP_TIMEOUT_SECS}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        env = systemd_env("PATH", &spec.path_var),
    )
}

/// Text for a plist `<string>`.
fn xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The launchd agent.
pub fn launchd_plist(spec: &Spec) -> String {
    let mut args = String::new();
    for a in spec.args() {
        let _ = writeln!(args, "        <string>{}</string>", xml(&a));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Installed by `odhive service install`; remove with `odhive service uninstall`. -->
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>
    <key>WorkingDirectory</key>
    <string>/</string>
    <key>Umask</key>
    <integer>63</integer>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>ExitTimeOut</key>
    <integer>{STOP_TIMEOUT_SECS}</integer>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        label = xml(LAUNCHD_LABEL),
        path = xml(&spec.path_var),
        log = xml(&spec.log),
    )
}

/// The file contents for `manager`.
pub fn render(manager: Manager, spec: &Spec) -> String {
    match manager {
        Manager::Systemd => systemd_unit(spec),
        Manager::Launchd => launchd_plist(spec),
    }
}

/// launchd's domain of the current user's GUI session.
fn gui_domain() -> String {
    format!("gui/{}", effective_uid())
}

/// Runs the service manager with `args`; returns its error output on failure.
async fn manage(manager: Manager, args: &[&str]) -> anyhow::Result<Result<(), String>> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(program) = find_executable(&path, manager.program()) else {
        bail!(
            "`{}` not found: OdHive can only install its service with {}",
            manager.program(),
            manager.name()
        );
    };
    let child = tokio::process::Command::new(&program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(MANAGER_TIMEOUT, child)
        .await
        .with_context(|| format!("`{} {}` did not finish", manager.program(), args.join(" ")))?
        .with_context(|| format!("could not run {}", program.display()))?;
    if out.status.success() {
        return Ok(Ok(()));
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Ok(Err(format!(
        "`{} {}` failed ({}){}",
        manager.program(),
        args.join(" "),
        out.status,
        if err.is_empty() {
            String::new()
        } else {
            format!(": {err}")
        }
    )))
}

/// Like [`manage`], but a failure is an error.
async fn must(manager: Manager, args: &[&str]) -> anyhow::Result<()> {
    manage(manager, args).await?.map_err(anyhow::Error::msg)
}

/// Whether the service is installed for the service manager.
pub fn installed(paths: &AppPaths) -> bool {
    Manager::current().file(paths).is_file()
}

/// Asks the service manager to start the installed service now.
pub async fn start_installed() -> anyhow::Result<()> {
    let manager = Manager::current();
    match manager {
        Manager::Systemd => must(manager, &["--user", "start", SYSTEMD_UNIT]).await,
        Manager::Launchd => {
            let target = format!("{}/{LAUNCHD_LABEL}", gui_domain());
            must(manager, &["kickstart", &target]).await
        }
    }
}

/// Where to read the installed service's output.
pub fn log_hint(paths: &AppPaths) -> String {
    match Manager::current() {
        Manager::Systemd => format!("journalctl --user -u {SYSTEMD_UNIT}"),
        Manager::Launchd => paths
            .data_dir
            .join(service_lock::LOG_FILE_NAME)
            .display()
            .to_string(),
    }
}

/// `odhive service install`.
pub async fn install(port: u16) -> anyhow::Result<ExitCode> {
    let paths = AppPaths::discover()?;
    let manager = Manager::current();
    let spec = Spec::for_current(&paths, port)?;
    let file = manager.file(&paths);
    let reinstall = file.is_file();
    let running = service_lock::probe(&paths.data_dir)?.is_some();

    if manager == Manager::Launchd && reinstall {
        // A loaded agent keeps its old definition until it is booted out.
        let target = format!("{}/{LAUNCHD_LABEL}", gui_domain());
        let _ = manage(manager, &["bootout", &target]).await?;
    }
    odhive_core::fsutil::write_private_atomic(&file, render(manager, &spec).as_bytes())
        .map_err(|e| anyhow::anyhow!("could not write {}: {}", e.path.display(), e.source))?;

    let registered = match manager {
        Manager::Systemd => {
            let reload = must(manager, &["--user", "daemon-reload"]).await;
            match reload {
                Ok(()) => must(manager, &["--user", "enable", SYSTEMD_UNIT]).await,
                Err(e) => Err(e),
            }
        }
        // RunAtLoad starts it; an OdHive already running makes it exit 0.
        Manager::Launchd => must(manager, &["bootstrap", &gui_domain(), &path_str(&file)]).await,
    };
    if let Err(e) = registered {
        // Not half-installed: the launcher would try to start it.
        let _ = std::fs::remove_file(&file);
        if manager == Manager::Systemd {
            let _ = manage(manager, &["--user", "daemon-reload"]).await;
        }
        return Err(e.context("could not register the OdHive service"));
    }
    eprintln!(
        "{} the OdHive service with {} ({}): it starts when you log in",
        if reinstall { "Updated" } else { "Installed" },
        manager.name(),
        file.display()
    );

    if running {
        eprintln!(
            "OdHive is already running; the new service definition applies from its next start (`odhive quit`, then `odhive`)"
        );
        return Ok(ExitCode::SUCCESS);
    }
    if manager == Manager::Systemd {
        must(manager, &["--user", "start", SYSTEMD_UNIT]).await?;
    }
    let state = crate::service::wait_started(&paths)
        .await
        .with_context(|| format!("see {}", log_hint(&paths)))?;
    eprintln!(
        "OdHive is running at http://localhost:{} (pid {})",
        state.port, state.pid
    );
    Ok(ExitCode::SUCCESS)
}

/// `odhive service uninstall`: stops the service (and its instances) and
/// removes its definition.
pub async fn uninstall() -> anyhow::Result<ExitCode> {
    let paths = AppPaths::discover()?;
    let manager = Manager::current();
    let file = manager.file(&paths);
    if !file.is_file() {
        eprintln!("The OdHive service is not installed");
        return Ok(ExitCode::SUCCESS);
    }
    match manager {
        Manager::Systemd => must(manager, &["--user", "disable", "--now", SYSTEMD_UNIT]).await?,
        Manager::Launchd => {
            let target = format!("{}/{LAUNCHD_LABEL}", gui_domain());
            // Fails when it isn't loaded, which is fine here.
            let _ = manage(manager, &["bootout", &target]).await?;
        }
    }
    std::fs::remove_file(&file).with_context(|| format!("could not remove {}", file.display()))?;
    if manager == Manager::Systemd {
        must(manager, &["--user", "daemon-reload"]).await?;
    }
    eprintln!("Removed the OdHive service ({})", file.display());
    Ok(ExitCode::SUCCESS)
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            exe: "/home/me/.cargo/bin/odhive".into(),
            port: 7069,
            path_var: "/home/me/.local/bin:/usr/bin".into(),
            log: "/home/me/.local/share/odhive/service.log".into(),
        }
    }

    #[test]
    fn the_unit_runs_the_detached_service_on_the_port() {
        let unit = systemd_unit(&spec());
        assert!(
            unit.contains(
                "ExecStart=\"/home/me/.cargo/bin/odhive\" \"--port\" \"7069\" \"serve\" \"--foreground\" \"--detached\"\n"
            ),
            "{unit}"
        );
        assert!(unit.contains("Environment=\"PATH=/home/me/.local/bin:/usr/bin\"\n"));
        assert!(unit.contains("KillMode=mixed\n"));
        assert!(unit.contains("Restart=on-failure\n"));
        assert!(unit.contains("WantedBy=default.target\n"));
    }

    #[test]
    fn unit_values_are_escaped() {
        assert_eq!(systemd_arg(r#"/a b/%h$X"q\"#), r#""/a b/%%h$$X\"q\\""#);
        assert_eq!(systemd_env("PATH", r#"/a%n:/b"c"#), r#""PATH=/a%%n:/b\"c""#);
        let mut s = spec();
        s.exe = "/opt/my apps/odhive".into();
        assert!(systemd_unit(&s).contains("ExecStart=\"/opt/my apps/odhive\" "));
    }

    #[test]
    fn the_plist_runs_the_detached_service_and_escapes_values() {
        let mut s = spec();
        s.exe = "/Users/me/a&b/<odhive>".into();
        let plist = launchd_plist(&s);
        assert!(plist.contains("<string>io.github.sidc11.odhive</string>"));
        assert!(plist.contains("<string>/Users/me/a&amp;b/&lt;odhive&gt;</string>\n        <string>--port</string>\n        <string>7069</string>\n        <string>serve</string>\n        <string>--foreground</string>\n        <string>--detached</string>\n"), "{plist}");
        assert!(plist.contains("<string>/home/me/.local/bin:/usr/bin</string>"));
        assert!(plist.contains("<key>Umask</key>\n    <integer>63</integer>"));
        assert!(!plist.contains("<odhive>"));
    }

    #[test]
    fn values_with_control_characters_or_bad_utf8_are_refused() {
        assert!(plain("x", OsStr::new("/a\nExecStart=/bin/evil")).is_err());
        assert!(plain("x", OsStr::new("/a\tb")).is_err());
        assert_eq!(plain("x", OsStr::new("/a b")).expect("ok"), "/a b");
        use std::os::unix::ffi::OsStrExt as _;
        assert!(plain("x", OsStr::from_bytes(b"/a\xff")).is_err());
    }

    #[test]
    fn the_service_path_keeps_only_absolute_entries() {
        assert_eq!(
            service_path(OsStr::new("/usr/bin::.:bin:/home/me/.local/bin")).expect("path"),
            "/usr/bin:/home/me/.local/bin"
        );
        assert_eq!(service_path(OsStr::new("")).expect("path"), "");
    }

    #[test]
    fn files_go_where_the_service_managers_look() {
        let paths = AppPaths {
            config_dir: "/home/me/.config/odhive".into(),
            data_dir: "/home/me/.local/share/odhive".into(),
            home_dir: "/home/me".into(),
        };
        assert_eq!(
            Manager::Systemd.file(&paths),
            Path::new("/home/me/.config/systemd/user/odhive.service")
        );
        assert_eq!(
            Manager::Launchd.file(&paths),
            Path::new("/home/me/Library/LaunchAgents/io.github.sidc11.odhive.plist")
        );
    }
}
