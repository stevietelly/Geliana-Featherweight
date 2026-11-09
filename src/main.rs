// In release builds, suppress the console window entirely.
// In debug builds, keep it available for development output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod disk;
mod log;
mod mux;
mod paths;
mod recorder;
mod tray;
mod video;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::HSTRING;

fn main() {
    // -----------------------------------------------------------------------
    // 1. Panic hook — write panics to the log file so they aren't silently lost
    //    in windowless (GUI-subsystem) release builds.
    // -----------------------------------------------------------------------
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {info}");
        eprintln!("{msg}");
        log::log(&msg);
    }));

    // -----------------------------------------------------------------------
    // 2. Log initialisation (truncate if > 1 MiB, write startup line).
    // -----------------------------------------------------------------------
    log::log_init();

    // -----------------------------------------------------------------------
    // 3. Single-instance guard via a named Windows mutex.
    //    If another instance already holds the mutex, exit immediately.
    // -----------------------------------------------------------------------
    let mutex_name = HSTRING::from("Local\\Featherweight-Recorder");
    let mutex_handle: HANDLE = unsafe {
        match CreateMutexW(None, true, &mutex_name) {
            Ok(h) => h,
            Err(e) => {
                log::log(&format!("CreateMutexW failed: {e}"));
                return;
            }
        }
    };

    // ERROR_ALREADY_EXISTS (0x000000B7) is set when another process owns the mutex.
    let last_error = unsafe { windows::Win32::Foundation::GetLastError() };
    if last_error == windows::Win32::Foundation::ERROR_ALREADY_EXISTS {
        log::log("Another instance is already running — exiting");
        // Release our handle before exiting.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(mutex_handle) };
        return;
    }

    // Keep the mutex handle alive for the lifetime of the process.
    // When main() returns, the OS will release it automatically.
    let _mutex_guard = MutexGuard(mutex_handle);

    // -----------------------------------------------------------------------
    // 4. Run the application event loop.
    // -----------------------------------------------------------------------
    app::run();

    log::log("Featherweight exiting normally");
}

/// RAII wrapper that closes the mutex handle when dropped.
struct MutexGuard(HANDLE);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}
