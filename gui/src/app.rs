use chrono::Local;
use egui::{Align, CentralPanel, Context, FontFamily, FontId, Layout, RichText, TextStyle};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};
use std::collections::VecDeque;
use tokio::sync::{mpsc, oneshot};
use lapsphere_common::types::*;

use crate::dbus_client::DbusClient;
use crate::theme::LapSphereTheme;
use crate::pages::{statistics, profiles, tuning, settings};
use crate::keyboard_shortcuts::KeyboardShortcuts;
use crate::polling_scheduler::{RefreshCoordinator, CoordinatorHandle};
use crate::system_tray::{SystemTray, TrayEvent};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Page {
    Statistics,
    Profiles,
    Tuning,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingsTab {
    Main,
    StatsConfiguration,
    Hardware,
    Logs,
    Help,
    About,
}

pub struct AppState {
    // Core data
    pub config: AppConfig,
    
    // Hardware info (updated in background)
    pub system_info: Option<SystemInfo>,
    pub memory_info: Option<MemoryInfo>,
    pub cpu_info: Option<CpuInfo>,
    pub gpu_info: Vec<GpuInfo>,
    pub battery_info: Option<BatteryInfo>,
    pub wifi_info: Vec<WiFiInfo>,
    pub gamepad_info: Vec<GamepadInfo>,
    pub fan_info: Vec<FanInfo>,
    pub storage_device_info: Vec<StorageDevice>,
    pub mount_info: Vec<MountInfo>,
    pub hardware_interface: Option<String>,
    pub keyboard_capabilities: Option<KeyboardCapabilities>,
    pub gpu_clock_ranges: Option<(u32, u32)>,
    pub gpu_clock_ranges_error: Option<String>,
    pub gpu_mem_clock_ranges: Option<(u32, u32)>,
    pub gpu_core_offset_limits: Option<(i32, i32)>,
    pub gpu_core_offset_error: Option<String>,
    pub gpu_mem_offset_limits: Option<(i32, i32)>,
    pub gpu_mem_offset_error: Option<String>,
    pub available_start_thresholds: Vec<u8>,
    pub available_end_thresholds: Vec<u8>,
    pub available_tdp_profiles: Vec<String>,
    pub webcam_enabled: Option<bool>,
    pub daemon_logs: VecDeque<LogEntry>,
    pub new_version_available: Option<String>,
    pub latest_changelog: Option<String>,
    pub log_filter_trace: bool,
    pub log_filter_debug: bool,
    pub log_filter_info: bool,
    pub log_filter_warn: bool,
    pub log_filter_error: bool,
    pub log_paused: bool,
    pub log_search_text: String,
    
    // UI state
    pub current_page: Page,
    pub settings_tab: SettingsTab,
    pub status_message: Option<StatusMessage>,
    pub restart_confirmation_pending: bool,
    pub pending_prime_profile: Option<String>,
    pub selected_fan_curve: usize,
    
    // Profile editing
    pub editing_profile_name: Option<String>,
    
    // Async state
    pub pending_battery_update: Option<oneshot::Receiver<Result<(), anyhow::Error>>>,
    
    // Refresh coordinator handle
    pub coordinator_handle: Option<CoordinatorHandle>,

    pub keyboard_brush_color: [u8; 3],
}

#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub text: String,
    pub is_error: bool,
    pub shown_at: Instant,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            config: AppConfig::default(),
            system_info: None,
            memory_info: None,
            cpu_info: None,
            gpu_info: Vec::new(),
            battery_info: None,
            wifi_info: Vec::new(),
            gamepad_info: Vec::new(),
            fan_info: Vec::new(),
            storage_device_info: Vec::new(),
            mount_info: Vec::new(),
            hardware_interface: None,
            gpu_clock_ranges: None,
            gpu_clock_ranges_error: None,
            gpu_mem_clock_ranges: None,
            gpu_core_offset_limits: None,
            gpu_core_offset_error: None,
            gpu_mem_offset_limits: None,
            gpu_mem_offset_error: None,
            available_start_thresholds: Vec::new(),
            available_end_thresholds: Vec::new(),
            available_tdp_profiles: Vec::new(),
            webcam_enabled: None,
            daemon_logs: VecDeque::new(),
            new_version_available: None,
            latest_changelog: None,
            log_filter_trace: false,
            log_filter_debug: false,
            log_filter_info: true,
            log_filter_warn: true,
            log_filter_error: true,
            log_paused: true,
            log_search_text: String::new(),
            keyboard_capabilities: None,
            current_page: Page::Statistics,
            settings_tab: SettingsTab::Main,
            status_message: None,
            restart_confirmation_pending: false,
            pending_prime_profile: None,
            editing_profile_name: None,
            pending_battery_update: None,
            coordinator_handle: None,
            selected_fan_curve: 0,
            keyboard_brush_color: [255, 255, 255],
        }
    }

    fn clamp_fan_selection(&mut self) {
        let fan_count = self.fan_info.len();
        if fan_count == 0 {
            self.selected_fan_curve = 0;
        } else if self.selected_fan_curve >= fan_count {
            self.selected_fan_curve = fan_count.saturating_sub(1);
        }
    }
    
pub fn load_config(&mut self) {
    if let Ok(config) = load_config_from_disk() {
        self.config = config;
        self.config.statistics_sections.section_order =
            statistics::normalize_section_order(&self.config.statistics_sections.section_order);
        self.log_filter_trace = self.config.log_filter_trace;
    }
}
    
    pub fn save_settings(&mut self) -> anyhow::Result<()> {
        save_settings_to_disk(&self.config)?;
        self.show_message("Settings saved", false);
        Ok(())
    }

    pub fn save_profiles(&mut self) -> anyhow::Result<()> {
        save_profiles_to_disk(&self.config)?;
        self.show_message("Profiles saved", false);
        Ok(())
    }
    
    pub fn show_message(&mut self, text: impl Into<String>, is_error: bool) {
        self.status_message = Some(StatusMessage {
            text: text.into(),
            is_error,
            shown_at: Instant::now(),
        });
    }
    
    pub fn current_profile(&self) -> Option<&Profile> {
        self.config.profiles.iter()
            .find(|p| p.name == self.config.current_profile)
    }
    
    pub fn current_profile_index(&self) -> Option<usize> {
        self.config.profiles.iter()
            .position(|p| p.name == self.config.current_profile)
    }
}

pub struct LapSphereApp {
    state: AppState,
    dbus_client: Option<DbusClient>,
    theme: LapSphereTheme,
    system_tray: Option<SystemTray>,
    force_quit: bool,
    
    // Background update channel
    hw_update_tx: mpsc::Sender<HardwareUpdate>,
    hw_update_rx: mpsc::Receiver<HardwareUpdate>,
    
    // Keyboard shortcuts
    shortcuts: KeyboardShortcuts,

    startup_frames: u32,

    last_tray_profile: String,
    last_tray_profiles_count: usize,
}

#[derive(Debug)]
pub enum HardwareUpdate {
    SystemInfo(SystemInfo),
    MemoryInfo(MemoryInfo),
    CpuInfo(CpuInfo),
    GpuInfo(Vec<GpuInfo>),
    BatteryInfo(BatteryInfo),
    WifiInfo(Vec<WiFiInfo>),
    GamepadInfo(Vec<GamepadInfo>),
    FanInfo(Vec<FanInfo>),
    StorageDeviceInfo(Vec<StorageDevice>),
    MountInfo(Vec<MountInfo>),
    HardwareInterface(String),
    WebcamState(bool),
    DaemonLogs(Vec<LogEntry>),
    UpdateInfo(String, String),
    GpuClockRanges(Result<(u32, u32), String>),
    GpuCoreOffsetLimits(Result<(i32, i32), String>),
    GpuMemOffsetLimits(Result<(i32, i32), String>),
    AvailableThresholds(Vec<u8>, Vec<u8>),
    TdpProfiles(Vec<String>),
    KeyboardCapabilities(KeyboardCapabilities),
}

impl LapSphereApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut state = AppState::new();
        state.load_config();
        
        // Create DBus client
        let dbus_client = match DbusClient::new() {
            Ok(client) => {
                log::info!("✅ Connected to LapSphere daemon");
                crate::dbus_client::set_shared_client(client.clone());
                Some(client)
            }
            Err(e) => {
                log::error!("❌ Failed to connect to daemon: {}", e);
                state.show_message(
                    format!("Failed to connect to daemon: {}", e),
                    true
                );
                None
            }
        };
        
        // Setup background polling with refresh coordinator
        // Use a bounded channel to prevent potential memory leaks if UI processing stalls
        let (hw_update_tx, hw_update_rx) = mpsc::channel(100);
        let coordinator_handle = if let Some(ref client) = dbus_client {
            let coordinator = RefreshCoordinator::new();
            let handle = coordinator.get_handle();
            
            // Setup refresh callback
            let client_clone = client.clone();
            let tx_clone = hw_update_tx.clone();
            tokio::spawn(async move {
                coordinator.run(move |component_id| {
                    // Trigger refresh for the component
                    let client = client_clone.clone();
                    let tx = tx_clone.clone();
                    let component = component_id.to_string();
                    
                    tokio::spawn(async move {
                        match component.as_str() {
                            "cpu" => {
                                match client.get_cpu_info().await {
                                    Ok(Ok(info)) => { let _ = tx.send(HardwareUpdate::CpuInfo(info)).await; }
                                    Ok(Err(e)) => log::error!("Failed to get CPU info: {}", e),
                                    Err(e) => log::error!("DBus error getting CPU info: {}", e),
                                }
                            }
                            "gpu" => {
                                match client.get_gpu_info().await {
                                    Ok(Ok(info)) => { let _ = tx.send(HardwareUpdate::GpuInfo(info)).await; }
                                    Ok(Err(e)) => log::error!("Failed to get GPU info: {}", e),
                                    Err(e) => log::error!("DBus error getting GPU info: {}", e),
                                }
                            }
                            "memory" => {
                                match client.get_memory_info().await {
                                    Ok(Ok(info)) => { let _ = tx.send(HardwareUpdate::MemoryInfo(info)).await; }
                                    Ok(Err(e)) => log::error!("Failed to get Memory info: {}", e),
                                    Err(e) => log::error!("DBus error getting Memory info: {}", e),
                                }
                            }
                            "fans" => {
                                match client.get_fan_info().await {
                                    Ok(Ok(info)) => { let _ = tx.send(HardwareUpdate::FanInfo(info)).await; }
                                    Ok(Err(e)) => log::error!("Failed to get Fan info: {}", e),
                                    Err(e) => log::error!("DBus error getting Fan info: {}", e),
                                }
                            }
                            "battery" => {
                                match client.get_battery_info().await {
                                    Ok(Ok(info)) => { let _ = tx.send(HardwareUpdate::BatteryInfo(info)).await; }
                                    Ok(Err(e)) => log::error!("Failed to get Battery info: {}", e),
                                    Err(e) => log::error!("DBus error getting Battery info: {}", e),
                                }
                            }
                            "wifi" => {
                                match client.get_wifi_info().await {
                                    Ok(Ok(info)) => { let _ = tx.send(HardwareUpdate::WifiInfo(info)).await; }
                                    Ok(Err(e)) => log::error!("Failed to get WiFi info: {}", e),
                                    Err(e) => log::error!("DBus error getting WiFi info: {}", e),
                                }
                            }
                            "gamepads" => {
                                match client.get_gamepad_info().await {
                                    Ok(Ok(info)) => { let _ = tx.send(HardwareUpdate::GamepadInfo(info)).await; }
                                    Ok(Err(e)) => log::error!("Failed to get Gamepad info: {}", e),
                                    Err(e) => log::error!("DBus error getting Gamepad info: {}", e),
                                }
                            }
                            "storage" => {
                                match client.get_storage_device_info().await {
                                    Ok(Ok(info)) => { let _ = tx.send(HardwareUpdate::StorageDeviceInfo(info)).await; }
                                    Ok(Err(e)) => log::error!("Failed to get Storage info: {}", e),
                                    Err(e) => log::error!("DBus error getting Storage info: {}", e),
                                }
                            }
                            "mount" => {
                                match client.get_mount_info().await {
                                    Ok(Ok(info)) => { let _ = tx.send(HardwareUpdate::MountInfo(info)).await; }
                                    Ok(Err(e)) => log::error!("Failed to get Mount info: {}", e),
                                    Err(e) => log::error!("DBus error getting Mount info: {}", e),
                                }
                            }
                            "webcam" => {
                                match client.get_webcam_state().await {
                                    Ok(Ok(state)) => { let _ = tx.send(HardwareUpdate::WebcamState(state)).await; }
                                    _ => {}
                                }
                            }
                            "logs" => {
                                match client.get_daemon_logs().await {
                                    Ok(Ok(logs)) => { let _ = tx.send(HardwareUpdate::DaemonLogs(logs)).await; }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    });
                }).await;
            });
            
            // Register components with their refresh intervals
            let _ = handle.register("cpu".to_string(), Duration::from_millis(state.config.statistics_sections.cpu_poll_rate));
            let _ = handle.register("gpu".to_string(), Duration::from_millis(state.config.statistics_sections.gpu_poll_rate));
            let _ = handle.register("memory".to_string(), Duration::from_millis(state.config.statistics_sections.memory_poll_rate));
            let _ = handle.register("fans".to_string(), Duration::from_millis(state.config.statistics_sections.fans_poll_rate));
            let _ = handle.register("battery".to_string(), Duration::from_millis(state.config.statistics_sections.battery_poll_rate));
            let _ = handle.register("wifi".to_string(), Duration::from_millis(state.config.statistics_sections.wifi_poll_rate));
            let _ = handle.register("gamepads".to_string(), Duration::from_millis(state.config.statistics_sections.gamepad_poll_rate));
            let _ = handle.register("storage".to_string(), Duration::from_millis(state.config.statistics_sections.storage_poll_rate));
            let _ = handle.register("mount".to_string(), Duration::from_millis(state.config.statistics_sections.storage_poll_rate));
            let _ = handle.register("gpu_overclock".to_string(), Duration::from_millis(state.config.statistics_sections.gpu_overclock_poll_rate));
            let _ = handle.register("webcam".to_string(), Duration::from_secs(5));
            let _ = handle.register("logs".to_string(), Duration::from_secs(5));

            // Initial system info load
            let client_clone = client.clone();
            let tx_clone = hw_update_tx.clone();
            tokio::spawn(async move {
                if let Ok(Ok(info)) = client_clone.get_system_info().await {
                    let _ = tx_clone.send(HardwareUpdate::SystemInfo(info)).await;
                }
            });

            // Fetch available thresholds
            let client_clone = client.clone();
            let tx_clone = hw_update_tx.clone();
            tokio::spawn(async move {
                let start_rx = client_clone.get_battery_available_start_thresholds();
                let end_rx = client_clone.get_battery_available_end_thresholds();

                match (start_rx.await, end_rx.await) {
                    (Ok(Ok(start)), Ok(Ok(end))) => {
                        let _ = tx_clone.send(HardwareUpdate::AvailableThresholds(start, end)).await;
                    }
                    _ => {}
                }
            });

            let client_clone = client.clone();
            let tx_clone = hw_update_tx.clone();
            tokio::spawn(async move {
                if let Ok(Ok(profiles)) = client_clone.get_tdp_profiles().await {
                    let _ = tx_clone.send(HardwareUpdate::TdpProfiles(profiles)).await;
                }
            });

            let client_clone = client.clone();
            let tx_clone = hw_update_tx.clone();
            tokio::spawn(async move {
                if let Ok(Ok(interface)) = client_clone.get_hardware_interface_info().await {
                    let _ = tx_clone.send(HardwareUpdate::HardwareInterface(interface)).await;
                }
            });

            let client_clone = client.clone();
            let tx_clone = hw_update_tx.clone();
            tokio::spawn(async move {
                if let Ok(Ok(caps)) = client_clone.get_keyboard_capabilities().await {
                    let _ = tx_clone.send(HardwareUpdate::KeyboardCapabilities(caps)).await;
                }
            });
            
            Some(handle)
        } else {
            None
        };

        // Check for updates
        let tx_update = hw_update_tx.clone();
        tokio::spawn(async move {
            let current_version = env!("CARGO_PKG_VERSION");
            let url = "https://api.github.com/repos/weter11/lapsphere/releases/latest";

            let agent = ureq::AgentBuilder::new()
                .user_agent("LapSphere-Update-Checker")
                .build();

            match agent.get(url).call() {
                Ok(response) => {
                    if let Ok(json) = response.into_json::<serde_json::Value>() {
                        if let Some(tag) = json["tag_name"].as_str() {
                            let latest = tag.trim_start_matches('v');
                            if latest != current_version {
                                let body = json["body"].as_str().unwrap_or("No changelog provided.").to_string();
                                let _ = tx_update.send(HardwareUpdate::UpdateInfo(latest.to_string(), body)).await;
                            }
                        }
                    }
                }
                Err(e) => log::warn!("Failed to check for updates: {}", e),
            }
        });
        
        // Set coordinator handle in state
        state.coordinator_handle = coordinator_handle.clone();
        
        // Apply theme
        let theme = LapSphereTheme::new(&state.config.theme, cc.egui_ctx.global_style().visuals.dark_mode);
        theme.apply_with_font_size(&cc.egui_ctx, &state.config.font_size);

        // Apply current profile to daemon on startup to ensure background jobs are active
        if let Some(profile) = state.current_profile().cloned() {
            if let Some(ref client) = dbus_client {
                let _ = client.apply_profile(profile);
            }
        }

        let system_tray = match SystemTray::new(&state.config.profiles, &state.config.current_profile) {
            Ok(tray) => Some(tray),
            Err(e) => {
                log::warn!("Failed to initialize system tray: {}", e);
                None
            }
        };
        
        let last_tray_profile = state.config.current_profile.clone();
        let last_tray_profiles_count = state.config.profiles.len();

        Self {
            state,
            dbus_client,
            theme,
            system_tray,
            force_quit: false,
            hw_update_tx,
            hw_update_rx,
            shortcuts: KeyboardShortcuts::new(),
            startup_frames: 10,
            last_tray_profile,
            last_tray_profiles_count,
        }
    }
    
    fn handle_hardware_updates(&mut self) {
        // Process all pending updates (non-blocking)
        while let Ok(update) = self.hw_update_rx.try_recv() {
            match update {
                HardwareUpdate::SystemInfo(info) => {
                    self.state.system_info = Some(info);
                }
                HardwareUpdate::MemoryInfo(info) => {
                    self.state.memory_info = Some(info);
                }
                HardwareUpdate::CpuInfo(info) => {
                    self.state.cpu_info = Some(info);
                }
                HardwareUpdate::GpuInfo(info) => {
                    self.state.gpu_info = info;

                    // Auto-populate ranges and limits if we found them in the periodic update
                    if let Some(nvidia) = self.state.gpu_info.iter().find(|g| g.name.contains("NVIDIA")) {
                        if self.state.gpu_clock_ranges.is_none() {
                            if let Some(range) = nvidia.core_clock_range {
                                self.state.gpu_clock_ranges = Some(range);
                                self.state.gpu_clock_ranges_error = None;
                            }
                        }
                        if self.state.gpu_core_offset_limits.is_none() {
                            if let Some(limits) = nvidia.core_offset_limits {
                                self.state.gpu_core_offset_limits = Some(limits);
                                self.state.gpu_core_offset_error = None;
                            }
                        }
                        if self.state.gpu_mem_offset_limits.is_none() {
                            if let Some(limits) = nvidia.memory_offset_limits {
                                self.state.gpu_mem_offset_limits = Some(limits);
                                self.state.gpu_mem_offset_error = None;
                            }
                        }
                    }
                }
                HardwareUpdate::BatteryInfo(info) => {
                    self.state.battery_info = Some(info);
                }
                HardwareUpdate::WifiInfo(info) => {
                    self.state.wifi_info = info;
                }
                HardwareUpdate::GamepadInfo(connected_gamepads) => {
                    self.state.gamepad_info = connected_gamepads.clone();

                    // Single reconciliation point (see gamepad_registry): stable
                    // uids persist as before, volatile sysfs-path uids are
                    // session-scoped so reconnects cannot grow the database,
                    // and newly resolved stable identities adopt same-device
                    // rows instead of duplicating them.
                    if crate::gamepad_registry::reconcile(
                        &mut self.state.config.remembered_gamepads,
                        &connected_gamepads,
                    ) {
                        let _ = self.state.save_settings();
                    }
                }
                HardwareUpdate::FanInfo(info) => {
                    self.state.fan_info = info;
                }
                HardwareUpdate::StorageDeviceInfo(info) => {
                    self.state.storage_device_info = info;
                }
                HardwareUpdate::MountInfo(info) => {
                    self.state.mount_info = info;
                }
                HardwareUpdate::HardwareInterface(info) => {
                    self.state.hardware_interface = Some(info);
                }
                HardwareUpdate::WebcamState(state) => {
                    self.state.webcam_enabled = Some(state);
                }
                HardwareUpdate::DaemonLogs(mut logs) => {
                    if !self.state.log_paused {
                        if logs.len() > 2000 {
                            logs.drain(0..logs.len() - 2000);
                        }
                        self.state.daemon_logs = logs.into();
                    }
                }
                HardwareUpdate::UpdateInfo(version, changelog) => {
                    self.state.new_version_available = Some(version);
                    self.state.latest_changelog = Some(changelog);
                }
                HardwareUpdate::GpuClockRanges(result) => {
                    match result {
                        Ok(ranges) => {
                            self.state.gpu_clock_ranges = Some(ranges);
                            self.state.gpu_clock_ranges_error = None;
                        }
                        Err(e) => self.state.gpu_clock_ranges_error = Some(e),
                    }
                }
                HardwareUpdate::GpuCoreOffsetLimits(result) => {
                    match result {
                        Ok(limits) => {
                            self.state.gpu_core_offset_limits = Some(limits);
                            self.state.gpu_core_offset_error = None;
                        }
                        Err(e) => self.state.gpu_core_offset_error = Some(e),
                    }
                }
                HardwareUpdate::GpuMemOffsetLimits(result) => {
                    match result {
                        Ok(limits) => {
                            self.state.gpu_mem_offset_limits = Some(limits);
                            self.state.gpu_mem_offset_error = None;
                        }
                        Err(e) => self.state.gpu_mem_offset_error = Some(e),
                    }
                }
                HardwareUpdate::AvailableThresholds(start, end) => {
                    self.state.available_start_thresholds = start;
                    self.state.available_end_thresholds = end;
                }
                HardwareUpdate::TdpProfiles(profiles) => {
                    self.state.available_tdp_profiles = profiles;
                }
                HardwareUpdate::KeyboardCapabilities(caps) => {
                    self.state.keyboard_capabilities = Some(caps);
                }
            }
        }
        
        // Check pending battery update
        if let Some(mut rx) = self.state.pending_battery_update.take() {
            match rx.try_recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    self.state.show_message(format!("Battery update failed: {}", e), true);
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    self.state.pending_battery_update = Some(rx);
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.state.show_message("Battery update channel closed", true);
                }
            }
        }
    }
    
    fn draw_top_bar(&mut self, ui: &mut egui::Ui) {
        let mut dismiss_update = false;
        if let Some(version) = &self.state.new_version_available {
            let version = version.clone();
            egui::Panel::top("update_banner").show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(RichText::new(format!("🚀 Update Available: v{}", version)).strong().color(egui::Color32::from_rgb(255, 200, 0)));
                    if ui.link("View Details").clicked() {
                        self.state.current_page = Page::Settings;
                        self.state.settings_tab = crate::app::SettingsTab::About;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Dismiss").clicked() {
                            dismiss_update = true;
                        }
                    });
                });
            });
        }

        if dismiss_update {
            self.state.new_version_available = None;
        }

        egui::Panel::top("top_bar").show_inside(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);

                let time_str = Local::now().format("%H:%M:%S").to_string();
                let date_str = Local::now().format("%Y-%m-%d").to_string();
                let profile_str = format!("Profile: {}", self.state.config.current_profile);
                let base_size = TextStyle::Small.resolve(&ui.global_style()).size;
                let top_bar_size = base_size + 1.0;
                let mono_font = FontId::new(top_bar_size, FontFamily::Monospace);
                let text_color = ui.visuals().text_color();
                let text_font = FontId::new(top_bar_size, FontFamily::Proportional);
                let right_width = ui.ctx().fonts_mut(|fonts| {
                    let time_width = fonts.layout_no_wrap(time_str.clone(), mono_font.clone(), text_color).size().x;
                    let date_width = fonts.layout_no_wrap(date_str.clone(), mono_font.clone(), text_color).size().x;
                    let profile_width = fonts.layout_no_wrap(profile_str.clone(), text_font.clone(), text_color).size().x;
                    time_width.max(date_width).max(profile_width)
                }) + 16.0;
                let tabs_width = (ui.available_width() - right_width).max(0.0);

                ui.allocate_ui_with_layout(
                    egui::vec2(tabs_width, ui.available_height()),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.selectable_value(&mut self.state.current_page, Page::Statistics, "📊 Statistics");
                            ui.selectable_value(&mut self.state.current_page, Page::Profiles, "📋 Profiles");
                            ui.selectable_value(&mut self.state.current_page, Page::Tuning, "🔧 Tuning");
                            ui.selectable_value(&mut self.state.current_page, Page::Settings, "⚙ Settings");
                            if ui.button("❓ Help").clicked() {
                                self.shortcuts.toggle_help();
                            }
                        });
                    },
                );

                ui.allocate_ui_with_layout(
                    egui::vec2(right_width, ui.available_height()),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(time_str).font(mono_font.clone()));
                            ui.label(RichText::new(date_str).font(mono_font.clone()));
                            ui.label(RichText::new(profile_str).font(text_font.clone()));
                        });
                    },
                );
            });
            ui.add_space(6.0);
        });
        
        // Status message bar (if any)
        if let Some(ref msg) = self.state.status_message.clone() {
            if msg.shown_at.elapsed() < Duration::from_secs(5) {
                egui::Panel::top("status_bar").show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        let color = if msg.is_error {
                            egui::Color32::from_rgb(220, 80, 80)
                        } else {
                            egui::Color32::from_rgb(80, 200, 120)
                        };
                        ui.colored_label(color, &msg.text);
                    });
                });
            } else {
                self.state.status_message = None;
            }
        }
    }

    fn handle_tray_events(&mut self, ctx: &Context) {
        let Some(tray) = self.system_tray.as_mut() else {
            return;
        };

        // Sync profile list if count changed
        if self.state.config.profiles.len() != self.last_tray_profiles_count {
            tray.set_profiles(&self.state.config.profiles);
            self.last_tray_profiles_count = self.state.config.profiles.len();
            // Force current profile sync as well since menu rebuilt
            tray.set_current_profile(&self.state.config.current_profile);
            self.last_tray_profile = self.state.config.current_profile.clone();
        }

        // Sync current profile if changed in main window
        if self.state.config.current_profile != self.last_tray_profile {
            tray.set_current_profile(&self.state.config.current_profile);
            self.last_tray_profile = self.state.config.current_profile.clone();
        }

        if let Some(event) = tray.handle_events() {
            match event {
                TrayEvent::ShowWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                TrayEvent::ShowStatistics => {
                    self.state.current_page = Page::Statistics;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                TrayEvent::SwitchProfile(idx) => {
                    if let Some(profile) = self.state.config.profiles.get(idx).cloned() {
                        self.state.config.current_profile = profile.name.clone();
                        if let Some(client) = &self.dbus_client {
                            let _ = client.apply_profile(profile);
                        }
                    }
                }
                TrayEvent::Quit => {
                    self.force_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }
}

impl eframe::App for LapSphereApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.startup_frames > 0 {
            let start_in_tray = std::env::args().any(|arg| arg == "--tray");
            if self.state.config.start_minimized || start_in_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            self.startup_frames -= 1;
        }

        // Handle keyboard shortcuts
        self.shortcuts.handle_shortcuts(&ctx, &mut self.state);
        
        // Handle background hardware updates
        self.handle_hardware_updates();

        self.state.clamp_fan_selection();

        self.handle_tray_events(&ctx);
        
        if ctx.input(|input| input.viewport().close_requested())
            && self.state.config.tray_enabled
            && !self.force_quit
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // Draw top bar
        self.draw_top_bar(ui);
        
        // Update theme if it's Auto to react to system theme changes
        if self.state.config.theme == Theme::Auto {
            let is_dark = ctx.global_style().visuals.dark_mode;
            if is_dark != self.theme.visuals.dark_mode {
                self.theme = LapSphereTheme::new(&self.state.config.theme, is_dark);
                self.theme.apply_with_font_size(&ctx, &self.state.config.font_size);
            }
        }

        // Draw main content
        CentralPanel::default().show_inside(ui, |ui| {
            match self.state.current_page {
                Page::Statistics => {
                    statistics::draw(ui, &mut self.state);
                }
                Page::Profiles => {
                    profiles::draw(ui, &mut self.state, self.dbus_client.as_ref());
                }
                Page::Tuning => {
                    let hw_update_tx = self.hw_update_tx.clone();
                    tuning::draw(ui, &mut self.state, self.dbus_client.as_ref(), hw_update_tx);
                }
                Page::Settings => {
                    settings::draw(ui, &mut self.state, &mut self.theme, &ctx, self.dbus_client.as_ref());
                }
            }
        });
        
        // Request repaint if there are pending updates
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(client) = &self.dbus_client {
            let client = client.clone();
            // Use a fresh runtime for shutdown to avoid potential nesting issues
            // and ensure commands are processed before the main runtime closes.
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                let _ = rt.block_on(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(2), client.set_all_fans_auto()).await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), client.shutdown_daemon()).await;
                });
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SettingsConfig {
    theme: Theme,
    start_minimized: bool,
    tray_enabled: bool,
    autostart: bool,
    cpu_scheduler: String,
    font_size: FontSize,
    statistics_sections: StatisticsSections,
    tuning_section_order: Vec<String>,
    battery_settings: BatterySettings,
    log_limit: usize,
    log_filter_trace: bool,
    remembered_gamepads: Vec<GamepadInfo>,
}

impl Default for SettingsConfig {
    fn default() -> Self {
        let config = AppConfig::default();
        Self {
            theme: config.theme,
            start_minimized: config.start_minimized,
            tray_enabled: config.tray_enabled,
            autostart: config.autostart,
            cpu_scheduler: config.cpu_scheduler,
            font_size: config.font_size,
            statistics_sections: config.statistics_sections,
            tuning_section_order: config.tuning_section_order,
            battery_settings: config.battery_settings,
            log_limit: config.log_limit,
            log_filter_trace: config.log_filter_trace,
            remembered_gamepads: config.remembered_gamepads.clone(),
        }
    }
}

impl From<&AppConfig> for SettingsConfig {
    fn from(config: &AppConfig) -> Self {
        Self {
            theme: config.theme.clone(),
            start_minimized: config.start_minimized,
            tray_enabled: config.tray_enabled,
            autostart: config.autostart,
            cpu_scheduler: config.cpu_scheduler.clone(),
            font_size: config.font_size.clone(),
            statistics_sections: config.statistics_sections.clone(),
            tuning_section_order: config.tuning_section_order.clone(),
            battery_settings: config.battery_settings.clone(),
            log_limit: config.log_limit,
            log_filter_trace: config.log_filter_trace,
            remembered_gamepads: config.remembered_gamepads.clone(),
        }
    }
}

impl SettingsConfig {
    fn apply_to(&self, config: &mut AppConfig) {
        config.theme = self.theme.clone();
        config.start_minimized = self.start_minimized;
        config.tray_enabled = self.tray_enabled;
        config.autostart = self.autostart;
        config.cpu_scheduler = self.cpu_scheduler.clone();
        config.font_size = self.font_size.clone();
        config.statistics_sections = self.statistics_sections.clone();
        config.tuning_section_order = self.tuning_section_order.clone();
        config.battery_settings = self.battery_settings.clone();
        config.log_limit = self.log_limit;
        config.log_filter_trace = self.log_filter_trace;
        config.remembered_gamepads = self.remembered_gamepads.clone();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ProfilesConfig {
    profiles: Vec<Profile>,
    current_profile: String,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        let config = AppConfig::default();
        Self {
            profiles: config.profiles,
            current_profile: config.current_profile,
        }
    }
}

impl From<&AppConfig> for ProfilesConfig {
    fn from(config: &AppConfig) -> Self {
        Self {
            profiles: config.profiles.clone(),
            current_profile: config.current_profile.clone(),
        }
    }
}

impl ProfilesConfig {
    fn apply_to(&self, config: &mut AppConfig) {
        config.profiles = self.profiles.clone();
        config.current_profile = self.current_profile.clone();
    }
}

fn load_settings_from_disk(path: &str) -> anyhow::Result<SettingsConfig> {
    let json = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&json)?)
}

fn load_profiles_from_disk(path: &str) -> anyhow::Result<ProfilesConfig> {
    let json = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&json)?)
}

pub fn get_config_dir() -> String {
    if cfg!(target_os = "windows") {
        let app_data = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        format!("{}/lapsphere", app_data.replace("\\", "/"))
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{}/.config/lapsphere", home)
    }
}

pub fn get_crash_dir() -> String {
    get_config_dir()
}

pub fn load_config_from_disk() -> anyhow::Result<AppConfig> {
    let config_dir = get_config_dir();
    let settings_path = format!("{}/settings.json", config_dir);
    let profiles_path = format!("{}/profiles.json", config_dir);
    let legacy_path = format!("{}/config.json", config_dir);

    let legacy_config = if Path::new(&legacy_path).exists() {
        let json = std::fs::read_to_string(&legacy_path)?;
        Some(serde_json::from_str::<AppConfig>(&json)?)
    } else {
        None
    };

    let settings = if Path::new(&settings_path).exists() {
        Some(load_settings_from_disk(&settings_path)?)
    } else {
        legacy_config.as_ref().map(SettingsConfig::from)
    };

    let profiles = if Path::new(&profiles_path).exists() {
        Some(load_profiles_from_disk(&profiles_path)?)
    } else {
        legacy_config.as_ref().map(ProfilesConfig::from)
    };

    let mut config = AppConfig::default();
    if let Some(settings) = settings {
        settings.apply_to(&mut config);
    }
    if let Some(profiles) = profiles {
        profiles.apply_to(&mut config);
    }

    if config.start_minimized {
        config.tray_enabled = true;
    }

    config.statistics_sections.section_order =
        statistics::normalize_section_order(&config.statistics_sections.section_order);

    if legacy_config.is_some()
        && (!Path::new(&settings_path).exists() || !Path::new(&profiles_path).exists())
    {
        if let Err(err) = save_settings_to_disk(&config) {
            log::warn!("Failed to migrate settings config: {}", err);
        }
        if let Err(err) = save_profiles_to_disk(&config) {
            log::warn!("Failed to migrate profiles config: {}", err);
        }
    }

    Ok(config)
}

fn save_settings_to_disk(config: &AppConfig) -> anyhow::Result<()> {
    let config_dir = get_config_dir();
    std::fs::create_dir_all(&config_dir)?;
    let settings_path = format!("{}/settings.json", config_dir);
    let json = serde_json::to_string_pretty(&SettingsConfig::from(config))?;
    std::fs::write(settings_path, json)?;

    // Push the new poll rates to the daemon so job intervals update live
    // (fire-and-forget; the daemon keeps its own defaults when this fails
    // or no daemon connection exists yet, e.g. during startup migration).
    if let Some(client) = crate::dbus_client::shared_client() {
        if let Ok(sections) = serde_json::to_string(&config.statistics_sections) {
            let _ = client.sync_daemon_poll_settings(sections);
        }
    }

    // Handle autostart
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME")?;
        let autostart_dir = format!("{}/.config/autostart", home);
    let desktop_file = format!("{}/io.lapsphere.LapSphere.desktop", autostart_dir);

        if config.autostart {
            std::fs::create_dir_all(&autostart_dir)?;
            let content = format!(
                "[Desktop Entry]\n\
                Type=Application\n\
                Name=LapSphere\n\
                Exec=lapsphere --tray\n\
                Icon=lapsphere\n\
                X-GNOME-Autostart-enabled=true\n"
            );
            std::fs::write(&desktop_file, content)?;
        } else {
            // Write a desktop file that explicitly disables autostart to override system-wide one
            std::fs::create_dir_all(&autostart_dir)?;
            let content = format!(
                "[Desktop Entry]\n\
                Type=Application\n\
                Name=LapSphere\n\
                Exec=lapsphere --tray\n\
                Icon=lapsphere\n\
                X-GNOME-Autostart-enabled=false\n\
                NoDisplay=true\n\
                Hidden=true\n"
            );
            std::fs::write(&desktop_file, content)?;
        }
    }

    Ok(())
}

fn save_profiles_to_disk(config: &AppConfig) -> anyhow::Result<()> {
    let config_dir = get_config_dir();
    std::fs::create_dir_all(&config_dir)?;
    let profiles_path = format!("{}/profiles.json", config_dir);
    let json = serde_json::to_string_pretty(&ProfilesConfig::from(config))?;
    std::fs::write(profiles_path, json)?;
    Ok(())
}
