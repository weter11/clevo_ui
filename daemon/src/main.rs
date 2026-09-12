mod dbus_interface;
mod daemon_settings;
mod hardware_control;
mod hardware_detection;
mod tuxedo_io;
mod battery_control;
mod polling_scheduler;

use anyhow::Result;
use tokio::signal;
use std::sync::{Arc, Mutex};
use std::collections::{VecDeque, HashMap};
use lapsphere_common::types::*;
use polling_scheduler::{PollingScheduler, PollJob};

pub struct HardwareCache {
    pub cpu_info: Option<CpuInfo>,
    pub memory_info: Option<MemoryInfo>,
    pub gpu_info: Vec<GpuInfo>,
    pub battery_info: Option<BatteryInfo>,
    pub fan_info: Vec<FanInfo>,
    pub wifi_info: Vec<WiFiInfo>,
    pub gamepad_info: Vec<GamepadInfo>,
    pub storage_device_info: Vec<StorageDevice>,
    pub mount_info: Vec<MountInfo>,
    pub system_info: Option<SystemInfo>,
}

pub static HARDWARE_CACHE: once_cell::sync::Lazy<Arc<Mutex<HardwareCache>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(HardwareCache {
        cpu_info: None,
        memory_info: None,
        gpu_info: Vec::new(),
        battery_info: None,
        fan_info: Vec::new(),
        wifi_info: Vec::new(),
        gamepad_info: Vec::new(),
        storage_device_info: Vec::new(),
        mount_info: Vec::new(),
        system_info: None,
    })));

// Set by GetGpuInfoFull D-Bus call to force one full NVML query on the next
// hardware_monitor tick (on-demand override for the GUI stats panel).
// The monitor job consumes and clears it, so the override is one-shot.
pub static FULL_NVML_REFRESH_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// Global fan daemon state
pub static FAN_DAEMON_STATE: once_cell::sync::Lazy<Arc<Mutex<Option<FanSettings>>>> = 
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(None)));

// Global polling scheduler handle
pub static SCHEDULER_HANDLE: once_cell::sync::OnceCell<polling_scheduler::SchedulerHandle> = 
    once_cell::sync::OnceCell::new();

// Active daemon poll-interval settings, synced from the GUI's settings.json
// (see daemon_settings for the field mapping and legacy defaults).
pub static DAEMON_POLL_SETTINGS: once_cell::sync::Lazy<
    Arc<Mutex<daemon_settings::DaemonPollSettings>>,
> = once_cell::sync::Lazy::new(|| {
    Arc::new(Mutex::new(daemon_settings::DaemonPollSettings::load()))
});

// Global GPU daemon state
pub static GPU_DAEMON_STATE: once_cell::sync::Lazy<Arc<Mutex<Option<lapsphere_common::types::GpuSettings>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(None)));

pub struct GpuOverclockStats {
    pub freq_offset: i32,
    pub drain_offset: i32,
    pub power_offset: i32,
    pub total_offset: i32,
}

pub static CURRENT_GPU_OVERCLOCK_STATS: once_cell::sync::Lazy<Arc<Mutex<Option<GpuOverclockStats>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(None)));

pub static LAST_APPLIED_OFFSET: once_cell::sync::Lazy<Arc<Mutex<Option<i32>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(None)));

pub static MANUAL_GPU_OFFSETS: once_cell::sync::Lazy<Arc<Mutex<HashMap<u32, (f32, f32)>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

pub static DAEMON_LOGS: once_cell::sync::Lazy<Arc<Mutex<VecDeque<LogEntry>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(VecDeque::with_capacity(2000))));

struct DaemonLogger {
    inner: env_logger::Logger,
}

impl log::Log for DaemonLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        // All levels are enabled for internal buffer (up to Debug per set_max_level)
        // Also allow env_logger to control its own filtering
        true
    }

    fn log(&self, record: &log::Record) {
        // Always capture all log levels into the buffer (Error, Warn, Info, Debug, Trace)
        // The global max level (Trace) controls what reaches this logger

        let mut level = record.level().to_string();
        let target = record.target().to_string();
        let message = record.args().to_string();

        // Move messages starting with zbus:: to trace
        if target.starts_with("zbus") || message.starts_with("zbus::") {
            level = "TRACE".to_string();
        }

        // Move all massages hw. to info as requested
        if target.starts_with("hw.") && level == "DEBUG" {
            level = "INFO".to_string();
        }

        let entry = LogEntry {
            level: level.clone(),
            target: target.clone(),
            message: message.clone(),
            timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        };

        {
            let mut logs = DAEMON_LOGS.lock().unwrap();

            // Deduplicate: avoid adding the same message twice in a row
            let is_duplicate = logs.back().map_or(false, |last| {
                last.level == level &&
                last.target == target &&
                last.message == message
            });

            if !is_duplicate {
                if logs.len() >= 2000 {
                    logs.pop_front();
                }
                logs.push_back(entry);
            }
        }

        // Only log to console if env_logger allows it
        if self.inner.enabled(record.metadata()) {
            self.inner.log(record);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut builder = env_logger::Builder::from_default_env();
    if std::env::var("RUST_LOG").is_err() {
        builder.filter_level(log::LevelFilter::Warn);
        builder.filter(Some("zbus"), log::LevelFilter::Warn);
    }
    let inner = builder.build();
    let _max_level = inner.filter();
    let logger = DaemonLogger { inner };

    log::set_boxed_logger(Box::new(logger)).unwrap();
    log::set_max_level(log::LevelFilter::Trace); // Allow up to Trace to reach our logger for buffer

    log::info!("Starting LapSphere Daemon");

    let args: Vec<String> = std::env::args().collect();

    if args.contains(&"--help".to_string()) || args.contains(&"-h".to_string()) {
        println!("LapSphere Daemon - Hardware Control for Uniwill/Clevo Laptops");
        println!("\nUsage: lapsphere-daemon [OPTIONS]");
        println!("\nOptions:");
        println!("  --gui       Launch the graphical user interface");
        println!("  --tray      Start minimized to system tray");
        println!("  --help, -h  Show this help message");
        println!("  --debug     Enable debug logging");
        println!("\nConfiguration:");
        println!("  Settings are stored in ~/.config/lapsphere/settings.json");
        println!("  Profiles are stored in ~/.config/lapsphere/profiles.json");
        println!("\nTo configure via CLI, you can edit these JSON files directly.");
        return Ok(());
    }

    let launch_gui = args.contains(&"--gui".to_string());
    let launch_tray = args.contains(&"--tray".to_string());

    // Collect other arguments to pass to the GUI
    let gui_args: Vec<String> = args.iter()
        .skip(1)
        .filter(|&a| a != "--gui")
        .cloned()
        .collect();

    // Check if running as root
    if unsafe { libc::geteuid() } != 0 {
        if !launch_gui && !launch_tray {
            println!("--- Hardware Statistics for Uniwill/Clevo Laptops (Limited - non-root) ---");
            match hardware_detection::get_cpu_info() {
                Ok(cpu) => {
                    println!("CPU: {}", cpu.name);
                    println!("  Load: {:.1}%", cpu.average_load);
                    println!("  Temp: {:.1}°C", cpu.package_temp);
                }
                Err(e) => println!("Error getting CPU info: {}", e),
            }
            match hardware_detection::get_memory_info() {
                Ok(mem) => {
                    println!("Memory: {:.1} / {:.1} GiB ({:.1}%)",
                        mem.used_gib, mem.total_gib, mem.used_percent);
                }
                Err(e) => println!("Error getting memory info: {}", e),
            }
            println!("\nError: Full daemon functionality requires root privileges.");
        } else {
            eprintln!("Error: Daemon must run as root to support GUI or tray modes.");
        }
        std::process::exit(1);
    }

    if !launch_gui && !launch_tray {
        println!("--- Hardware Statistics for Uniwill/Clevo Laptops ---");
        match hardware_detection::get_cpu_info() {
            Ok(cpu) => {
                println!("CPU: {}", cpu.name);
                println!("  Load: {:.1}%", cpu.average_load);
                println!("  Temp: {:.1}°C", cpu.package_temp);
                if let Some(power) = cpu.package_power {
                    println!("  Power: {:.1}W", power);
                }
            }
            Err(e) => println!("Error getting CPU info: {}", e),
        }

        match hardware_detection::get_memory_info() {
            Ok(mem) => {
                println!("Memory: {:.1} / {:.1} GiB ({:.1}%)",
                    mem.used_gib, mem.total_gib, mem.used_percent);
            }
            Err(e) => println!("Error getting memory info: {}", e),
        }

        if let Ok(gpus) = hardware_detection::get_gpu_info() {
            for gpu in gpus {
                println!("GPU: {}", gpu.name);
                if let Some(load) = gpu.load { println!("  Load: {:.1}%", load); }
                if let Some(temp) = gpu.temperature { println!("  Temp: {:.1}°C", temp); }
            }
        }

        println!("\nDaemon is running. Press Ctrl+C to exit.");
    }

    // Initialize hardware interfaces. `TuxedoIo::shared()` opens /dev/tuxedo_io
    // once for the whole daemon (opening it runs interface detection), so no poll
    // tick re-opens the device or re-probes the interface.
    let tuxedo_io = if tuxedo_io::TuxedoIo::is_available() {
        match tuxedo_io::TuxedoIo::shared() {
            Some(io) => {
                let interface = match io.get_interface() {
                    tuxedo_io::HardwareInterface::Clevo => "Clevo",
                    tuxedo_io::HardwareInterface::Uniwill => "Uniwill",
                    tuxedo_io::HardwareInterface::None => "None",
                };
                log::debug!("Detected hardware interface: {}", interface);
                log::debug!("Number of fans: {}", io.get_fan_count());
                Some(io)
            }
            None => {
                log::warn!("Failed to initialize tuxedo_io");
                None
            }
        }
    } else {
        log::debug!("/dev/tuxedo_io not available - some features will be disabled");
        None
    };

    // Check battery charge control
    if battery_control::BatteryControl::is_available() {
        log::debug!("Battery charge control (flexicharger) is available");
    } else {
        log::debug!("Battery charge control not available");
    }

    // Create and start polling scheduler
    let scheduler = PollingScheduler::new();
    let scheduler_handle = scheduler.get_handle();
    
    // Store handle globally for DBus interface to use
    SCHEDULER_HANDLE.set(scheduler_handle.clone()).ok();
    
    // Initial hardware poll to populate cache immediately
    log::debug!("Performing initial hardware detection...");
    refresh_hardware_cache();
    log::debug!("Initial hardware detection complete");

    // Start scheduler in background
    tokio::spawn(async move {
        scheduler.run().await;
    });

    // Add hardware monitor polling job
    let hw_monitor_fn = || {
        refresh_hardware_cache();
        Ok(())
    };

    let hw_monitor_job = PollJob::new(
        "hardware_monitor".to_string(),
        // Feeds CPU + Memory + GPU (+ everything else) from one shared cache,
        // so it runs at the fastest of the consumer section rates.
        DAEMON_POLL_SETTINGS.lock().unwrap().hardware_monitor(),
        hw_monitor_fn,
    );

    if let Err(e) = scheduler_handle.add_job(hw_monitor_job) {
        log::error!("Failed to add hardware monitor job: {}", e);
    } else {
        log::debug!("Hardware monitor polling job added");
    }

    // Add fan control polling job if hardware is available
    if let Some(io) = tuxedo_io {
        let fan_io = Arc::new(io);
        let poll_fn = {
            let fan_io = fan_io.clone();
            move || {
                let settings = {
                    let state = FAN_DAEMON_STATE.lock().unwrap();
                    state.clone()
                };

                if let Some(ref fan_settings) = settings {
                    if fan_settings.control_enabled {
                        // Sort curves for each fan
                        let sorted_curves: Vec<Vec<(u8, u8)>> = fan_settings.curves.iter().map(|c| {
                            let mut points = c.points.clone();
                            points.sort_by_key(|p| p.0);
                            points
                        }).collect();

                        apply_fan_curves(&fan_io, fan_settings, &sorted_curves)?;
                    }
                }
                Ok(())
            }
        };

        let fan_job = PollJob::new(
            "fan_control".to_string(),
            DAEMON_POLL_SETTINGS.lock().unwrap().fan_control(),
            poll_fn,
        );

        if let Err(e) = scheduler_handle.add_job(fan_job) {
            log::error!("Failed to add fan control job: {}", e);
        } else {
            log::debug!("Fan control polling job added");
        }
    }

    // Add GPU overclocking polling job
    let gpu_poll_fn = || {
        let settings = {
            let state = GPU_DAEMON_STATE.lock().unwrap();
            state.clone()
        };

        if let Some(ref gpu_settings) = settings {
            apply_gpu_overclocking(gpu_settings)?;
        } else {
            {
                let mut stats = CURRENT_GPU_OVERCLOCK_STATS.lock().unwrap();
                *stats = None;
            }
            {
                let mut last = LAST_APPLIED_OFFSET.lock().unwrap();
                *last = None;
            }
        }
        Ok(())
    };

    let log_tick = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let gpu_job_poll = {
        let log_tick = log_tick.clone();
        move || {
            let tick = log_tick.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if tick % 60 == 0 {
                log::debug!(target: "daemon", "heartbeat uptime_tick={}", tick);
            }
            gpu_poll_fn()
        }
    };

    let gpu_job = PollJob::new(
        "gpu_overclock".to_string(),
        DAEMON_POLL_SETTINGS.lock().unwrap().gpu_overclock(),
        gpu_job_poll,
    );

    if let Err(e) = scheduler_handle.add_job(gpu_job) {
        log::error!("Failed to add GPU overclocking job: {}", e);
    } else {
        log::debug!("GPU overclocking polling job added");
    }

    // Start DBus service
    let connection = zbus::Connection::system().await?;
    let connection_clone = connection.clone();
    tokio::spawn(async move {
        if let Err(e) = dbus_interface::start_service(connection_clone).await {
            log::error!("Failed to start DBus service: {}", e);
        }
    });

    log::debug!("DBus service started");

    // Launch GUI if requested
    if launch_gui {
        use tokio::process::Command;

        let target_uid = std::env::var("SUDO_UID")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .or_else(|| std::env::var("PKEXEC_UID").ok().and_then(|v| v.parse::<u32>().ok()));

        if let Some(uid) = target_uid {
            log::info!("Launching GUI as user UID {}", uid);

            // Try to find the binary in the same directory as the daemon or in PATH
            let current_exe = std::env::current_exe().ok();
            let gui_bin_path = current_exe.and_then(|p| p.parent().map(|parent| parent.join("lapsphere")));

            let mut gui_cmd = if let Some(ref path) = gui_bin_path.filter(|p| p.exists()) {
                Command::new(path)
            } else {
                Command::new("lapsphere")
            };
            gui_cmd.args(&gui_args);
            gui_cmd.uid(uid);

            // Inherit environment variables that might be needed for X11/Wayland
            for var in &[
                "DISPLAY",
                "XAUTHORITY",
                "WAYLAND_DISPLAY",
                "DBUS_SESSION_BUS_ADDRESS",
                "XDG_RUNTIME_DIR",
                "XDG_SESSION_TYPE",
                "XDG_CURRENT_DESKTOP",
                "GDK_BACKEND",
                "QT_QPA_PLATFORM",
            ] {
                if let Ok(val) = std::env::var(var) {
                    gui_cmd.env(var, val);
                }
            }

            match gui_cmd.spawn() {
                Ok(mut child) => {
                    tokio::spawn(async move {
                        match child.wait().await {
                            Ok(status) => {
                                log::info!("GUI exited with status: {}, shutting down daemon", status);
                            }
                            Err(e) => {
                                log::error!("Error waiting for GUI: {}, shutting down daemon", e);
                            }
                        }
                        let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGINT);
                    });
                }
                Err(e) => {
                    log::error!("Failed to launch GUI: {}", e);
                    // If we failed to launch requested GUI, should we exit?
                    // User said "if user exit gui, then stop daemon", so if it never starts...
                    let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGINT);
                }
            }
        } else {
            log::warn!("--gui flag passed but couldn't determine target UID (SUDO_UID or PKEXEC_UID not set)");
            // If we are root but not via sudo/pkexec, we probably shouldn't launch GUI as root
        }
    }

    // Wait for shutdown signal
    signal::ctrl_c().await?;
    log::info!("Shutting down daemon");

    // Cleanup
    if let Err(e) = crate::hardware_control::restore_cpu_frequency_limits() {
        log::error!("Failed to restore CPU frequency limits on exit: {}", e);
    }

    Ok(())
}

pub fn refresh_hardware_cache() {
    let cpu_info = hardware_detection::get_cpu_info().ok();
    let memory_info = hardware_detection::get_memory_info().ok();
    let gpu_info = hardware_detection::get_gpu_info().unwrap_or_default();
    let battery_info = hardware_detection::get_battery_info().ok();
    let fan_info = hardware_detection::get_all_fan_info().unwrap_or_default();
    let wifi_info = hardware_detection::get_wifi_info().unwrap_or_default();
    let gamepad_info = hardware_detection::get_gamepad_info().unwrap_or_default();
    let storage_device_info = hardware_detection::get_storage_device_info().unwrap_or_default();
    let mount_info = hardware_detection::get_mount_info().unwrap_or_default();
    let system_info = hardware_detection::get_system_info().ok();

    {
        let mut cache = HARDWARE_CACHE.lock().unwrap();
        cache.cpu_info = cpu_info;
        cache.memory_info = memory_info;
        cache.gpu_info = gpu_info;
        cache.battery_info = battery_info;
        cache.fan_info = fan_info;
        cache.wifi_info = wifi_info;
        cache.gamepad_info = gamepad_info;
        cache.storage_device_info = storage_device_info;
        cache.mount_info = mount_info;
        cache.system_info = system_info;
    }
}

fn apply_fan_curves(io: &tuxedo_io::TuxedoIo, settings: &FanSettings, sorted_curves: &[Vec<(u8, u8)>]) -> Result<()> {
    for (i, curve) in settings.curves.iter().enumerate() {
        if curve.fan_id >= io.get_fan_count() {
            continue;
        }
        
        let temp = match io.get_fan_temperature(curve.fan_id) {
            Ok(t) => t as f32,
            Err(e) => {
                log::warn!("Failed to read fan {} temperature: {}", curve.fan_id, e);
                continue;
            }
        };
        
        let speed = calculate_fan_speed(&sorted_curves[i], temp);
        
        if let Err(e) = io.set_fan_speed(curve.fan_id, speed as u32) {
            log::error!("Failed to set fan {} speed: {}", curve.fan_id, e);
        } else {
            log::debug!("Fan {}: temp={}°C, speed={}%", curve.fan_id, temp, speed);
        }
    }
    
    Ok(())
}

fn apply_gpu_overclocking(gpu_settings: &lapsphere_common::types::GpuSettings) -> Result<()> {
    // Clear stats and last offset if advanced control or manual clocks are disabled
    if !gpu_settings.advanced_control || !gpu_settings.manual_clocks {
        {
            let mut stats = CURRENT_GPU_OVERCLOCK_STATS.lock().unwrap();
            *stats = None;
        }
        {
            let mut last = LAST_APPLIED_OFFSET.lock().unwrap();
            *last = None;
        }
        if !gpu_settings.manual_clocks {
            // Only clear if not already cleared to avoid waking up GPU unnecessarily
            let needs_clear = {
                let map = MANUAL_GPU_OFFSETS.lock().unwrap();
                map.get(&0).map_or(true, |offsets| offsets.0 != 0.0 || offsets.1 != 0.0)
            };

            if needs_clear {
                log::info!("Manual clocks disabled, resetting GPU offsets to 0");
                let _ = crate::hardware_control::set_gpu_core_offset(0, 0.0);
                let _ = crate::hardware_control::set_gpu_memory_offset(0, 0.0);
                {
                    let mut map = MANUAL_GPU_OFFSETS.lock().unwrap();
                    map.insert(0, (0.0, 0.0));
                }
            }
        }
        return Ok(());
    }

    // 1. Get current GPU stats (temperature, power, frequency)
    // We only do this if advanced control is enabled
    let gpus = crate::hardware_detection::get_gpu_info()?;
    let nvidia_gpu = gpus.iter().find(|g| g.name.to_lowercase().contains("nvidia"));

    if let Some(gpu) = nvidia_gpu {
        let status_lower = gpu.status.to_lowercase();
        let is_suspended = status_lower.contains("suspended");
        let is_pstate = status_lower.starts_with('p');

        // If suspended, don't do anything
        if is_suspended {
            return Ok(());
        }

        // Check if GPU is in an active state for overclocking (typically P0)
        if !is_pstate {
            return Ok(());
        }

        let temp = gpu.temperature.unwrap_or(0.0);
        let power = gpu.power.unwrap_or(0.0);
        let freq = gpu.frequency.unwrap_or(0) as f32;

        let adv = &gpu_settings.advanced;

        // Freq Offset calculation
        let freq_offset = if freq <= adv.frequency_min as f32 {
            adv.freq_offset_max as f32
        } else if freq >= adv.frequency_max as f32 {
            adv.freq_offset_min as f32
        } else {
            let ratio = (freq - adv.frequency_min as f32) / (adv.frequency_max - adv.frequency_min) as f32;
            adv.freq_offset_max as f32 - ratio * (adv.freq_offset_max - adv.freq_offset_min) as f32
        };

        // Drain Offset calculation
        let mut drain_offset = 0.0;
        if adv.drain_offset_control {
            let is_high_freq = freq >= adv.high_freq_min as f32 && freq <= adv.high_freq_max as f32;
            let is_low_freq = freq >= adv.low_freq_min as f32 && freq <= adv.low_freq_max as f32;

            if adv.critical_temp_range_control && temp >= adv.critical_temp_min as f32 && temp <= adv.critical_temp_max as f32 {
                if is_low_freq {
                    drain_offset = adv.drain_offset_lmin as f32;
                } else if is_high_freq {
                    drain_offset = adv.drain_offset_hmin as f32;
                }
            } else if temp > adv.temperature_max as f32 {
                if is_low_freq {
                    drain_offset = adv.drain_offset_lmax as f32;
                } else if is_high_freq {
                    drain_offset = adv.drain_offset_hmin as f32;
                }
            } else {
                let temp_range = (adv.temperature_max - adv.temperature_min) as f32;
                let temp_ratio = if temp_range > 0.0 {
                    ((temp - adv.temperature_min as f32) / temp_range).clamp(0.0, 1.0)
                } else {
                    0.0
                };

                if is_high_freq {
                    // linearly decreased from 'drain_offset_hmax' to 'drain_offset_hmin'
                    drain_offset = adv.drain_offset_hmax as f32 - temp_ratio * (adv.drain_offset_hmax - adv.drain_offset_hmin) as f32;
                } else {
                    // linearly increased from 'drain_offset_lmin' to 'drain_offset_lmax'
                    drain_offset = adv.drain_offset_lmin as f32 + temp_ratio * (adv.drain_offset_lmax - adv.drain_offset_lmin) as f32;
                }
            }
        }

        // Power Offset calculation
        let mut power_offset = 0.0;
        if adv.power_offset_control {
            if power <= adv.plimit_min as f32 {
                power_offset = adv.power_offset_max as f32;
            } else if power >= adv.plimit_max as f32 {
                power_offset = adv.power_offset_min as f32;
            } else {
                let p_range = (adv.plimit_max - adv.plimit_min) as f32;
                let p_ratio = if p_range > 0.0 {
                    (power - adv.plimit_min as f32) / p_range
                } else {
                    0.0
                };
                power_offset = adv.power_offset_max as f32 - p_ratio * (adv.power_offset_max - adv.power_offset_min) as f32;
            }
        }

        // Total Offset
        let total_offset = freq_offset + drain_offset + power_offset;

        if status_lower != "p0" {
            let mut last = LAST_APPLIED_OFFSET.lock().unwrap();
            if *last != Some(0) {
                crate::hardware_control::set_gpu_core_offset(0, 0.0)?;
                *last = Some(0);
                log::debug!("Cleared dynamic GPU offset (P-state not 0)");
            }
            drop(last);
            let mut stats = CURRENT_GPU_OVERCLOCK_STATS.lock().unwrap();
            *stats = None;
            return Ok(());
        }

        let final_offset = {
             // SMART ROUNDING
             let threshold = adv.smart_rounding_threshold as f32;
             if threshold > 0.0 {
                 let multiples = (total_offset / threshold).floor();
                 let remainder = total_offset - (multiples * threshold);
                 if remainder >= (2.0/3.0) * threshold {
                     (multiples + 1.0) * threshold
                 } else {
                     multiples * threshold
                 }
             } else {
                 total_offset
             }
        };

        let final_offset_i32 = final_offset as i32;

        // ONLY APPLY IF CHANGED (fix stuttering)
        {
            let mut last = LAST_APPLIED_OFFSET.lock().unwrap();
            if *last != Some(final_offset_i32) {
                crate::hardware_control::set_gpu_core_offset(0, final_offset_i32 as f32)?;
                *last = Some(final_offset_i32);
                if final_offset_i32 == 0 {
                    log::debug!("Cleared dynamic GPU offset (P-state not 0)");
                } else {
                    log::debug!("Applied new dynamic GPU offset: {} MHz", final_offset_i32);
                }
            }
        }

        // Update global stats for UI
        let mut stats = CURRENT_GPU_OVERCLOCK_STATS.lock().unwrap();
        *stats = Some(GpuOverclockStats {
            freq_offset: freq_offset as i32,
            drain_offset: drain_offset as i32,
            power_offset: power_offset as i32,
            total_offset: final_offset as i32,
        });
    }
    Ok(())
}

fn calculate_fan_speed(sorted_points: &[(u8, u8)], temp: f32) -> u8 {
    if sorted_points.is_empty() {
        return 50; // Default fallback
    }
    
    if sorted_points.len() == 1 {
        return sorted_points[0].1;
    }
    
    if temp <= sorted_points[0].0 as f32 {
        return sorted_points[0].1;
    }
    
    if temp >= sorted_points[sorted_points.len() - 1].0 as f32 {
        return sorted_points[sorted_points.len() - 1].1;
    }
    
    for i in 0..sorted_points.len() - 1 {
        let (temp1, speed1) = sorted_points[i];
        let (temp2, speed2) = sorted_points[i + 1];
        
        if temp >= temp1 as f32 && temp <= temp2 as f32 {
            let ratio = (temp - temp1 as f32) / (temp2 as f32 - temp1 as f32);
            let speed = speed1 as f32 + ratio * (speed2 as f32 - speed1 as f32);
            return speed.round() as u8;
        }
    }
    
    50 // Fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;
    
    #[test]
    fn test_daemon_logger_captures_all_levels() {
        // This test verifies that the DaemonLogger captures all log levels
        // Note: In a real environment, we would need to initialize the logger
        // Here we're just verifying the static log buffer can be accessed
        let logs = DAEMON_LOGS.lock().unwrap();
        assert!(logs.capacity() >= 500, "Log buffer should have capacity of at least 500");
    }
    
    #[test]
    fn test_logger_enabled_returns_true() {
        // Create a dummy env_logger
        let inner = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Info)
            .build();
        let logger = DaemonLogger { inner };
        
        // Test that enabled() returns true for all metadata
        let metadata = log::Metadata::builder()
            .level(log::Level::Debug)
            .target("test")
            .build();
        assert!(logger.enabled(&metadata), "Logger should enable all levels");
        
        let metadata = log::Metadata::builder()
            .level(log::Level::Error)
            .target("test")
            .build();
        assert!(logger.enabled(&metadata), "Logger should enable Error level");
    }
}
