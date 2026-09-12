mod app;
mod dbus_client;
mod gamepad_registry;
mod theme;
mod pages;
mod keyboard_shortcuts;
mod widgets;
mod polling_scheduler;
mod system_tray;

use app::LapSphereApp;
use chrono::Local;
use std::fs;
use std::panic;

fn setup_panic_hook() {
    panic::set_hook(Box::new(|panic_info| {
        let timestamp = Local::now().format("%Y%m%d_%H%M%S");
        let crash_dir = app::get_crash_dir();
        let _ = fs::create_dir_all(&crash_dir);

        let file_path = format!("{}/crash_{}.log", crash_dir, timestamp);

        let mut message = String::new();
        if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            message = s.to_string();
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            message = s.clone();
        }

        let location = panic_info.location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();

        let backtrace = format!("{:?}", std::backtrace::Backtrace::capture());

        let report = format!(
            "LapSphere GUI Crash Report\n\
             ==========================\n\
             Time: {}\n\
             Panic: {}{}\n\n\
             Backtrace:\n\
             {}",
            Local::now().format("%Y-%m-%d %H:%M:%S"),
            message,
            location,
            backtrace
        );

        let _ = fs::write(file_path, report);

        #[cfg(target_os = "linux")]
        eprintln!("Application panicked! Crash report saved to ~/.config/lapsphere");
        #[cfg(target_os = "windows")]
        eprintln!("Application panicked! Crash report saved to %APPDATA%\\lapsphere");
    }));
}

#[cfg(target_os = "linux")]
fn check_single_instance_linux(rt: &tokio::runtime::Runtime) -> Option<zbus::Connection> {
    rt.block_on(async {
        match zbus::Connection::session().await {
            Ok(conn) => {
                // Use the explicit DBus proxy to request name and check the reply
                let dbus = match zbus::fdo::DBusProxy::new(&conn).await {
                    Ok(proxy) => proxy,
                    Err(e) => {
                        log::error!("Failed to create DBus proxy: {}", e);
                        return Some(conn);
                    }
                };

                let reply = dbus.request_name(
                    "io.lapsphere.Gui".try_into().unwrap(),
                    zbus::fdo::RequestNameFlags::DoNotQueue.into()
                ).await;

                match reply {
                    Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => Some(conn),
                    Ok(_) => {
                        eprintln!("Another instance of LapSphere GUI is already running.");
                        None
                    }
                    Err(e) => {
                        log::error!("DBus error requesting name: {}", e);
                        Some(conn)
                    }
                }
            }
            Err(e) => {
                log::error!("Failed to connect to session bus for single instance check: {}", e);
                None
            }
        }
    })
}

#[cfg(target_os = "windows")]
fn check_single_instance_windows() -> Option<isize> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::System::Threading::CreateMutexA;
    use std::ptr::null;

    let name = b"Global\\io.lapsphere.Gui\0";
    unsafe {
        let handle: HANDLE = CreateMutexA(null(), 1, name.as_ptr());
        if handle == 0 {
            return Some(0);
        }
        let err = windows_sys::Win32::Foundation::GetLastError();
        if err == ERROR_ALREADY_EXISTS {
            eprintln!("Another instance of LapSphere GUI is already running.");
            return None;
        }
        Some(handle)
    }
}

// Replace glibc malloc with mimalloc: glibc's per-thread arena pool retains
// freed-but-unused memory (up to 64 MB per arena, one per malloc-thread) and
// never returns it to the OS. Under the GUI's ~8k alloc/s D-Bus polling load
// that arena ratchet accumulated ~8 GB of resident-but-free heap over hours
// (see heaptrack probe: real Rust heap stayed at 27 MB peak / 22 MB leaked
// while RSS grew to 3.6 GB). mimalloc bounds arenas and reuses freed blocks,
// eliminating the retention entirely.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<(), eframe::Error> {
    env_logger::init();
    setup_panic_hook();

    let args: Vec<String> = std::env::args().collect();
    let start_in_tray_arg = args.contains(&"--tray".to_string());

    let config = app::load_config_from_disk().unwrap_or_default();
    let start_minimized = start_in_tray_arg || config.start_minimized;

    // Create and enter a Tokio runtime context.
    // This is required for `tokio::spawn` to work in the `DbusClient`.
    let rt = tokio::runtime::Runtime::new().expect("Unable to create a Tokio runtime");
    let _enter = rt.enter();

    #[cfg(target_os = "linux")]
    let _instance_guard = match check_single_instance_linux(&rt) {
        Some(conn) => conn,
        None => return Ok(()),
    };

    #[cfg(target_os = "windows")]
    let _instance_guard = match check_single_instance_windows() {
        Some(mutex) => mutex,
        None => return Ok(()),
    };
    
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([570.0, 620.0])
            .with_min_inner_size([440.0, 470.0])
            .with_icon(load_icon())
            .with_visible(!start_minimized),
        ..Default::default()
    };
    
    eframe::run_native(
        "LapSphere",
        options,
        Box::new(move |cc| Ok(Box::new(LapSphereApp::new(cc)))),
    )
}

fn load_icon() -> egui::IconData {
    let width = 32;
    let height = 32;
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 4) as usize;

            // Gaming-themed "L" icon: neon green on dark background
            let is_l_vertical = x >= 10 && x <= 14 && y >= 6 && y <= 26;
            let is_l_horizontal = x >= 10 && x <= 22 && y >= 22 && y <= 26;

            if is_l_vertical || is_l_horizontal {
                rgba[idx] = 0;     // R
                rgba[idx + 1] = 255; // G
                rgba[idx + 2] = 0;   // B
                rgba[idx + 3] = 255; // A
            } else {
                rgba[idx] = 26;
                rgba[idx + 1] = 26;
                rgba[idx + 2] = 26;
                rgba[idx + 3] = 255;
            }
        }
    }

    egui::IconData {
        rgba,
        width,
        height,
    }
}
