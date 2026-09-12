use anyhow::{anyhow, Result};
use nvml_wrapper::Nvml;
use once_cell::sync::Lazy;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use lapsphere_common::types::*;
use crate::tuxedo_io::{TuxedoIo, HardwareInterface};

static CPU_LIMITS_MODIFIED: AtomicBool = AtomicBool::new(false);
static LAST_APPLIED_PROFILE: Lazy<Mutex<Option<Profile>>> = Lazy::new(|| Mutex::new(None));

fn get_cpu_count() -> Result<u32> {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo")?;
    let count = cpuinfo.lines()
        .filter(|line| line.starts_with("processor"))
        .count();
    Ok(count as u32)
}

pub fn set_cpu_governor(governor: &str) -> Result<()> {
    let cpu_count = get_cpu_count()?;
    
    for i in 0..cpu_count {
        let path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_governor", i);
        fs::write(&path, governor)
            .map_err(|e| anyhow!("Failed to set governor for CPU {}: {}", i, e))?;
    }
    
    log::info!(target: "hw.cpu", "set_governor profile=\"{}\"", governor);
    crate::refresh_hardware_cache();
    Ok(())
}

pub fn set_cpu_frequency_limits(min_freq: u64, max_freq: u64) -> Result<()> {
    let cpu_count = get_cpu_count()?;
    
    // IMPORTANT: Set max first, then min to avoid conflicts
    // If current min > new max, setting max first will fail
    // If current max < new min, setting min first will fail
    
    // First, read current values
    let current_min = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_min_freq")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(min_freq);
    
    let current_max = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(max_freq);
    
    for i in 0..cpu_count {
        let min_path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_min_freq", i);
        let max_path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_max_freq", i);
        
        // Determine order based on current vs new values
        if max_freq < current_max || min_freq > current_min {
            // Set max first
            fs::write(&max_path, max_freq.to_string())
                .map_err(|e| anyhow!("Failed to set max frequency for CPU {}: {}", i, e))?;
            fs::write(&min_path, min_freq.to_string())
                .map_err(|e| anyhow!("Failed to set min frequency for CPU {}: {}", i, e))?;
        } else {
            // Set min first
            fs::write(&min_path, min_freq.to_string())
                .map_err(|e| anyhow!("Failed to set min frequency for CPU {}: {}", i, e))?;
            fs::write(&max_path, max_freq.to_string())
                .map_err(|e| anyhow!("Failed to set max frequency for CPU {}: {}", i, e))?;
        }
    }
    
    CPU_LIMITS_MODIFIED.store(true, Ordering::SeqCst);
    log::info!(target: "hw.cpu", "set_freq_limits min={} max={}", min_freq, max_freq);
    Ok(())
}

pub fn restore_cpu_frequency_limits() -> Result<()> {
    if !CPU_LIMITS_MODIFIED.load(Ordering::SeqCst) {
        return Ok(());
    }

    log::info!(target: "hw.cpu", "Restoring CPU frequency limits to hardware defaults");
    let (hw_min, hw_max) = crate::hardware_detection::read_hw_frequency_limits()?;

    if let (Some(min), Some(max)) = (hw_min, hw_max) {
        set_cpu_frequency_limits(min, max)?;
    }

    Ok(())
}

pub fn set_cpu_boost(enabled: bool) -> Result<()> {
    // AMD cpufreq boost
    let amd_path = "/sys/devices/system/cpu/cpufreq/boost";
    if Path::new(amd_path).exists() {
        fs::write(amd_path, if enabled { "1" } else { "0" })?;
        log::info!(target: "hw.cpu", "set_amd_boost enabled={}", enabled);
        return Ok(());
    }
    
    // Intel turbo
    let intel_path = "/sys/devices/system/cpu/intel_pstate/no_turbo";
    if Path::new(intel_path).exists() {
        fs::write(intel_path, if enabled { "0" } else { "1" })?;
        log::info!(target: "hw.cpu", "set_intel_turbo enabled={}", enabled);
        return Ok(());
    }
    
    // AMD P-State boost (if using amd-pstate driver)
    let amd_pstate_boost = "/sys/devices/system/cpu/amd_pstate/cpb_boost";
    if Path::new(amd_pstate_boost).exists() {
        fs::write(amd_pstate_boost, if enabled { "1" } else { "0" })?;
        log::info!(target: "hw.cpu", "set_amd_pstate_boost enabled={}", enabled);
        return Ok(());
    }
    
    Err(anyhow!("Boost control not available"))
}

pub fn set_smt(enabled: bool) -> Result<()> {
    let path = "/sys/devices/system/cpu/smt/control";
    if !Path::new(path).exists() {
        return Err(anyhow!("SMT control not available"));
    }
    
    fs::write(path, if enabled { "on" } else { "off" })?;
    log::info!(target: "hw.cpu", "set_smt enabled={}", enabled);
    Ok(())
}

pub fn set_amd_pstate_status(status: &str) -> Result<()> {
    let path = "/sys/devices/system/cpu/amd_pstate/status";
    if !Path::new(path).exists() {
        return Err(anyhow!("AMD pstate not available"));
    }
    
    if !["passive", "active", "guided"].contains(&status) {
        return Err(anyhow!("Invalid AMD pstate status: {}", status));
    }
    
    fs::write(path, status)?;
    log::info!(target: "hw.cpu", "set_amd_pstate_status status=\"{}\"", status);
    crate::refresh_hardware_cache();
    Ok(())
}

pub fn set_intel_pstate_status(status: &str) -> Result<()> {
    let path = "/sys/devices/system/cpu/intel_pstate/status";
    if !Path::new(path).exists() {
        return Err(anyhow!("Intel pstate not available"));
    }

    if !["passive", "active"].contains(&status) {
        return Err(anyhow!("Invalid Intel pstate status: {}", status));
    }

    fs::write(path, status)?;
    log::info!(target: "hw.cpu", "set_intel_pstate_status status=\"{}\"", status);
    crate::refresh_hardware_cache();
    Ok(())
}

pub fn apply_profile(profile: &Profile) -> Result<()> {
    // Check if this profile is already applied to avoid redundant hardware calls
    {
        let mut last_profile = LAST_APPLIED_PROFILE.lock().unwrap();
        if let Some(ref last) = *last_profile {
            if last == profile {
                log::info!(target: "hw.detect", "Profile '{}' is already applied, skipping", profile.name);
                return Ok(());
            }
        }
        *last_profile = Some(profile.clone());
    }

    log::info!(target: "hw.detect", "Applying profile: {}", profile.name);
    
    // Apply CPU settings
    if let Some(ref governor) = profile.cpu_settings.governor {
        set_cpu_governor(governor)?;
    }
    
    if let Some(ref tdp_profile) = profile.cpu_settings.tdp_profile {
        set_tdp_profile(tdp_profile)?;
    }

    if let Some(io) = TuxedoIo::shared() {
        if io.get_interface() == HardwareInterface::Uniwill {
            if let Some(val) = profile.cpu_settings.tdp0 {
                let _ = io.set_tdp(0, val);
            }
            if let Some(val) = profile.cpu_settings.tdp1 {
                let _ = io.set_tdp(1, val);
            }
            if let Some(val) = profile.cpu_settings.tdp2 {
                let _ = io.set_tdp(2, val);
            }
        }
    }
    
    if let Some(ref amd_status) = profile.cpu_settings.amd_pstate_status {
        set_amd_pstate_status(amd_status)?;
    }

    if let Some(ref intel_status) = profile.cpu_settings.intel_pstate_status {
        set_intel_pstate_status(intel_status)?;
    }
    
    if let Some(ref epp) = profile.cpu_settings.energy_performance_preference {
        set_energy_performance_preference(epp)?;
    }
    
    if let (Some(min), Some(max)) = (profile.cpu_settings.min_frequency, profile.cpu_settings.max_frequency) {
        set_cpu_frequency_limits(min, max)?;
    }

    // Apply GPU settings
    let nvidia_gpu_idx = {
        let cache = crate::HARDWARE_CACHE.lock().unwrap();
        cache.gpu_info.iter()
            .find(|g| g.name.to_lowercase().contains("nvidia"))
            .and_then(|g| g.nvml_index)
            .unwrap_or(0)
    };

    if let Some(limit) = profile.gpu_settings.power_limit {
        let _ = set_gpu_power_limit(nvidia_gpu_idx, limit);
    }

    if let Some(core_offset) = profile.gpu_settings.core_offset {
        let _ = set_gpu_core_offset(nvidia_gpu_idx, core_offset as f32);
    }

    if let Some(memory_offset) = profile.gpu_settings.memory_offset {
        let _ = set_gpu_memory_offset(nvidia_gpu_idx, memory_offset as f32);
    }

    if let (Some(min_clock), Some(max_clock)) = (profile.gpu_settings.min_gpu_clock, profile.gpu_settings.max_gpu_clock) {
        let _ = set_gpu_locked_clocks(nvidia_gpu_idx, min_clock, max_clock);
    } else {
        let _ = reset_gpu_clocks(nvidia_gpu_idx);
    }
    
    if let Some(boost) = profile.cpu_settings.boost {
        set_cpu_boost(boost)?;
    }
    
    if let Some(smt) = profile.cpu_settings.smt {
        set_smt(smt)?;
    }
    
    // Apply keyboard settings
    apply_keyboard_settings(&profile.keyboard_settings)?;
    
    // Apply screen settings
    apply_screen_settings(&profile.screen_settings)?;
    
    // Apply fan settings - update daemon state
    apply_fan_settings(&profile.fan_settings)?;

    // Apply NVIDIA fan settings
    for fan_setting in &profile.gpu_settings.nvidia_fans {
        if fan_setting.manual {
            let _ = set_gpu_fan_speed(fan_setting.device_index, fan_setting.fan_id, fan_setting.speed);
        } else {
            let _ = set_gpu_fan_auto(fan_setting.device_index, fan_setting.fan_id);
        }
    }
    
    log::info!(target: "hw.detect", "Profile '{}' applied successfully", profile.name);
    Ok(())
}

pub fn apply_battery_settings(settings: &BatterySettings) -> Result<()> {
    if !crate::battery_control::BatteryControl::is_available() {
        log::info!(target: "hw.battery", "Battery control not available, skipping");
        return Ok(());
    }

    let battery = crate::battery_control::BatteryControl::new()?;

    if settings.control_enabled {
        battery.set_charge_type("Custom")?;
        battery.set_charge_control_start_threshold(settings.charge_start_threshold)?;
        battery.set_charge_control_end_threshold(settings.charge_end_threshold)?;
        log::info!(target: "hw.battery",
            "set_thresholds enabled=true start={} end={}",
            settings.charge_start_threshold,
            settings.charge_end_threshold
        );
    } else {
        battery.set_charge_type("Standard")?;
        log::info!(target: "hw.battery", "set_thresholds enabled=false mode=\"Standard\"");
    }

    Ok(())
}

fn apply_keyboard_settings(settings: &KeyboardSettings) -> Result<()> {
    if !settings.control_enabled {
        log::info!(target: "hw.kbd", "keyboard_control enabled=false");
        if let Ok(kbd) = RgbKeyboardControl::new() {
            let white_mode = KeyboardMode::SingleColor {
                r: 255,
                g: 255,
                b: 255,
                brightness: 50,
            };
            let _ = kbd.set_mode(&white_mode);
        }
        return Ok(());
    }
    
    if let Ok(kbd) = RgbKeyboardControl::new() {
        kbd.set_mode(&settings.mode)?;
        log::info!(target: "hw.kbd", "keyboard_settings applied=true");
        Ok(())
    } else {
        log::warn!(target: "hw.kbd", "Keyboard control not available");
        Err(anyhow!("Keyboard control not available"))
    }
}

pub fn preview_keyboard_settings(settings: &KeyboardSettings) -> Result<()> {
    if let Ok(kbd) = RgbKeyboardControl::new() {
        kbd.set_mode(&settings.mode)?;
        Ok(())
    } else {
        Err(anyhow!("Keyboard control not available"))
    }
}

fn apply_screen_settings(settings: &ScreenSettings) -> Result<()> {
    if settings.system_control {
        log::info!(target: "hw.screen", "Using system screen brightness control");
        return Ok(());
    }
    
    let backlight_paths = [
        "/sys/class/backlight/intel_backlight",
        "/sys/class/backlight/amdgpu_bl0",
        "/sys/class/backlight/amdgpu_bl1",
        "/sys/class/backlight/acpi_video0",
    ];
    
    for base_path in &backlight_paths {
        let brightness_path = format!("{}/brightness", base_path);
        let max_brightness_path = format!("{}/max_brightness", base_path);
        
        if Path::new(&brightness_path).exists() {
            let max_brightness: u32 = fs::read_to_string(&max_brightness_path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(255);
            
            let actual_brightness = ((settings.brightness as u32) * max_brightness) / 100;
            
            // Write to actual_brightness first (this is writable)
            let actual_path = format!("{}/actual_brightness", base_path);
            if Path::new(&actual_path).exists() {
                if let Err(e) = fs::write(&actual_path, actual_brightness.to_string()) {
                    log::warn!(target: "hw.screen", "Could not write to actual_brightness: {}", e);
                }
            }
            
            // Then write to brightness
            match fs::write(&brightness_path, actual_brightness.to_string()) {
                Ok(_) => {
                    log::info!(target: "hw.screen", "set_brightness level={}% path=\"{}\"", settings.brightness, base_path);
                    return Ok(());
                }
                Err(e) => {
                    log::warn!(target: "hw.screen", "Failed to set brightness at {}: {}", base_path, e);
                    continue;
                }
            }
        }
    }
    
    Err(anyhow!("No writable backlight control found"))
}

pub fn set_tdp_profile(profile_name: &str) -> Result<()> {
    if !TuxedoIo::is_available() {
        return Err(anyhow!("TDP profiles not available"));
    }
    
    let io = TuxedoIo::shared().ok_or_else(|| anyhow!("tuxedo_io not available"))?;
    let profiles = io.get_available_profiles()?;
    
    if let Some(profile_id) = profiles.iter().position(|p| p == profile_name) {
        io.set_performance_profile(profile_id as u32)?;
        log::info!(target: "hw.cpu", "set_tdp_profile name=\"{}\" id={}", profile_name, profile_id);
        Ok(())
    } else {
        Err(anyhow!("Profile '{}' not found. Available: {:?}", profile_name, profiles))
    }
}

pub fn set_fan_speed(fan_id: u32, speed_percent: u32) -> Result<()> {
    if !TuxedoIo::is_available() {
        return Err(anyhow!("Fan control not available"));
    }
    
    let speed = speed_percent.min(100);
    log::info!(target: "hw.fan", "DBus request: set fan {} to {}%", fan_id, speed);
    let io = TuxedoIo::shared().ok_or_else(|| anyhow!("tuxedo_io not available"))?;
    io.set_fan_speed(fan_id, speed)?;
    
    log::info!(target: "hw.fan", "set_fan id={} speed={}%", fan_id, speed);
    Ok(())
}

pub fn set_fan_auto(_fan_id: u32) -> Result<()> {
    if !TuxedoIo::is_available() {
        return Err(anyhow!("Fan control not available"));
    }
    
    let io = TuxedoIo::shared().ok_or_else(|| anyhow!("tuxedo_io not available"))?;
    io.set_fan_auto()?;
    
    log::info!(target: "hw.fan", "set_fans_auto");
    Ok(())
}

fn apply_fan_settings(settings: &FanSettings) -> Result<()> {
    if !TuxedoIo::is_available() {
        log::info!(target: "hw.fan", "Fan control not available (/dev/tuxedo_io not present)");
        return Ok(());
    }
    
    log::info!(target: "hw.fan", "Applying fan settings: enabled={}", settings.control_enabled);
    
    // Update the global fan daemon state
    {
        let mut state = crate::FAN_DAEMON_STATE.lock().unwrap();
        if settings.control_enabled {
            *state = Some(settings.clone());
            log::info!(target: "hw.fan", "fan_daemon enabled=true curves={}", settings.curves.len());
        } else {
            *state = None;
            log::info!(target: "hw.fan", "fan_daemon enabled=false");
        }
    }
    
    if !settings.control_enabled {
        set_fan_auto(0)?;
    }
    
    Ok(())
}

pub fn set_webcam_state(enabled: bool) -> Result<()> {
    if !TuxedoIo::is_available() {
        return Err(anyhow!("Webcam control not available"));
    }
    
    let io = TuxedoIo::shared().ok_or_else(|| anyhow!("tuxedo_io not available"))?;
    io.set_webcam_state(enabled)?;
    
    log::info!(target: "hw.detect", "set_webcam enabled={}", enabled);
    Ok(())
}

pub fn get_webcam_state() -> Result<bool> {
    if !TuxedoIo::is_available() {
        // Return true as default if driver not present
        return Ok(true);
    }
    
    let io = TuxedoIo::shared().ok_or_else(|| anyhow!("tuxedo_io not available"))?;
    if io.get_interface() != HardwareInterface::Clevo {
        // Return true for non-Clevo hardware (standard state)
        return Ok(true);
    }

    match io.get_webcam_state() {
        Ok(state) => Ok(state),
        Err(_) => Ok(true), // Fallback to true on error
    }
}


use nvml_wrapper::enum_wrappers::device::{Clock, PerformanceState};
use nvml_wrapper::enums::device::GpuLockedClocksSetting;

static NVML: Lazy<Result<Nvml, nvml_wrapper::error::NvmlError>> = Lazy::new(|| Nvml::init());

pub fn get_nvml() -> Result<&'static Nvml> {
    match &*NVML {
        Ok(nvml) => Ok(nvml),
        Err(e) => Err(anyhow!("Failed to initialize NVML: {}", e)),
    }
}

pub fn set_gpu_locked_clocks(device_index: u32, min_clock: u32, max_clock: u32) -> Result<()> {
    let nvml = get_nvml()?;
    let mut device = nvml.device_by_index(device_index)?;
    device.set_gpu_locked_clocks(GpuLockedClocksSetting::Numeric {
        min_clock_mhz: min_clock,
        max_clock_mhz: max_clock,
    })?;
    Ok(())
}


pub fn reset_gpu_clocks(device_index: u32) -> Result<()> {
    let nvml = get_nvml()?;
    let mut device = nvml.device_by_index(device_index)?;
    device.reset_gpu_locked_clocks()?;
    Ok(())
}

pub fn set_gpu_core_offset(device_index: u32, offset: f32) -> Result<()> {
    let nvml = get_nvml()?;
    let mut device = nvml.device_by_index(device_index)?;
    device.set_clock_offset(Clock::Graphics, PerformanceState::Zero, offset.round() as i32)?;
    {
        let mut map = crate::MANUAL_GPU_OFFSETS.lock().unwrap();
        let entry = map.entry(device_index).or_insert((0.0, 0.0));
        entry.0 = offset;
    }
    log::info!(target: "hw.gpu", "set_core_offset gpu={} offset={} offset_rounded={}", device_index, offset, offset.round());
    Ok(())
}

pub fn set_gpu_memory_offset(device_index: u32, offset: f32) -> Result<()> {
    let nvml = get_nvml()?;
    let mut device = nvml.device_by_index(device_index)?;
    device.set_clock_offset(Clock::Memory, PerformanceState::Zero, offset.round() as i32)?;
    {
        let mut map = crate::MANUAL_GPU_OFFSETS.lock().unwrap();
        let entry = map.entry(device_index).or_insert((0.0, 0.0));
        entry.1 = offset;
    }
    log::info!(target: "hw.gpu", "set_mem_offset gpu={} offset={} offset_rounded={}", device_index, offset, offset.round());
    Ok(())
}

pub fn set_gpu_power_limit(device_index: u32, limit_watts: u32) -> Result<()> {
    let nvml = get_nvml()?;
    let mut device = nvml.device_by_index(device_index)?;
    device.set_power_management_limit(limit_watts * 1000)?; // Watts to mW
    log::info!(target: "hw.gpu", "set_power_limit gpu={} limit={}W", device_index, limit_watts);
    Ok(())
}

pub fn set_gpu_fan_speed(device_index: u32, fan_index: u32, speed_percent: u32) -> Result<()> {
    let nvml = get_nvml()?;
    let mut device = nvml.device_by_index(device_index)?;
    device.set_fan_speed(fan_index, speed_percent)?;
    log::info!(target: "hw.gpu", "set_fan_speed gpu={} fan={} speed={}%", device_index, fan_index, speed_percent);
    Ok(())
}

pub fn set_gpu_fan_auto(device_index: u32, fan_index: u32) -> Result<()> {
    let nvml = get_nvml()?;
    let mut device = nvml.device_by_index(device_index)?;
    device.set_default_fan_speed(fan_index)?;
    log::info!(target: "hw.gpu", "set_fan_auto gpu={} fan={}", device_index, fan_index);
    Ok(())
}

fn find_binary(cmd: &str) -> Option<String> {
    let paths = ["/usr/bin", "/usr/sbin", "/usr/local/bin", "/usr/local/sbin", "/sbin", "/bin"];
    for path in paths {
        let full_path = format!("{}/{}", path, cmd);
        if Path::new(&full_path).exists() {
            return Some(full_path);
        }
    }
    None
}

pub fn set_prime_profile(profile: &str) -> Result<()> {
    let valid_profiles = ["on-demand", "nvidia", "intel"];
    if !valid_profiles.contains(&profile) {
        return Err(anyhow!("Invalid prime profile: {}", profile));
    }

    // Check for optimus-manager first (common on Arch)
    if let Some(path) = find_binary("optimus-manager") {
        let opt_mode = match profile {
            "on-demand" => "hybrid",
            "intel" => "integrated",
            "nvidia" => "nvidia",
            _ => profile,
        };

        log::info!(target: "hw.gpu", "set_prime_profile mode=\"{}\" tool=\"optimus-manager\"", opt_mode);
        let output = std::process::Command::new(path)
            .arg("--switch")
            .arg(opt_mode)
            .arg("--no-confirm")
            .output()?;

        if !output.status.success() {
            return Err(anyhow!("optimus-manager command failed: {}", String::from_utf8_lossy(&output.stderr)));
        }
        return Ok(());
    }

    // Fallback to prime-select (Ubuntu/Debian)
    let output = std::process::Command::new("prime-select")
        .arg(profile)
        .output()?;
    if !output.status.success() {
        return Err(anyhow!("prime-select command failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    log::info!(target: "hw.gpu", "set_prime_profile mode=\"{}\" tool=\"prime-select\"", profile);
    Ok(())
}

pub fn set_energy_performance_preference(epp: &str) -> Result<()> {
    let cpu_count = get_cpu_count()?;
    
    let valid_values = ["performance", "balance_performance", "balance_power", "power", 
                       "default", "balance-performance", "balance-power"];
    if !valid_values.contains(&epp) {
        return Err(anyhow!("Invalid EPP value: {}", epp));
    }
    
    for i in 0..cpu_count {
        let path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/energy_performance_preference", i);
        if Path::new(&path).exists() {
            fs::write(&path, epp)
                .map_err(|e| anyhow!("Failed to set EPP for CPU {}: {}", i, e))?;
        }
    }
    
    log::info!(target: "hw.cpu", "set_epp preference=\"{}\"", epp);
    Ok(())
}

#[derive(Debug, Clone)]
pub struct RgbKeyboardControl {
    paths: Vec<String>,
    tuxedo_io: Option<Arc<TuxedoIo>>,
}

impl RgbKeyboardControl {
    pub fn new() -> Result<Self> {
        let paths = Self::find_all_keyboard_backlight_paths();
        let tuxedo_io = TuxedoIo::shared();

        if paths.is_empty() && tuxedo_io.is_none() {
            return Err(anyhow!("No keyboard backlight control available"));
        }

        Ok(Self { paths, tuxedo_io })
    }
    
    
    fn find_all_keyboard_backlight_paths() -> Vec<String> {
        let paths = Vec::new();
        
        // Priority 1: tuxedo_keyboard platform device
        let platform_base = "/sys/devices/platform/tuxedo_keyboard/leds";
        if Path::new(platform_base).exists() {
            // Check for 3-zone
            let zones = ["left", "center", "right"];
            let mut found_zones = Vec::new();
            for zone in zones {
                let path = format!("{}/{}:kbd_backlight", platform_base, zone);
                if Path::new(&path).exists() {
                    found_zones.push(path);
                }
            }

            if !found_zones.is_empty() {
                return found_zones;
            }

            // Check for single zone
            let single = format!("{}/rgb:kbd_backlight", platform_base);
            if Path::new(&single).exists() {
                return vec![single];
            }
        }

        // Priority 2: Standard /sys/class/leds
        if let Ok(entries) = fs::read_dir("/sys/class/leds") {
            let mut kbd_entries: Vec<String> = entries.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.contains("kbd_backlight"))
                .map(|n| format!("/sys/class/leds/{}", n))
                .collect();

            // Sort to have a consistent order (e.g. left, center, right if they are named so)
            kbd_entries.sort();

            if !kbd_entries.is_empty() {
                return kbd_entries;
            }
        }
        
        paths
    }
    
    pub fn set_zone_color(&self, zone_idx: usize, red: u8, green: u8, blue: u8) -> Result<()> {
        if let Some(ref io) = self.tuxedo_io {
            if io.get_interface() == HardwareInterface::Clevo {
                if let Ok(_) = io.set_clevo_keyboard_color(zone_idx as u8, red, green, blue) {
                    // Also try to set via sysfs for consistency, but ignore errors if it fails
                    // as ioctl already succeeded.
                }
            }
        }

        let path = self.paths.get(zone_idx)
            .ok_or_else(|| anyhow!("Invalid zone index: {}", zone_idx))?;

        let color_path = format!("{}/multi_intensity", path);
        if !Path::new(&color_path).exists() {
            return Err(anyhow!("RGB control not available for zone {}", zone_idx));
        }
        
        let color_str = format!("{} {} {}", red, green, blue);
        fs::write(&color_path, color_str)?;
        
        log::info!(target: "hw.kbd", "set_zone_color zone={} r={} g={} b={}", zone_idx, red, green, blue);
        Ok(())
    }
    
    pub fn set_brightness(&self, brightness: u8) -> Result<()> {
        if let Some(ref io) = self.tuxedo_io {
            if io.get_interface() == HardwareInterface::Clevo {
                let _ = io.set_clevo_keyboard_brightness(brightness);
            }
        }

        for path in &self.paths {
            let brightness_path = format!("{}/brightness", path);
            let max_brightness_path = format!("{}/max_brightness", path);

            let max_brightness: u32 = if let Ok(max_str) = fs::read_to_string(&max_brightness_path) {
                max_str.trim().parse().unwrap_or(255)
            } else {
                255
            };

            let actual_brightness = ((brightness as u32) * max_brightness) / 100;
            let _ = fs::write(&brightness_path, actual_brightness.to_string());
        }
        
        log::info!(target: "hw.kbd", "set_brightness level={}%", brightness);
        Ok(())
    }
    
    pub fn set_mode(&self, mode: &lapsphere_common::types::KeyboardMode) -> Result<()> {
        use lapsphere_common::types::KeyboardMode;
        match mode {
            KeyboardMode::SingleColor { r, g, b, brightness } => {
                // For Clevo, explicitly set mode 0 (Custom/Static)
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0x00000000);
                    }
                }

                let num_zones = self.paths.len().max(if self.tuxedo_io.is_some() { 3 } else { 0 });
                for i in 0..num_zones {
                    let _ = self.set_zone_color(i, *r, *g, *b);
                }
                self.set_brightness(*brightness)?;
            }
            KeyboardMode::PerKeyRGB { keys, brightness } => {
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0x00000000);
                    }
                }

                for (i, color) in keys.iter().enumerate() {
                    let _ = self.set_zone_color(i, color.r, color.g, color.b);
                }
                self.set_brightness(*brightness)?;
            }
            KeyboardMode::MultipleZones { zones, brightness } => {
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0x00000000);
                    }
                }

                for (i, zone) in zones.iter().enumerate() {
                    let _ = self.set_zone_color(i, zone.r, zone.g, zone.b);
                }
                self.set_brightness(*brightness)?;
            }
            KeyboardMode::Breathe { r, g, b, brightness, speed } => {
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0x1002a000); // BREATHE
                    }
                }
                self.write_effect_mode(1, "breathing")?;
                self.write_effect_speed(*speed)?;
                for i in 0..self.paths.len() {
                    let _ = self.set_zone_color(i, *r, *g, *b);
                }
                self.set_brightness(*brightness)?;
                log::info!(target: "hw.kbd", "set_mode mode=\"breathing\" speed={}", speed);
            }
            KeyboardMode::Wave { brightness, speed } => {
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0xB0000000); // WAVE
                    }
                }
                self.write_effect_mode(7, "wave")?;
                self.write_effect_speed(*speed)?;
                self.set_brightness(*brightness)?;
                log::info!(target: "hw.kbd", "set_mode mode=\"wave\" speed={}", speed);
            }
            KeyboardMode::Cycle { brightness, speed } => {
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0x33010000); // CYCLE
                    }
                }
                self.write_effect_mode(2, "cycle")?;
                self.write_effect_speed(*speed)?;
                self.set_brightness(*brightness)?;
                log::info!(target: "hw.kbd", "set_mode mode=\"cycle\" speed={}", speed);
            }
            KeyboardMode::Dance { brightness, speed } => {
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0x80000000); // DANCE
                    }
                }
                self.write_effect_mode(3, "dance")?;
                self.write_effect_speed(*speed)?;
                self.set_brightness(*brightness)?;
                log::info!(target: "hw.kbd", "set_mode mode=\"dance\" speed={}", speed);
            }
            KeyboardMode::Flash { r, g, b, brightness, speed } => {
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0xA0000000); // FLASH
                    }
                }
                self.write_effect_mode(4, "flash")?;
                self.write_effect_speed(*speed)?;
                for i in 0..self.paths.len() {
                    let _ = self.set_zone_color(i, *r, *g, *b);
                }
                self.set_brightness(*brightness)?;
                log::info!(target: "hw.kbd", "set_mode mode=\"flash\" speed={}", speed);
            }
            KeyboardMode::RandomColor { brightness, speed } => {
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0x70000000); // RANDOM_COLOR
                    }
                }
                self.write_effect_mode(5, "random")?;
                self.write_effect_speed(*speed)?;
                self.set_brightness(*brightness)?;
                log::info!(target: "hw.kbd", "set_mode mode=\"random\" speed={}", speed);
            }
            KeyboardMode::Tempo { brightness, speed } => {
                if let Some(ref io) = self.tuxedo_io {
                    if io.get_interface() == HardwareInterface::Clevo {
                        let _ = io.set_clevo_keyboard_mode(0x90000000); // TEMPO
                    }
                }
                self.write_effect_mode(6, "tempo")?;
                self.write_effect_speed(*speed)?;
                self.set_brightness(*brightness)?;
                log::info!(target: "hw.kbd", "set_mode mode=\"tempo\" speed={}", speed);
            }
        }
        Ok(())
    }

    fn write_effect_mode(&self, mode_value: u8, fallback: &str) -> Result<()> {
        for path in &self.paths {
            let mode_path = format!("{}/mode", path);
            if Path::new(&mode_path).exists() {
                let value = mode_value.to_string();
                if fs::write(&mode_path, &value).is_err() {
                    let _ = fs::write(&mode_path, fallback);
                }
            }
        }
        Ok(())
    }

    fn write_effect_speed(&self, speed: u8) -> Result<()> {
        for path in &self.paths {
            let speed_path = format!("{}/speed", path);
            if Path::new(&speed_path).exists() {
                let _ = fs::write(&speed_path, speed.to_string());
            }
        }
        Ok(())
    }
}
