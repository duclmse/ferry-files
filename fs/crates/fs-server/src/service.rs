//! Running fsd under the system's service manager: the Windows SCM, systemd
//! on Linux, launchd on macOS.
//!
//! A service is either for one user (`Scope::User`: a systemd user unit or a
//! LaunchAgent, running as that user while they are signed in) or for the
//! whole computer (`Scope::System`: the SCM, a systemd system unit or a
//! LaunchDaemon, started at boot). Windows services are always `System`.
//!
//! A `System` service starts the daemon with `daemon --service`, which lets
//! every local user connect over IPC (see `api::ipc_server`).
//!
//!   fsd --config /path/config.toml install-service [--user | --system]
//!   fsd uninstall-service [--user | --system]

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{AppError, Result};

/// Name of the Windows service, the systemd unit (`fsd.service`) and, as a
/// launchd label, the job.
pub const SERVICE_NAME: &str = if cfg!(windows) {
    "FsDaemon"
} else if cfg!(target_os = "macos") {
    "fs.fsd"
} else {
    "fsd"
};

const DESCRIPTION: &str = "fs file-search daemon";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    System,
}

impl Scope {
    /// Windows services belong to the computer; elsewhere a desktop user's
    /// index of their own files is the common case and needs no root.
    pub fn default_for_os() -> Scope {
        if cfg!(windows) {
            Scope::System
        } else {
            Scope::User
        }
    }
}

/// What `install` did, for the user.
#[derive(Debug)]
pub struct Installed {
    /// The unit or plist written, if the service manager uses one.
    pub file: Option<PathBuf>,
    pub notes: Vec<String>,
}

/// Registers fsd with the service manager and starts it. Installing again
/// replaces the earlier registration.
pub fn install(exe: &Path, config: &Path, scope: Scope) -> Result<Installed> {
    #[cfg(windows)]
    {
        if scope == Scope::User {
            return Err(AppError::Service(
                "Windows services run for the whole computer; leave out --user".into(),
            ));
        }
        windows::install(exe, config)?;
        Ok(Installed {
            file: None,
            notes: vec![format!(
                "Runs as LocalSystem at boot. Stop it with `sc stop {SERVICE_NAME}`."
            )],
        })
    }
    #[cfg(target_os = "macos")]
    {
        launchd::install(exe, config, scope)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        systemd::install(exe, config, scope)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (exe, config, scope);
        Err(AppError::Service(
            "no service manager on this platform".into(),
        ))
    }
}

/// Stops the service and removes its registration.
pub fn uninstall(scope: Scope) -> Result<()> {
    #[cfg(windows)]
    {
        if scope == Scope::User {
            return Err(AppError::Service(
                "Windows services run for the whole computer; leave out --user".into(),
            ));
        }
        windows::uninstall()
    }
    #[cfg(target_os = "macos")]
    {
        launchd::uninstall(scope)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        systemd::uninstall(scope)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = scope;
        Err(AppError::Service(
            "no service manager on this platform".into(),
        ))
    }
}

/// The arguments the service manager starts fsd with.
fn daemon_args(config: &Path, scope: Scope) -> Vec<String> {
    let mut args = vec![
        "--config".to_owned(),
        config.to_string_lossy().into_owned(),
        "daemon".to_owned(),
    ];
    if scope == Scope::System {
        args.push("--service".to_owned());
    }
    args
}

/// Runs a service-manager command, turning a failure into its own words.
#[cfg_attr(windows, allow(dead_code))]
fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| AppError::Service(format!("cannot run {program}: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(AppError::Service(format!(
        "`{program} {}` failed: {}",
        args.join(" "),
        stderr.trim()
    )))
}

#[cfg_attr(windows, allow(dead_code))]
fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| AppError::Service("HOME is not set".into()))
}

#[cfg_attr(windows, allow(dead_code))]
fn write_file(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::Service(format!("cannot create {}: {e}", dir.display())))?;
    }
    std::fs::write(path, text)
        .map_err(|e| AppError::Service(format!("cannot write {}: {e}", path.display())))
}

/// The folder relative paths in the config are read from: the config's own,
/// since a service starts in `/` or `System32`.
#[cfg_attr(windows, allow(dead_code))]
fn work_dir(config: &Path) -> PathBuf {
    config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

// ---------------------------------------------------------------------------
// systemd
// ---------------------------------------------------------------------------

#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
mod systemd {
    use super::*;

    pub(super) fn unit_path(scope: Scope) -> Result<PathBuf> {
        Ok(match scope {
            Scope::System => PathBuf::from("/etc/systemd/system/fsd.service"),
            Scope::User => {
                let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
                    Some(d) => PathBuf::from(d),
                    None => home()?.join(".config"),
                };
                base.join("systemd/user/fsd.service")
            }
        })
    }

    /// One word of `ExecStart=`: quoted, with systemd's specifier (`%`) and
    /// variable (`$`) characters doubled so they stay literal.
    pub(super) fn quote(word: &str) -> String {
        let mut out = String::with_capacity(word.len() + 2);
        out.push('"');
        for c in word.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '%' => out.push_str("%%"),
                '$' => out.push_str("$$"),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    pub(super) fn unit(exe: &Path, config: &Path, scope: Scope) -> String {
        let mut exec = quote(&exe.to_string_lossy());
        for a in daemon_args(config, scope) {
            exec.push(' ');
            exec.push_str(&quote(&a));
        }
        let wanted_by = match scope {
            Scope::System => "multi-user.target",
            Scope::User => "default.target",
        };
        let dir = work_dir(config).to_string_lossy().replace('%', "%%");
        // Type=notify: the daemon reports ready over sd_notify once its APIs
        // are up. TimeoutStopSec leaves time to flush the index on SIGTERM.
        format!(
            "[Unit]\n\
             Description={DESCRIPTION}\n\
             After=local-fs.target\n\
             \n\
             [Service]\n\
             Type=notify\n\
             ExecStart={exec}\n\
             WorkingDirectory={dir}\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             TimeoutStopSec=60\n\
             \n\
             [Install]\n\
             WantedBy={wanted_by}\n"
        )
    }

    fn systemctl(scope: Scope, args: &[&str]) -> Result<()> {
        let mut all = Vec::with_capacity(args.len() + 1);
        if scope == Scope::User {
            all.push("--user");
        }
        all.extend_from_slice(args);
        run("systemctl", &all)
    }

    pub(super) fn install(exe: &Path, config: &Path, scope: Scope) -> Result<Installed> {
        let path = unit_path(scope)?;
        write_file(&path, &unit(exe, config, scope))?;
        systemctl(scope, &["daemon-reload"])?;
        systemctl(scope, &["enable", "fsd.service"])?;
        // `restart`, not `start`: a reinstall must pick up the new unit.
        systemctl(scope, &["restart", "fsd.service"])?;
        let mut notes = vec![];
        match scope {
            Scope::User => {
                notes.push("Runs while you are signed in. To keep it running after you sign out: `loginctl enable-linger`.".into());
                notes.push("Logs: `journalctl --user -u fsd`.".into());
            }
            Scope::System => {
                notes.push("Runs as root at boot. Logs: `journalctl -u fsd`.".into());
            }
        }
        Ok(Installed {
            file: Some(path),
            notes,
        })
    }

    pub(super) fn uninstall(scope: Scope) -> Result<()> {
        let path = unit_path(scope)?;
        if !path.exists() {
            return Err(AppError::Service(format!(
                "no service installed at {}",
                path.display()
            )));
        }
        // Not installed as running is fine; the unit still goes.
        let _ = systemctl(scope, &["disable", "--now", "fsd.service"]);
        std::fs::remove_file(&path)
            .map_err(|e| AppError::Service(format!("cannot remove {}: {e}", path.display())))?;
        systemctl(scope, &["daemon-reload"])
    }
}

// ---------------------------------------------------------------------------
// launchd
// ---------------------------------------------------------------------------

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod launchd {
    use super::*;

    pub(super) fn plist_path(scope: Scope) -> Result<PathBuf> {
        let name = format!("{SERVICE_NAME}.plist");
        Ok(match scope {
            Scope::System => PathBuf::from("/Library/LaunchDaemons").join(name),
            Scope::User => home()?.join("Library/LaunchAgents").join(name),
        })
    }

    fn log_path(scope: Scope) -> Result<PathBuf> {
        Ok(match scope {
            Scope::System => PathBuf::from("/Library/Logs/fsd.log"),
            Scope::User => home()?.join("Library/Logs/fsd.log"),
        })
    }

    pub(super) fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    pub(super) fn plist(exe: &Path, config: &Path, scope: Scope, log: &Path) -> String {
        let s = |v: &str| format!("<string>{}</string>", xml_escape(v));
        let mut args = vec![s(&exe.to_string_lossy())];
        args.extend(daemon_args(config, scope).iter().map(|a| s(a)));
        // KeepAlive restarts it after a crash but not after a clean stop.
        // ProcessType Standard: the default throttles a background job's CPU
        // and disk, which would slow searches. ExitTimeOut leaves time to
        // flush the index on SIGTERM.
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key>{label}\n\
             \t<key>ProgramArguments</key>\n\
             \t<array>\n\t\t{args}\n\t</array>\n\
             \t<key>WorkingDirectory</key>{dir}\n\
             \t<key>RunAtLoad</key><true/>\n\
             \t<key>KeepAlive</key>\n\
             \t<dict><key>SuccessfulExit</key><false/></dict>\n\
             \t<key>ProcessType</key><string>Standard</string>\n\
             \t<key>ExitTimeOut</key><integer>60</integer>\n\
             \t<key>StandardErrorPath</key>{log}\n\
             </dict>\n\
             </plist>\n",
            label = s(SERVICE_NAME),
            args = args.join("\n\t\t"),
            dir = s(&work_dir(config).to_string_lossy()),
            log = s(&log.to_string_lossy()),
        )
    }

    /// launchctl's domain: the signed-in user's GUI session, or the system.
    fn domain(scope: Scope) -> Result<String> {
        match scope {
            Scope::System => Ok("system".into()),
            Scope::User => {
                let out = Command::new("id")
                    .arg("-u")
                    .output()
                    .map_err(|e| AppError::Service(format!("cannot run id: {e}")))?;
                let uid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                if uid.is_empty() {
                    return Err(AppError::Service("cannot tell the user id".into()));
                }
                Ok(format!("gui/{uid}"))
            }
        }
    }

    pub(super) fn install(exe: &Path, config: &Path, scope: Scope) -> Result<Installed> {
        let path = plist_path(scope)?;
        let log = log_path(scope)?;
        let domain = domain(scope)?;
        // A job already loaded refuses a second bootstrap.
        let _ = run(
            "launchctl",
            &["bootout", &format!("{domain}/{SERVICE_NAME}")],
        );
        write_file(&path, &plist(exe, config, scope, &log))?;
        run(
            "launchctl",
            &["bootstrap", &domain, &path.to_string_lossy()],
        )?;
        Ok(Installed {
            file: Some(path),
            notes: vec![
                format!("Errors before logging starts go to {}.", log.display()),
                "macOS keeps background programs out of Desktop, Documents, Downloads and \
                 removable disks: to index them, add fsd under System Settings > Privacy & \
                 Security > Full Disk Access."
                    .into(),
            ],
        })
    }

    pub(super) fn uninstall(scope: Scope) -> Result<()> {
        let path = plist_path(scope)?;
        if !path.exists() {
            return Err(AppError::Service(format!(
                "no service installed at {}",
                path.display()
            )));
        }
        let domain = domain(scope)?;
        let _ = run(
            "launchctl",
            &["bootout", &format!("{domain}/{SERVICE_NAME}")],
        );
        std::fs::remove_file(&path)
            .map_err(|e| AppError::Service(format!("cannot remove {}: {e}", path.display())))
    }
}

// ---------------------------------------------------------------------------
// Windows SCM
// ---------------------------------------------------------------------------

/// Registers `SERVICE_ACCEPT_PRESHUTDOWN` so the SCM gives ~20s for a clean
/// index flush before force-terminating, instead of the 5s given for SHUTDOWN.
#[cfg(target_os = "windows")]
pub mod windows {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use tokio_util::sync::CancellationToken;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl,
            ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    use super::{daemon_args, Scope, DESCRIPTION, SERVICE_NAME};
    use crate::daemon::RunOptions;
    use crate::error::{AppError, Result};

    pub const SERVICE_DISPLAY: &str = "Fs Daemon";

    // Config path is stashed here before the SCM dispatcher hijacks the thread.
    static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    fn err(what: &str, e: impl std::fmt::Display) -> AppError {
        AppError::Service(format!("{what}: {e}"))
    }

    /// Called from `main` when `daemon --service` is passed.
    /// Hands control to the Windows SCM dispatcher; returns only after the service stops.
    pub fn run_as_service(config_path: PathBuf) -> Result<()> {
        CONFIG_PATH
            .set(config_path)
            .map_err(|_| AppError::Service("CONFIG_PATH already set".into()))?;
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .map_err(|e| err("service dispatcher", e))
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            tracing::error!("Windows service error: {e}");
        }
    }

    fn run_service() -> Result<()> {
        let shutdown = CancellationToken::new();
        let shutdown_svc = shutdown.clone();

        let status_handle = service_control_handler::register(SERVICE_NAME, move |control| {
            match control {
                // PRESHUTDOWN fires first and gives ~20s - enough to flush the index.
                // Cancelling the token triggers daemon graceful shutdown (flush + commit).
                ServiceControl::Preshutdown | ServiceControl::Stop | ServiceControl::Shutdown => {
                    shutdown_svc.cancel();
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        })
        .map_err(|e| err("register service handler", e))?;

        set_status(
            &status_handle,
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN,
            ServiceExitCode::Win32(0),
        );

        let config_path = CONFIG_PATH
            .get()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("config.toml"));

        let rt = tokio::runtime::Runtime::new().map_err(|e| err("tokio runtime", e))?;

        let opts = RunOptions {
            system_service: true,
        };
        let exit_code = rt.block_on(async move {
            match crate::daemon::run_with_options(config_path, shutdown, opts).await {
                Ok(()) => ServiceExitCode::Win32(0),
                Err(e) => {
                    tracing::error!("daemon error: {e}");
                    ServiceExitCode::ServiceSpecific(1)
                }
            }
        });

        set_status(
            &status_handle,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            exit_code,
        );
        Ok(())
    }

    fn set_status(
        handle: &service_control_handler::ServiceStatusHandle,
        state: ServiceState,
        controls: ServiceControlAccept,
        exit: ServiceExitCode,
    ) {
        let _ = handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: controls,
            exit_code: exit,
            checkpoint: 0,
            wait_hint: Duration::from_secs(30),
            process_id: None,
        });
    }

    /// Registers the service with the SCM and starts it. An existing
    /// registration is replaced, so a reinstall picks up a new path.
    pub fn install(exe_path: &std::path::Path, config_path: &std::path::Path) -> Result<()> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .map_err(|e| err("open service manager (run as administrator)", e))?;

        if manager
            .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
            .is_ok()
        {
            uninstall()?;
        }

        let service = manager
            .create_service(
                &service_info(exe_path, config_path),
                ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
            )
            .map_err(|e| err("create service", e))?;
        service
            .set_description(DESCRIPTION)
            .map_err(|e| err("set description", e))?;
        service
            .start::<&str>(&[])
            .map_err(|e| err("start service", e))?;
        tracing::info!("service '{}' installed", SERVICE_NAME);
        Ok(())
    }

    /// The executable and its arguments stay separate: windows-service quotes
    /// each one, so a whole command line passed as the path became a single
    /// quoted "file name" and the service could never start.
    fn service_info(exe_path: &std::path::Path, config_path: &std::path::Path) -> ServiceInfo {
        ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe_path.to_path_buf(),
            launch_arguments: daemon_args(config_path, Scope::System)
                .into_iter()
                .map(OsString::from)
                .collect(),
            dependencies: vec![],
            account_name: None,
            account_password: None,
        }
    }

    /// Stops the service, waits for it to flush its index, and removes it.
    pub fn uninstall() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| err("open service manager (run as administrator)", e))?;

        let svc = manager
            .open_service(
                SERVICE_NAME,
                ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
            )
            .map_err(|e| err("open service", e))?;

        let status = svc.query_status().map_err(|e| err("query service", e))?;
        if status.current_state != ServiceState::Stopped {
            // Already stopping is fine; the wait below covers it.
            let _ = svc.stop();
            let deadline = Instant::now() + Duration::from_secs(40);
            while Instant::now() < deadline {
                match svc.query_status() {
                    Ok(s) if s.current_state == ServiceState::Stopped => break,
                    Ok(_) => std::thread::sleep(Duration::from_millis(250)),
                    Err(e) => return Err(err("query service", e)),
                }
            }
        }

        svc.delete().map_err(|e| err("delete service", e))?;
        // The SCM removes the entry once the last handle closes; a reinstall
        // right after must not find it still marked for deletion.
        drop(svc);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && manager
                .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
                .is_ok()
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::info!("service '{}' uninstalled", SERVICE_NAME);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn path_and_arguments_are_separate() {
            let info = service_info(
                std::path::Path::new(r"C:\Program Files\fs\fsd.exe"),
                std::path::Path::new(r"C:\ProgramData\fs\config.toml"),
            );
            assert_eq!(
                info.executable_path,
                PathBuf::from(r"C:\Program Files\fs\fsd.exe")
            );
            assert_eq!(
                info.launch_arguments,
                [
                    "--config",
                    r"C:\ProgramData\fs\config.toml",
                    "daemon",
                    "--service"
                ]
                .map(OsString::from)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_system_service_shares_ipc() {
        let cfg = Path::new("/etc/fs/config.toml");
        assert!(!daemon_args(cfg, Scope::User).contains(&"--service".to_owned()));
        assert_eq!(
            daemon_args(cfg, Scope::System),
            ["--config", "/etc/fs/config.toml", "daemon", "--service"]
        );
    }

    #[test]
    fn systemd_words_keep_spaces_and_specifiers_literal() {
        assert_eq!(systemd::quote("/opt/my fs/fsd"), "\"/opt/my fs/fsd\"");
        assert_eq!(systemd::quote(r#"a"b\c%d$e"#), r#""a\"b\\c%%d$$e""#);
    }

    #[test]
    fn systemd_unit_runs_the_daemon_from_the_config_folder() {
        let unit = systemd::unit(
            Path::new("/usr/local/bin/fsd"),
            Path::new("/home/me/.config/fs/config.toml"),
            Scope::User,
        );
        assert!(unit.contains(
            "ExecStart=\"/usr/local/bin/fsd\" \"--config\" \"/home/me/.config/fs/config.toml\" \"daemon\"\n"
        ));
        assert!(unit.contains("WorkingDirectory=/home/me/.config/fs\n"));
        assert!(unit.contains("Type=notify\n"));
        assert!(unit.contains("WantedBy=default.target\n"));

        let system = systemd::unit(
            Path::new("/usr/bin/fsd"),
            Path::new("/etc/fs/config.toml"),
            Scope::System,
        );
        assert!(system.contains("\"daemon\" \"--service\"\n"));
        assert!(system.contains("WantedBy=multi-user.target\n"));
    }

    #[test]
    fn user_unit_goes_under_xdg_config_home() {
        // Only this test touches XDG_CONFIG_HOME.
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg");
        let p = systemd::unit_path(Scope::User).unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(p, PathBuf::from("/tmp/xdg/systemd/user/fsd.service"));
        assert_eq!(
            systemd::unit_path(Scope::System).unwrap(),
            PathBuf::from("/etc/systemd/system/fsd.service")
        );
    }

    #[test]
    fn launchd_plist_escapes_and_lists_each_argument() {
        let p = launchd::plist(
            Path::new("/Applications/A & B/fsd"),
            Path::new("/Users/me/fs/config.toml"),
            Scope::User,
            Path::new("/Users/me/Library/Logs/fsd.log"),
        );
        assert!(p.contains("<string>/Applications/A &amp; B/fsd</string>"));
        assert!(p.contains("<string>--config</string>"));
        assert!(p.contains("<string>/Users/me/fs/config.toml</string>"));
        assert!(p.contains("<string>daemon</string>\n\t</array>"));
        assert!(p.contains("<key>WorkingDirectory</key><string>/Users/me/fs</string>"));
        assert!(!p.contains("--service"));
        assert!(launchd::plist(
            Path::new("/usr/local/bin/fsd"),
            Path::new("/etc/fs/config.toml"),
            Scope::System,
            Path::new("/Library/Logs/fsd.log"),
        )
        .contains("<string>--service</string>"));
    }
}
