//! Windows service wrapper.
//!
//! Session 0 cannot capture an interactive desktop, so the service does not
//! stream directly. It launches the host executable again, with
//! `--session-helper`, inside the active console session, and relaunches it if
//! the active session changes or the helper exits. This is the standard
//! structure for an agent that must keep working across sign-in, sign-out and
//! fast-user-switching. Installing or removing the service requires
//! administrator rights.

use std::ffi::{OsStr, OsString};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, TerminateProcess, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};

pub const SERVICE_NAME: &str = "ImperioHost";
pub const SERVICE_DISPLAY: &str = "Imperio Remote Desktop Host";
const NO_SESSION: u32 = 0xFFFF_FFFF;

/// Install the service to auto-start as LocalSystem.
pub fn install() -> anyhow::Result<()> {
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let exe = std::env::current_exe().context("current exe")?;
    let manager = ServiceManager::local_computer(
        None::<&OsStr>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("open SCM (run as administrator?)")?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .context("create service")?;
    let _ = service.set_description("Hosts Imperio remote desktop sessions for this machine.");
    service.start(&[] as &[&OsStr]).context("start service")?;
    println!("Installed and started service '{SERVICE_NAME}'.");
    Ok(())
}

/// Stop and delete the service.
pub fn uninstall() -> anyhow::Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
        .context("open SCM (run as administrator?)")?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .context("open service")?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        std::thread::sleep(Duration::from_secs(1));
    }
    service.delete().context("delete service")?;
    println!("Removed service '{SERVICE_NAME}'.");
    Ok(())
}

// --- SCM entry -----------------------------------------------------------

/// Called from `main` when started with `--service`. Blocks until the SCM
/// stops the service.
pub fn run() -> anyhow::Result<()> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("service dispatcher (not started by the SCM?)")?;
    Ok(())
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = service_worker() {
        tracing::error!(error = %e, "service worker failed");
    }
}

fn service_worker() -> anyhow::Result<()> {
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Preshutdown | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;

    let running = |controls, state| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: controls,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status_handle.set_service_status(running(
        ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN,
        ServiceState::Running,
    ))?;
    tracing::info!("service running");

    // Supervise a helper process in the active console session.
    let mut child: Option<HelperProcess> = None;
    let mut current_session = NO_SESSION;
    loop {
        if stop_rx.recv_timeout(Duration::from_millis(500)).is_ok() {
            break;
        }
        let active = unsafe { WTSGetActiveConsoleSessionId() };
        // Session changed (sign-in/out, fast user switch): restart the helper.
        if active != current_session {
            if let Some(c) = child.take() {
                c.terminate();
            }
            current_session = active;
        }
        // Dead or missing helper in a real session: (re)launch it.
        let alive = child.as_ref().is_some_and(|c| c.is_alive());
        if !alive && active != NO_SESSION && active != 0 {
            if let Some(c) = child.take() {
                c.close();
            }
            match launch_in_session(active) {
                Ok(c) => {
                    tracing::info!(session = active, "launched session helper");
                    child = Some(c);
                }
                Err(e) => tracing::warn!(session = active, error = %e, "helper launch failed; retrying"),
            }
        }
    }

    if let Some(c) = child.take() {
        c.terminate();
    }
    status_handle.set_service_status(running(ServiceControlAccept::empty(), ServiceState::Stopped))?;
    tracing::info!("service stopped");
    Ok(())
}

struct HelperProcess {
    process: HANDLE,
    thread: HANDLE,
}

impl HelperProcess {
    fn is_alive(&self) -> bool {
        let mut code = 0u32;
        // STILL_ACTIVE (259) means the process has not exited.
        unsafe { GetExitCodeProcess(self.process, &mut code).is_ok() && code == 259 }
    }
    fn terminate(self) {
        unsafe {
            let _ = TerminateProcess(self.process, 1);
        }
        self.close();
    }
    fn close(self) {
        unsafe {
            let _ = CloseHandle(self.process);
            let _ = CloseHandle(self.thread);
        }
    }
}

/// Launch `imperio-host --session-helper` as the interactive user of
/// `session_id`, on their default desktop.
fn launch_in_session(session_id: u32) -> anyhow::Result<HelperProcess> {
    let exe = std::env::current_exe().context("current exe")?;
    let mut user_token = HANDLE::default();
    // SAFETY: valid out-pointer; only succeeds for the machine account.
    unsafe { WTSQueryUserToken(session_id, &mut user_token) }
        .context("WTSQueryUserToken (service not LocalSystem, or no user signed in)")?;
    let _guard = HandleGuard(user_token);

    let mut primary = HANDLE::default();
    unsafe {
        DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    }
    .context("DuplicateTokenEx")?;
    let _pguard = HandleGuard(primary);

    let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut env, Some(primary), false) }.context("CreateEnvironmentBlock")?;

    let mut cmd: Vec<u16> = format!("\"{}\" --session-helper", exe.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    let mut si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    let result = unsafe {
        CreateProcessAsUserW(
            Some(primary),
            None,
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(env),
            None,
            &si,
            &mut pi,
        )
    };
    // Keep si referenced until after the call.
    let _ = &mut si;
    unsafe {
        let _ = DestroyEnvironmentBlock(env);
    }
    result.context("CreateProcessAsUser")?;
    Ok(HelperProcess { process: pi.hProcess, thread: pi.hThread })
}

struct HandleGuard(HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// True when this process was started by the Service Control Manager.
pub fn running_under_scm(args: &[String]) -> bool {
    args.iter().any(|a| a == "--service")
}
