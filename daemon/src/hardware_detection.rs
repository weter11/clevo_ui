use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::Path;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;
use std::mem;
use once_cell::sync::Lazy;
use crate::tuxedo_io::{TuxedoIo, HardwareInterface};
use systemstat::{System, Platform};
// use tuxedo_io::TuxedoIo;
use lapsphere_common::types::*;
use nix::ioctl_readwrite;
use std::os::fd::{AsRawFd, RawFd};

// Thread-safe storage for previous CPU stats
static PREVIOUS_CPU_STATS: Mutex<Option<HashMap<u32, CpuStats>>> = Mutex::new(None);
static PREVIOUS_NET_STATS: Lazy<Mutex<HashMap<String, NetStats>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static PREVIOUS_STORAGE_STATS: Lazy<Mutex<HashMap<String, StorageStats>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

static NVIDIA_NAMES_CACHE: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));

#[derive(Clone)]
struct NvidiaMetadata {
    architecture: Option<String>,
    supported_p_states: Vec<String>,
    power_limit_range: Option<(u32, u32)>,
    supports_gpu_offset: bool,
    supports_mem_offset: bool,
    vram_type: Option<String>,
    vram_vendor: Option<String>,
    vram_bus_width: Option<u32>,
    vram_total: Option<u64>,
    core_clock_range: Option<(u32, u32)>,
    memory_clock_range: Option<(u32, u32)>,
    core_offset_limits: Option<(i32, i32)>,
    memory_offset_limits: Option<(i32, i32)>,
}

static NVIDIA_METADATA_CACHE: Lazy<Mutex<HashMap<u32, NvidiaMetadata>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// Hybrid metric-fetching (RTD3-aware idle tier)
//
// Any NVML call on an RTD3-capable dGPU resets the kernel's
// autosuspend_delay_ms (20 s) timer, even at 0% utilization. Polling NVML at
// 1 Hz therefore pins an active-but-idle GPU awake forever. Strategy:
//   - suspended          -> sysfs-only stub, zero NVML calls (existing behavior)
//   - active, util > 0   -> full NVML query; results snapshotted into IDLE_CACHE
//   - active, util == 0  -> serve last-known values from IDLE_CACHE, ZERO NVML
//                           calls, so the autosuspend timer can expire and
//                           the GPU drops to runtime_status "suspended"
// The cache goes stale after IDLE_CACHE_TTL_SECS; stale entries are served as
// None so the GUI shows real staleness instead of frozen values.
// GetGpuInfoFull() sets FULL_NVML_REFRESH_REQUESTED for a one-shot bypass
// (explicit user demand beats power saving).
// ---------------------------------------------------------------------------

const IDLE_CACHE_TTL_SECS: u64 = 30;

#[derive(Clone)]
struct GpuIdleSnapshot {
    frequency: Option<u64>,
    memory_frequency: Option<u64>,
    temperature: Option<f32>,
    load: Option<f32>,
    power: Option<f32>,
    voltage: Option<f32>, // NVAPI voltage captured alongside the full query
}

struct IdleCacheEntry {
    snapshot: Option<GpuIdleSnapshot>,
    captured_at: Instant,
}

/// Per-GPU last-known metrics from the most recent full (util > 0) NVML pass.
static IDLE_METRICS_CACHE: Lazy<Mutex<HashMap<u32, IdleCacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Freshness probe without touching the GPU (safe to call from any path).
#[allow(dead_code)]
fn idle_cache_fresh_for(index: u32) -> bool {
    let cache = IDLE_METRICS_CACHE.lock().unwrap();
    cache
        .get(&index)
        .map(|e| e.captured_at.elapsed().as_secs() < IDLE_CACHE_TTL_SECS)
        .unwrap_or(false)
}

static PREVIOUS_RAPL_STATS: Mutex<Option<(f64, Instant)>> = Mutex::new(None);

const BITS_PER_BYTE: f64 = 8.0;
const BITS_PER_MEGABIT: f64 = 1_000_000.0;

#[derive(Debug, Clone)]
struct CpuStats {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
}

#[derive(Debug, Clone)]
struct NetStats {
    rx_bytes: u64,
    tx_bytes: u64,
    timestamp: Instant,
}

#[derive(Debug, Clone)]
struct StorageStats {
    read_ios: u64,
    read_sectors: u64,
    write_ios: u64,
    write_sectors: u64,
    timestamp: Instant,
}

impl CpuStats {
    fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq
    }
    
    fn work(&self) -> u64 {
        self.user + self.nice + self.system + self.irq + self.softirq
    }
}

fn read_cpu_stats() -> Result<HashMap<u32, CpuStats>> {
    let stat = fs::read_to_string("/proc/stat")?;
    let mut stats = HashMap::new();
    
    for line in stat.lines() {
        if line.starts_with("cpu") && !line.starts_with("cpu ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 8 {
                continue;
            }
            
            let cpu_id: u32 = parts[0].trim_start_matches("cpu").parse()?;
            let user: u64 = parts[1].parse()?;
            let nice: u64 = parts[2].parse()?;
            let system: u64 = parts[3].parse()?;
            let idle: u64 = parts[4].parse()?;
            let iowait: u64 = parts[5].parse()?;
            let irq: u64 = parts[6].parse()?;
            let softirq: u64 = parts[7].parse()?;
            
            stats.insert(cpu_id, CpuStats {
                user, nice, system, idle, iowait, irq, softirq,
            });
        }
    }
    
    Ok(stats)
}

fn calculate_cpu_load() -> Result<HashMap<u32, f32>> {
    let current_stats = read_cpu_stats()?;
    
    // Get previous stats from thread-safe storage
    let mut prev_stats_lock = PREVIOUS_CPU_STATS.lock().unwrap();
    
    let loads = if let Some(ref prev_stats) = *prev_stats_lock {
        // Calculate load based on delta from previous call
        let mut loads = HashMap::new();
        
        for (cpu_id, current) in current_stats.iter() {
            if let Some(prev) = prev_stats.get(cpu_id) {
                let total_diff = current.total().saturating_sub(prev.total());
                let work_diff = current.work().saturating_sub(prev.work());
                
                let load = if total_diff > 0 {
                    (work_diff as f32 / total_diff as f32) * 100.0
                } else {
                    0.0
                };
                
                loads.insert(*cpu_id, load);
            } else {
                // New CPU appeared, assume 0% load
                loads.insert(*cpu_id, 0.0);
            }
        }
        
        loads
    } else {
        // First call - no previous stats available, return 0% for all CPUs
        current_stats.keys().map(|&id| (id, 0.0)).collect()
    };
    
    // Store current stats for next call
    *prev_stats_lock = Some(current_stats);
    
    Ok(loads)
}

// Scheduler detection
fn get_scheduler_info() -> (String, Vec<String>) {
    let scheduler = fs::read_to_string("/sys/kernel/debug/sched/features")
        .or_else(|_| fs::read_to_string("/proc/sys/kernel/sched_features"))
        .ok()
        .and_then(|content| {
            if content.contains("EEVDF") {
                Some("EEVDF".to_string())
            } else {
                Some("CFS".to_string())
            }
        })
        .unwrap_or_else(|| "CFS".to_string());
    
    let available = vec!["CFS".to_string(), "EEVDF".to_string()];
    (scheduler, available)
}

fn get_cpu_name() -> String {
    static CACHED_NAME: Lazy<String> = Lazy::new(|| {
        if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
            for line in cpuinfo.lines() {
                if line.starts_with("model name") {
                    if let Some(name) = line.split(':').nth(1) {
                        return name.trim().to_string();
                    }
                }
            }
        }
        "Unknown CPU".to_string()
    });
    CACHED_NAME.clone()
}


fn get_cpu_topology() -> (u32, u32) {
    static CACHED_TOPOLOGY: Lazy<(u32, u32)> = Lazy::new(|| {
        let mut logical = 0;
        let mut physical = 0;

        let output = std::process::Command::new("lscpu").output();
        if let Ok(out) = output {
            let s = String::from_utf8_lossy(&out.stdout);
            let mut _threads_per_core = 1;
            let mut cores_per_socket = 0;
            let mut sockets = 0;

            for line in s.lines() {
                let line = line.trim();
                if line.starts_with("CPU(s):") {
                    logical = line.split(':').nth(1).unwrap_or("").trim().parse().unwrap_or(0);
                } else if line.starts_with("Thread(s) per core:") {
                    _threads_per_core = line.split(':').nth(1).unwrap_or("").trim().parse().unwrap_or(1);
                } else if line.starts_with("Core(s) per socket:") {
                    cores_per_socket = line.split(':').nth(1).unwrap_or("").trim().parse().unwrap_or(0);
                } else if line.starts_with("Socket(s):") {
                    sockets = line.split(':').nth(1).unwrap_or("").trim().parse().unwrap_or(1);
                }
            }

            physical = cores_per_socket * sockets;
        }

        // Fallback if lscpu fails or gives incomplete info
        if logical == 0 {
            logical = fs::read_to_string("/proc/cpuinfo")
                .map(|s| s.lines().filter(|l| l.starts_with("processor")).count() as u32)
                .unwrap_or(1);
        }
        if physical == 0 {
            physical = logical;
        }

        (physical, logical)
    });
    *CACHED_TOPOLOGY
}


fn read_cpu_frequencies(logical_cores: u32) -> Vec<u64> {
    let mut freqs = Vec::with_capacity(logical_cores as usize);
    
    // Try sysfs first as it is core-specific and efficient
    for i in 0..logical_cores {
        let mut found = false;
        for filename in &["scaling_cur_freq", "cpuinfo_cur_freq"] {
            let path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/{}", i, filename);
            if let Ok(s) = fs::read_to_string(&path) {
                if let Ok(freq) = s.trim().parse::<u64>() {
                    freqs.push(freq);
                    found = true;
                    break;
                }
            }
        }
        if found { continue; }
        freqs.push(0); // Placeholder
    }

    // If any are still 0, try parsing /proc/cpuinfo once
    if freqs.iter().any(|&f| f == 0) {
        if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
            let mut core_idx = 0;
            for line in cpuinfo.lines() {
                if line.starts_with("cpu MHz") {
                    if let Some(mhz_str) = line.split(':').nth(1) {
                        if let Ok(mhz) = mhz_str.trim().parse::<f64>() {
                            if core_idx < freqs.len() && freqs[core_idx] == 0 {
                                freqs[core_idx] = (mhz * 1000.0) as u64;
                            }
                            core_idx += 1;
                        }
                    }
                }
            }
        }
    }

    // Fill remaining with a default value
    for f in freqs.iter_mut() {
        if *f == 0 { *f = 2000000; }
    }
    
    freqs
}

fn get_core_temp(cpu: u32) -> f32 {
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let name_path = entry.path().join("name");
            if let Ok(name) = fs::read_to_string(&name_path) {
                let name = name.trim();
                if name == "k10temp" {
                    return get_package_temp().unwrap_or(0.0);
                } else if name == "coretemp" {
                    let temp_path = entry.path().join(format!("temp{}_input", cpu + 2));
                    if let Ok(temp_str) = fs::read_to_string(&temp_path) {
                        if let Ok(temp) = temp_str.trim().parse::<f32>() {
                            return temp / 1000.0;
                        }
                    }
                }
            }
        }
    }
    0.0
}

fn get_package_temp() -> Result<f32> {
    for entry in fs::read_dir("/sys/class/hwmon")? {
        let entry = entry?;
        let name_path = entry.path().join("name");
        if let Ok(name) = fs::read_to_string(&name_path) {
            let name = name.trim();
            if name == "k10temp" {
                let temp_path = entry.path().join("temp1_input");
                if let Ok(temp_str) = fs::read_to_string(&temp_path) {
                    if let Ok(temp) = temp_str.trim().parse::<f32>() {
                        return Ok(temp / 1000.0);
                    }
                }
            } else if name == "coretemp" {
                let temp_path = entry.path().join("temp1_input");
                if let Ok(temp_str) = fs::read_to_string(&temp_path) {
                    if let Ok(temp) = temp_str.trim().parse::<f32>() {
                        return Ok(temp / 1000.0);
                    }
                }
            } else if name == "zenpower" {
                let temp_path = entry.path().join("temp1_input");
                if let Ok(temp_str) = fs::read_to_string(&temp_path) {
                    if let Ok(temp) = temp_str.trim().parse::<f32>() {
                        return Ok(temp / 1000.0);
                    }
                }
            }
        }
    }
    Err(anyhow!("Package temperature not found"))
}

fn read_hwmon_power(hwmon_path: &Path) -> Result<f32> {
    let power_input_path = hwmon_path.join("power1_input");
    if let Ok(power_str) = fs::read_to_string(&power_input_path) {
        if let Ok(microwatts) = power_str.trim().parse::<f32>() {
            return Ok(microwatts / 1_000_000.0);
        }
    }
    
    let power_avg_path = hwmon_path.join("power1_average");
    if let Ok(power_str) = fs::read_to_string(&power_avg_path) {
        if let Ok(microwatts) = power_str.trim().parse::<f32>() {
            return Ok(microwatts / 1_000_000.0);
        }
    }
    
    Err(anyhow!("No power reading available"))
}

fn try_rapl() -> Result<f32> {
    for entry in fs::read_dir("/sys/class/powercap")? {
        let entry = entry?;
        let path = entry.path();
        
        if let Ok(name) = fs::read_to_string(path.join("name")) {
            if name.trim() == "package-0" {
                if let Ok(energy_str) = fs::read_to_string(path.join("energy_uj")) {
                    if let Ok(energy) = energy_str.trim().parse::<f64>() {
                        let now = Instant::now();
                        let mut prev_lock = PREVIOUS_RAPL_STATS.lock().unwrap();

                        let power = if let Some((prev_energy, prev_time)) = *prev_lock {
                            let elapsed = now.duration_since(prev_time).as_secs_f64();
                            if elapsed > 0.01 { // Only update if enough time has passed
                                let diff = energy - prev_energy;
                                if diff >= 0.0 {
                                    // energy is in microjoules.
                                    // Power (Watts) = Joules / Seconds
                                    (diff / 1_000_000.0 / elapsed) as f32
                                } else {
                                    0.0 // Counter reset?
                                }
                            } else {
                                return Err(anyhow!("RAPL: Too soon for update"));
                            }
                        } else {
                            0.0
                        };

                        *prev_lock = Some((energy, now));
                        return Ok(power);
                    }
                }
            }
        }
    }
    Err(anyhow!("RAPL not available"))
}

fn is_amd_cpu() -> bool {
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
        for line in cpuinfo.lines() {
            if line.starts_with("vendor_id") {
                return line.contains("AuthenticAMD");
            }
        }
    }
    false
}

fn get_amd_dgpu_count() -> u32 {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name_str = name.to_string_lossy();
                if name_str.starts_with("card") && !name_str.contains("-") {
                    let device_path = path.join("device/vendor");
                    if let Ok(vendor) = fs::read_to_string(&device_path) {
                        if vendor.trim() == "0x1002" {
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    if count > 1 { count - 1 } else { 0 }
}

fn get_all_power_sources() -> Vec<PowerSource> {
    let mut sources = Vec::new();
    
    if let Ok(power) = try_rapl() {
        sources.push(PowerSource {
            name: "RAPL".to_string(),
            value: power,
            description: "Intel/AMD RAPL (Running Average Power Limit)".to_string(),
        });
    }
    
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let name_path = entry.path().join("name");
            if let Ok(name) = fs::read_to_string(&name_path) {
                let name = name.trim();
                
                match name {
                    "amdgpu" => {
                        let power_input = entry.path().join("power1_input");
                        let power_avg = entry.path().join("power1_average");
                        
                        if power_input.exists() || power_avg.exists() {
                            if let Ok(power) = read_hwmon_power(&entry.path()) {
                                sources.push(PowerSource {
                                    name: "amdgpu".to_string(),
                                    value: power,
                                    description: "AMD APU Total Power (CPU+iGPU)".to_string(),
                                });
                            }
                        }
                    },
                    "zenpower" => {
                        if let Ok(power) = read_hwmon_power(&entry.path()) {
                            sources.push(PowerSource {
                                name: "zenpower".to_string(),
                                value: power,
                                description: "Zenpower Driver (AMD Ryzen)".to_string(),
                            });
                        }
                    },
                    "amd_energy" => {
                        if let Ok(power) = read_hwmon_power(&entry.path()) {
                            sources.push(PowerSource {
                                name: "amd_energy".to_string(),
                                value: power,
                                description: "AMD Energy Driver".to_string(),
                            });
                        }
                    },
                    _ => {}
                }
            }
        }
    }
    
    sources
}

fn get_cpu_power() -> Option<f32> {
    let all_sources = get_all_power_sources();
    
    if is_amd_cpu() && get_amd_dgpu_count() == 0 {
        if let Some(amdgpu) = all_sources.iter().find(|s| s.name == "amdgpu") {
            return Some(amdgpu.value);
        }
    }
    
    if is_amd_cpu() {
        if let Some(zenpower) = all_sources.iter().find(|s| s.name == "zenpower") {
            return Some(zenpower.value);
        }
        
        if let Some(amd_energy) = all_sources.iter().find(|s| s.name == "amd_energy") {
            return Some(amd_energy.value);
        }
    }
    
    if let Some(rapl) = all_sources.iter().find(|s| s.name == "RAPL") {
        return Some(rapl.value);
    }
    
    None
}

fn detect_cpu_capabilities() -> CpuCapabilities {
    let base_path = "/sys/devices/system/cpu/cpu0/cpufreq";
    
    CpuCapabilities {
        has_boost: Path::new("/sys/devices/system/cpu/cpufreq/boost").exists() ||
                   Path::new("/sys/devices/system/cpu/intel_pstate/no_turbo").exists(),
        
        has_cpuinfo_max_freq: Path::new(&format!("{}/cpuinfo_max_freq", base_path)).exists(),
        
        has_cpuinfo_min_freq: Path::new(&format!("{}/cpuinfo_min_freq", base_path)).exists(),
        
        has_scaling_driver: Path::new(&format!("{}/scaling_driver", base_path)).exists() ||
                           Path::new("/sys/devices/system/cpu/cpufreq/policy0/scaling_driver").exists(),
        
        has_energy_performance_preference: 
            Path::new(&format!("{}/energy_performance_preference", base_path)).exists(),
        
        has_scaling_governor: Path::new(&format!("{}/scaling_governor", base_path)).exists(),
        
        has_smt: Path::new("/sys/devices/system/cpu/smt/control").exists(),
        
        has_scaling_min_freq: Path::new(&format!("{}/scaling_min_freq", base_path)).exists(),
        
        has_scaling_max_freq: Path::new(&format!("{}/scaling_max_freq", base_path)).exists(),
        
        has_available_governors: 
            Path::new(&format!("{}/scaling_available_governors", base_path)).exists(),
        
        has_amd_pstate: Path::new("/sys/devices/system/cpu/amd_pstate/status").exists(),
        has_intel_pstate: Path::new("/sys/devices/system/cpu/intel_pstate/status").exists(),
    }
}

fn read_governor() -> Result<String> {
    let path = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor";
    
    if !Path::new(path).exists() {
        return Ok("not_available".to_string());
    }
    
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| anyhow!("Failed to read governor: {}", e))
}

fn read_available_governors() -> Result<Vec<String>> {
    let path = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_available_governors";
    
    if !Path::new(path).exists() {
        return Ok(vec![]);
    }
    
    let governors = fs::read_to_string(path)?;
    Ok(governors.split_whitespace().map(String::from).collect())
}

fn is_boost_enabled() -> Result<bool> {
    if let Ok(boost) = fs::read_to_string("/sys/devices/system/cpu/cpufreq/boost") {
        return Ok(boost.trim() == "1");
    }
    
    if let Ok(no_turbo) = fs::read_to_string("/sys/devices/system/cpu/intel_pstate/no_turbo") {
        return Ok(no_turbo.trim() == "0");
    }
    
    Ok(false)
}

fn is_smt_enabled() -> Result<bool> {
    let path = "/sys/devices/system/cpu/smt/control";
    
    if !Path::new(path).exists() {
        return Ok(true);
    }
    
    let status = fs::read_to_string(path)?;
    Ok(status.trim() == "on")
}

fn read_scaling_driver() -> Result<String> {
    let path = "/sys/devices/system/cpu/cpufreq/policy0/scaling_driver";
    
    if !Path::new(path).exists() {
        return Ok("unknown".to_string());
    }
    
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| anyhow!("Failed to read scaling driver: {}", e))
}

fn read_amd_pstate_status() -> Result<String> {
    let path = "/sys/devices/system/cpu/amd_pstate/status";
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| anyhow!("Failed to read AMD pstate status: {}", e))
}

fn read_intel_pstate_status() -> Result<String> {
    let path = "/sys/devices/system/cpu/intel_pstate/status";
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| anyhow!("Failed to read Intel pstate status: {}", e))
}

fn read_frequency_limits() -> (Option<u64>, Option<u64>) {
    let min_freq = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_min_freq")
        .ok()
        .and_then(|s| s.trim().parse().ok());
    
    let max_freq = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq")
        .ok()
        .and_then(|s| s.trim().parse().ok());
    
    (min_freq, max_freq)
}

pub fn read_hw_frequency_limits() -> Result<(Option<u64>, Option<u64>)> {
    let min_path = "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_min_freq";
    let max_path = "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq";

    let min_freq = fs::read_to_string(min_path).ok().and_then(|s| s.trim().parse().ok());
    let max_freq = fs::read_to_string(max_path).ok().and_then(|s| s.trim().parse().ok());

    Ok((min_freq, max_freq))
}

fn read_energy_performance_preference() -> Option<String> {
    let path = "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference";
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_available_epp_options() -> Vec<String> {
    let path = "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_available_preferences";
    
    if let Ok(content) = fs::read_to_string(path) {
        content.split_whitespace().map(String::from).collect()
    } else {
        vec![
            "performance".to_string(),
            "balance_performance".to_string(),
            "balance_power".to_string(),
            "power".to_string(),
        ]
    }
}

pub fn get_tdp_profiles() -> Result<Vec<String>> {
    if !TuxedoIo::is_available() {
        log::debug!(target: "hw.detect", "TDP profiles not available (/dev/tuxedo_io not present)");
        return Ok(vec![]);
    }
    
    match TuxedoIo::shared() {
        Some(io) => {
            match io.get_available_profiles() {
                Ok(profiles) => {
                    static LOGGED_ONCE: Mutex<bool> = Mutex::new(false);
                    let mut logged = LOGGED_ONCE.lock().unwrap();
                    if !*logged {
                        log::debug!(target: "hw.detect", "Available TDP profiles: {:?}", profiles);
                        *logged = true;
                    }
                    Ok(profiles)
                }
                Err(e) => {
                    log::warn!(target: "hw.detect", "Failed to get TDP profiles: {}", e);
                    Ok(vec![])
                }
            }
        }
        None => {
            log::warn!(target: "hw.detect", "Failed to open /dev/tuxedo_io");
            Ok(vec![])
        }
    }
}

/// Internal helper to get base memory clock ranges across all performance states
fn get_base_memory_clock_ranges(device: &nvml_wrapper::Device) -> Result<(u32, u32)> {
    let mut range = None;
    if let Ok(supported_states) = device.supported_performance_states() {
        let mut m_min = u32::MAX;
        let mut m_max = 0;
        for pstate in supported_states {
            if let Ok((p_min, p_max)) = device.min_max_clock_of_pstate(Clock::Memory, pstate) {
                if p_min < m_min { m_min = p_min; }
                if p_max > m_max { m_max = p_max; }
            }
        }
        if m_min != u32::MAX {
            range = Some((m_min, m_max));
        }
    }

    if range.is_none() {
        if let Ok(clocks) = device.supported_memory_clocks() {
            if let (Some(&c_min), Some(&c_max)) = (clocks.iter().min(), clocks.iter().max()) {
                range = Some((c_min, c_max));
            }
        }
    }

    range.ok_or_else(|| anyhow!("Could not determine memory clock ranges"))
}

pub fn get_current_tdp_profile() -> Result<String> {
    if !TuxedoIo::is_available() {
        return Err(anyhow!("TDP profiles not available"));
    }
    
    let io = TuxedoIo::shared().ok_or_else(|| anyhow!("tuxedo_io not available"))?;
    let profiles = get_tdp_profiles()?;
    if profiles.is_empty() {
        return Err(anyhow!("No TDP profiles available"));
    }

    if io.get_interface() == HardwareInterface::Uniwill {
        if let Ok(profile_id) = io.get_uw_performance_profile() {
            // profile_id: 1=powersave, 2=enthusiast, 3=overboost
            let idx = (profile_id.saturating_sub(1)) as usize;
            if idx < profiles.len() {
                return Ok(profiles[idx].clone());
            }
        }
    }
    
    // Fallback to the first profile
    Ok(profiles[0].clone())
}


pub fn get_all_fan_info() -> Result<Vec<FanInfo>> {
    let mut all_fans = Vec::new();

    // 1. Get system fans (Tuxedo/Uniwill/Clevo)
    if TuxedoIo::is_available() {
        if let Some(io) = TuxedoIo::shared() {
            let fan_settings = crate::FAN_DAEMON_STATE.lock().unwrap();
            let manual_mode = fan_settings.as_ref().map_or(false, |s| s.control_enabled);

            for fan_id in 0..io.get_fan_count() {
                if let Ok(speed) = io.get_fan_speed(fan_id) {
                    let temperature = io.get_fan_temperature(fan_id).ok().map(|t| t as f32);
                    all_fans.push(FanInfo {
                        id: fan_id,
                        name: format!("System Fan {}", fan_id),
                        rpm_or_percent: speed,
                        temperature,
                        is_rpm: false,
                        mode: Some(if manual_mode { "Manual".to_string() } else { "Auto".to_string() }),
                    });
                }
            }
        }
    }

    // 2. Get NVIDIA GPU fans
    let gpu_settings = crate::GPU_DAEMON_STATE.lock().unwrap();

    // Check suspension status first to avoid waking up GPU
    let mut nvidia_active = false;
    if let Ok(entries) = fs::read_dir("/sys/bus/pci/drivers/nvidia") {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().contains(':') {
                let status_path = entry.path().join("power/runtime_status");
                if let Ok(status) = fs::read_to_string(status_path) {
                    if status.trim() != "suspended" {
                        nvidia_active = true;
                        break;
                    }
                }
            }
        }
    }

    // RTD3 idle-tier: skip NVML entirely when the GPU is active-but-idle
    // (fresh idle snapshot present). NVML calls here would reset the kernel's
    // 20 s autosuspend timer and pin the dGPU awake. The GUI falls back to the
    // last-known values from get_gpu_info()'s idle cache.
    if nvidia_active && !idle_cache_fresh_for(0) {
        if let Ok(nvml) = get_nvml() {
            if let Ok(device_count) = nvml.device_count() {
                for i in 0..device_count {
                    // Check specific GPU status again
                    let mut is_suspended = true;
                    if let Ok(pci_info) = nvml.device_by_index(i).and_then(|d| d.pci_info()) {
                        let bus_id = pci_info.bus_id.to_lowercase();
                        let status_path = format!("/sys/bus/pci/devices/{}/power/runtime_status", bus_id);
                        if let Ok(status) = fs::read_to_string(status_path) {
                            if status.trim() != "suspended" {
                                is_suspended = false;
                            }
                        }
                    }

                    if is_suspended { continue; }

                    if let Ok(device) = nvml.device_by_index(i) {
                        if let Ok(num_fans) = device.num_fans() {
                            for f in 0..num_fans {
                                let speed = device.fan_speed(f).unwrap_or(0);

                                let mode = if let Some(settings) = &*gpu_settings {
                                    if settings.nvidia_fans.iter().any(|s| s.device_index == i && s.fan_id == f && s.manual) {
                                        "Manual".to_string()
                                    } else {
                                        "Auto".to_string()
                                    }
                                } else {
                                    "Auto".to_string()
                                };

                                all_fans.push(FanInfo {
                                    id: 100 + i * 10 + f, // Unique ID for GPU fans
                                    name: format!("NVIDIA GPU {} Fan {}", i, f),
                                    rpm_or_percent: speed,
                                    temperature: device.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu).ok().map(|t| t as f32),
                                    is_rpm: false,
                                    mode: Some(mode),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(all_fans)
}


pub fn get_cpu_info() -> Result<CpuInfo> {
    let name = get_cpu_name();
    let (physical_cores, logical_cores) = get_cpu_topology();
    
    let loads = calculate_cpu_load().unwrap_or_default();
    
    let frequencies = read_cpu_frequencies(logical_cores);
    let mut cores = Vec::new();
    
    for i in 0..logical_cores {
        let freq = frequencies[i as usize];
        cores.push(CoreInfo {
            id: i,
            frequency: freq,
            load: loads.get(&i).copied().unwrap_or(0.0),
            temperature: get_core_temp(i),
        });
    }
    
    let average_frequency = if !frequencies.is_empty() {
        frequencies.iter().sum::<u64>() / frequencies.len() as u64
    } else {
        0
    };
    
    let loads_vec: Vec<f32> = loads.values().copied().collect();
    let average_load = if !loads_vec.is_empty() {
        loads_vec.iter().sum::<f32>() / loads_vec.len() as f32
    } else {
        0.0
    };
    
    let package_temp = get_package_temp().unwrap_or(0.0);
    let package_power = get_cpu_power();
    
    let capabilities = detect_cpu_capabilities();
    
    let governor = if capabilities.has_scaling_governor {
        read_governor().unwrap_or_else(|_| "unknown".to_string())
    } else {
        "not_available".to_string()
    };
    
    let available_governors = if capabilities.has_available_governors {
        read_available_governors().unwrap_or_else(|_| vec![])
    } else {
        vec![]
    };
    
    let boost_enabled = if capabilities.has_boost {
        is_boost_enabled().unwrap_or(false)
    } else {
        false
    };
    
    let smt_enabled = if capabilities.has_smt {
        is_smt_enabled().unwrap_or(true)
    } else {
        true
    };
    
    let scaling_driver = if capabilities.has_scaling_driver {
        read_scaling_driver().unwrap_or_else(|_| "unknown".to_string())
    } else {
        "not_available".to_string()
    };
    
    let amd_pstate_status = if capabilities.has_amd_pstate {
        read_amd_pstate_status().ok()
    } else {
        None
    };

    let intel_pstate_status = if capabilities.has_intel_pstate {
        read_intel_pstate_status().ok()
    } else {
        None
    };
    
    let (min_freq, max_freq) = if capabilities.has_scaling_min_freq && capabilities.has_scaling_max_freq {
        read_frequency_limits()
    } else {
        (None, None)
    };
    
    let (hw_min_freq, hw_max_freq) = if capabilities.has_cpuinfo_min_freq && capabilities.has_cpuinfo_max_freq {
        read_hw_frequency_limits().unwrap_or((None, None))
    } else {
        (None, None)
    };
    
    let energy_performance_preference = if capabilities.has_energy_performance_preference {
        read_energy_performance_preference()
    } else {
        None
    };
    
    let available_epp_options = if capabilities.has_energy_performance_preference {
        read_available_epp_options()
    } else {
        vec![]
    };

    let all_power_sources = get_all_power_sources();
    
    let power_source = all_power_sources.iter()
        .find(|s| s.name == "amdgpu")
        .or_else(|| all_power_sources.iter().find(|s| s.name == "RAPL"))
        .cloned()
        .map(|s| s.name);

    let (scheduler, available_schedulers) = get_scheduler_info();

    let mut tdp0 = None;
    let mut tdp1 = None;
    let mut tdp2 = None;
    let mut tdp0_range = None;
    let mut tdp1_range = None;
    let mut tdp2_range = None;

    if let Ok(io) = TuxedoIo::new() {
        if io.get_interface() == HardwareInterface::Uniwill {
            tdp0 = io.get_tdp(0).ok();
            tdp1 = io.get_tdp(1).ok();
            tdp2 = io.get_tdp(2).ok();

            if let (Ok(min), Ok(max)) = (io.get_tdp_min(0), io.get_tdp_max(0)) {
                tdp0_range = Some((min, max));
            }
            if let (Ok(min), Ok(max)) = (io.get_tdp_min(1), io.get_tdp_max(1)) {
                tdp1_range = Some((min, max));
            }
            if let (Ok(min), Ok(max)) = (io.get_tdp_min(2), io.get_tdp_max(2)) {
                tdp2_range = Some((min, max));
            }
        }
    }

    Ok(CpuInfo {
        name,
        average_frequency,
        average_load,
        package_temp,
        package_power,
        cores,
        physical_cores,
        logical_cores,
        governor,
        available_governors,
        boost_enabled,
        smt_enabled,
        scaling_driver,
        amd_pstate_status,
        intel_pstate_status,
        min_freq,
        max_freq,
        hw_min_freq,
        hw_max_freq,
        all_power_sources,
        power_source,
        energy_performance_preference,
        available_epp_options,
        tdp0,
        tdp1,
        tdp2,
        tdp0_range,
        tdp1_range,
        tdp2_range,
        capabilities,
        scheduler,
        available_schedulers,
    })
}

fn get_memory_type_and_freq() -> (Option<String>, Option<u64>) {
    static CACHED_MEM_METADATA: Lazy<(Option<String>, Option<u64>)> = Lazy::new(|| {
        let dmidecode_path = find_binary("dmidecode").unwrap_or_else(|| "dmidecode".to_string());
        let output = std::process::Command::new(dmidecode_path)
            .args(["-t", "memory"])
            .output();

        let mut mem_type = None;
        let mut mem_speed = None;

        if let Ok(out) = output {
            let s = String::from_utf8_lossy(&out.stdout);
            for line in s.lines() {
                let line = line.trim();
                if line.contains("Type: DDR") || line.contains("Type: LPDDR") {
                    mem_type = Some(line.split(':').nth(1).unwrap_or("").trim().to_string());
                }
                if (line.starts_with("Speed:")
                    || line.starts_with("Configured Memory Speed:")
                    || line.starts_with("Configured Clock Speed:"))
                    && mem_speed.is_none()
                {
                    let speed_str = line.split(':').nth(1).unwrap_or("").trim();
                    if !speed_str.to_lowercase().contains("unknown") && !speed_str.is_empty() {
                        // Expecting something like "3200 MT/s" or "3200 MHz"
                        if let Some(speed_val) = speed_str.split_whitespace().next() {
                            if let Ok(val) = speed_val.parse::<u64>() {
                                mem_speed = Some(val);
                            }
                        }
                    }
                }
            }
        }
        (mem_type, mem_speed)
    });
    CACHED_MEM_METADATA.clone()
}


pub fn get_memory_info() -> Result<MemoryInfo> {
    let sys = System::new();
    let (total_gib, free_gib, available_gib, used_gib, used_percent) = match sys.memory() {
        Ok(mem) => {
            let total = mem.total.as_u64() as f64 / (1024.0 * 1024.0 * 1024.0);
            let free = mem.free.as_u64() as f64 / (1024.0 * 1024.0 * 1024.0);
            let available = mem.platform_memory.meminfo.get("MemAvailable")
                .map_or(free, |v| v.as_u64() as f64 / (1024.0 * 1024.0 * 1024.0));
            let used = total - available;
            let percent = if total > 0.0 { (used / total * 100.0) as f32 } else { 0.0 };
            (total, free, available, used, percent)
        }
        Err(_) => (0.0, 0.0, 0.0, 0.0, 0.0),
    };

    let (memory_type, memory_frequency) = get_memory_type_and_freq();

    Ok(MemoryInfo {
        total_gib,
        used_gib,
        free_gib,
        available_gib,
        used_percent,
        memory_type,
        memory_frequency,
    })
}

fn get_tuxedo_kernel_modules() -> String {
    if let Ok(modules) = fs::read_to_string("/proc/modules") {
        let module_names: Vec<String> = modules
            .lines()
            .filter(|line| line.contains("tuxedo"))
            .map(|line| line.split_whitespace().next().unwrap_or("").to_string())
            .collect();
        if module_names.is_empty() {
            "Not available".to_string()
        } else {
            module_names.join(", ")
        }
    } else {
        "Not available".to_string()
    }
}

fn has_ite_keyboard() -> bool {
    if let Ok(entries) = fs::read_dir("/sys/bus/hid/devices") {
        for entry in entries.flatten() {
            if let Ok(uevent) = fs::read_to_string(entry.path().join("uevent")) {
                if uevent.contains("HID_ID=0003:0000048D:") {
                    return true;
                }
            }
        }
    }
    false
}

pub fn get_keyboard_capabilities() -> KeyboardCapabilities {
    let mut capabilities = KeyboardCapabilities {
        keyboard_type: KeyboardType::None,
        supports_brightness: false,
        supports_color: false,
        supports_effects: false,
        num_zones: 0,
    };

    // Check for tuxedo_keyboard sysfs
    let base_path = "/sys/devices/platform/tuxedo_keyboard/leds";
    if Path::new(base_path).exists() {
        // Check for multiple zones
        let mut zones = 0;
        if Path::new(&format!("{}/left:kbd_backlight", base_path)).exists()
            || Path::new(&format!("{}/rgb:kbd_backlight", base_path)).exists()
        {
            zones = 1;
            capabilities.keyboard_type = KeyboardType::SingleZoneRGB;

            if Path::new(&format!("{}/center:kbd_backlight", base_path)).exists()
                && Path::new(&format!("{}/right:kbd_backlight", base_path)).exists()
            {
                zones = 3;
                capabilities.keyboard_type = KeyboardType::ThreeZoneRGB;
            }
        }

        if zones > 0 {
            capabilities.num_zones = zones;
            capabilities.supports_brightness = true;
            capabilities.supports_color = true;

            // Effects support - usually if there is a 'mode' file
            let test_path = if zones == 3 {
                format!("{}/left:kbd_backlight/mode", base_path)
            } else {
                format!("{}/rgb:kbd_backlight/mode", base_path)
            };
            capabilities.supports_effects = Path::new(&test_path).exists();

            return capabilities;
        }
    }

    // Fallback: check /sys/class/leds/
    let class_leds = "/sys/class/leds";
    if let Ok(entries) = fs::read_dir(class_leds) {
        let mut kbd_leds = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains("kbd_backlight") {
                kbd_leds.push(name);
            }
        }

        if !kbd_leds.is_empty() {
            capabilities.supports_brightness = true;

            let has_rgb = kbd_leds.iter().any(|n| n.contains("rgb:"));
            let has_tuxedo = kbd_leds.iter().any(|n| n.contains("tuxedo:"));

            if has_rgb || has_tuxedo {
                capabilities.supports_color = true;

                // Count zones
                let zones = kbd_leds.len() as u32;
                capabilities.num_zones = zones;

                if zones == 1 {
                    capabilities.keyboard_type = KeyboardType::SingleZoneRGB;
                } else if zones == 3 {
                    capabilities.keyboard_type = KeyboardType::ThreeZoneRGB;
                } else if zones == 4 {
                    capabilities.keyboard_type = KeyboardType::FourZoneRGB;
                } else {
                    capabilities.keyboard_type = KeyboardType::SingleZoneRGB;
                }

                // Check for effects in the first one
                let first_path = format!("{}/{}/mode", class_leds, kbd_leds[0]);
                capabilities.supports_effects = Path::new(&first_path).exists();
            } else {
                capabilities.keyboard_type = KeyboardType::WhiteOnly;
                capabilities.num_zones = 1;
            }
        }
    }

    // Per-key check - some ITE controllers might not show up as standard LEDs yet
    // or they might have a lot of them. If we see > 10 kbd_backlight leds, it might be per-key.
    if capabilities.num_zones > 10 || (capabilities.keyboard_type == KeyboardType::None && has_ite_keyboard()) {
        capabilities.keyboard_type = KeyboardType::PerKeyRGB;
        // If it's ITE, it usually supports brightness and color even if not yet fully detected via sysfs
        if capabilities.num_zones == 0 {
            capabilities.supports_brightness = true;
            capabilities.supports_color = true;
        }
    }

    capabilities
}

pub fn get_system_info() -> Result<SystemInfo> {
    static CACHED_SYSTEM_INFO: Mutex<Option<SystemInfo>> = Mutex::new(None);

    {
        let cache = CACHED_SYSTEM_INFO.lock().unwrap();
        if let Some(info) = &*cache {
            return Ok(info.clone());
        }
    }

    let mut product_name = fs::read_to_string("/sys/class/dmi/id/product_name")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    if product_name == "Unknown" || product_name.is_empty() {
        if let Ok(model) = fs::read_to_string("/proc/device-tree/model") {
            product_name = model.trim_matches('\0').trim().to_string();
        }
    }

    let product_sku = fs::read_to_string("/sys/class/dmi/id/product_sku")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();
    
    let manufacturer = fs::read_to_string("/sys/class/dmi/id/sys_vendor")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    if manufacturer == "Unknown" {
        // Try to get from os-release or something else?
        // For now just leave it.
    }

    let board_name = fs::read_to_string("/sys/class/dmi/id/board_name")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();
    
    let bios_version = fs::read_to_string("/sys/class/dmi/id/bios_version")
        .unwrap_or_else(|_| "Unknown".to_string())
        .trim()
        .to_string();

    let kernel_modules = get_tuxedo_kernel_modules();
    
    let info = SystemInfo {
        product_name,
        product_sku,
        manufacturer,
        board_name,
        bios_version,
        kernel_modules,
    };

    {
        let mut cache = CACHED_SYSTEM_INFO.lock().unwrap();
        *cache = Some(info.clone());
    }

    Ok(info)
}

pub fn get_gpu_info() -> Result<Vec<GpuInfo>> {
    let mut gpus = Vec::new();
    
    // First, try to get NVIDIA GPU info via NVML
    if Path::new("/sys/bus/pci/drivers/nvidia").exists() {
        if let Ok(nvidia_gpus) = get_nvidia_gpu_info() {
            for gpu in nvidia_gpus {
                gpus.push(gpu);
            }
        }
    }

    // Also get iGPU info from /sys/class/drm for Intel/AMD integrated graphics
    for i in 0..4 {
        let card_path = format!("/sys/class/drm/card{}", i);
        if !Path::new(&card_path).exists() {
            continue;
        }
        
        let device_path = format!("{}/device", card_path);
        let vendor_path = format!("{}/vendor", device_path);
        
        if let Ok(vendor) = fs::read_to_string(&vendor_path) {
            let vendor = vendor.trim();
            
            // Skip NVIDIA GPUs as we already got them from NVML
            if vendor == "0x10de" {
                continue;
            }
            
            let device_id_path = format!("{}/device", device_path);
            let device_id = fs::read_to_string(&device_id_path)
                .unwrap_or_else(|_| "unknown".to_string())
                .trim()
                .to_string();
            
            let name = match vendor {
                "0x1002" => get_amd_gpu_name(&device_id).unwrap_or_else(|| format!("AMD iGPU")),
                "0x8086" => get_intel_gpu_name(&device_id).unwrap_or_else(|| format!("Intel iGPU")),
                _ => format!("GPU {}", i),
            };
            
            let gpu_type = GpuType::Integrated;
            
            let status_path = format!("{}/power/runtime_status", device_path);
            let status = fs::read_to_string(&status_path)
                .unwrap_or_else(|_| "active".to_string())
                .trim()
                .to_string();
            
            // Read frequency
            let frequency = read_gpu_frequency(&device_path);
            
            // Read memory frequency for iGPUs
            let memory_frequency = read_gpu_memory_frequency(&device_path);
            
            // Read temperature
            let temperature = read_gpu_temperature(&device_path);
            
            // Read load
            let load = read_gpu_load(&device_path);
            
            // Read power
            let power = read_gpu_power(&device_path);
            
            // Read voltage (optional)
            let voltage = read_gpu_voltage(&device_path);
            
            gpus.push(GpuInfo {
                name,
                gpu_type,
                status,
                frequency,
                memory_frequency,
                temperature,
                hotspot_temperature: None,  // Not available for AMD GPUs
                memory_temperature: None,    // Not available for AMD GPUs
                load,
                power,
                voltage,
                freq_offset: None,
                drain_offset: None,
                power_offset: None,
                total_offset: None,
                min_core_clock: None,
                max_core_clock: None,
                min_memory_clock: None,
                max_memory_clock: None,
                core_clock_range: None,
                memory_clock_range: None,
                core_offset_limits: None,
                memory_offset_limits: None,
                is_desktop: false,
                architecture: None,
                nvml_index: None,
                driver_version: None,
                supported_p_states: vec![],
                supports_power_limit: false,
                power_limit_range: None,
                supports_gpu_offset: false,
                supports_mem_offset: false,
                fan_speed_range: None,
                vram_type: None,
                vram_vendor: None,
                vram_bus_width: None,
                vram_bandwidth: None,
                vram_total: None,
            });
        }
    }
    
    if gpus.is_empty() {
        return Err(anyhow!("No GPUs detected"));
    }
    
    Ok(gpus)
}

/// Get AMD GPU name from device ID
fn get_amd_gpu_name(device_id: &str) -> Option<String> {
    // Common AMD iGPU device IDs
    match device_id {
        // Radeon Vega series (Ryzen APUs)
        "0x15dd" => Some("AMD Radeon Vega 8".to_string()),
        "0x15d8" => Some("AMD Radeon Vega 3".to_string()),
        "0x1636" => Some("AMD Radeon Graphics (Renoir)".to_string()),
        "0x1638" => Some("AMD Radeon Graphics (Cezanne)".to_string()),
        // RDNA2/RDNA3 iGPUs
        "0x164c" => Some("AMD Radeon 680M".to_string()),
        "0x164d" => Some("AMD Radeon 660M".to_string()),
        "0x15bf" => Some("AMD Radeon 780M".to_string()),
        "0x15c8" => Some("AMD Radeon 760M".to_string()),
        _ => None,
    }
}

/// Get Intel GPU name from device ID
fn get_intel_gpu_name(device_id: &str) -> Option<String> {
    // Common Intel iGPU device IDs
    match device_id {
        // Intel UHD Graphics
        "0x3ea0" | "0x3ea5" => Some("Intel UHD Graphics 620".to_string()),
        "0x9a49" => Some("Intel UHD Graphics (Tiger Lake)".to_string()),
        "0x9a78" => Some("Intel UHD Graphics (Rocket Lake)".to_string()),
        // Intel Iris Xe
        "0x9a40" | "0x9a60" => Some("Intel Iris Xe Graphics".to_string()),
        "0xa7a0" | "0xa7a1" => Some("Intel Iris Xe Graphics (Alder Lake)".to_string()),
        // Intel Iris Plus
        "0x8a52" | "0x8a5a" => Some("Intel Iris Plus Graphics".to_string()),
        // Intel Arc
        "0x56a0" | "0x56a1" => Some("Intel Arc Graphics (DG2)".to_string()),
        _ => None,
    }
}

use crate::hardware_control::get_nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, PerformanceState};



/// Internal helper to get base (un-offset) GPU clock ranges for P-State 0
fn get_base_gpu_clock_ranges(device: &nvml_wrapper::Device) -> Result<(u32, u32)> {
    match device.min_max_clock_of_pstate(Clock::Graphics, PerformanceState::Zero) {
        Ok((min, max)) => Ok((min, max)),
        Err(e) => {
            log::warn!(target: "hw.detect", "Failed to get P0 clock ranges via min_max_clock_of_pstate: {}. Using fallback.", e);
            // Fallback to absolute max supported clocks if P0 ranges fail
            let mut fallback = None;
            if let Ok(mem_clocks) = device.supported_memory_clocks() {
                if let Some(&target_mem_clock) = mem_clocks.iter().max() {
                    if let Ok(clocks) = device.supported_graphics_clocks(target_mem_clock) {
                        if let (Some(&c_min), Some(&c_max)) = (clocks.iter().min(), clocks.iter().max()) {
                            fallback = Some((c_min, c_max));
                        }
                    }
                }
            }
            fallback.ok_or_else(|| anyhow!("Could not determine graphics clock ranges for P-State 0: {}", e))
        }
    }
}

pub fn get_gpu_clock_ranges(device_index: u32) -> Result<(u32, u32)> {
    // Try cache first to avoid waking up GPU
    let cached_range = {
        let cache = NVIDIA_METADATA_CACHE.lock().unwrap();
        cache.get(&device_index).and_then(|m| m.core_clock_range)
    };

    let (mut min, mut max) = if let Some(range) = cached_range {
        range
    } else {
        // If not in cache, check if GPU is suspended before waking it up
        if is_gpu_suspended_by_index(device_index) {
            return Err(anyhow!("GPU suspended and metadata not cached"));
        }
        let nvml = get_nvml()?;
        let device = nvml.device_by_index(device_index)?;
        get_base_gpu_clock_ranges(&device)?
    };

    // Add current core offset if any to show real-time effective ranges in the UI
    let offset = {
        let map = crate::MANUAL_GPU_OFFSETS.lock().unwrap();
        map.get(&device_index).map(|(c, _)| *c).unwrap_or(0.0)
    };

    min = (min as f32 + offset).max(0.0) as u32;
    max = (max as f32 + offset).max(0.0) as u32;

    Ok((min, max))
}


pub fn get_gpu_core_offset_limits(device_index: u32) -> Result<(i32, i32)> {
    // Try cache first
    let cached_limits = {
        let cache = NVIDIA_METADATA_CACHE.lock().unwrap();
        cache.get(&device_index).and_then(|m| m.core_offset_limits)
    };

    if let Some(limits) = cached_limits {
        return Ok(limits);
    }

    if is_gpu_suspended_by_index(device_index) {
        return Err(anyhow!("GPU suspended and metadata not cached"));
    }

    let nvml = get_nvml()?;
    let device = nvml.device_by_index(device_index)?;
    let offset_info = device.clock_offset(Clock::Graphics, PerformanceState::Zero)?;
    Ok((offset_info.min_clock_offset_mhz, offset_info.max_clock_offset_mhz))
}

pub fn get_gpu_memory_offset_limits(device_index: u32) -> Result<(i32, i32)> {
    // Try cache first
    let cached_limits = {
        let cache = NVIDIA_METADATA_CACHE.lock().unwrap();
        cache.get(&device_index).and_then(|m| m.memory_offset_limits)
    };

    if let Some(limits) = cached_limits {
        return Ok(limits);
    }

    if is_gpu_suspended_by_index(device_index) {
        return Err(anyhow!("GPU suspended and metadata not cached"));
    }

    let nvml = get_nvml()?;
    let device = nvml.device_by_index(device_index)?;
    let offset_info = device.clock_offset(Clock::Memory, PerformanceState::Zero)?;
    Ok((offset_info.min_clock_offset_mhz, offset_info.max_clock_offset_mhz))
}

fn is_gpu_suspended_by_index(index: u32) -> bool {
    let mut nvidia_pci_ids = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/bus/pci/drivers/nvidia") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(':') {
                nvidia_pci_ids.push(name);
            }
        }
    }
    nvidia_pci_ids.sort();

    if let Some(id) = nvidia_pci_ids.get(index as usize) {
        let status_path = format!("/sys/bus/pci/drivers/nvidia/{}/power/runtime_status", id);
        if let Ok(status) = fs::read_to_string(status_path) {
            return status.trim().eq_ignore_ascii_case("suspended");
        }
    }
    false
}

// NVIDIA Direct Driver Constants and Structs
const NV_IOCTL_MAGIC: u8 = b'F';
const NV_ESC_RM_ALLOC: u8 = 0x23;
const NV_ESC_RM_CONTROL: u8 = 0x2B;
const NV_ESC_REGISTER_FD: u8 = 0x27;

const NV01_DEVICE_0: u32 = 0x00000080;
const NV20_SUBDEVICE_0: u32 = 0x00002080;

// NVIDIA RM Control API constants
// These values are verified to match NVIDIA driver headers and LACT implementation
const NV2080_CTRL_CMD_FB_GET_INFO: u32 = 0x20800101;
const NV2080_CTRL_FB_INFO_INDEX_RAM_TYPE: u32 = 0x01;
const NV2080_CTRL_FB_INFO_INDEX_BUS_WIDTH: u32 = 0x02;
const NV2080_CTRL_FB_INFO_INDEX_MEMORYINFO_VENDOR_ID: u32 = 0x06;

type NvHandle = u32;

#[allow(non_snake_case)]
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NVOS21_PARAMETERS {
    hRoot: NvHandle,
    hObjectParent: NvHandle,
    hObjectNew: NvHandle,
    hClass: u32,
    pAllocParms: *mut std::ffi::c_void,
    status: u32,
}

#[allow(non_snake_case)]
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NVOS64_PARAMETERS {
    hRoot: NvHandle,
    hObjectParent: NvHandle,
    hObjectNew: NvHandle,
    hClass: u32,
    pAllocParms: *mut std::ffi::c_void,
    pRightsRequested: *mut std::ffi::c_void,
    paramsSize: u32,
    flags: u32,
    status: u32,
}

#[allow(non_snake_case)]
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NVOS54_PARAMETERS {
    hClient: NvHandle,
    hObject: NvHandle,
    cmd: u32,
    flags: u32,
    params: *mut std::ffi::c_void,
    paramsSize: u32,
    status: u32,
}

#[allow(non_snake_case)]
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NV0080_ALLOC_PARAMETERS {
    deviceId: u32,
    hClientShare: NvHandle,
    hTargetClient: NvHandle,
    hTargetDevice: NvHandle,
    flags: u32,
    pad: [u32; 2],
}

#[allow(non_snake_case)]
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
struct NV2080_ALLOC_PARAMETERS {
    subdeviceNumber: u32,
}

#[allow(non_snake_case)]
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NV2080_CTRL_FB_GET_INFO_PARAMS {
    fbInfoListSize: u32,
    fbInfoList: *mut NV2080_CTRL_FB_INFO,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NV2080_CTRL_FB_INFO {
    index: u32,
    data: u32,
}

ioctl_readwrite!(rm_alloc_nvos21, NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, NVOS21_PARAMETERS);
ioctl_readwrite!(rm_alloc_nvos64, NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, NVOS64_PARAMETERS);
ioctl_readwrite!(register_fd, NV_IOCTL_MAGIC, NV_ESC_REGISTER_FD, RawFd);
ioctl_readwrite!(rm_control_nvos54, NV_IOCTL_MAGIC, NV_ESC_RM_CONTROL, NVOS54_PARAMETERS);

struct NvidiaDriverHandle {
    nvidiactl_fd: std::fs::File,
    #[allow(dead_code)] // Keeps the device file descriptor open for the lifetime of this handle
    device_fd: std::fs::File,
    client_handle: NvHandle,
    subdevice_handle: NvHandle,
}

impl NvidiaDriverHandle {
    fn open(minor_number: u32) -> Result<Self> {
        log::debug!(target: "hw.detect", "NvidiaDriverHandle::open: Attempting to open for minor {}", minor_number);
        
        // Open /dev/nvidiactl with enhanced error reporting
        let nvidiactl_fd = fs::File::options()
            .read(true)
            .write(true)
            .open("/dev/nvidiactl")
            .with_context(|| "Failed to open /dev/nvidiactl - ensure NVIDIA driver is loaded and you have permissions (try: sudo usermod -aG video <username>)")?;
        log::debug!(target: "hw.detect", "NvidiaDriverHandle::open: Opened /dev/nvidiactl");

        let mut client_params: NVOS21_PARAMETERS = unsafe { std::mem::zeroed() };
        unsafe {
            rm_alloc_nvos21(nvidiactl_fd.as_raw_fd(), &mut client_params)
                .with_context(|| "Failed to allocate NVIDIA RM client handle via IOCTL NV_ESC_RM_ALLOC")?;
        }
        let client_handle = client_params.hObjectNew;
        log::debug!(target: "hw.detect", "NvidiaDriverHandle::open: Got client_handle=0x{:x}", client_handle);

        // Open device-specific file with enhanced error reporting  
        let device_path = format!("/dev/nvidia{}", minor_number);
        let device_fd = fs::File::options()
            .read(true)
            .write(true)
            .open(&device_path)
            .with_context(|| format!("Failed to open {} - GPU device may not exist or is not accessible", device_path))?;
        log::debug!(target: "hw.detect", "NvidiaDriverHandle::open: Opened /dev/nvidia{}", minor_number);

        let mut dev_fd_raw = device_fd.as_raw_fd();
        unsafe {
            register_fd(device_fd.as_raw_fd(), &mut dev_fd_raw)
                .with_context(|| format!("Failed to register device FD for /dev/nvidia{} via IOCTL NV_ESC_REGISTER_FD", minor_number))?;
        }
        log::debug!(target: "hw.detect", "NvidiaDriverHandle::open: Registered device FD");

        let mut alloc_params: NV0080_ALLOC_PARAMETERS = unsafe { std::mem::zeroed() };
        alloc_params.deviceId = minor_number;
        let mut device_request = NVOS64_PARAMETERS {
            hRoot: client_handle,
            hObjectParent: client_handle,
            hObjectNew: 0,
            hClass: NV01_DEVICE_0,
            pAllocParms: &mut alloc_params as *mut _ as *mut _,
            pRightsRequested: std::ptr::null_mut(),
            paramsSize: std::mem::size_of::<NV0080_ALLOC_PARAMETERS>() as u32,
            flags: 0,
            status: 0,
        };
        unsafe {
            rm_alloc_nvos64(nvidiactl_fd.as_raw_fd(), &mut device_request)
                .with_context(|| "Failed IOCTL NV_ESC_RM_ALLOC for device object")?;
        }
        if device_request.status != 0 {
            log::error!(target: "hw.detect", "NvidiaDriverHandle::open: Failed to alloc device handle: status=0x{:08x}", device_request.status);
            return Err(anyhow!("Failed to alloc device handle: status=0x{:08x}", device_request.status));
        }
        let device_handle = device_request.hObjectNew;
        log::debug!(target: "hw.detect", "NvidiaDriverHandle::open: Got device_handle=0x{:x}", device_handle);

        let mut subdevice_alloc: NV2080_ALLOC_PARAMETERS = Default::default();
        let mut subdevice_request = NVOS64_PARAMETERS {
            hRoot: client_handle,
            hObjectParent: device_handle,
            hObjectNew: 0,
            hClass: NV20_SUBDEVICE_0,
            pAllocParms: &mut subdevice_alloc as *mut _ as *mut _,
            pRightsRequested: std::ptr::null_mut(),
            paramsSize: std::mem::size_of::<NV2080_ALLOC_PARAMETERS>() as u32,
            flags: 0,
            status: 0,
        };
        unsafe {
            rm_alloc_nvos64(nvidiactl_fd.as_raw_fd(), &mut subdevice_request)
                .with_context(|| "Failed IOCTL NV_ESC_RM_ALLOC for subdevice object")?;
        }
        if subdevice_request.status != 0 {
            log::error!(target: "hw.detect", "NvidiaDriverHandle::open: Failed to alloc subdevice handle: status=0x{:08x}", subdevice_request.status);
            return Err(anyhow!("Failed to alloc subdevice handle: status=0x{:08x}", subdevice_request.status));
        }
        log::debug!(target: "hw.detect", "NvidiaDriverHandle::open: Got subdevice_handle=0x{:x}, successfully opened", subdevice_request.hObjectNew);

        Ok(Self {
            nvidiactl_fd,
            device_fd,
            client_handle,
            subdevice_handle: subdevice_request.hObjectNew,
        })
    }

    fn get_fb_info(&self, index: u32) -> Result<u32> {
        let mut info = NV2080_CTRL_FB_INFO { index, data: 0 };
        let mut params = NV2080_CTRL_FB_GET_INFO_PARAMS {
            fbInfoListSize: 1,
            fbInfoList: &mut info,
        };
        let mut request = NVOS54_PARAMETERS {
            hClient: self.client_handle,
            hObject: self.subdevice_handle,
            cmd: NV2080_CTRL_CMD_FB_GET_INFO,
            flags: 0,
            params: &mut params as *mut _ as *mut _,
            paramsSize: std::mem::size_of::<NV2080_CTRL_FB_GET_INFO_PARAMS>() as u32,
            status: 0,
        };
        log::debug!(target: "hw.detect", "get_fb_info: index=0x{:02x}, hClient=0x{:x}, hObject=0x{:x}, cmd=0x{:08x}, paramsSize={}",
            index, self.client_handle, self.subdevice_handle, request.cmd, request.paramsSize);
        
        let ioctl_result = unsafe {
            rm_control_nvos54(self.nvidiactl_fd.as_raw_fd(), &mut request)
        };
        
        // Log IOCTL result
        if let Err(ref e) = ioctl_result {
            log::error!(target: "hw.detect", "get_fb_info: IOCTL failed for index 0x{:02x}: errno={}, error={}", 
                index, 
                std::io::Error::last_os_error().raw_os_error().unwrap_or(-1),
                e);
            return Err(anyhow!("IOCTL NV_ESC_RM_CONTROL failed: {}", e));
        }
        
        log::debug!(target: "hw.detect", "get_fb_info: IOCTL succeeded, request.status=0x{:08x}, info.data=0x{:08x}", 
            request.status, info.data);
        
        if request.status != 0 {
            // Decode common NVIDIA RM error codes
            let error_desc = match request.status {
                0x0000ffff => "NV_ERR_GENERIC",
                0x00000001 => "NV_ERR_INVALID_ARGUMENT", 
                0x00000002 => "NV_ERR_INVALID_OBJECT_HANDLE",
                0x00000003 => "NV_ERR_INVALID_OBJECT_PARENT",
                0x00000005 => "NV_ERR_INSUFFICIENT_RESOURCES",
                0x00000006 => "NV_ERR_INVALID_FLAGS",
                0x00000008 => "NV_ERR_INVALID_STATE",
                0x0000000a => "NV_ERR_NOT_SUPPORTED",
                0x0000000d => "NV_ERR_OBJECT_NOT_FOUND",
                0x00000056 => "NV_ERR_GPU_NOT_FULL_POWER",
                _ => "UNKNOWN_ERROR",
            };
            log::error!(target: "hw.detect", "get_fb_info: RM control returned error status 0x{:08x} ({}) for index 0x{:02x}", 
                request.status, error_desc, index);
            return Err(anyhow!("RM control failed: status=0x{:08x} ({})", request.status, error_desc));
        }
        Ok(info.data)
    }
}

// NVAPI Constants
const NVAPI_LIBRARY: &str = "libnvidia-api.so.1";
const QUERY_NVAPI_INITIALIZE: u32 = 0x0150e828;
const QUERY_NVAPI_UNLOAD: u32 = 0xd22bdd7e;
const QUERY_NVAPI_ENUM_PHYSICAL_GPUS: u32 = 0xe5ac921f;
const QUERY_NVAPI_THERMALS: u32 = 0x65fe3aad;  // Undocumented - thermal sensors
const QUERY_NVAPI_VOLTAGE: u32 = 0x465f9bcf;   // Undocumented - voltage

const NVAPI_MAX_PHYSICAL_GPUS: usize = 64;

type NvPhysicalGpuHandle = *mut std::ffi::c_void;
type NvApiStatus = i32;

#[repr(C)]
struct NvApiThermals {
    version: u32,
    mask: i32,
    values: [i32; 40],
}

#[repr(C)]
struct NvApiVoltage {
    version: u32,
    flags: u32,
    padding_1: [u32; 8],
    value_uv: u32,
    padding_2: [u32; 8],
}

// Helper function to calculate VRAM bandwidth from metadata
/// Calculates VRAM bandwidth in GB/s based on memory type, bus width, and clock speed.
///
/// # Arguments
/// * `vram_type` - The type of VRAM (e.g., "GDDR6", "GDDR6X", "GDDR5")
/// * `vram_bus_width` - Bus width in bits (e.g., 256)
/// * `memory_clock_range` - Range of memory clock speeds in MHz (min, max)
///
/// # Returns
/// Bandwidth in GB/s, or None if required parameters are missing
///
/// # Formula
/// Bandwidth (GB/s) = (Max Clock MHz × Multiplier × Bus Width bits) / 8000
///
/// Where multiplier depends on memory type:
/// - GDDR6X: 16.0 (due to PAM4 signaling)
/// - GDDR6: 8.0 (quad data rate)
/// - GDDR5: 4.0 (quad data rate)
/// - Others: 2.0 (default DDR)
fn calculate_vram_bandwidth(vram_type: Option<&String>, vram_bus_width: Option<u32>, memory_clock_range: Option<(u32, u32)>) -> Option<f32> {
    if let (Some(bus), Some((_, max_mem))) = (vram_bus_width, memory_clock_range) {
        let mut multiplier = 2.0; // Default for DDR
        if let Some(t) = vram_type {
            if t.contains("GDDR6X") {
                multiplier = 16.0;
            } else if t.contains("GDDR6") {
                multiplier = 8.0;
            } else if t.contains("GDDR5") {
                multiplier = 4.0;
            }
        }
        // Bandwidth (GB/s) = (Clock * Multiplier * Bus Width) / 8 bits / 1000 MHz
        Some((max_mem as f32 * multiplier * bus as f32) / 8000.0)
    } else {
        None
    }
}

// Function to get NVIDIA extended stats (hotspot, memory temp, voltage)
fn get_vram_info(minor_number: u32) -> (Option<String>, Option<String>, Option<u32>, Option<f32>) {
    // Returns (type, vendor, bus_width, bandwidth)
    log::debug!(target: "hw.detect", "Attempting to get VRAM info for NVIDIA device minor {}", minor_number);
    match NvidiaDriverHandle::open(minor_number) {
        Ok(handle) => {
            let ram_type_val = handle.get_fb_info(NV2080_CTRL_FB_INFO_INDEX_RAM_TYPE)
                .inspect_err(|e| log::warn!(target: "hw.detect", 
                    "Failed to get RAM type for minor {} (index=0x{:02x}): {} - This may indicate driver/GPU incompatibility or suspended GPU state", 
                    minor_number, NV2080_CTRL_FB_INFO_INDEX_RAM_TYPE, e))
                .ok();
            
            let bus_width = handle.get_fb_info(NV2080_CTRL_FB_INFO_INDEX_BUS_WIDTH)
                .inspect_err(|e| log::warn!(target: "hw.detect", 
                    "Failed to get bus width for minor {} (index=0x{:02x}): {} - This may indicate driver/GPU incompatibility or suspended GPU state", 
                    minor_number, NV2080_CTRL_FB_INFO_INDEX_BUS_WIDTH, e))
                .ok();
            
            let vendor_id = handle.get_fb_info(NV2080_CTRL_FB_INFO_INDEX_MEMORYINFO_VENDOR_ID)
                .inspect_err(|e| log::warn!(target: "hw.detect", 
                    "Failed to get vendor ID for minor {} (index=0x{:02x}): {} - This may indicate driver/GPU incompatibility or suspended GPU state", 
                    minor_number, NV2080_CTRL_FB_INFO_INDEX_MEMORYINFO_VENDOR_ID, e))
                .ok();

            log::debug!(target: "hw.detect", "VRAM raw info for minor {}: type={:?}, bus={:?}, vendor={:?}",
                minor_number, ram_type_val, bus_width, vendor_id);
            
            // Log summary of detection results
            if ram_type_val.is_some() || bus_width.is_some() || vendor_id.is_some() {
                log::debug!(target: "hw.detect", "Successfully retrieved partial VRAM info for minor {}: type={}, bus={}, vendor={}",
                    minor_number, 
                    ram_type_val.map(|v| format!("0x{:08x}", v)).unwrap_or_else(|| "None".to_string()),
                    bus_width.map(|v| format!("{} bits", v)).unwrap_or_else(|| "None".to_string()),
                    vendor_id.map(|v| format!("0x{:08x}", v)).unwrap_or_else(|| "None".to_string()));
            } else {
                log::warn!(target: "hw.detect", "Failed to retrieve any VRAM info for minor {} - all queries returned errors", minor_number);
            }

            let ram_type = ram_type_val.map(|v| match v {
                0x00000001 => "SDRAM",
                0x00000002 => "DDR1",
                0x00000003 => "DDR2",
                0x00000004 => "DDR3",
                0x00000005 => "GDDR2",
                0x00000006 => "GDDR3",
                0x00000007 => "GDDR4",
                0x00000008 => "GDDR5",
                0x00000009 => "LPDDR2",
                0x0000000A => "GDDR5X",
                0x0000000B => "GDDR6",
                0x0000000C => "GDDR6X",
                0x0000000D => "HBM1",
                0x0000000E => "HBM2",
                0x0000000F => "HBM3",
                0x00000010 => "LPDDR4",
                0x00000011 => "LPDDR5",
                0x00000012 => "GDDR7",
                _ => "Unknown",
            }.to_string());

            let vendor = vendor_id.map(|v| match v {
                0x00000001 => "Micron",
                0x00000002 => "Samsung",
                0x00000003 => "Qimonda",
                0x00000004 => "Elpida",
                0x00000005 => "Etron",
                0x00000006 => "Nanya",
                0x00000007 => "Hynix",
                0x00000008 => "Mosel",
                0x00000009 => "Winbond",
                0x0000000A => "ESMT",
                _ => "Unknown",
            }.to_string());

            // Bandwidth calculation: (Clock * 2 (for DDR) * BusWidth) / 8 / 1000?
            // Actually bandwidth is complex to calculate without current clock.
            // NVML already provides memory bandwidth in some versions but not all.
            // nvidia/nvidia.rs doesn't seem to calculate it, just returns None.

            (ram_type, vendor, bus_width, None)
        }
        Err(e) => {
            // Enhanced error logging with more diagnostics
            let error_msg = format!("{}", e);
            
            // Check for common error conditions
            if error_msg.contains("Permission denied") || error_msg.contains("EACCES") {
                log::error!(target: "hw.detect", 
                    "Failed to open NvidiaDriverHandle for minor {}: Permission denied - Daemon may need to run as root or user needs to be in 'video' group. Error: {}", 
                    minor_number, e);
            } else if error_msg.contains("No such file") || error_msg.contains("ENOENT") {
                log::error!(target: "hw.detect", 
                    "Failed to open NvidiaDriverHandle for minor {}: Device not found - NVIDIA driver may not be loaded or GPU is not available. Error: {}", 
                    minor_number, e);
            } else if error_msg.contains("Device or resource busy") || error_msg.contains("EBUSY") {
                log::error!(target: "hw.detect", 
                    "Failed to open NvidiaDriverHandle for minor {}: Device busy - Another process may be using the GPU. Error: {}", 
                    minor_number, e);
            } else {
                log::error!(target: "hw.detect", 
                    "Failed to open NvidiaDriverHandle for minor {}: {} - Check that NVIDIA driver is loaded and device /dev/nvidia{} exists", 
                    minor_number, e, minor_number);
            }
            
            (None, None, None, None)
        }
    }
}

fn get_nvidia_extended_stats(gpu_index: u32) -> (Option<f32>, Option<f32>, Option<f32>) {
    // Returns (hotspot_temp, memory_temp, voltage_v)
    // Note: This loads and initializes NVAPI on each call. This is acceptable for 
    // polling intervals of 1+ seconds but may need optimization for higher frequencies.
    
    unsafe {
        // Load library - must be kept alive until after unload is called
        let lib = match libloading::Library::new(NVAPI_LIBRARY) {
            Ok(l) => l,
            Err(_) => return (None, None, None),
        };
        
        // Get query interface function
        let query_interface: libloading::Symbol<unsafe extern "C" fn(u32) -> *const ()> = 
            match lib.get(b"nvapi_QueryInterface\0") {
                Ok(f) => f,
                Err(_) => return (None, None, None),
            };
        
        // Initialize NVAPI
        let init_fn = query_interface(QUERY_NVAPI_INITIALIZE);
        if init_fn.is_null() { return (None, None, None); }
        let init: unsafe extern "C" fn() -> NvApiStatus = mem::transmute(init_fn);
        let init_result = init();
        
        // Helper to safely unload NVAPI before returning
        let safe_unload = || {
            let unload_fn = query_interface(QUERY_NVAPI_UNLOAD);
            if !unload_fn.is_null() {
                let unload: unsafe extern "C" fn() -> NvApiStatus = mem::transmute(unload_fn);
                let _ = unload();
            }
        };
        
        if init_result != 0 {
            return (None, None, None);
        }
        
        // Enumerate GPUs
        let enum_fn = query_interface(QUERY_NVAPI_ENUM_PHYSICAL_GPUS);
        if enum_fn.is_null() {
            safe_unload();
            return (None, None, None);
        }
        let enum_gpus: unsafe extern "C" fn(
            handles: &mut [NvPhysicalGpuHandle; NVAPI_MAX_PHYSICAL_GPUS],
            count: &mut u32,
        ) -> NvApiStatus = mem::transmute(enum_fn);
        
        let mut handles = [std::ptr::null_mut(); NVAPI_MAX_PHYSICAL_GPUS];
        let mut count = 0u32;
        if enum_gpus(&mut handles, &mut count) != 0 {
            safe_unload();
            return (None, None, None);
        }
        
        if gpu_index >= count {
            safe_unload();
            return (None, None, None);
        }
        let handle = handles[gpu_index as usize];
        
        let mut hotspot_temp = None;
        let mut memory_temp = None;
        let mut voltage = None;
        
        // Get thermals
        let thermals_fn = query_interface(QUERY_NVAPI_THERMALS);
        if !thermals_fn.is_null() {
            let get_thermals: unsafe extern "C" fn(
                handle: NvPhysicalGpuHandle,
                sensors: &mut NvApiThermals,
            ) -> NvApiStatus = mem::transmute(thermals_fn);
            
            // Calculate mask by probing (some GPUs fail if unsupported bits are set)
            let mut mask = 1;
            let mut sensors = NvApiThermals {
                version: (mem::size_of::<NvApiThermals>() | (2 << 16)) as u32,
                mask: 1,
                values: [0; 40],
            };

            if get_thermals(handle, &mut sensors) == 0 {
                for bit in 0..32 {
                    sensors.mask = 1 << bit;
                    if get_thermals(handle, &mut sensors) != 0 {
                        mask = sensors.mask - 1;
                        break;
                    }
                    if bit == 31 { mask = !0; }
                }
            }

            sensors.mask = mask;
            if get_thermals(handle, &mut sensors) == 0 {
                // Hotspot is at index 9
                let hotspot_raw = sensors.values[9] as f32 / 256.0;
                if hotspot_raw > 0.0 && hotspot_raw < 255.0 {
                    hotspot_temp = Some(hotspot_raw);
                }
                
                // VRAM/Memory is at index 15
                let vram_raw = sensors.values[15] as f32 / 256.0;
                if vram_raw > 0.0 && vram_raw < 255.0 {
                    memory_temp = Some(vram_raw);
                }
            }
        }
        
        // Get voltage
        let voltage_fn = query_interface(QUERY_NVAPI_VOLTAGE);
        if !voltage_fn.is_null() {
            let get_voltage: unsafe extern "C" fn(
                handle: NvPhysicalGpuHandle,
                data: &mut NvApiVoltage,
            ) -> NvApiStatus = mem::transmute(voltage_fn);
            
            let mut volt_data = NvApiVoltage {
                version: (mem::size_of::<NvApiVoltage>() | (1 << 16)) as u32,
                flags: 0,
                padding_1: [0; 8],
                value_uv: 0,
                padding_2: [0; 8],
            };
            
            if get_voltage(handle, &mut volt_data) == 0 && volt_data.value_uv > 0 {
                voltage = Some(volt_data.value_uv as f32 / 1_000_000.0); // Convert µV to V
            }
        }
        
        // Unload NVAPI before library is dropped
        safe_unload();
        
        // Keep library alive until after all NVAPI calls and unload
        drop(lib);
        
        (hotspot_temp, memory_temp, voltage)
    }
}

fn get_nvidia_gpu_info() -> Result<Vec<GpuInfo>> {
    let (manual_clocks_enabled, _advanced_control_enabled) = {
        let state = crate::GPU_DAEMON_STATE.lock().unwrap();
        state.as_ref().map_or((false, false), |s| (s.manual_clocks, s.advanced_control))
    };

    // 1. Check sysfs for NVIDIA devices and their status to avoid waking up suspended GPUs
    let mut nvidia_pci_ids = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/bus/pci/drivers/nvidia") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(':') {
                nvidia_pci_ids.push(name);
            }
        }
    }
    nvidia_pci_ids.sort();

    if nvidia_pci_ids.is_empty() {
        return Ok(vec![]);
    }

    let mut all_suspended = true;
    let mut statuses = Vec::new();
    for id in &nvidia_pci_ids {
        let status_path = format!("/sys/bus/pci/drivers/nvidia/{}/power/runtime_status", id);
        let status = fs::read_to_string(status_path).unwrap_or_default().trim().to_lowercase();
        if status != "suspended" {
            all_suspended = false;
        }
        statuses.push(status);
    }

    // On-demand override (GetGpuInfoFull / GUI stats panel): loaded BEFORE any
    // shortcut so an armed flag punches through BOTH fast paths — including
    // all-suspended, where the documented contract is to deliberately wake the
    // GPU and serve live NVML values. Clearing happens only in the full-poll
    // path (one-shot), never in the stubs.
    let force_full_poll =
        crate::FULL_NVML_REFRESH_REQUESTED.load(std::sync::atomic::Ordering::Relaxed);

    // If all detected NVIDIA GPUs are suspended, bypass NVML completely to keep them asleep
    if all_suspended && !force_full_poll {
        let mut gpus = Vec::new();
        let names = NVIDIA_NAMES_CACHE.lock().unwrap();
        for (i, status) in statuses.into_iter().enumerate() {
            let name = names.get(i).cloned().unwrap_or_else(|| "NVIDIA GPU".to_string());
            
            // Retrieve cached metadata (including VRAM info) if available
            let cached_metadata = {
                let cache = NVIDIA_METADATA_CACHE.lock().unwrap();
                cache.get(&(i as u32)).cloned()
            };
            
            let (vram_type, vram_vendor, vram_bus_width, vram_bandwidth, vram_total) =
                if let Some(ref meta) = cached_metadata {
                    let bandwidth = calculate_vram_bandwidth(meta.vram_type.as_ref(), meta.vram_bus_width, meta.memory_clock_range);
                    log::debug!(target: "hw.detect", 
                        "GPU {} (all suspended): Using cached VRAM - Type: {:?}, Vendor: {:?}, Bus: {:?} bits, BW: {:?} GB/s, Total: {:?} MiB",
                        i, meta.vram_type, meta.vram_vendor, meta.vram_bus_width, bandwidth, meta.vram_total);
                    (meta.vram_type.clone(), meta.vram_vendor.clone(), meta.vram_bus_width, bandwidth, meta.vram_total)
                } else {
                    log::debug!(target: "hw.detect", "GPU {} (all suspended): No cached VRAM info available", i);
                    (None, None, None, None, None)
                };
            
            gpus.push(GpuInfo {
                name,
                gpu_type: GpuType::Discrete,
                status,
                frequency: None,
                memory_frequency: None,
                temperature: None,
                hotspot_temperature: None,
                memory_temperature: None,
                load: None,
                power: None,
                voltage: None,
                freq_offset: None,
                drain_offset: None,
                power_offset: None,
                total_offset: None,
                min_core_clock: None,
                max_core_clock: None,
                min_memory_clock: None,
                max_memory_clock: None,
                core_clock_range: cached_metadata.as_ref().and_then(|m| m.core_clock_range),
                memory_clock_range: cached_metadata.as_ref().and_then(|m| m.memory_clock_range),
                core_offset_limits: cached_metadata.as_ref().and_then(|m| m.core_offset_limits),
                memory_offset_limits: cached_metadata.as_ref().and_then(|m| m.memory_offset_limits),
                is_desktop: false,
                architecture: cached_metadata.as_ref().and_then(|m| m.architecture.clone()),
                nvml_index: Some(i as u32),
                driver_version: None,
                supported_p_states: cached_metadata.as_ref().map(|m| m.supported_p_states.clone()).unwrap_or_default(),
                supports_power_limit: cached_metadata.as_ref().and_then(|m| m.power_limit_range).is_some(),
                power_limit_range: cached_metadata.as_ref().and_then(|m| m.power_limit_range),
                supports_gpu_offset: cached_metadata.as_ref().map(|m| m.supports_gpu_offset).unwrap_or(false),
                supports_mem_offset: cached_metadata.as_ref().map(|m| m.supports_mem_offset).unwrap_or(false),
                fan_speed_range: None,
                vram_type,
                vram_vendor,
                vram_bus_width,
                vram_bandwidth,
                vram_total,
            });
        }
        return Ok(gpus);
    }

    // -----------------------------------------------------------------------
    // RTD3 hybrid strategy — idle tier decision (BEFORE any NVMS/NVML touch).
    //
    // Field-verified: ANY periodic NVML call (even device.name() /
    // performance_state()) resets the kernel's 20 s autosuspend_delay_ms
    // timer, keeping an active-but-idle dGPU awake forever. The previous
    // "P4+ still polls NVML and eventually suspends" model never suspended.
    //
    // If every ACTIVE GPU has a fresh idle snapshot (last full poll observed
    // utilization == 0), serve last-known values and skip NVML entirely this
    // tick so the autosuspend timer can expire and the GPU drops into
    // runtime_status "suspended". Snapshots older than IDLE_CACHE_TTL_SECS
    // expire; the next tick then does one full poll (honest staleness).
    // GetGpuInfoFull() sets FULL_NVML_REFRESH_REQUESTED for a one-shot bypass.
    // -----------------------------------------------------------------------
    if !force_full_poll {
        let all_active_idle = statuses.iter().enumerate().all(|(idx, status)| {
            if status == "suspended" {
                return true;
            }
            idle_cache_fresh_for(idx as u32)
        });

        if all_active_idle {
            log::debug!(target: "hw.detect",
                "NVIDIA GPU(s) active-but-idle with fresh snapshot: bypassing NVML (RTD3 idle tier)");
            let mut gpus = Vec::new();
            {
                let names = NVIDIA_NAMES_CACHE.lock().unwrap();
                let cache_guard = IDLE_METRICS_CACHE.lock().unwrap();
                for (i, status) in statuses.iter().enumerate() {
                    let name = names
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| "NVIDIA GPU".to_string());

                    let cached_metadata = {
                        let meta_cache = NVIDIA_METADATA_CACHE.lock().unwrap();
                        meta_cache.get(&(i as u32)).cloned()
                    };

                    let (vram_type, vram_vendor, vram_bus_width, vram_bandwidth, vram_total) =
                        if let Some(ref meta) = cached_metadata {
                            let bandwidth = calculate_vram_bandwidth(
                                meta.vram_type.as_ref(),
                                meta.vram_bus_width,
                                meta.memory_clock_range,
                            );
                            (
                                meta.vram_type.clone(),
                                meta.vram_vendor.clone(),
                                meta.vram_bus_width,
                                bandwidth,
                                meta.vram_total,
                            )
                        } else {
                            (None, None, None, None, None)
                        };

                    // Expired entries were already dropped by the freshness
                    // scan above; every active GPU is guaranteed fresh here.
                    let snap = cache_guard
                        .get(&(i as u32))
                        .and_then(|e| e.snapshot.clone())
                        .unwrap_or(GpuIdleSnapshot {
                            frequency: None,
                            memory_frequency: None,
                            temperature: None,
                            load: None,
                            power: None,
                            voltage: None,
                        });

                    let mut gpu_info = GpuInfo {
                        name,
                        gpu_type: GpuType::Discrete,
                        status: status.clone(),
                        frequency: snap.frequency,
                        memory_frequency: snap.memory_frequency,
                        temperature: snap.temperature,
                        hotspot_temperature: None, // NVAPI-only; not cached
                        memory_temperature: None,  // NVAPI-only; not cached
                        load: snap.load,
                        power: snap.power,
                        voltage: snap.voltage,
                        freq_offset: None,
                        drain_offset: None,
                        power_offset: None,
                        total_offset: None,
                        min_core_clock: None,
                        max_core_clock: None,
                        min_memory_clock: None,
                        max_memory_clock: None,
                        core_clock_range: cached_metadata.as_ref().and_then(|m| m.core_clock_range),
                        memory_clock_range: cached_metadata.as_ref().and_then(|m| m.memory_clock_range),
                        core_offset_limits: cached_metadata.as_ref().and_then(|m| m.core_offset_limits),
                        memory_offset_limits: cached_metadata.as_ref().and_then(|m| m.memory_offset_limits),
                        is_desktop: false,
                        architecture: cached_metadata.as_ref().and_then(|m| m.architecture.clone()),
                        nvml_index: Some(i as u32),
                        driver_version: None,
                        supported_p_states: cached_metadata
                            .as_ref()
                            .map(|m| m.supported_p_states.clone())
                            .unwrap_or_default(),
                        supports_power_limit: cached_metadata
                            .as_ref()
                            .and_then(|m| m.power_limit_range)
                            .is_some(),
                        power_limit_range: cached_metadata.as_ref().and_then(|m| m.power_limit_range),
                        supports_gpu_offset: cached_metadata
                            .as_ref()
                            .map(|m| m.supports_gpu_offset)
                            .unwrap_or(false),
                        supports_mem_offset: cached_metadata
                            .as_ref()
                            .map(|m| m.supports_mem_offset)
                            .unwrap_or(false),
                        fan_speed_range: None,
                        vram_type,
                        vram_vendor,
                        vram_bus_width,
                        vram_bandwidth,
                        vram_total,
                    };

                    // Report applied offsets (lock-only reads, no NVML) so the
                    // GUI keeps showing the active clock offset while idle.
                    if gpu_info.name.to_lowercase().contains("nvidia")
                        && manual_clocks_enabled
                    {
                        let stats_lock = crate::CURRENT_GPU_OVERCLOCK_STATS.lock().unwrap();
                        if let Some(ref stats) = *stats_lock {
                            gpu_info.freq_offset = Some(stats.freq_offset);
                            gpu_info.drain_offset = Some(stats.drain_offset);
                            gpu_info.power_offset = Some(stats.power_offset);
                            gpu_info.total_offset = Some(stats.total_offset);
                        } else {
                            let manual_map = crate::MANUAL_GPU_OFFSETS.lock().unwrap();
                            if let Some(offsets) = manual_map.get(&(i as u32)) {
                                let core: f32 = offsets.0;
                                gpu_info.total_offset = Some(core.round() as i32);
                            }
                        }
                    }

                    gpus.push(gpu_info);
                }
            }
            return Ok(gpus);
        }
    }

    // At least one GPU is active with work to do (no snapshot yet, snapshot
    // expired, or explicit GetGpuInfoFull override) — do a full NVML pass.
    let nvml = get_nvml()?;
    let mut gpus = Vec::new();

    let driver_version = nvml.sys_driver_version().ok();

    let device_count = nvml.device_count().unwrap_or(0);
    for i in 0..device_count {
        // Use pre-read status to avoid waking up the GPU
        let status_from_sysfs = statuses.get(i as usize).cloned();
        let is_suspended = status_from_sysfs
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("suspended"))
            .unwrap_or(false);

        // Override punch-through: under FULL_NVML_REFRESH_REQUESTED we must
        // actually query NVML (waking a suspended GPU) instead of serving the
        // stub — otherwise the flag burns without producing live values.
        if is_suspended && !force_full_poll {
            let name = {
                let cache = NVIDIA_NAMES_CACHE.lock().unwrap();
                cache.get(i as usize).cloned().unwrap_or_else(|| "NVIDIA GPU".to_string())
            };
            
            // Retrieve cached metadata (including VRAM info) if available
            let cached_metadata = {
                let cache = NVIDIA_METADATA_CACHE.lock().unwrap();
                cache.get(&i).cloned()
            };
            
            let (vram_type, vram_vendor, vram_bus_width, vram_bandwidth, vram_total) =
                if let Some(ref meta) = cached_metadata {
                    let bandwidth = calculate_vram_bandwidth(meta.vram_type.as_ref(), meta.vram_bus_width, meta.memory_clock_range);
                    log::debug!(target: "hw.detect", 
                        "GPU {} (suspended): Using cached VRAM - Type: {:?}, Vendor: {:?}, Bus: {:?} bits, BW: {:?} GB/s, Total: {:?} MiB",
                        i, meta.vram_type, meta.vram_vendor, meta.vram_bus_width, bandwidth, meta.vram_total);
                    (meta.vram_type.clone(), meta.vram_vendor.clone(), meta.vram_bus_width, bandwidth, meta.vram_total)
                } else {
                    log::debug!(target: "hw.detect", "GPU {} (suspended): No cached VRAM info available", i);
                    (None, None, None, None, None)
                };
            
            gpus.push(GpuInfo {
                name,
                gpu_type: GpuType::Discrete,
                status: "suspended".to_string(),
                frequency: None,
                memory_frequency: None,
                temperature: None,
                hotspot_temperature: None,
                memory_temperature: None,
                load: None,
                power: None,
                voltage: None,
                freq_offset: None,
                drain_offset: None,
                power_offset: None,
                total_offset: None,
                min_core_clock: None,
                max_core_clock: None,
                min_memory_clock: None,
                max_memory_clock: None,
                core_clock_range: cached_metadata.as_ref().and_then(|m| m.core_clock_range),
                memory_clock_range: cached_metadata.as_ref().and_then(|m| m.memory_clock_range),
                core_offset_limits: cached_metadata.as_ref().and_then(|m| m.core_offset_limits),
                memory_offset_limits: cached_metadata.as_ref().and_then(|m| m.memory_offset_limits),
                is_desktop: false,
                architecture: cached_metadata.as_ref().and_then(|m| m.architecture.clone()),
                nvml_index: Some(i),
                driver_version: driver_version.clone(),
                supported_p_states: cached_metadata.as_ref().map(|m| m.supported_p_states.clone()).unwrap_or_default(),
                supports_power_limit: cached_metadata.as_ref().and_then(|m| m.power_limit_range).is_some(),
                power_limit_range: cached_metadata.as_ref().and_then(|m| m.power_limit_range),
                supports_gpu_offset: cached_metadata.as_ref().map(|m| m.supports_gpu_offset).unwrap_or(false),
                supports_mem_offset: cached_metadata.as_ref().map(|m| m.supports_mem_offset).unwrap_or(false),
                fan_speed_range: None,
                vram_type,
                vram_vendor,
                vram_bus_width,
                vram_bandwidth,
                vram_total,
            });
            continue;
        }

        // Active GPU - proceed with NVML
        let device = match nvml.device_by_index(i) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let name = device.name().unwrap_or_else(|_| "NVIDIA GPU".to_string());

        // Update name cache
        {
            let mut cache = NVIDIA_NAMES_CACHE.lock().unwrap();
            if cache.len() <= i as usize {
                cache.push(name.clone());
            } else {
                cache[i as usize] = name.clone();
            }
        }

        let gpu_type = GpuType::Discrete;

        // Get performance state
        use nvml_wrapper::enum_wrappers::device::PerformanceState;
        let pstate = device.performance_state().ok();

        let status = match pstate {
            Some(state) => {
                // Map nvml_wrapper::PerformanceState to "PX" format for GUI
                match state {
                    PerformanceState::Zero => "P0".to_string(),
                    PerformanceState::One => "P1".to_string(),
                    PerformanceState::Two => "P2".to_string(),
                    PerformanceState::Three => "P3".to_string(),
                    PerformanceState::Four => "P4".to_string(),
                    PerformanceState::Five => "P5".to_string(),
                    PerformanceState::Six => "P6".to_string(),
                    PerformanceState::Seven => "P7".to_string(),
                    PerformanceState::Eight => "P8".to_string(),
                    PerformanceState::Nine => "P9".to_string(),
                    PerformanceState::Ten => "P10".to_string(),
                    PerformanceState::Eleven => "P11".to_string(),
                    PerformanceState::Twelve => "P12".to_string(),
                    PerformanceState::Thirteen => "P13".to_string(),
                    PerformanceState::Fourteen => "P14".to_string(),
                    PerformanceState::Fifteen => "P15".to_string(),
                    PerformanceState::Unknown => "unknown".to_string(),
                }
            }
            None => status_from_sysfs.unwrap_or_else(|| "active".to_string()),
        };

        let pstate_val = pstate.map(|s| match s {
            PerformanceState::Zero => 0,
            PerformanceState::One => 1,
            PerformanceState::Two => 2,
            PerformanceState::Three => 3,
            PerformanceState::Four => 4,
            PerformanceState::Five => 5,
            PerformanceState::Six => 6,
            PerformanceState::Seven => 7,
            PerformanceState::Eight => 8,
            PerformanceState::Nine => 9,
            PerformanceState::Ten => 10,
            PerformanceState::Eleven => 11,
            PerformanceState::Twelve => 12,
            PerformanceState::Thirteen => 13,
            PerformanceState::Fourteen => 14,
            PerformanceState::Fifteen => 15,
            PerformanceState::Unknown => 99,
        }).unwrap_or(0);

        // Determine if we should poll monitoring stats
        // Logic for all modes:
        // 1. If suspended: poll nothing.
        // 2. If P0-P3: poll NVML and NVAPI (all stats).
        // 3. If P4+ (including P8): poll NVML only (no NVAPI/direct ioctls).
        // This ensures visibility (P0-P3) while allowing the GPU to enter
        // low-power states (P8) and eventually suspend.
        let (should_poll_nvml, should_poll_nvapi) = if is_suspended {
            (false, false)
        } else if pstate_val <= 3 {
            (true, true)
        } else {
            // P4+, including P8
            (true, false)
        };

        let (frequency, memory_frequency, temperature, load, power) = if !should_poll_nvml {
            (None, None, None, None, None)
        } else {
            (
                device.clock_info(nvml_wrapper::enum_wrappers::device::Clock::Graphics)
                    .ok()
                    .map(|c| c as u64),
                device.clock_info(nvml_wrapper::enum_wrappers::device::Clock::Memory)
                    .ok()
                    .map(|c| c as u64),
                device.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
                    .ok()
                    .map(|t| t as f32),
                device.utilization_rates().ok().map(|u| u.gpu as f32),
                device.power_usage().ok().map(|p| p as f32 / 1000.0),
            )
        };

        // Get extended stats via NVAPI
        let (hotspot_temp, memory_temp, nvapi_voltage) = if !is_suspended && should_poll_nvapi {
            get_nvidia_extended_stats(i)
        } else {
            (None, None, None)
        };

        let voltage = nvapi_voltage;

        // RTD3 idle-tier bookkeeping: every executed full pass refreshes the
        // snapshot + timestamp. A pass only reaches here when the cache was
        // cold/stale or GetGpuInfoFull forced it — i.e. this NVML touch was
        // already authorized — so recording "last known values as of now"
        // re-arms the quiet window. (Conditionally preserving an old
        // timestamp here would leave the entry permanently stale and turn
        // the TTL re-poll into a 1 Hz NVML hot loop.)
        {
            let mut cache = IDLE_METRICS_CACHE.lock().unwrap();
            cache.insert(
                i,
                IdleCacheEntry {
                    snapshot: Some(GpuIdleSnapshot {
                        frequency,
                        memory_frequency,
                        temperature,
                        load,
                        power,
                        voltage,
                    }),
                    captured_at: Instant::now(),
                },
            );
        }

        let (min_core_clock, max_core_clock) = (None, None); // NVML wrapper 0.11 doesn't have a getter

        let num_fans = device.num_fans().unwrap_or(0);
        let is_desktop = false; // Deprecated, using capability flags instead

        // Get metadata from cache or fetch it
        let metadata = {
            let mut cache = NVIDIA_METADATA_CACHE.lock().unwrap();
            if let Some(meta) = cache.get(&i) {
                log::debug!(target: "hw.detect", "GPU {}: Using cached metadata", i);
                // Check if cached metadata is incomplete - if so, retry getting it
                // This handles cases where initial detection failed (GPU suspended, driver not ready, etc.)
                let incomplete_vram = meta.vram_type.is_none() && meta.vram_vendor.is_none() && meta.vram_bus_width.is_none();
                let incomplete_ranges = meta.core_clock_range.is_none() || meta.memory_clock_range.is_none();
                let incomplete_offsets = meta.core_offset_limits.is_none() || meta.memory_offset_limits.is_none();

                if incomplete_vram || incomplete_ranges || incomplete_offsets {
                    log::debug!(target: "hw.detect", "GPU {}: Cached metadata is incomplete, retrying detection", i);
                    let minor_number = device.minor_number().unwrap_or(i);
                    
                    let (vram_type, vram_vendor, vram_bus_width, _) = if incomplete_vram {
                        get_vram_info(minor_number)
                    } else {
                        (meta.vram_type.clone(), meta.vram_vendor.clone(), meta.vram_bus_width, None)
                    };

                    let core_range = if meta.core_clock_range.is_none() {
                        get_base_gpu_clock_ranges(&device).ok()
                    } else {
                        meta.core_clock_range
                    };

                    let mem_range = if meta.memory_clock_range.is_none() {
                        get_base_memory_clock_ranges(&device).ok()
                    } else {
                        meta.memory_clock_range
                    };

                    let core_offset_limits = if meta.core_offset_limits.is_none() {
                        device.clock_offset(Clock::Graphics, PerformanceState::Zero).ok()
                            .map(|o| (o.min_clock_offset_mhz, o.max_clock_offset_mhz))
                    } else {
                        meta.core_offset_limits
                    };

                    let memory_offset_limits = if meta.memory_offset_limits.is_none() {
                        device.clock_offset(Clock::Memory, PerformanceState::Zero).ok()
                            .map(|o| (o.min_clock_offset_mhz, o.max_clock_offset_mhz))
                    } else {
                        meta.memory_offset_limits
                    };

                    let updated_meta = NvidiaMetadata {
                        vram_type,
                        vram_vendor,
                        vram_bus_width,
                        core_clock_range: core_range,
                        memory_clock_range: mem_range,
                        core_offset_limits,
                        memory_offset_limits,
                        ..meta.clone()
                    };
                    cache.insert(i, updated_meta.clone());
                    updated_meta
                } else {
                    log::debug!(target: "hw.detect", "GPU {}: Using cached metadata", i);
                    meta.clone()
                }
            } else {
                log::info!(target: "hw.detect", "GPU {}: Initializing metadata cache (first detection)", i);
                let arch = device.architecture().ok().map(|arch| arch.to_string());
                let p_states = device.supported_performance_states().ok()
                    .map(|states| states.iter().map(|s| format!("{:?}", s)).collect())
                    .unwrap_or_default();
                let p_limit_range = match device.power_management_limit_constraints() {
                    Ok(constraints) => Some((constraints.min_limit / 1000, constraints.max_limit / 1000)),
                    Err(_) => None,
                };
                let s_gpu_offset = device.clock_offset(Clock::Graphics, PerformanceState::Zero).is_ok();
                let s_mem_offset = device.clock_offset(Clock::Memory, PerformanceState::Zero).is_ok();

                let vram_total = device.memory_info().ok().map(|m| m.total / 1024 / 1024);

                let minor_number = device.minor_number().unwrap_or(i);
                log::debug!(target: "hw.detect", "GPU {}: Performing initial VRAM detection for minor {}", i, minor_number);
                let (vram_type, vram_vendor, vram_bus_width, _) = get_vram_info(minor_number);

                let core_range = get_base_gpu_clock_ranges(&device).ok();
                let mem_range = get_base_memory_clock_ranges(&device).ok();


                let core_offset_limits = device.clock_offset(Clock::Graphics, PerformanceState::Zero).ok()
                    .map(|o| (o.min_clock_offset_mhz, o.max_clock_offset_mhz));
                let memory_offset_limits = device.clock_offset(Clock::Memory, PerformanceState::Zero).ok()
                    .map(|o| (o.min_clock_offset_mhz, o.max_clock_offset_mhz));

                let meta = NvidiaMetadata {
                    architecture: arch,
                    supported_p_states: p_states,
                    power_limit_range: p_limit_range,
                    supports_gpu_offset: s_gpu_offset,
                    supports_mem_offset: s_mem_offset,
                    vram_type,
                    vram_vendor,
                    vram_bus_width,
                    vram_total,
                    core_clock_range: core_range,
                    memory_clock_range: mem_range,
                    core_offset_limits,
                    memory_offset_limits,
                };
                cache.insert(i, meta.clone());
                meta
            }
        };

        let architecture = metadata.architecture;
        let supported_p_states = metadata.supported_p_states;
        let power_limit_range = metadata.power_limit_range;
        let supports_power_limit = power_limit_range.is_some();
        let supports_gpu_offset = metadata.supports_gpu_offset;
        let supports_mem_offset = metadata.supports_mem_offset;
        let v_type = metadata.vram_type;
        let v_vendor = metadata.vram_vendor;
        let v_bus = metadata.vram_bus_width;
        let v_total = metadata.vram_total;
        // Apply current offset to the cached base range for real-time reporting
        let core_clock_range = metadata.core_clock_range.map(|(min, max)| {
            let offset = {
                let map = crate::MANUAL_GPU_OFFSETS.lock().unwrap();
                map.get(&i).map(|(c, _)| *c).unwrap_or(0.0)
            };
            ((min as f32 + offset).max(0.0) as u32, (max as f32 + offset).max(0.0) as u32)
        });
        let memory_clock_range = metadata.memory_clock_range;
        let core_offset_limits = metadata.core_offset_limits;
        let memory_offset_limits = metadata.memory_offset_limits;
        let v_bw = calculate_vram_bandwidth(v_type.as_ref(), v_bus, memory_clock_range);

        // Log VRAM info for diagnostics (rate-limited)
        static LAST_VRAM_LOG: Lazy<Mutex<HashMap<u32, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));
        let should_log_vram = {
            let mut last_log = LAST_VRAM_LOG.lock().unwrap();
            match last_log.get(&i) {
                Some(instant) if instant.elapsed() < std::time::Duration::from_secs(60) => false,
                _ => {
                    last_log.insert(i, Instant::now());
                    true
                }
            }
        };

        if should_log_vram {
            if v_type.is_some() || v_vendor.is_some() || v_bus.is_some() {
                if v_bw.is_none() && memory_clock_range.is_none() {
                    log::debug!(target: "hw.detect",
                        "GPU {}: VRAM detected - Type: {:?}, Vendor: {:?}, Bus Width: {:?} bits, Bandwidth: N/A (memory clock range unknown)",
                        i, v_type, v_vendor, v_bus);
                } else {
                    log::debug!(target: "hw.detect",
                        "GPU {}: VRAM detected - Type: {:?}, Vendor: {:?}, Bus Width: {:?} bits, Bandwidth: {:?} GB/s",
                        i, v_type, v_vendor, v_bus, v_bw);
                }
            } else {
                log::warn!(target: "hw.detect",
                    "GPU {}: VRAM info not available - Check daemon logs above for detailed error messages",
                    i);
            }
        }

        let mut gpu_info = GpuInfo {
            name: name.clone(),
            gpu_type,
            status,
            frequency,
            memory_frequency,
            temperature,
            hotspot_temperature: hotspot_temp,
            memory_temperature: memory_temp,
            load,
            power,
            voltage,
            freq_offset: None,
            drain_offset: None,
            power_offset: None,
            total_offset: None,
            min_core_clock,
            max_core_clock,
            min_memory_clock: None,
            max_memory_clock: None,
            core_clock_range,
            memory_clock_range,
            core_offset_limits,
            memory_offset_limits,
            is_desktop,
            architecture,
            nvml_index: Some(i),
            driver_version: driver_version.clone(),
            supported_p_states,
            supports_power_limit,
            power_limit_range,
            supports_gpu_offset,
            supports_mem_offset,
            fan_speed_range: if num_fans > 0 { Some((0, 100)) } else { None },
            vram_type: v_type,
            vram_vendor: v_vendor,
            vram_bus_width: v_bus,
            vram_bandwidth: v_bw,
            vram_total: v_total,
        };

        // Fill in offsets if they exist in global state (assuming first NVIDIA GPU for now)
        if name.to_lowercase().contains("nvidia") && manual_clocks_enabled {
            let stats_lock = crate::CURRENT_GPU_OVERCLOCK_STATS.lock().unwrap();
            if let Some(ref stats) = *stats_lock {
                gpu_info.freq_offset = Some(stats.freq_offset);
                gpu_info.drain_offset = Some(stats.drain_offset);
                gpu_info.power_offset = Some(stats.power_offset);
                gpu_info.total_offset = Some(stats.total_offset);
            } else {
                // Fallback to manual offsets if dynamic is not active
                let manual_map = crate::MANUAL_GPU_OFFSETS.lock().unwrap();
                if let Some(offsets) = manual_map.get(&i) {
                    let core: f32 = offsets.0;
                    gpu_info.total_offset = Some(core.round() as i32);
                }
            }
        }

        gpus.push(gpu_info);
    }

    // One-shot GetGpuInfoFull override consumed after the full pass.
    if force_full_poll {
        crate::FULL_NVML_REFRESH_REQUESTED.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    Ok(gpus)
}

fn read_gpu_frequency(device_path: &str) -> Option<u64> {
    // AMD
    if let Ok(freq_str) = fs::read_to_string(format!("{}/pp_dpm_sclk", device_path)) {
        for line in freq_str.lines() {
            if line.contains('*') {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    // Handle both "Mhz" and "MHz" patterns
                    let freq_str = parts[1].trim_end_matches("Mhz").trim_end_matches("MHz");
                    if let Ok(freq) = freq_str.parse::<u64>() {
                        return Some(freq);
                    }
                }
            }
        }
    }
    
    // Intel
    if let Ok(freq_str) = fs::read_to_string(format!("{}/gt_cur_freq_mhz", device_path)) {
        if let Ok(freq) = freq_str.trim().parse::<u64>() {
            return Some(freq);
        }
    }
    
    None
}

fn read_gpu_memory_frequency(device_path: &str) -> Option<u64> {
    // AMD - memory clock from pp_dpm_mclk
    if let Ok(freq_str) = fs::read_to_string(format!("{}/pp_dpm_mclk", device_path)) {
        for line in freq_str.lines() {
            if line.contains('*') {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    // Handle both "Mhz" and "MHz" patterns
                    let freq_str = parts[1].trim_end_matches("Mhz").trim_end_matches("MHz");
                    if let Ok(freq) = freq_str.parse::<u64>() {
                        return Some(freq);
                    }
                }
            }
        }
    }
    
    // Intel doesn't typically expose memory frequency separately
    None
}

fn read_gpu_temperature(device_path: &str) -> Option<f32> {
    // Check hwmon
    let hwmon_path = format!("{}/hwmon", device_path);
    if let Ok(entries) = fs::read_dir(&hwmon_path) {
        for entry in entries.flatten() {
            let temp_input = entry.path().join("temp1_input");
            if let Ok(temp_str) = fs::read_to_string(&temp_input) {
                if let Ok(temp) = temp_str.trim().parse::<f32>() {
                    return Some(temp / 1000.0);
                }
            }
        }
    }
    
    // AMD specific
    if let Ok(temp_str) = fs::read_to_string(format!("{}/gpu_busy_percent", device_path)) {
        if let Ok(temp) = temp_str.trim().parse::<f32>() {
            return Some(temp);
        }
    }
    
    None
}

fn read_gpu_load(device_path: &str) -> Option<f32> {
    // AMD
    if let Ok(load_str) = fs::read_to_string(format!("{}/gpu_busy_percent", device_path)) {
        if let Ok(load) = load_str.trim().parse::<f32>() {
            return Some(load);
        }
    }
    
    // Intel
    if let Ok(_load_str) = fs::read_to_string(format!("{}/gt_RP0_freq_mhz", device_path)) {
        // Intel doesn't directly expose load, would need calculation
    }
    
    None
}

fn read_gpu_power(device_path: &str) -> Option<f32> {
    let hwmon_path = format!("{}/hwmon", device_path);
    if let Ok(entries) = fs::read_dir(&hwmon_path) {
        for entry in entries.flatten() {
            // Try power1_average first
            let power_avg = entry.path().join("power1_average");
            if let Ok(power_str) = fs::read_to_string(&power_avg) {
                if let Ok(microwatts) = power_str.trim().parse::<f32>() {
                    return Some(microwatts / 1_000_000.0);
                }
            }
            
            // Try power1_input
            let power_input = entry.path().join("power1_input");
            if let Ok(power_str) = fs::read_to_string(&power_input) {
                if let Ok(microwatts) = power_str.trim().parse::<f32>() {
                    return Some(microwatts / 1_000_000.0);
                }
            }
        }
    }

    None
}

fn read_gpu_voltage(device_path: &str) -> Option<f32> {
    let hwmon_path = format!("{}/hwmon", device_path);
    if let Ok(entries) = fs::read_dir(&hwmon_path) {
        for entry in entries.flatten() {
            let voltage_input = entry.path().join("in0_input");
            if let Ok(volt_str) = fs::read_to_string(&voltage_input) {
                if let Ok(millivolts) = volt_str.trim().parse::<f32>() {
                    return Some(millivolts / 1000.0);
                }
            }
        }
    }
    None
}

// WiFi information detection
fn find_binary(cmd: &str) -> Option<String> {
    let paths = ["/usr/bin", "/usr/sbin", "/usr/local/bin", "/usr/local/sbin", "/sbin", "/bin"];
    for path in paths {
        let full_path = format!("{}/{}", path, cmd);
        if Path::new(&full_path).exists() {
            return Some(full_path);
        }
    }
    // Try default path (hope it is in PATH)
    Some(cmd.to_string())
}

fn get_pci_info(interface: &str) -> (Option<String>, Option<String>) {
    let device_path = format!("/sys/class/net/{}/device", interface);
    if let Ok(link) = fs::read_link(&device_path) {
        if let Some(pci_addr) = link.file_name().and_then(|n| n.to_str()) {
            let cmd = find_binary("lspci").unwrap_or_else(|| "lspci".to_string());
            if let Ok(output) = std::process::Command::new(cmd)
                .args(["-s", pci_addr, "-k"])
                .output()
            {
                if output.status.success() {
                    let info = String::from_utf8_lossy(&output.stdout);
                    let mut controller = None;
                    let mut subsystem = None;
                    for line in info.lines() {
                        let trimmed = line.trim();
                        if line.contains("Network controller:") {
                            controller = line.split("Network controller:").nth(1).map(|s| s.trim().to_string());
                        } else if trimmed.starts_with("Subsystem:") {
                            subsystem = trimmed.split("Subsystem:").nth(1).map(|s| s.trim().to_string());
                        }
                    }
                    return (controller, subsystem);
                }
            }
        }
    }
    (None, None)
}

pub fn get_wifi_info() -> Result<Vec<WiFiInfo>> {
    let mut wifi_devices = Vec::new();
    
    // Find WiFi network interfaces
    let net_path = Path::new("/sys/class/net");
    if !net_path.exists() {
        return Err(anyhow!("Network interfaces not found"));
    }
    
    for entry in fs::read_dir(net_path)? {
        let entry = entry?;
        let interface = entry.file_name().to_string_lossy().to_string();
        
        // Check if it's a wireless interface
        // Check /wireless (old) or /phy80211 (new)
        let wireless_path = format!("/sys/class/net/{}/wireless", interface);
        let phy_path = format!("/sys/class/net/{}/phy80211", interface);
        if !Path::new(&wireless_path).exists() && !Path::new(&phy_path).exists() {
            continue;
        }
        
        // Get driver name
        let driver_path = format!("/sys/class/net/{}/device/driver/module", interface);
        let driver = if let Ok(link) = fs::read_link(&driver_path) {
            link.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string()
        } else {
            "unknown".to_string()
        };
        
        let (driver_version, firmware_version) = read_wifi_driver_info(&interface);
        let temperature = read_wifi_temperature(&interface);
        
        // Get PCI info (Network controller and Subsystem)
        let (network_controller, subsystem) = get_pci_info(&interface);
        
        // Get all details from iw
        let (ssid, channel, channel_width, channel_freq, tx_bitrate, rx_bitrate, iw_rx_bytes, iw_tx_bytes, iw_signal) = get_wifi_details(&interface);

        // Signal level priority: iw > /proc/net/wireless
        let signal_level = iw_signal.or_else(|| read_wifi_signal(&interface));

        // RX/TX bytes priority: iw > /sys/class/net
        let (final_tx_bytes, final_rx_bytes) = match (iw_tx_bytes, iw_rx_bytes) {
            (Some(tx), Some(rx)) => (tx, rx),
            _ => read_wifi_bytes(&interface).unwrap_or((0, 0)),
        };

        // Calculate actual throughput
        let (tx_rate, rx_rate) = read_wifi_rates(&interface, final_tx_bytes, final_rx_bytes);
        
        log::debug!(target: "hw.detect", "WiFi {} details: SSID={:?}, Signal={:?}, Channel={:?}, Rates={:?}/{:?}",
                   interface, ssid, signal_level, channel, tx_rate, rx_rate);

        wifi_devices.push(WiFiInfo {
            interface,
            driver,
            driver_version,
            firmware_version,
            temperature,
            signal_level,
            channel,
            channel_width,
            channel_freq,
            tx_rate,
            rx_rate,
            ssid,
            tx_bitrate,
            rx_bitrate,
            rx_bytes: Some(final_rx_bytes),
            tx_bytes: Some(final_tx_bytes),
            network_controller,
            subsystem,
        });
    }
    
    if wifi_devices.is_empty() {
        return Err(anyhow!("No WiFi devices found"));
    }
    
    Ok(wifi_devices)
}

fn get_wifi_details(interface: &str) -> (
    Option<String>,
    Option<u32>,
    Option<u32>,
    Option<u32>,
    Option<f64>,
    Option<f64>,
    Option<u64>,
    Option<u64>,
    Option<i32>
) {
    let mut ssid = None;
    let mut channel = None;
    let mut width = None;
    let mut freq = None;
    let mut tx_bitrate = None;
    let mut rx_bitrate = None;
    let mut rx_bytes = None;
    let mut tx_bytes = None;
    let mut signal = None;

    // Try to get connection info from iw dev <interface> link
    let iw_cmd = find_binary("iw").unwrap_or_else(|| "iw".to_string());
    if let Ok(output) = std::process::Command::new(&iw_cmd)
        .args(["dev", interface, "link"])
        .output()
    {
        if output.status.success() {
            let info = String::from_utf8_lossy(&output.stdout);
            log::debug!("iw link output for {}: {}", interface, info);
            
            for line in info.lines() {
                let trimmed = line.trim();
                let lower = trimmed.to_lowercase();
                
                if let Some(pos) = lower.find("ssid:") {
                    ssid = normalize_ssid(trimmed[pos + 5..].trim());
                } else if let Some(pos) = lower.find("freq:") {
                    let part = trimmed[pos + 5..].trim();
                    freq = part.split_whitespace().next().and_then(|s| s.parse().ok());
                } else if lower.contains("rx bitrate:") {
                    rx_bitrate = parse_wifi_rate(trimmed);
                } else if lower.contains("tx bitrate:") {
                    tx_bitrate = parse_wifi_rate(trimmed);
                } else if let Some(pos) = lower.find("rx:") {
                    let part = trimmed[pos + 3..].trim();
                    rx_bytes = part.split_whitespace().next().and_then(|s| s.parse().ok());
                } else if let Some(pos) = lower.find("tx:") {
                    let part = trimmed[pos + 3..].trim();
                    tx_bytes = part.split_whitespace().next().and_then(|s| s.parse().ok());
                } else if let Some(pos) = lower.find("signal:") {
                    let part = trimmed[pos + 7..].trim();
                    signal = part.split_whitespace().next().and_then(|s| s.parse().ok());
                }
            }
        }
    }

    // Get channel and width from iw dev <interface> info
    if let Ok(output) = std::process::Command::new(&iw_cmd)
        .args(["dev", interface, "info"])
        .output()
    {
        if output.status.success() {
            let info = String::from_utf8_lossy(&output.stdout);
            log::debug!("iw info output for {}: {}", interface, info);
            
            for line in info.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("channel") {
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    
                    // Parse channel number
                    if let Some(ch_str) = parts.get(1) {
                        if let Ok(ch) = ch_str.parse::<u32>() {
                            channel = Some(ch);
                        }
                    }
                    
                    // Parse channel width
                    if let Some(pos) = trimmed.find("width:") {
                        if let Some(width_str) = trimmed[pos + 6..].split_whitespace().next() {
                            if let Ok(w) = width_str.parse::<u32>() {
                                width = Some(w);
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: try iwgetid for SSID if not found
    if ssid.is_none() {
        if let Some(cmd) = find_binary("iwgetid") {
            if let Ok(output) = std::process::Command::new(cmd)
                .arg("-r")
                .arg(interface)
                .output()
            {
                if output.status.success() {
                    let value = String::from_utf8_lossy(&output.stdout);
                    ssid = normalize_ssid(&value);
                }
            }
        }
    }

    // Last resort: try iwconfig for SSID
    if ssid.is_none() {
        if let Some(cmd) = find_binary("iwconfig") {
            if let Ok(output) = std::process::Command::new(cmd)
                .arg(interface)
                .output()
            {
                if output.status.success() {
                    let info = String::from_utf8_lossy(&output.stdout);
                    for line in info.lines() {
                        if let Some(pos) = line.find("ESSID:") {
                            let value = line[pos + 6..].trim().trim_matches('"');
                            ssid = normalize_ssid(value);
                            break;
                        }
                    }
                }
            }
        }
    }

    (ssid, channel, width, freq, tx_bitrate, rx_bitrate, rx_bytes, tx_bytes, signal)
}

fn read_wifi_temperature(interface: &str) -> Option<f32> {
    // Try device-specific hwmon first
    let temp_path = format!("/sys/class/net/{}/device/hwmon", interface);
    if let Ok(hwmon_entries) = fs::read_dir(&temp_path) {
        for hwmon_entry in hwmon_entries.flatten() {
            let temp_input_path = hwmon_entry.path().join("temp1_input");
            if let Ok(temp_str) = fs::read_to_string(&temp_input_path) {
                if let Ok(temp_millidegrees) = temp_str.trim().parse::<i32>() {
                    return Some(temp_millidegrees as f32 / 1000.0);
                }
            }
        }
    }

    // Fallback: search all hwmons for one associated with this device
    let device_path = format!("/sys/class/net/{}/device", interface);
    if let Ok(net_dev_path) = fs::canonicalize(&device_path) {
        if let Ok(hwmon_dir) = fs::read_dir("/sys/class/hwmon") {
            for entry in hwmon_dir.flatten() {
                if let Ok(hwmon_dev_path) = fs::canonicalize(entry.path().join("device")) {
                    if hwmon_dev_path == net_dev_path {
                        if let Ok(temp_str) = fs::read_to_string(entry.path().join("temp1_input")) {
                            if let Ok(temp_millidegrees) = temp_str.trim().parse::<i32>() {
                                return Some(temp_millidegrees as f32 / 1000.0);
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback 2: thermal zones
    if let Ok(thermal_dir) = fs::read_dir("/sys/class/thermal") {
        for entry in thermal_dir.flatten() {
            if let Ok(type_str) = fs::read_to_string(entry.path().join("type")) {
                let type_lower = type_str.to_lowercase();
                if type_lower.contains("wifi") || type_lower.contains("iwlwifi") {
                    if let Ok(temp_str) = fs::read_to_string(entry.path().join("temp")) {
                        if let Ok(temp_millidegrees) = temp_str.trim().parse::<i32>() {
                            return Some(temp_millidegrees as f32 / 1000.0);
                        }
                    }
                }
            }
        }
    }

    None
}

fn read_wifi_signal(interface: &str) -> Option<i32> {
    if let Ok(wireless) = fs::read_to_string("/proc/net/wireless") {
        for line in wireless.lines().skip(2) {
            let trimmed = line.trim();
            if trimmed.starts_with(interface) {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 4 {
                    if let Ok(signal) = parts[3].trim_end_matches('.').parse::<i32>() {
                        return Some(signal);
                    }
                }
            }
        }
    }
    None
}

fn read_wifi_driver_info(interface: &str) -> (Option<String>, Option<String>) {
    let ethtool_cmd = find_binary("ethtool").unwrap_or_else(|| "ethtool".to_string());
    if let Ok(output) = std::process::Command::new(ethtool_cmd)
        .args(["-i", interface])
        .output()
    {
        if output.status.success() {
            let info = String::from_utf8_lossy(&output.stdout);
            let mut driver_version = None;
            let mut firmware_version = None;

            for line in info.lines() {
                let trimmed = line.trim();
                let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
                if parts.len() == 2 {
                    let key = parts[0].trim().to_lowercase();
                    let value = parts[1].trim();
                    if key == "version" {
                        driver_version = normalize_ssid(value);
                    } else if key == "firmware-version" {
                        firmware_version = normalize_ssid(value);
                    }
                }
            }

            return (driver_version, firmware_version);
        }
    }

    (None, None)
}

fn normalize_ssid(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_matches('"');
    if trimmed.is_empty() || trimmed == "off/any" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_wifi_rate(line: &str) -> Option<f64> {
    // Make sure there is a space after bitrate:
    let sanitized = line.replace("bitrate:", "bitrate: ");
    let parts: Vec<&str> = sanitized.split_whitespace().collect();
    let rate_index = parts.iter().position(|part| *part == "bitrate:");
    let value = rate_index
        .and_then(|idx| parts.get(idx + 1))
        .and_then(|value| value.parse::<f64>().ok())?;
    let unit = rate_index
        .and_then(|idx| parts.get(idx + 2))
        .copied()
        .unwrap_or("MBit/s")
        .trim_end_matches(',');

    Some(match unit {
        "Gbit/s" | "Gbit/sec" | "GBit/s" => value * 1000.0,
        "Kbit/s" | "Kbit/sec" | "KBit/s" => value / 1000.0,
        _ => value,
    })
}

fn read_wifi_bytes(interface: &str) -> Option<(u64, u64)> {
    let tx_bytes_path = format!("/sys/class/net/{}/statistics/tx_bytes", interface);
    let rx_bytes_path = format!("/sys/class/net/{}/statistics/rx_bytes", interface);
    let tx_bytes = fs::read_to_string(tx_bytes_path).ok()?.trim().parse().ok()?;
    let rx_bytes = fs::read_to_string(rx_bytes_path).ok()?.trim().parse().ok()?;
    Some((tx_bytes, rx_bytes))
}

fn read_wifi_rates(interface: &str, tx_bytes: u64, rx_bytes: u64) -> (Option<f64>, Option<f64>) {
    let now = Instant::now();
    let mut stats = PREVIOUS_NET_STATS.lock().unwrap();
    let rates = if let Some(prev) = stats.get(interface) {
        let elapsed = now.duration_since(prev.timestamp).as_secs_f64();
        if elapsed > 0.0 {
            let tx_rate = (tx_bytes.saturating_sub(prev.tx_bytes) as f64 * BITS_PER_BYTE)
                / elapsed
                / BITS_PER_MEGABIT;
            let rx_rate = (rx_bytes.saturating_sub(prev.rx_bytes) as f64 * BITS_PER_BYTE)
                / elapsed
                / BITS_PER_MEGABIT;
            (Some(tx_rate), Some(rx_rate))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    stats.insert(
        interface.to_string(),
        NetStats {
            rx_bytes,
            tx_bytes,
            timestamp: now,
        },
    );

    rates
}

fn normalize_uid(uid: String) -> String {
    let trimmed = uid.trim();
    // Check if it looks like a MAC address (6 pairs of hex digits separated by colons or dashes, or just 12 hex digits)
    let is_mac = (trimmed.len() == 17 && (trimmed.contains(':') || trimmed.contains('-'))) ||
                 (trimmed.len() == 12 && trimmed.chars().all(|c| c.is_ascii_hexdigit()));

    if is_mac {
        trimmed.replace(':', "").replace('-', "").to_lowercase()
    } else {
        trimmed.to_string()
    }
}

pub fn get_gamepad_info() -> Result<Vec<GamepadInfo>> {
    let mut gamepads = Vec::new();
    let mut seen_uids = std::collections::HashSet::new();

    if let Ok(entries) = fs::read_dir("/sys/class/input") {
        for entry in entries.flatten() {
            let input_name = entry.file_name().to_string_lossy().to_string();
            if input_name.starts_with("input") {
                let path = entry.path();

                // Find event child to check udev properties
                let mut is_gamepad = false;
                let mut udev_uid = None;
                if let Ok(children) = fs::read_dir(&path) {
                    for child in children.flatten() {
                        let child_name = child.file_name().to_string_lossy().to_string();
                        if child_name.starts_with("event") {
                            if let Ok(uevent) = fs::read_to_string(child.path().join("uevent")) {
                                let mut major = None;
                                let mut minor = None;
                                for line in uevent.lines() {
                                    if let Some(val) = line.strip_prefix("MAJOR=") {
                                        major = Some(val);
                                    } else if let Some(val) = line.strip_prefix("MINOR=") {
                                        minor = Some(val);
                                    }
                                }

                                if let (Some(maj), Some(min)) = (major, minor) {
                                    let udev_path = format!("/run/udev/data/c{}:{}", maj, min);
                                    if let Ok(udev_data) = fs::read_to_string(udev_path) {
                                        if udev_data.contains("E:ID_INPUT_JOYSTICK=1") {
                                            is_gamepad = true;
                                        }

                                        // Try to find a stable UID from udev data
                                        let mut id_path = None;
                                        let mut id_serial = None;
                                        let mut id_serial_short = None;
                                        for line in udev_data.lines() {
                                            if let Some(val) = line.strip_prefix("E:ID_PATH=") {
                                                id_path = Some(val.to_string());
                                            } else if let Some(val) = line.strip_prefix("E:ID_SERIAL_SHORT=") {
                                                id_serial_short = Some(val.to_string());
                                            } else if let Some(val) = line.strip_prefix("E:ID_SERIAL=") {
                                                id_serial = Some(val.to_string());
                                            }
                                        }
                                        // Priority: Serial Short > Serial > Path
                        udev_uid = id_serial_short.or(id_serial).or(id_path).map(normalize_uid);

                                        if is_gamepad {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Fallback to name heuristic if udev info is missing or not joystick
                if !is_gamepad {
                    if let Ok(device_name) = fs::read_to_string(path.join("name")) {
                        let device_name_lower = device_name.to_lowercase();
                        if (device_name_lower.contains("controller") ||
                            device_name_lower.contains("gamepad") ||
                            device_name_lower.contains("joystick")) &&
                           !device_name_lower.contains("keyboard") {
                            is_gamepad = true;
                        }
                    }
                }

                // Apply exclusions based on name (even if udev says joystick)
                if is_gamepad {
                    if let Ok(device_name) = fs::read_to_string(path.join("name")) {
                        let device_name_lower = device_name.to_lowercase();
                        if device_name_lower.contains("touchpad") ||
                           device_name_lower.contains("motion sensors") ||
                           device_name_lower.contains("consumer control") ||
                           device_name_lower.contains("system control") {
                            is_gamepad = false;
                        }
                    }
                }

                if is_gamepad {
                    if let Ok(device_name) = fs::read_to_string(path.join("name")) {
                        let device_name = device_name.trim().to_string();
                        // Use sysfs path as absolute fallback for UID if udev failed
                        let mut uid = udev_uid.unwrap_or_else(|| path.to_string_lossy().to_string());

                        // Try to get uniq (MAC address) which is very stable across connection types
                        if let Ok(uniq) = fs::read_to_string(path.join("device/uniq")) {
                            let uniq = uniq.trim();
                            if !uniq.is_empty() && uniq != "00:00:00:00:00:00" {
                                uid = normalize_uid(uniq.to_string());
                            }
                        }

                        if !seen_uids.contains(&uid) {
                            let bustype = fs::read_to_string(path.join("id/bustype"))
                                .ok()
                                .and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
                                .unwrap_or(0);

                            let connection_type = match bustype {
                                0x03 => ConnectionType::Wired,
                                0x05 => ConnectionType::Wireless,
                                _ => ConnectionType::Unknown,
                            };

                            let (battery_level, power_status) = find_battery_for_input(&path);

                            gamepads.push(GamepadInfo {
                                name: device_name,
                                id: input_name,
                                uid: uid.clone(),
                                status: GamepadStatus::Connected,
                                battery_level,
                                connection_type,
                                power_status,
                            });
                            seen_uids.insert(uid);
                        }
                    }
                }
            }
        }
    }

    Ok(gamepads)
}

fn find_battery_for_input(input_path: &Path) -> (Option<u8>, PowerStatus) {
    if let Ok(device_path) = fs::canonicalize(input_path.join("device")) {
        let mut current = Some(device_path.as_path());
        while let Some(path) = current {
            let ps_path = path.join("power_supply");
            if ps_path.exists() {
                if let Ok(ps_entries) = fs::read_dir(ps_path) {
                    for ps_entry in ps_entries.flatten() {
                        let mut level = None;
                        let mut status = PowerStatus::Unknown;
                        if let Ok(cap) = fs::read_to_string(ps_entry.path().join("capacity")) {
                            level = cap.trim().parse().ok();
                        }
                        if let Ok(st) = fs::read_to_string(ps_entry.path().join("status")) {
                            status = match st.trim().to_lowercase().as_str() {
                                "charging" => PowerStatus::Charging,
                                "discharging" => PowerStatus::Discharging,
                                "full" => PowerStatus::Full,
                                _ => PowerStatus::Unknown,
                            };
                        }
                        return (level, status);
                    }
                }
            }

            // Also check for power_supply as a sibling in some cases or child of parent
            current = path.parent();
            if let Some(p) = current {
                if p == Path::new("/sys/devices") || p == Path::new("/sys") {
                    break;
                }
            }
        }
    }

    // Fallback: search all power supplies for names matching the input device
    if let Ok(ps_entries) = fs::read_dir("/sys/class/power_supply") {
        for ps_entry in ps_entries.flatten() {
            let ps_name = ps_entry.file_name().to_string_lossy().to_lowercase();
            if ps_name.contains("controller") || ps_name.contains("gamepad") {
                 let mut level = None;
                 let mut status = PowerStatus::Unknown;
                 if let Ok(cap) = fs::read_to_string(ps_entry.path().join("capacity")) {
                     level = cap.trim().parse().ok();
                 }
                 if let Ok(st) = fs::read_to_string(ps_entry.path().join("status")) {
                     status = match st.trim().to_lowercase().as_str() {
                         "charging" => PowerStatus::Charging,
                         "discharging" => PowerStatus::Discharging,
                         "full" => PowerStatus::Full,
                         _ => PowerStatus::Unknown,
                     };
                 }
                 return (level, status);
            }
        }
    }

    (None, PowerStatus::Unknown)
}

pub fn get_battery_info() -> Result<BatteryInfo> {
    let base = if Path::new("/sys/class/power_supply/BAT0").exists() {
        "/sys/class/power_supply/BAT0"
    } else if Path::new("/sys/class/power_supply/BAT1").exists() {
        "/sys/class/power_supply/BAT1"
    } else {
        return Err(anyhow!("No battery found"));
    };

    let status = read_sysfs_string(&format!("{}/status", base)).unwrap_or_else(|_| "Unknown".to_string());

    let charge_full = read_sysfs_u64(&format!("{}/charge_full", base)).or_else(|_| read_sysfs_u64(&format!("{}/energy_full", base))).ok();
    let charge_full_design = read_sysfs_u64(&format!("{}/charge_full_design", base)).or_else(|_| read_sysfs_u64(&format!("{}/energy_full_design", base))).ok();

    let battery_health = if let (Some(full), Some(design)) = (charge_full, charge_full_design) {
        if design > 0 {
            Some((full as f32 / design as f32) * 100.0)
        } else {
            None
        }
    } else {
        None
    };

    Ok(BatteryInfo {
        status,
        voltage_mv: read_sysfs_u64(&format!("{}/voltage_now", base))? / 1000,
        current_ma: read_sysfs_i64(&format!("{}/current_now", base))? / 1000,
        charge_percent: read_sysfs_u64(&format!("{}/capacity", base))?,
        capacity_mah: charge_full.unwrap_or(0) / 1000,
        battery_health,
        manufacturer: read_sysfs_string(&format!("{}/manufacturer", base))?,
        model: read_sysfs_string(&format!("{}/model_name", base))?,
        charge_start_threshold: read_sysfs_u64(&format!("{}/charge_control_start_threshold", base)).ok().map(|v| v as u8),
        charge_end_threshold: read_sysfs_u64(&format!("{}/charge_control_end_threshold", base)).ok().map(|v| v as u8),
    })
}

pub fn get_mount_info() -> Result<Vec<MountInfo>> {
    let sys = System::new();
    let mut mounts_info = Vec::new();

    if let Ok(mounts) = sys.mounts() {
        for mount in mounts.iter().filter(|m| m.fs_mounted_on == "/" || m.fs_mounted_on == "/home") {
            let total = mount.total.as_u64();
            let avail = mount.avail.as_u64();
            let used = total - avail;
            let used_percent = if total > 0 { (used as f64 / total as f64) * 100.0 } else { 0.0 };

            mounts_info.push(MountInfo {
                mount_point: mount.fs_mounted_on.clone(),
                filesystem_type: mount.fs_type.clone(),
                total_gb: total / 1_000_000_000,
                used_gb: used / 1_000_000_000,
                used_percent,
            });
        }
    }

    Ok(mounts_info)
}

fn read_sysfs_u64(path: &str) -> Result<u64> {
    Ok(fs::read_to_string(path)?.trim().parse()?)
}

fn read_sysfs_i64(path: &str) -> Result<i64> {
    Ok(fs::read_to_string(path)?.trim().parse()?)
}

fn read_sysfs_string(path: &str) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

fn find_storage_temperature(block_device_path: &Path) -> Option<f32> {
    // Strategy 1: Check device/hwmon
    if let Ok(hwmon_entries) = std::fs::read_dir(block_device_path.join("device/hwmon")) {
        for hwmon_entry in hwmon_entries.flatten() {
            if let Some(temp) = read_hwmon_storage_temp(&hwmon_entry.path()) {
                return Some(temp);
            }
        }
    }

    // Strategy 2: Check device/device/hwmon (common for NVMe)
    if let Ok(hwmon_entries) = std::fs::read_dir(block_device_path.join("device/device/hwmon")) {
        for hwmon_entry in hwmon_entries.flatten() {
            if let Some(temp) = read_hwmon_storage_temp(&hwmon_entry.path()) {
                return Some(temp);
            }
        }
    }

    // Strategy 3: Global search for hwmon associated with this device
    if let Ok(device_link) = fs::canonicalize(block_device_path.join("device")) {
        if let Ok(hwmon_dir) = fs::read_dir("/sys/class/hwmon") {
            for entry in hwmon_dir.flatten() {
                if let Ok(hwmon_device_link) = fs::canonicalize(entry.path().join("device")) {
                    // Check if hwmon device is same as or parent of block device
                    if device_link.starts_with(&hwmon_device_link) || hwmon_device_link.starts_with(&device_link) {
                        if let Some(temp) = read_hwmon_storage_temp(&entry.path()) {
                            return Some(temp);
                        }
                    }
                }
            }
        }
    }

    None
}

fn read_hwmon_storage_temp(path: &Path) -> Option<f32> {
    // Try temp1_input, then temp2_input (sometimes composite)
    for i in 1..=3 {
        let temp_path = path.join(format!("temp{}_input", i));
        if let Ok(temp_str) = fs::read_to_string(&temp_path) {
            if let Ok(temp_millidegrees) = temp_str.trim().parse::<i32>() {
                return Some(temp_millidegrees as f32 / 1000.0);
            }
        }
    }
    None
}

fn read_sector_size(path: &Path) -> u64 {
    fs::read_to_string(path.join("queue/hw_sector_size"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(512)
}

fn read_storage_stats(path: &Path) -> Option<(u64, u64, u64, u64)> {
    let stats = fs::read_to_string(path.join("stat")).ok()?;
    let parts: Vec<&str> = stats.split_whitespace().collect();
    if parts.len() < 7 {
        return None;
    }
    let read_ios = parts.first()?.parse::<u64>().ok()?;
    let read_sectors = parts.get(2)?.parse::<u64>().ok()?;
    let write_ios = parts.get(4)?.parse::<u64>().ok()?;
    let write_sectors = parts.get(6)?.parse::<u64>().ok()?;
    Some((read_ios, read_sectors, write_ios, write_sectors))
}

fn calculate_storage_rates(
    device: &str,
    read_ios: u64,
    read_sectors: u64,
    write_ios: u64,
    write_sectors: u64,
    sector_size: u64,
) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    let now = Instant::now();
    let mut stats = PREVIOUS_STORAGE_STATS.lock().unwrap();
    let rates = if let Some(prev) = stats.get(device) {
        let elapsed = now.duration_since(prev.timestamp).as_secs_f64();
        if elapsed > 0.0 {
            let read_bytes = read_sectors.saturating_sub(prev.read_sectors) as f64 * sector_size as f64;
            let write_bytes = write_sectors.saturating_sub(prev.write_sectors) as f64 * sector_size as f64;
            let read_speed = read_bytes / elapsed / 1_000_000.0;
            let write_speed = write_bytes / elapsed / 1_000_000.0;
            let read_iops = read_ios.saturating_sub(prev.read_ios) as f64 / elapsed;
            let write_iops = write_ios.saturating_sub(prev.write_ios) as f64 / elapsed;
            (Some(read_speed), Some(write_speed), Some(read_iops), Some(write_iops))
        } else {
            (None, None, None, None)
        }
    } else {
        (None, None, None, None)
    };

    stats.insert(
        device.to_string(),
        StorageStats {
            read_ios,
            read_sectors,
            write_ios,
            write_sectors,
            timestamp: now,
        },
    );

    rates
}

pub fn get_storage_device_info() -> Result<Vec<StorageDevice>> {
    let mut storage_devices = Vec::new();

    for entry in std::fs::read_dir("/sys/block")? {
        let entry = entry?;
        let dev_name = entry.file_name().to_string_lossy().to_string();

        if dev_name.starts_with("loop") || dev_name.starts_with("ram") {
            continue;
        }

        let path = entry.path();
        let model = std::fs::read_to_string(path.join("device/model"))
            .unwrap_or_else(|_| dev_name.clone())
            .trim()
            .to_string();

        let size_gb = if let Ok(size_str) = std::fs::read_to_string(path.join("size")) {
            if let Ok(sectors) = size_str.trim().parse::<u64>() {
                (sectors * 512) / 1_000_000_000
            } else {
                0
            }
        } else {
            0
        };

        let sector_size = read_sector_size(&path);
        let (read_speed, write_speed, read_iops, write_iops) = match read_storage_stats(&path) {
            Some((read_ios, read_sectors, write_ios, write_sectors)) => {
                calculate_storage_rates(&dev_name, read_ios, read_sectors, write_ios, write_sectors, sector_size)
            }
            None => (None, None, None, None),
        };

        // Try to read temperature from hwmon
        let temperature = find_storage_temperature(&path);

        storage_devices.push(StorageDevice {
            device: format!("/dev/{}", dev_name),
            model,
            size_gb,
            temperature,
            read_speed,
            write_speed,
            read_iops,
            write_iops,
        });
    }

    Ok(storage_devices)
}

#[cfg(test)]
mod wifi_tests {
    use super::*;

    #[test]
    fn test_parse_wifi_rate() {
        assert_eq!(parse_wifi_rate("rx bitrate: 1.1 MBit/s"), Some(1.1));
        assert_eq!(parse_wifi_rate("tx bitrate: 2.2 MBit/s"), Some(2.2));
        assert_eq!(parse_wifi_rate("tx bitrate:3.3 MBit/s"), Some(3.3));
        assert_eq!(parse_wifi_rate("rx bitrate: 4.4 GBit/s"), Some(4400.0));
    }

    #[test]
    fn test_normalize_ssid() {
        assert_eq!(normalize_ssid(" \"Generic-SSID\" "), Some("Generic-SSID".to_string()));
        assert_eq!(normalize_ssid(" Random SSID "), Some("Random SSID".to_string()));
        assert_eq!(normalize_ssid(" off/any "), None);
        assert_eq!(normalize_ssid(""), None);
    }

    #[test]
    fn test_ethtool_parsing() {
        let output = "driver: iwlwifi\nversion: 6.8.0-90-generic\nfirmware-version: 77.b405f9d4.0 cc-a0-77.ucode\nbus-info: 0000:02:00.0\nsupports-statistics: yes\nsupports-test: yes\nsupports-eeprom-access: no\nsupports-register-dump: yes\nsupports-priv-flags: no";

        let mut driver_version = None;
        let mut firmware_version = None;

        for line in output.lines() {
            let trimmed = line.trim();
            let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
            if parts.len() == 2 {
                let key = parts[0].trim().to_lowercase();
                let value = parts[1].trim();
                if key == "version" {
                    driver_version = normalize_ssid(value);
                } else if key == "firmware-version" {
                    firmware_version = normalize_ssid(value);
                }
            }
        }

        assert_eq!(driver_version, Some("6.8.0-90-generic".to_string()));
        assert_eq!(firmware_version, Some("77.b405f9d4.0 cc-a0-77.ucode".to_string()));
    }
}

// ---------------------------------------------------------------------------
// RTD3 hybrid-strategy hardware-in-the-loop test.
//
// Requires: NVIDIA dGPU present with runtime PM enabled. Exercises the REAL
// detection path at the REAL 1 Hz cadence — no NVML mocks.
//
// Acceptance (user spec):
//   1. With GPU active and idle, repeated get_nvidia_gpu_info(false) calls
//      must NOT keep resetting the autosuspend timer -> runtime_status
//      transitions to "suspended" within ~21 s + margin.
//   2. GetGpuInfoFull override wakes/queries and re-arms the quiet window.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod rtd3_hybrid_tests {
    // Serializes the HIL tests against each other: both mutate process-global
    // state (FULL_NVML_REFRESH_REQUESTED, IDLE_METRICS_CACHE) and cargo runs
    // test threads in parallel by default. Production has a single monitor
    // thread, so this lock guards tests only.
    static HIL_GLOBAL_STATE_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
        once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

    use super::*;

    fn runtime_status() -> String {
        std::fs::read_to_string(
            "/sys/bus/pci/devices/0000:01:00.0/power/runtime_status",
        )
        .unwrap_or_else(|_| "unknown".into())
        .trim()
        .to_string()
    }

    #[test]
    #[ignore] // HIL: run explicitly with `cargo test -p lapsphere-daemon -- --ignored`
    fn hil_rtd3_idle_gate_transitions_to_suspended_and_override_wakes()
    {
        let _hil_guard = HIL_GLOBAL_STATE_LOCK.lock().unwrap();

        // ---- Phase 0: baseline ------------------------------------------
        // Make the gate's "bypassing NVML (RTD3 idle tier)" debug line
        // visible so the run proves the idle tier actually short-circuits
        // (vs. silently full-polling every tick, which looks identical in
        // runtime_status alone). Run with RUST_LOG=hw.detect=debug.
        let _ = env_logger::Builder::from_env(env_logger::Env::default())
            .is_test(true)
            .try_init();
        IDLE_METRICS_CACHE.lock().unwrap().clear();
        let _ = crate::FULL_NVML_REFRESH_REQUESTED.swap(
            true,
            std::sync::atomic::Ordering::Relaxed,
        );

        // Phase 1 — bootstrap pass (cold cache): full poll authorized,
        // populates IDLE_METRICS_CACHE.
        let t0 = std::time::Instant::now();
        let gpus = get_nvidia_gpu_info().expect("bootstrap full poll failed");
        assert!(!gpus.is_empty(), "no GPUs returned");
        assert!(
            IDLE_METRICS_CACHE.lock().unwrap().contains_key(&0),
            "idle cache not populated by bootstrap pass"
        );
        println!("[+] bootstrap pass ok in {:?}, status={}", t0.elapsed(), runtime_status());

        // ---- Phase 2: idle gate — 1 Hz ticks, ZERO further NVML ---------
        // If any tick touched NVML, the 20 s timer would reset and the
        // status could never reach "suspended".
        let mut saw_cached = false;
        for tick in 0..26 {
            let gpus = get_nvidia_gpu_info().unwrap();
            if let Some(g) = gpus.iter().find(|g| g.gpu_type == GpuType::Discrete) {
                if tick < 2 {
                    // While still fresh+active, values must come from cache.
                    assert!(
                        g.voltage.is_some() || g.temperature.is_some(),
                        "tick {}: expected last-known cached metrics, got all-None",
                        tick
                    );
                    saw_cached = true;
                }
            }
            let status = runtime_status();
            println!("[tick {:>2}] {:>4.1}s status={}", tick, t0.elapsed().as_secs_f32(), status);
            if status == "suspended" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }

        let suspended_at = t0.elapsed();
        assert_eq!(
            runtime_status(),
            "suspended",
            "GPU did NOT suspend within ~26 s of idle ticks — NVML pin survived"
        );
        assert!(
            suspended_at.as_secs() <= 30,
            "suspension took {:?}, expected ~21 s window",
            suspended_at
        );
        assert!(saw_cached, "idle tier never served cached metrics");
        println!("[+] ACCEPTANCE 1 MET: active -> suspended in {:?}", suspended_at);

        // ---- Phase 3: suspended stub path serves last-known --------------
        let gpus = get_nvidia_gpu_info().unwrap();
        let g = gpus.iter().find(|g| g.gpu_type == GpuType::Discrete).unwrap();
        assert_eq!(g.status, "suspended");
        assert_eq!(runtime_status(), "suspended", "stub path woke the GPU");
        println!("[+] suspended path ok, temp={:?} (last-known)", g.temperature);

        // ---- Phase 4: on-demand full override ---------------------------
        crate::FULL_NVML_REFRESH_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
        let before = runtime_status();
        let gpus = get_nvidia_gpu_info().unwrap(); // flag consumed inside
        assert!(
            !crate::FULL_NVML_REFRESH_REQUESTED.load(std::sync::atomic::Ordering::Relaxed),
            "override flag not consumed"
        );
        let g = gpus.iter().find(|g| g.gpu_type == GpuType::Discrete).unwrap();
        println!(
            "[+] override pass ok: was {} -> now {}, voltage={:?}",
            before, runtime_status(), g.voltage
        );
    }

    /// Override-path coverage that does NOT require suspension (runnable even
    /// while nvidia-persistenced pins runtime_usage): cold cache -> bootstrap
    /// full poll -> forced one-shot re-poll consumes the flag and refreshes
    /// the snapshot timestamp.
    #[test]
    #[ignore]
    fn hil_rtd3_override_flag_consumed_and_snapshot_rearmed()
    {
        let _hil_guard = HIL_GLOBAL_STATE_LOCK.lock().unwrap();

        let _ = env_logger::Builder::from_env(env_logger::Env::default())
            .is_test(true)
            .try_init();

        IDLE_METRICS_CACHE.lock().unwrap().clear();
        crate::FULL_NVML_REFRESH_REQUESTED.store(false, std::sync::atomic::Ordering::Relaxed);

        // Bootstrap: cold cache forces a full NVML pass and populates cache.
        let gpus = get_nvidia_gpu_info().expect("bootstrap poll failed");
        assert!(!gpus.is_empty());
        assert!(
            IDLE_METRICS_CACHE.lock().unwrap().contains_key(&0),
            "cache not populated"
        );

        // Idle tier engages on next tick (fresh snapshot, util==0 observed):
        // this must NOT consume a not-requested flag nor touch NVML.
        let _ = get_nvidia_gpu_info().unwrap();
        assert!(
            !crate::FULL_NVML_REFRESH_REQUESTED.load(std::sync::atomic::Ordering::Relaxed),
            "idle tick must not set the override flag"
        );

        // Force one-shot override: consumed exactly once, snapshot re-armed.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        crate::FULL_NVML_REFRESH_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = get_nvidia_gpu_info().unwrap();
        assert!(
            !crate::FULL_NVML_REFRESH_REQUESTED.load(std::sync::atomic::Ordering::Relaxed),
            "override flag not consumed by forced pass"
        );
        println!("[+] override consumed + snapshot re-armed; status={}", runtime_status());

        // Subsequent tick returns to quiet tier again (fresh timestamp).
        let _ = get_nvidia_gpu_info().unwrap();
        println!("[+] back to idle tier; final status={}", runtime_status());
    }
}
