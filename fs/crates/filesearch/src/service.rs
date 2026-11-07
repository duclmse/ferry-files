/// Windows Service integration for the filesearch daemon.
///
/// Registers `SERVICE_ACCEPT_PRESHUTDOWN` so the SCM gives ~20s for a clean
/// index flush before force-terminating, instead of the 5s given for SHUTDOWN.
///
/// To install:
///   filesearch.exe --config C:\path\config.toml install-service
/// To uninstall:
///   filesearch.exe uninstall-service
/// To run under SCM (called automatically by the service dispatcher):
///   filesearch.exe --config C:\path\config.toml daemon --service
///
#[cfg(target_os = "windows")]
pub mod windows {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::Duration;

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

    use crate::error::{AppError, Result};

    pub const SERVICE_NAME: &str = "FilesearchDaemon";
    pub const SERVICE_DISPLAY: &str = "Filesearch Daemon";
    pub const SERVICE_DESCRIPTION: &str = "Cross-platform file-search indexer daemon";

    // Config path is stashed here before the SCM dispatcher hijacks the thread.
    static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    /// Called from `main` when `daemon --service` is passed.
    /// Hands control to the Windows SCM dispatcher; returns only after the service stops.
    pub fn run_as_service(config_path: PathBuf) -> Result<()> {
        CONFIG_PATH
            .set(config_path)
            .map_err(|_| AppError::Config("CONFIG_PATH already set".into()))?;
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .map_err(|e| AppError::Config(format!("service dispatcher: {e}")))
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
        .map_err(|e| AppError::Config(format!("register service handler: {e}")))?;

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

        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| AppError::Config(format!("tokio runtime: {e}")))?;

        let exit_code = rt.block_on(async move {
            match crate::daemon::run_with_shutdown(config_path, shutdown).await {
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

    // -------------------------------------------------------------------------
    // Install / uninstall helpers
    // -------------------------------------------------------------------------

    /// Register the service with the Windows SCM.
    /// `exe_path` should be the absolute path to `filesearch.exe`.
    /// `config_path` will be passed as `--config <path>`.
    pub fn install(exe_path: &std::path::Path, config_path: &std::path::Path) -> Result<()> {
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
                .map_err(|e| AppError::Config(format!("open service manager: {e}")))?;

        let binary_path = std::ffi::OsString::from(format!(
            "\"{}\" --config \"{}\" daemon --service",
            exe_path.display(),
            config_path.display(),
        ));

        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: PathBuf::from(&binary_path),
            launch_arguments: vec![],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };

        manager
            .create_service(&info, ServiceAccess::CHANGE_CONFIG)
            .map_err(|e| AppError::Config(format!("create service: {e}")))?;

        tracing::info!("service '{}' installed", SERVICE_NAME);
        Ok(())
    }

    /// Remove the service from the Windows SCM.
    pub fn uninstall() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| AppError::Config(format!("open service manager: {e}")))?;

        let svc = manager
            .open_service(SERVICE_NAME, ServiceAccess::DELETE)
            .map_err(|e| AppError::Config(format!("open service: {e}")))?;

        svc.delete()
            .map_err(|e| AppError::Config(format!("delete service: {e}")))?;

        tracing::info!("service '{}' uninstalled", SERVICE_NAME);
        Ok(())
    }
}

// On non-Windows the module is empty; callers guard with #[cfg(target_os = "windows")].
#[cfg(not(target_os = "windows"))]
pub mod windows {}
