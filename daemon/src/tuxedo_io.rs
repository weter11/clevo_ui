use anyhow::{anyhow, Result};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use nix::errno::Errno;
use nix::libc;

const TUXEDO_IO_DEVICE: &str = "/dev/tuxedo_io";
const IOCTL_MAGIC: u8 = 0xEC;
const MAGIC_READ_CL: u8 = IOCTL_MAGIC + 1;
const MAGIC_WRITE_CL: u8 = IOCTL_MAGIC + 2;
const MAGIC_READ_UW: u8 = IOCTL_MAGIC + 3;
const MAGIC_WRITE_UW: u8 = IOCTL_MAGIC + 4;

// Hardware check ioctls
// nix::ioctl_read!(ioctl_cl_hw_interface_id, MAGIC_READ_CL, 0x00, [u8; 30]);
// nix::ioctl_read!(ioctl_hwcheck_cl, IOCTL_MAGIC, 0x05, i32);
// nix::ioctl_read!(ioctl_hwcheck_uw, IOCTL_MAGIC, 0x06, i32);

// Clevo read ioctls
// nix::ioctl_read!(ioctl_cl_faninfo1, MAGIC_READ_CL, 0x10, i32);
// nix::ioctl_read!(ioctl_cl_faninfo2, MAGIC_READ_CL, 0x11, i32);
// nix::ioctl_read!(ioctl_cl_faninfo3, MAGIC_READ_CL, 0x12, i32);
// nix::ioctl_read!(ioctl_cl_webcam_sw, MAGIC_READ_CL, 0x13, i32);

// Clevo write ioctls
// nix::ioctl_write_ptr!(ioctl_cl_fanspeed, MAGIC_WRITE_CL, 0x10, i32);
// nix::ioctl_write_ptr!(ioctl_cl_fanauto, MAGIC_WRITE_CL, 0x11, i32);
// nix::ioctl_write_ptr!(ioctl_cl_webcam_sw_w, MAGIC_WRITE_CL, 0x12, i32);
// nix::ioctl_write_ptr!(ioctl_cl_perf_profile, MAGIC_WRITE_CL, 0x15, i32);

// Uniwill read ioctls
// nix::ioctl_read!(ioctl_uw_fanspeed, MAGIC_READ_UW, 0x10, i32);
// nix::ioctl_read!(ioctl_uw_fanspeed2, MAGIC_READ_UW, 0x11, i32);
// nix::ioctl_read!(ioctl_uw_fan_temp, MAGIC_READ_UW, 0x12, i32);
// nix::ioctl_read!(ioctl_uw_fan_temp2, MAGIC_READ_UW, 0x13, i32);
// nix::ioctl_read!(ioctl_uw_tdp0, MAGIC_READ_UW, 0x18, i32);
// nix::ioctl_read!(ioctl_uw_tdp1, MAGIC_READ_UW, 0x19, i32);
// nix::ioctl_read!(ioctl_uw_tdp2, MAGIC_READ_UW, 0x1a, i32);
// nix::ioctl_read!(ioctl_uw_tdp0_min, MAGIC_READ_UW, 0x1b, i32);
// nix::ioctl_read!(ioctl_uw_tdp1_min, MAGIC_READ_UW, 0x1c, i32);
// nix::ioctl_read!(ioctl_uw_tdp2_min, MAGIC_READ_UW, 0x1d, i32);
// nix::ioctl_read!(ioctl_uw_tdp0_max, MAGIC_READ_UW, 0x1e, i32);
// nix::ioctl_read!(ioctl_uw_tdp1_max, MAGIC_READ_UW, 0x1f, i32);
// nix::ioctl_read!(ioctl_uw_tdp2_max, MAGIC_READ_UW, 0x20, i32);
// nix::ioctl_read!(ioctl_uw_profs_available, MAGIC_READ_UW, 0x21, i32);

// Uniwill write ioctls
// nix::ioctl_write_ptr!(ioctl_uw_fanspeed_w, MAGIC_WRITE_UW, 0x10, i32);
// nix::ioctl_write_ptr!(ioctl_uw_fanspeed2_w, MAGIC_WRITE_UW, 0x11, i32);
// nix::ioctl_write_int!(ioctl_uw_fanauto, MAGIC_WRITE_UW, 0x14);
// nix::ioctl_write_ptr!(ioctl_uw_tdp0_w, MAGIC_WRITE_UW, 0x15, i32);
// nix::ioctl_write_ptr!(ioctl_uw_tdp1_w, MAGIC_WRITE_UW, 0x16, i32);
// nix::ioctl_write_ptr!(ioctl_uw_tdp2_w, MAGIC_WRITE_UW, 0x17, i32);
// nix::ioctl_write_ptr!(ioctl_uw_perf_prof, MAGIC_WRITE_UW, 0x18, i32);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HardwareInterface {
    Clevo,
    Uniwill,
    None,
}

static CLEVO_AUTO_DISABLED: AtomicBool = AtomicBool::new(false);

pub struct TuxedoIo {
    device: std::fs::File,
    interface: HardwareInterface,
    fan_count: u32,
}

impl std::fmt::Debug for TuxedoIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuxedoIo")
            .field("interface", &self.interface)
            .field("fan_count", &self.fan_count)
            .finish()
    }
}

impl TuxedoIo {
    // Linux ioctl macros equivalent - manually constructed for 64-bit systems
    // _IOR(type, nr, size)  = _IOC(_IOC_READ, type, nr, size)
    // _IOW(type, nr, size)  = _IOC(_IOC_WRITE, type, nr, size)
    // _IO(type, nr)         = _IOC(_IOC_NONE, type, nr, 0)
    // _IOC(dir, type, nr, size) = (dir << 30) | (size << 16) | (type << 8) | nr
    
    const _IOC_NONE: u64 = 0;
    const _IOC_WRITE: u64 = 1;
    const _IOC_READ: u64 = 2;
    
    // For 64-bit systems, use 8-byte size for pointer types
    const PTR_SIZE: u64 = 8;
    
    fn ior(type_: u8, nr: u8, size: u64) -> libc::c_ulong {
        ((Self::_IOC_READ << 30) | (size << 16) | ((type_ as u64) << 8) | (nr as u64)) as libc::c_ulong
    }
    
    fn iow(type_: u8, nr: u8, size: u64) -> libc::c_ulong {
        ((Self::_IOC_WRITE << 30) | (size << 16) | ((type_ as u64) << 8) | (nr as u64)) as libc::c_ulong
    }
    
    fn io(type_: u8, nr: u8) -> libc::c_ulong {
        ((Self::_IOC_NONE << 30) | ((type_ as u64) << 8) | (nr as u64)) as libc::c_ulong
    }

    fn ioctl_read_i32(fd: i32, request: libc::c_ulong) -> Result<i32> {
        let mut data: i32 = 0;
        let res = unsafe { libc::ioctl(fd, request, &mut data as *mut i32) };
        Errno::result(res)
            .map_err(|e| anyhow!("ioctl read failed (req={:#x}): {}", request, e))?;
        Ok(data)
    }
    
    fn ioctl_write_i32(fd: i32, request: libc::c_ulong, data: i32) -> Result<()> {
        let res = unsafe { libc::ioctl(fd, request, &data as *const i32) };
        Errno::result(res)
            .map_err(|e| anyhow!("ioctl write failed (req={:#x}): {}", request, e))?;
        Ok(())
    }
    
    fn ioctl_write_only(fd: i32, request: libc::c_ulong, arg: i32) -> Result<()> {
        let res = unsafe { libc::ioctl(fd, request, arg) };
        Errno::result(res)
            .map_err(|e| anyhow!("ioctl write failed (req={:#x}): {}", request, e))?;
        Ok(())
    }
    
    /// Process-wide shared instance.
    ///
    /// Opening `/dev/tuxedo_io` also runs interface detection (two hardware-check
    /// ioctls plus fan-info probes). Callers used to open the device themselves,
    /// which put that detection inside poll loops; the daemon now opens it once
    /// and shares the handle.
    pub fn shared() -> Option<Arc<Self>> {
        static SHARED: OnceLock<Option<Arc<TuxedoIo>>> = OnceLock::new();
        SHARED
            .get_or_init(|| TuxedoIo::new().ok().map(Arc::new))
            .clone()
    }

    pub fn new() -> Result<Self> {
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .open(TUXEDO_IO_DEVICE)?;

        let interface = Self::detect_interface(&device)?;
        let fan_count = Self::detect_fan_count(&device, interface)?;

        static LOGGED_ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED_ONCE.swap(true, std::sync::atomic::Ordering::SeqCst) {
            log::info!(target: "hw.detect", "platform={:?} fan_count={}", interface, fan_count);
        }

        Ok(TuxedoIo {
            device,
            interface,
            fan_count,
        })
    }

    pub fn is_available() -> bool {
        std::path::Path::new(TUXEDO_IO_DEVICE).exists()
    }

    pub fn get_interface(&self) -> HardwareInterface {
        self.interface
    }

    pub fn get_fan_count(&self) -> u32 {
        self.fan_count
    }

    fn clevo_raw_to_percent(raw: u8) -> u32 {
        // Clevo returns raw 0..255
        ((raw as u32 * 100) + 127) / 255
    }

    fn clevo_percent_to_raw(percent: u32) -> u8 {
        let p = percent.min(100);
        (((p * 255) + 50) / 100) as u8
    }

    fn detect_interface(device: &std::fs::File) -> Result<HardwareInterface> {
        let fd = device.as_raw_fd();

        // Try hardware check ioctls first (0x05 for Clevo, 0x06 for Uniwill)
        let cl_check = Self::ior(IOCTL_MAGIC, 0x05, Self::PTR_SIZE);
        let uw_check = Self::ior(IOCTL_MAGIC, 0x06, Self::PTR_SIZE);

        let cl_res = Self::ioctl_read_i32(fd, cl_check);
        let uw_res = Self::ioctl_read_i32(fd, uw_check);

        static LAST_LOG: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
        let should_log = {
            let mut last_log = LAST_LOG.lock().unwrap();
            match *last_log {
                Some(instant) if instant.elapsed() < std::time::Duration::from_secs(60) => false,
                _ => {
                    *last_log = Some(std::time::Instant::now());
                    true
                }
            }
        };

        if matches!(cl_res, Ok(1)) {
            if should_log {
                log::debug!(target: "hw.detect", "Detected Clevo interface via hardware check");
            }
            return Ok(HardwareInterface::Clevo);
        }
        if matches!(uw_res, Ok(1)) {
            if should_log {
                log::debug!(target: "hw.detect", "Detected Uniwill interface via hardware check");
            }
            return Ok(HardwareInterface::Uniwill);
        }

        // Fallback: try to read faninfo to detect interface
        let probe_cl = Self::ioctl_read_i32(fd, Self::ior(MAGIC_READ_CL, 0x10, Self::PTR_SIZE));
        if probe_cl.is_ok() {
            if should_log {
                log::debug!(target: "hw.detect", "Detected Clevo interface via faninfo probe");
            }
            return Ok(HardwareInterface::Clevo);
        }

        let probe_uw = Self::ioctl_read_i32(fd, Self::ior(MAGIC_READ_UW, 0x10, Self::PTR_SIZE));
        if probe_uw.is_ok() {
            if should_log {
                log::debug!(target: "hw.detect", "Detected Uniwill interface via fanspeed probe");
            }
            return Ok(HardwareInterface::Uniwill);
        }

        if should_log {
            log::warn!(target: "hw.detect", "No hardware interface detected");
        }
        Ok(HardwareInterface::None)
    }

    fn detect_fan_count(
        device: &std::fs::File,
        interface: HardwareInterface,
    ) -> Result<u32> {
        let fd = device.as_raw_fd();

        match interface {
            HardwareInterface::Clevo => {
                let mut count = 0;
                for fan_id in 0..3u32 {
                    let seq = 0x10 + fan_id as u8;
                    let request = Self::ior(MAGIC_READ_CL, seq, Self::PTR_SIZE);
                    
                    if let Ok(raw) = Self::ioctl_read_i32(fd, request) {
                        // Use temp2 field (bits 16-23) to check if fan exists
                        let temp2 = ((raw >> 16) & 0xFF) as u32;
                        if temp2 <= 1 {
                            break;
                        }
                        count += 1;
                    } else {
                        break;
                    }
                }
                Ok(count)
            }

            HardwareInterface::Uniwill => {
                let r0 = Self::ioctl_read_i32(fd, Self::ior(MAGIC_READ_UW, 0x10, Self::PTR_SIZE));
                if r0.is_err() {
                    return Ok(0);
                }
                let r1 = Self::ioctl_read_i32(fd, Self::ior(MAGIC_READ_UW, 0x11, Self::PTR_SIZE));
                Ok(if r1.is_ok() { 2 } else { 1 })
            }

            HardwareInterface::None => Ok(0),
        }
    }

    // Fan control methods
    pub fn get_fan_speed(&self, fan_id: u32) -> Result<u32> {
        let fd = self.device.as_raw_fd();

        match self.interface {
            HardwareInterface::Clevo => {
                if fan_id >= 3 {
                    return Err(anyhow!("Invalid Clevo fan ID: {}", fan_id));
                }
                
                let seq = 0x10 + fan_id as u8;
                let request = Self::ior(MAGIC_READ_CL, seq, Self::PTR_SIZE);
                let raw = Self::ioctl_read_i32(fd, request)?;

                Ok(Self::clevo_raw_to_percent((raw & 0xFF) as u8))
            }

            HardwareInterface::Uniwill => {
                if fan_id >= 2 {
                    return Err(anyhow!("Invalid Uniwill fan ID: {}", fan_id));
                }
                
                let seq = 0x10 + fan_id as u8;
                let request = Self::ior(MAGIC_READ_UW, seq, Self::PTR_SIZE);
                let val = Self::ioctl_read_i32(fd, request)?;
                Ok(val as u32)
            }

            HardwareInterface::None => Err(anyhow!("No hardware interface")),
        }
    }

    pub fn set_fan_speed(&self, fan_id: u32, speed_percent: u32) -> Result<()> {
        let fd = self.device.as_raw_fd();

        match self.interface {
            HardwareInterface::Clevo => {
                let speed_percent = speed_percent.min(100);
                
                // Step 1: Disable auto mode (critical for Clevo!)
                if !CLEVO_AUTO_DISABLED.load(Ordering::SeqCst) {
                    log::debug!(target: "hw.fan", "Disabling Clevo auto mode for manual fan control");
                    let manual_val: i32 = 0;
                    let auto_request = Self::iow(MAGIC_WRITE_CL, 0x11, Self::PTR_SIZE);
                    Self::ioctl_write_i32(fd, auto_request, manual_val)?;
                    CLEVO_AUTO_DISABLED.store(true, Ordering::SeqCst);
                }
                
                // Step 2: Read current speeds for all fans
                let mut current_raw = [0u8; 3];
                for i in 0..self.fan_count.min(3) {
                    let seq = 0x10 + i as u8;
                    let request = Self::ior(MAGIC_READ_CL, seq, Self::PTR_SIZE);
                    
                    if let Ok(raw) = Self::ioctl_read_i32(fd, request) {
                        current_raw[i as usize] = (raw & 0xFF) as u8;
                    }
                }

                // Step 3: Update the requested fan speed
                if fan_id >= 3 {
                    return Err(anyhow!("Invalid Clevo fan ID: {}", fan_id));
                }
                current_raw[fan_id as usize] = Self::clevo_percent_to_raw(speed_percent);

                // Step 4: Pack all fan speeds into a single i32
                let packed = (current_raw[0] as i32)
                    | ((current_raw[1] as i32) << 8)
                    | ((current_raw[2] as i32) << 16);

                log::debug!(target: "hw.fan",
                    "Setting Clevo fan {} to {}% (raw: {:#04x}), packed: {:#08x}",
                    fan_id, speed_percent, current_raw[fan_id as usize], packed
                );

                // Step 5: Write the packed value
                let speed_request = Self::iow(MAGIC_WRITE_CL, 0x10, Self::PTR_SIZE);
                Self::ioctl_write_i32(fd, speed_request, packed)?;

                log::info!(target: "hw.fan", "set_clevo_fan id={} speed={}%", fan_id, speed_percent);
                Ok(())
            }

            HardwareInterface::Uniwill => {
                let val: i32 = speed_percent.min(200) as i32;
                let seq = match fan_id {
                    0 => 0x10,
                    1 => 0x11,
                    _ => return Err(anyhow!("Invalid Uniwill fan ID: {}", fan_id)),
                };

                log::debug!(target: "hw.fan", "Setting Uniwill fan {} to {}%", fan_id, speed_percent);

                let request = Self::iow(MAGIC_WRITE_UW, seq, Self::PTR_SIZE);
                Self::ioctl_write_i32(fd, request, val)?;

                log::info!(target: "hw.fan", "set_uniwill_fan id={} speed={}%", fan_id, speed_percent);
                Ok(())
            }

            HardwareInterface::None => Err(anyhow!("No hardware interface")),
        }
    }

    pub fn set_fan_auto(&self) -> Result<()> {
        let fd = self.device.as_raw_fd();

        match self.interface {
            HardwareInterface::Clevo => {
                let auto_val: i32 = 0xF;
                log::debug!(target: "hw.fan", "Setting Clevo fans to auto mode");
                
                let request = Self::iow(MAGIC_WRITE_CL, 0x11, Self::PTR_SIZE);
                Self::ioctl_write_i32(fd, request, auto_val)?;
                CLEVO_AUTO_DISABLED.store(false, Ordering::SeqCst);
                
                log::info!(target: "hw.fan", "set_clevo_fans_auto");
                Ok(())
            }

            HardwareInterface::Uniwill => {
                log::debug!(target: "hw.fan", "Setting Uniwill fans to auto mode");
                
                // Uniwill uses _IO (no data argument)
                let request = Self::io(MAGIC_WRITE_UW, 0x14);
                Self::ioctl_write_only(fd, request, 1)?;
                
                log::info!(target: "hw.fan", "set_uniwill_fans_auto");
                Ok(())
            }

            HardwareInterface::None => Err(anyhow!("No hardware interface")),
        }
    }

    pub fn get_tdp(&self, tdp_index: u8) -> Result<u32> {
        if self.interface != HardwareInterface::Uniwill {
            return Err(anyhow!("TDP control only available on Uniwill interface"));
        }
        let fd = self.device.as_raw_fd();
        let seq = 0x18 + tdp_index;
        let request = Self::ior(MAGIC_READ_UW, seq, Self::PTR_SIZE);
        let val = Self::ioctl_read_i32(fd, request)?;
        Ok(val as u32)
    }

    pub fn get_tdp_min(&self, tdp_index: u8) -> Result<u32> {
        if self.interface != HardwareInterface::Uniwill {
            return Err(anyhow!("TDP control only available on Uniwill interface"));
        }
        let fd = self.device.as_raw_fd();
        let seq = 0x1b + tdp_index;
        let request = Self::ior(MAGIC_READ_UW, seq, Self::PTR_SIZE);
        let val = Self::ioctl_read_i32(fd, request)?;
        Ok(val as u32)
    }

    pub fn get_tdp_max(&self, tdp_index: u8) -> Result<u32> {
        if self.interface != HardwareInterface::Uniwill {
            return Err(anyhow!("TDP control only available on Uniwill interface"));
        }
        let fd = self.device.as_raw_fd();
        let seq = 0x1e + tdp_index;
        let request = Self::ior(MAGIC_READ_UW, seq, Self::PTR_SIZE);
        let val = Self::ioctl_read_i32(fd, request)?;
        Ok(val as u32)
    }

    pub fn set_tdp(&self, tdp_index: u8, value: u32) -> Result<()> {
        if self.interface != HardwareInterface::Uniwill {
            return Err(anyhow!("TDP control only available on Uniwill interface"));
        }
        let fd = self.device.as_raw_fd();
        let seq = 0x15 + tdp_index;
        let request = Self::iow(MAGIC_WRITE_UW, seq, Self::PTR_SIZE);
        Self::ioctl_write_i32(fd, request, value as i32)
    }

    pub fn get_uw_performance_profile(&self) -> Result<u32> {
        if self.interface != HardwareInterface::Uniwill {
            return Err(anyhow!("Uniwill performance profile only available on Uniwill interface"));
        }
        let fd = self.device.as_raw_fd();
        let request = Self::ior(MAGIC_READ_UW, 0x14, Self::PTR_SIZE);
        let mode_data = Self::ioctl_read_i32(fd, request)? as u32;

        // According to tuxedo_io.c:
        // Case 1: (0xa0 set, 0x10 cleared) -> PROFILE_POWERSAVE (1)
        // Case 2: (0xa0 cleared, 0x10 cleared) -> PROFILE_ENTHUSIAST (2)
        // Case 3: (0x10 set, 0xa0 cleared) -> PROFILE_OVERBOOST (3)
        let a0_set = (mode_data & 0xa0) == 0xa0;
        let a0_cleared = (mode_data & 0xa0) == 0;
        let bit10_set = (mode_data & 0x10) != 0;
        let bit10_cleared = (mode_data & 0x10) == 0;

        if a0_set && bit10_cleared {
            Ok(1)
        } else if a0_cleared && bit10_cleared {
            Ok(2)
        } else if bit10_set && a0_cleared {
            Ok(3)
        } else {
            // Default to enthusiast if unknown
            Ok(2)
        }
    }

    pub fn get_fan_temperature(&self, fan_id: u32) -> Result<u32> {
        let fd = self.device.as_raw_fd();

        match self.interface {
            HardwareInterface::Clevo => {
                if fan_id >= 3 {
                    return Err(anyhow!("Invalid Clevo fan ID: {}", fan_id));
                }
                
                let seq = 0x10 + fan_id as u8;
                let request = Self::ior(MAGIC_READ_CL, seq, Self::PTR_SIZE);
                let raw = Self::ioctl_read_i32(fd, request)?;

                // Use temp2 field (bits 16-23) - more reliable on Clevo
                let temp2 = ((raw >> 16) & 0xFF) as u32;
                if temp2 <= 1 {
                    return Err(anyhow!("Fan {} not available", fan_id));
                }
                Ok(temp2)
            }

            HardwareInterface::Uniwill => {
                if fan_id >= 2 {
                    return Err(anyhow!("Invalid Uniwill fan ID: {}", fan_id));
                }
                
                let seq = 0x12 + fan_id as u8;
                let request = Self::ior(MAGIC_READ_UW, seq, Self::PTR_SIZE);
                let val = Self::ioctl_read_i32(fd, request)?;
                
                if val <= 0 {
                    return Err(anyhow!("Fan {} not available", fan_id));
                }
                Ok(val as u32)
            }

            HardwareInterface::None => Err(anyhow!("No hardware interface")),
        }
    }
    
    // Performance profile methods
    pub fn get_available_profiles(&self) -> Result<Vec<String>> {
        match self.interface {
            HardwareInterface::Clevo => {
                Ok(vec![
                    "quiet".to_string(),
                    "power_saving".to_string(),
                    "performance".to_string(),
                    "entertainment".to_string(),
                ])
            }
            HardwareInterface::Uniwill => {
                let fd = self.device.as_raw_fd();
                let request = Self::ior(MAGIC_READ_UW, 0x21, Self::PTR_SIZE);
                let result = Self::ioctl_read_i32(fd, request)?;
                
                let mut profiles = vec![];
                if result >= 2 {
                    profiles.push("power_save".to_string());
                    profiles.push("enthusiast".to_string());
                }
                if result >= 3 {
                    profiles.push("overboost".to_string());
                }
                Ok(profiles)
            }
            HardwareInterface::None => Ok(vec![]),
        }
    }
    
    pub fn set_performance_profile(&self, profile_id: u32) -> Result<()> {
        let fd = self.device.as_raw_fd();
        
        match self.interface {
            HardwareInterface::Clevo => {
                if profile_id > 3 {
                    return Err(anyhow!("Invalid Clevo profile ID: {}", profile_id));
                }
                
                log::debug!(target: "hw.detect", "Setting Clevo performance profile to {}", profile_id);
                
                let request = Self::iow(MAGIC_WRITE_CL, 0x15, Self::PTR_SIZE);
                Self::ioctl_write_i32(fd, request, profile_id as i32)?;
                
                log::info!(target: "hw.detect", "set_clevo_perf_profile id={}", profile_id);
                Ok(())
            }
            HardwareInterface::Uniwill => {
                if profile_id < 1 || profile_id > 3 {
                    return Err(anyhow!("Invalid Uniwill profile ID: {}", profile_id));
                }
                
                log::debug!(target: "hw.detect", "Setting Uniwill performance profile to {}", profile_id);
                
                let request = Self::iow(MAGIC_WRITE_UW, 0x18, Self::PTR_SIZE);
                Self::ioctl_write_i32(fd, request, profile_id as i32)?;
                
                log::info!(target: "hw.detect", "set_uniwill_perf_profile id={}", profile_id);
                Ok(())
            }
            HardwareInterface::None => Err(anyhow!("No hardware interface available")),
        }
    }
    
    
    // Webcam control (Clevo only)
    pub fn get_webcam_state(&self) -> Result<bool> {
        if self.interface != HardwareInterface::Clevo {
            return Err(anyhow!("Webcam control only available on Clevo interface"));
        }
        
        let fd = self.device.as_raw_fd();
        let request = Self::ior(MAGIC_READ_CL, 0x13, Self::PTR_SIZE);
        let result = Self::ioctl_read_i32(fd, request)?;
        
        Ok(result != 0)
    }
    
    pub fn set_webcam_state(&self, enabled: bool) -> Result<()> {
        if self.interface != HardwareInterface::Clevo {
            return Err(anyhow!("Webcam control only available on Clevo interface"));
        }
        
        let fd = self.device.as_raw_fd();
        let value: i32 = if enabled { 1 } else { 0 };
        
        let request = Self::iow(MAGIC_WRITE_CL, 0x12, Self::PTR_SIZE);
        Self::ioctl_write_i32(fd, request, value)
    }

    // Keyboard control (Clevo only)
    pub fn set_clevo_keyboard_mode(&self, mode_val: u32) -> Result<()> {
        if self.interface != HardwareInterface::Clevo {
            return Err(anyhow!("Clevo keyboard control only available on Clevo interface"));
        }

        let fd = self.device.as_raw_fd();
        // Use sequence 0x67 (CLEVO_CMD_SET_KB_RGB_LEDS)
        let request = Self::iow(MAGIC_WRITE_CL, 0x67, Self::PTR_SIZE);
        // Data needs to be zero-extended to u64 to avoid sign-extension bugs in 64-bit ioctl
        let data = mode_val as u64;
        let res = unsafe { libc::ioctl(fd, request, &data as *const u64) };
        Errno::result(res)
            .map_err(|e| anyhow!("ioctl write failed (req={:#x}): {}", request, e))?;
        Ok(())
    }

    pub fn set_clevo_keyboard_color(&self, zone: u8, r: u8, g: u8, b: u8) -> Result<()> {
        if self.interface != HardwareInterface::Clevo {
            return Err(anyhow!("Clevo keyboard control only available on Clevo interface"));
        }

        // Prefix for zones: 0xF0=Left, 0xF1=Center, 0xF2=Right
        let prefix = match zone {
            0 => 0xF0000000u32,
            1 => 0xF1000000u32,
            2 => 0xF2000000u32,
            _ => return Err(anyhow!("Invalid Clevo keyboard zone: {}", zone)),
        };

        // Hardware expects 0xZZ BB RR GG
        let packed_color = ((b as u32) << 16) | ((r as u32) << 8) | (g as u32);
        self.set_clevo_keyboard_mode(prefix | packed_color)
    }

    pub fn set_clevo_keyboard_brightness(&self, brightness_percent: u8) -> Result<()> {
        if self.interface != HardwareInterface::Clevo {
            return Err(anyhow!("Clevo keyboard control only available on Clevo interface"));
        }

        // Prefix 0xF4 for brightness
        let prefix = 0xF4000000u32;
        let val = brightness_percent.min(100) as u32;
        self.set_clevo_keyboard_mode(prefix | val)
    }
}
