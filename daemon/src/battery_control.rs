use anyhow::{anyhow, Result};
use crate::hw_gate;
use std::fs;
use std::path::{Path, PathBuf};

pub struct BatteryControl {
    battery_path: PathBuf,
}

/// Every charge-control attribute is backed by the same firmware call (the
/// Clevo `_DSM` cmd 0x04 / flexicharger path), so a single permanent failure
/// disables the whole capability instead of being retried at poll rate — the
/// failed call costs ~7 lines of kernel log each time (see `hw_gate`).
fn check_gate() -> Result<()> {
    if hw_gate::enabled(hw_gate::BATTERY_CHARGE_CONTROL) {
        Ok(())
    } else {
        Err(anyhow!(
            "battery charge control unavailable: {} failed permanently \
             (firmware defect); capability disabled until the daemon restarts",
            hw_gate::BATTERY_CHARGE_CONTROL
        ))
    }
}

fn read_gated(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(e) => {
            hw_gate::note_failure(hw_gate::BATTERY_CHARGE_CONTROL);
            Err(e.into())
        }
    }
}

fn write_gated(path: &Path, value: &str) -> Result<()> {
    match fs::write(path, value) {
        Ok(()) => Ok(()),
        Err(e) => {
            hw_gate::note_failure(hw_gate::BATTERY_CHARGE_CONTROL);
            Err(e.into())
        }
    }
}

impl BatteryControl {
    pub fn new() -> Result<Self> {
        let battery_path = Self::find_battery_path()?;
        Ok(Self { battery_path })
    }
    
    pub fn is_available() -> bool {
        Self::find_battery_path().is_ok()
    }
    
    fn find_battery_path() -> Result<PathBuf> {
        for bat in &["BAT0", "BAT1"] {
            let path = PathBuf::from(format!("/sys/class/power_supply/{}", bat));
            if path.exists() {
                // Check if charge control is available
                let charge_type_path = path.join("charge_type");
                if charge_type_path.exists() {
                    return Ok(path);
                }
            }
        }
        Err(anyhow!("No battery with charge control found"))
    }
    
    /// Get charge control mode: "Standard" or "Custom"
    pub fn get_charge_type(&self) -> Result<String> {
        check_gate()?;
        let path = self.battery_path.join("charge_type");
        let content = read_gated(&path)?;
        Ok(content.trim().to_string())
    }
    
    /// Set charge control mode: "Standard" or "Custom"
    pub fn set_charge_type(&self, charge_type: &str) -> Result<()> {
        if charge_type != "Standard" && charge_type != "Custom" {
            return Err(anyhow!("Invalid charge type. Must be 'Standard' or 'Custom'"));
        }
        check_gate()?;
        
        let path = self.battery_path.join("charge_type");
        write_gated(&path, charge_type)?;
        Ok(())
    }
    
    /// Get charge start threshold (percentage)
    pub fn get_charge_control_start_threshold(&self) -> Result<u8> {
        check_gate()?;
        let path = self.battery_path.join("charge_control_start_threshold");
        let content = read_gated(&path)?;
        let value: u8 = content.trim().parse()?;
        Ok(value)
    }
    
    /// Set charge start threshold (percentage)
    pub fn set_charge_control_start_threshold(&self, threshold: u8) -> Result<()> {
        if threshold > 100 {
            return Err(anyhow!("Threshold must be between 0 and 100"));
        }
        check_gate()?;
        
        let path = self.battery_path.join("charge_control_start_threshold");
        write_gated(&path, &threshold.to_string())?;
        Ok(())
    }
    
    /// Get charge end threshold (percentage)
    pub fn get_charge_control_end_threshold(&self) -> Result<u8> {
        check_gate()?;
        let path = self.battery_path.join("charge_control_end_threshold");
        let content = read_gated(&path)?;
        let value: u8 = content.trim().parse()?;
        Ok(value)
    }
    
    /// Set charge end threshold (percentage)
    pub fn set_charge_control_end_threshold(&self, threshold: u8) -> Result<()> {
        if threshold > 100 {
            return Err(anyhow!("Threshold must be between 0 and 100"));
        }
        check_gate()?;
        
        let path = self.battery_path.join("charge_control_end_threshold");
        write_gated(&path, &threshold.to_string())?;
        Ok(())
    }
    
    /// Get available start thresholds
    pub fn get_available_start_thresholds(&self) -> Result<Vec<u8>> {
        let path = self.battery_path.join("charge_control_start_available_thresholds");
        if !path.exists() {
            // Return default values if not available
            return Ok(vec![40, 50, 60, 70, 80, 95]);
        }
        check_gate()?;
        
        let content = read_gated(&path)?;
        let thresholds: Vec<u8> = content
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        Ok(thresholds)
    }
    
    /// Get available end thresholds
    pub fn get_available_end_thresholds(&self) -> Result<Vec<u8>> {
        let path = self.battery_path.join("charge_control_end_available_thresholds");
        if !path.exists() {
            // Return default values if not available
            return Ok(vec![60, 70, 80, 90, 100]);
        }
        check_gate()?;
        
        let content = read_gated(&path)?;
        let thresholds: Vec<u8> = content
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        Ok(thresholds)
    }
    
}
