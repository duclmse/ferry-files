/// Windows Service integration. Only compiled on Windows.
///
/// To install: `sc create rar2zip binPath= "C:\path\rar2zip.exe run --service
///              --config C:\path\config.toml" start= auto`
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
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    use crate::config;
    use crate::error::{AppError, Result};

    const SERVICE_NAME: &str = "rar2zip";

    // Config path is stored before the dispatcher takes over the main thread.
    static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    /// Called from `main` when `--service` is passed. Hands control to the
    /// Windows SCM dispatcher; returns only after the service stops.
    pub fn run_as_service(config_path: PathBuf) -> Result<()> {
        CONFIG_PATH
            .set(config_path)
            .map_err(|_| AppError::Config("CONFIG_PATH already set".into()))?;

        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .map_err(|e| AppError::Config(format!("service dispatcher: {e}")))
    }

    fn service_main(_args: Vec<OsString>) {
        let shutdown = CancellationToken::new();
        let shutdown_svc = shutdown.clone();

        let status_handle =
            service_control_handler::register(SERVICE_NAME, move |control| match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    shutdown_svc.cancel();
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            })
            .expect("register service control handler");

        set_status(
            &status_handle,
            ServiceState::Running,
            ServiceControlAccept::STOP,
            ServiceExitCode::Win32(0),
        );

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

        let exit_code = rt.block_on(async {
            let path = CONFIG_PATH
                .get()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("config.toml"));
            match config::load(&path) {
                Ok(cfg) => match crate::app::run(cfg, shutdown).await {
                    Ok(()) => ServiceExitCode::Win32(0),
                    Err(e) => {
                        tracing::error!("service error: {e}");
                        ServiceExitCode::ServiceSpecific(1)
                    }
                },
                Err(e) => {
                    tracing::error!("failed to load config: {e}");
                    ServiceExitCode::ServiceSpecific(2)
                }
            }
        });

        set_status(
            &status_handle,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            exit_code,
        );
    }

    fn set_status(
        handle: &windows_service::service_control_handler::ServiceStatusHandle,
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
            wait_hint: Duration::default(),
            process_id: None,
        });
    }
}

// On non-Windows the module is empty; callers guard with #[cfg(target_os = "windows")].
#[cfg(not(target_os = "windows"))]
pub mod windows {}
