# LapSphere

A hardware control and monitoring application for Uniwill/Clevo laptops on Linux.

LapSphere provides a modern GUI for tuning CPU and GPU performance, configuring fan curves, controlling keyboard backlighting, managing battery charge thresholds, and viewing live system statistics — all through a root daemon with a per-user GUI frontend.

---

## Features

### Hardware Monitoring (Statistics)
- **CPU** — per-core frequency, load, temperature, power draw (RAPL, amdgpu, zenpower), governor, boost state, AMD/Intel P-State, EPP
- **Memory** — used/available/total, type and speed (via dmidecode)
- **GPU** — frequency, memory frequency, temperature, hotspot/VRAM temps (via NVAPI), load, power, voltage, clock offsets
- **Battery** — charge %, voltage, current, power, health, charge thresholds
- **WiFi** — SSID, signal level, channel, PHY rate, actual throughput, temperature
- **Storage** — read/write speed, IOPS, temperature, mount usage
- **Fans** — speed, temperature per fan, control mode
- **Gamepads** — connection type, battery level, power status (persistent across sessions)

### CPU Tuning
- Scaling governor selection
- Frequency min/max limits
- CPU boost / Turbo toggle
- SMT / Hyperthreading toggle
- AMD P-State mode (active / passive / guided)
- Intel P-State mode (active / passive)
- Energy Performance Preference (EPP)
- Uniwill TDP control (PL1 / PL2 / PL4) with per-device ranges
- TDP performance profile (powersave / enthusiast / overboost on supported hardware)

### GPU Tuning (NVIDIA)
- GPU core and memory locked clock ranges
- Core clock offset (static, applied immediately via NVML)
- Memory clock offset
- Power limit
- **Advanced dynamic overclocking** — daemon applies offsets every poll cycle based on live temperature, frequency, and power draw, with configurable drain/power/critical-temp control zones and smart rounding
- NVIDIA fan speed control (manual per-fan or auto)
- PRIME profile switching (on-demand / nvidia / intel) via optimus-manager or prime-select

### Keyboard Backlight
- Supports single-zone RGB, 3-zone RGB, 4-zone RGB, per-key RGB, white-only keyboards
- Modes: Static color, multiple zones, per-key painting, Breathe, Cycle, Dance, Flash, Random Color, Tempo, Wave
- Live preview without saving

### Fan Control
- Custom fan curves per fan with a graphical editor
- Drag points or edit numerically
- Auto mode restore on profile switch

### Battery Charge Control
- Charge start/end threshold (flexicharger) via sysfs
- Supports Clevo legacy and CC4 flexicharger interfaces
- Available threshold values queried from hardware

### Profile System
- Multiple named profiles, each storing all tuning settings
- Instant apply on switch (hardware settings sent to daemon via DBus)
- Profiles persist to `~/.config/lapsphere/profiles.json`

### Other
- System tray with profile switcher
- Configurable per-section polling rates
- Daemon log viewer with level filtering, search, and copy
- Crash report generation to `~/.config/lapsphere/`
- Auto-start support
- Update checker (GitHub releases API)

---

## Architecture

```
┌──────────────────────────────────┐
│         lapsphere (GUI)          │  runs as regular user
│  egui frontend, DBus client      │
└────────────┬─────────────────────┘
             │ DBus (io.lapsphere.Control)
             │ system bus
┌────────────▼─────────────────────┐
│      lapsphere-daemon            │  runs as root
│  Hardware polling, sysfs writes  │
│  NVML, tuxedo_io ioctl, NVAPI    │
└──────────────────────────────────┘
```

The daemon exposes a system DBus service (`io.lapsphere.Control`) and polls hardware on configurable intervals. The GUI communicates exclusively through DBus — it never writes to hardware directly.

The daemon can also launch the GUI as the invoking user when started with `--gui`, which is the intended systemd/polkit flow.

---

## Requirements

### Runtime
- Linux kernel with tuxedo-drivers loaded (for fan/keyboard/TDP control on Clevo/Uniwill hardware)
- `dbus`
- `dmidecode` (for memory type/speed detection)
- `iw`, `ethtool` (for WiFi info)
- `pciutils` (`lspci`, for WiFi controller info)
- NVIDIA driver + NVML (`libnvidia-ml.so`) for GPU tuning
- `libnvidia-api.so.1` for hotspot/VRAM temperatures and voltage (optional)
- `optimus-manager` or `prime-select` for PRIME profile switching (optional)

### Build
- Rust toolchain (stable, tested with recent versions)
- `libdbus-1-dev`
- `pkg-config`
- GTK3 development headers (`libgtk-3-dev`)
- Wayland/X11 development libraries

---

## Building

```bash
cargo build --release --all
```

Binaries are placed in `target/release/`:
- `lapsphere` — GUI
- `lapsphere-daemon` — system daemon

### GitHub Actions artifacts

The CI workflow uploads two package artifacts per successful run:

- `lapsphere-ubuntu-24.04-deb` — a Debian package containing both binaries and their service, DBus, desktop, and icon files.
- `lapsphere-arch-pkg` — an Arch package containing the same application files.

The package artifacts are the supported CI outputs. A workflow run also has a source commit, so an artifact created by an older run is an older snapshot even when its package version remains `0.1.0`. The date shown in `debian/changelog` is package release metadata and does not identify when a CI artifact was built.

---

## Installation (Debian/Ubuntu)

A `debian/` directory is included for building a `.deb` package:

```bash
dpkg-buildpackage -us -uc -b
sudo dpkg -i ../lapsphere_*.deb
```

This installs:
- `/usr/bin/lapsphere` and `/usr/bin/lapsphere-daemon`
- DBus policy `/usr/share/dbus-1/system.d/io.lapsphere.Control.conf`
- DBus activation service `/usr/share/dbus-1/system-services/io.lapsphere.Control.service`
- Desktop entry and icon

---

## Running

### Recommended (via daemon)

```bash
# Start daemon (requires root)
sudo lapsphere-daemon --gui
```

The `--gui` flag causes the daemon to launch the GUI as the original user (read from `SUDO_UID` or `PKEXEC_UID`).

### Manual

```bash
# Terminal 1 (root)
sudo lapsphere-daemon

# Terminal 2 (user)
lapsphere
```

### Tray-only mode

```bash
lapsphere --tray
```

---

## Configuration

Settings are split into two files under `~/.config/lapsphere/`:

| File | Contents |
|---|---|
| `settings.json` | Theme, font size, polling rates, battery settings, autostart |
| `profiles.json` | All tuning profiles and the active profile name |

A legacy single-file format (`config.json`) is automatically migrated on first run.

---

## Project Structure

```
common/          Shared types (CpuInfo, GpuInfo, Profile, AppConfig, …)
daemon/          System daemon
  src/
    main.rs              Startup, polling scheduler, GPU overclock loop
    dbus_interface.rs    Exposed DBus methods
    hardware_detection.rs  Read-only hardware queries
    hardware_control.rs    Write paths (sysfs, NVML, tuxedo_io)
    tuxedo_io.rs         ioctl wrapper for /dev/tuxedo_io
    battery_control.rs   Charge threshold sysfs control
    polling_scheduler.rs Tokio-based job scheduler
gui/             egui GUI
  src/
    app.rs               App state, update loop, hardware channel
    dbus_client.rs       Async DBus command dispatch
    pages/               Statistics, Profiles, Tuning, Settings views
    widgets/             Fan curve editor
    theme.rs             Dark/light theme definitions
    polling_scheduler.rs Client-side refresh coordinator
    system_tray.rs       ksni system tray integration
drivers_src/     Vendored tuxedo-drivers kernel module sources
```

---

## Hardware Support

| Feature | Clevo | Uniwill |
|---|---|---|
| Fan control | ✅ | ✅ |
| TDP profiles | ✅ | ✅ |
| Uniwill TDP (PL1/PL2/PL4) | — | ✅ |
| Keyboard RGB | ✅ (3-zone, 1-zone) | ✅ (1-zone) |
| Webcam toggle | ✅ | — |
| Battery thresholds | ✅ (flexicharger) | via sysfs |

NVIDIA GPU features require a supported NVIDIA driver and card. AMD and Intel iGPUs are read-only (stats only).

---

## Caution for Uniwill
Be carefully if your laptop is Uniwill to use it with, where speed limit in RPM instead of percent, that feature is hard to implement correctly, Tuxedo know speed limits on every there laptop, I'm lacking that info.

## License

GPL-2.0 — see `debian/copyright`.
