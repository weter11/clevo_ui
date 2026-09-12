//! Remembered-gamepad database reconciliation.
//!
//! Identity contract: a remembered entry is only as stable as the daemon's
//! `uid`. When the daemon cannot resolve a hardware identity (Bluetooth HID
//! nodes expose no `device/uniq`, and udev property data can be absent at
//! poll time), it falls back to the volatile sysfs input path
//! (`/sys/class/input/inputNN`), which the kernel renumbers on every
//! reconnect. Entries under such uids are therefore SESSION-SCOPED:
//!
//! * visible while the pad is connected (the statistics page renders this
//!   database),
//! * removed instead of being flipped to `Disconnected`,
//! * never allowed to accumulate one row per reconnect (the historical bug:
//!   every session of a BT DualShock added another `inputNN` row forever).

use lapsphere_common::types::{GamepadInfo, GamepadStatus};

/// A uid the daemon had to fall back to the sysfs input node path for.
/// These paths are re-assigned by the kernel on every re-enumeration, so
/// two sessions of the same physical pad surface as two different pads.
pub fn is_volatile_uid(uid: &str) -> bool {
    uid.starts_with("/sys/")
}

/// Reconcile the remembered-pad database in place against one daemon
/// snapshot. Returns `true` when anything changed (caller persists).
///
/// Rules:
/// 1. Stable-uid entries keep the existing semantics: status/battery/name
///    refresh in place while connected; absent entries flip to
///    `Disconnected` and stay persisted.
/// 2. Volatile-uid (`/sys/...`) entries are session-scoped: updated while
///    connected, removed when gone — a reconnecting pad reuses or creates
///    exactly one row instead of growing the database.
/// 3. A connected pad with a newly resolved STABLE uid that matches no
///    stored uid may adopt the identity of an existing row with the same
///    name + connection type (same physical device re-identified); other
///    unknown connected pads are appended.
pub fn reconcile(remembered: &mut Vec<GamepadInfo>, connected: &[GamepadInfo]) -> bool {
    let mut changed = false;

    // 1) Update existing entries; drop vanished session-scoped ones.
    remembered.retain_mut(|rem| match connected.iter().find(|c| c.uid == rem.uid) {
        Some(conn) => {
            if rem.status != GamepadStatus::Connected
                || rem.battery_level != conn.battery_level
                || rem.power_status != conn.power_status
                || rem.connection_type != conn.connection_type
                || rem.name != conn.name
            {
                rem.status = GamepadStatus::Connected;
                rem.name = conn.name.clone();
                rem.battery_level = conn.battery_level;
                rem.power_status = conn.power_status.clone();
                rem.connection_type = conn.connection_type.clone();
                changed = true;
            }
            true
        }
        None => {
            if is_volatile_uid(&rem.uid) {
                changed = true;
                false
            } else {
                if rem.status != GamepadStatus::Disconnected {
                    rem.status = GamepadStatus::Disconnected;
                    changed = true;
                }
                true
            }
        }
    });

    // 2) Adopt or append connected pads not yet known under their uid.
    for conn in connected {
        if remembered.iter().any(|r| r.uid == conn.uid) {
            continue;
        }
        if !is_volatile_uid(&conn.uid) {
            if let Some(slot) = remembered.iter_mut().find(|r| {
                r.uid != conn.uid
                    && r.name == conn.name
                    && r.connection_type == conn.connection_type
            }) {
                slot.id = conn.id.clone();
                slot.uid = conn.uid.clone();
                slot.status = GamepadStatus::Connected;
                slot.battery_level = conn.battery_level;
                slot.power_status = conn.power_status.clone();
                changed = true;
                continue;
            }
        }
        let mut entry = conn.clone();
        entry.status = GamepadStatus::Connected;
        remembered.push(entry);
        changed = true;
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use lapsphere_common::types::{ConnectionType, PowerStatus};

    fn pad(uid: &str, name: &str, ct: ConnectionType, status: GamepadStatus) -> GamepadInfo {
        GamepadInfo {
            name: name.to_string(),
            id: "inputX".to_string(),
            uid: uid.to_string(),
            status,
            battery_level: Some(77),
            connection_type: ct,
            power_status: PowerStatus::Discharging,
        }
    }

    #[test]
    fn startup_sweep_removes_disconnected_volatile_rows_only() {
        let mut db = vec![
            pad(
                "/sys/class/input/input38",
                "Wireless Controller",
                ConnectionType::Wireless,
                GamepadStatus::Disconnected,
            ),
            pad(
                "f4939fdd9ec9",
                "Wireless Controller",
                ConnectionType::Wireless,
                GamepadStatus::Disconnected,
            ),
            pad(
                "/sys/class/input/input41",
                "Wireless Controller",
                ConnectionType::Wireless,
                GamepadStatus::Disconnected,
            ),
        ];
        assert!(reconcile(&mut db, &[]));
        assert_eq!(db.len(), 1);
        assert_eq!(db[0].uid, "f4939fdd9ec9");
    }

    #[test]
    fn reconnecting_volatile_pad_never_grows_database() {
        let mut db = vec![];
        // Session 1: pad enumerated as input38.
        assert!(reconcile(
            &mut db,
            &[pad(
                "/sys/class/input/input38",
                "Wireless Controller",
                ConnectionType::Wireless,
                GamepadStatus::Connected,
            )]
        ));
        assert_eq!(db.len(), 1);
        // Pad disconnects: session-scoped row must vanish, not linger.
        assert!(reconcile(&mut db, &[]));
        assert!(db.is_empty());
        // Session 2: same physical pad, kernel assigned input51.
        assert!(reconcile(
            &mut db,
            &[pad(
                "/sys/class/input/input51",
                "Wireless Controller",
                ConnectionType::Wireless,
                GamepadStatus::Connected,
            )]
        ));
        assert_eq!(db.len(), 1);
        assert_eq!(db[0].uid, "/sys/class/input/input51");
    }

    #[test]
    fn stable_entries_keep_persisting_across_disconnect() {
        let mut db = vec![pad(
            "f4939fdd9ec9",
            "Wireless Controller",
            ConnectionType::Wireless,
            GamepadStatus::Connected,
        )];
        assert!(reconcile(&mut db, &[]));
        assert_eq!(db.len(), 1);
        assert_eq!(db[0].status, GamepadStatus::Disconnected);
    }

    #[test]
    fn newly_resolved_stable_uid_adopts_matching_row() {
        // First session resolved only an ID_PATH-style uid; a later session
        // resolves the real MAC for the same physical pad.
        let mut db = vec![pad(
            "pci-0000:05:00.4-usb-0:2:1.0",
            "Wireless Controller",
            ConnectionType::Wireless,
            GamepadStatus::Disconnected,
        )];
        assert!(reconcile(
            &mut db,
            &[pad(
                "f4939fdd9ec9",
                "Wireless Controller",
                ConnectionType::Wireless,
                GamepadStatus::Connected,
            )]
        ));
        assert_eq!(db.len(), 1, "must adopt, not duplicate");
        assert_eq!(db[0].uid, "f4939fdd9ec9");
        assert_eq!(db[0].status, GamepadStatus::Connected);
    }

    #[test]
    fn no_change_reports_false() {
        let mut db = vec![pad(
            "stable",
            "Pad",
            ConnectionType::Wired,
            GamepadStatus::Connected,
        )];
        let snap = vec![pad(
            "stable",
            "Pad",
            ConnectionType::Wired,
            GamepadStatus::Connected,
        )];
        assert!(!reconcile(&mut db, &snap));
    }
}
