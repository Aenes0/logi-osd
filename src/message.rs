//! Textos, ícones e tons das pílulas OSD (pt-BR). Funções puras.

use std::time::Duration;

use crate::battery::BatteryAlert;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon { CapsLock, NumLock, Insert, FnLock, KeyboardBacklight, Battery, Charging, Switch }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone { Accent, Dim, Warning, Critical }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value { None, Text(String, Tone), Dots { on: u8, total: u8 } }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pill { pub icon: Icon, pub icon_tone: Tone, pub label: String, pub value: Value, pub duration: Duration }

/// `Insert` fica só por interface: o teclado não reporta Insert via HID++ (fora do escopo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKey { Caps, Num, Insert }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Durations { pub normal: Duration, pub critical: Duration } // 1500 / 4000 ms

pub const FALLBACK_NAME: &str = "Dispositivo Logitech";

/// Nome exibido: o aparado, ou `FALLBACK_NAME` se vazio.
fn display_name(name: &str) -> &str {
    let trimmed = name.trim();
    if trimmed.is_empty() { FALLBACK_NAME } else { trimmed }
}

pub fn lock(key: LockKey, on: bool, d: &Durations) -> Pill {
    let (icon, label) = match key {
        LockKey::Caps => (Icon::CapsLock, "Caps Lock"),
        LockKey::Num => (Icon::NumLock, "Num Lock"),
        LockKey::Insert => (Icon::Insert, "Insert"),
    };
    let (text, tone) = if on { ("Ativado", Tone::Accent) } else { ("Desativado", Tone::Dim) };
    Pill {
        icon,
        icon_tone: Tone::Accent,
        label: label.into(),
        value: Value::Text(text.into(), tone),
        duration: d.normal,
    }
}

pub fn fn_lock(media_keys: bool, d: &Durations) -> Pill {
    let text = if media_keys { "Teclas de mídia" } else { "Teclas F" };
    Pill {
        icon: Icon::FnLock,
        icon_tone: Tone::Accent,
        label: "Fn".into(),
        value: Value::Text(text.into(), Tone::Dim),
        duration: d.normal,
    }
}

/// Bolinhas: `total = levels - 1` (nível 0 = luz apagada); `on = level` limitado a `total`.
pub fn backlight(level: u8, levels: u8, d: &Durations) -> Pill {
    let total = levels.saturating_sub(1);
    Pill {
        icon: Icon::KeyboardBacklight,
        icon_tone: Tone::Accent,
        label: "Teclado".into(),
        value: Value::Dots { on: level.min(total), total },
        duration: d.normal,
    }
}

pub fn battery(alert: BatteryAlert, name: &str, d: &Durations) -> Pill {
    let name = display_name(name);
    match alert {
        BatteryAlert::Low(p) => battery_low(name, p, Tone::Warning, d.normal),
        BatteryAlert::Critical(p) => battery_low(name, p, Tone::Critical, d.critical),
        BatteryAlert::Charging(p) => Pill {
            icon: Icon::Charging,
            icon_tone: Tone::Accent,
            label: format!("{name} carregando"),
            value: Value::Text(format!("{p}%"), Tone::Dim),
            duration: d.normal,
        },
        BatteryAlert::Charged => Pill {
            icon: Icon::Charging,
            icon_tone: Tone::Accent,
            label: format!("{name} carregado"),
            value: Value::None,
            duration: d.normal,
        },
    }
}

/// Aviso de bateria baixa: ícone e valor no mesmo tom (laranja ou vermelho).
fn battery_low(name: &str, percent: u8, tone: Tone, duration: Duration) -> Pill {
    Pill {
        icon: Icon::Battery,
        icon_tone: tone,
        label: format!("{name}: bateria baixa"),
        value: Value::Text(format!("{percent}%"), tone),
        duration,
    }
}

/// `hosts` = `(total de hosts, host atual 0-based)`, como em `DeviceInfo::hosts`.
/// O canal só aparece quando o dispositivo tem mais de um host.
pub fn connected(name: &str, hosts: Option<(u8, u8)>, d: &Durations) -> Pill {
    let value = match hosts {
        Some((total, current)) if total > 1 => {
            Value::Text(format!("Canal {}", current.saturating_add(1)), Tone::Dim)
        }
        _ => Value::None,
    };
    Pill {
        icon: Icon::Switch,
        icon_tone: Tone::Accent,
        label: format!("{} conectado", display_name(name)),
        value,
        duration: d.normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn ms(n: u64) -> Duration { Duration::from_millis(n) }
    const D: Durations = Durations { normal: ms(1500), critical: ms(4000) };

    #[test] fn caps_on()  { let p = lock(LockKey::Caps, true, &D); assert_eq!((p.icon, p.label.as_str()), (Icon::CapsLock, "Caps Lock")); assert_eq!(p.value, Value::Text("Ativado".into(), Tone::Accent)); assert_eq!(p.duration, ms(1500)); }
    #[test] fn num_off()  { assert_eq!(lock(LockKey::Num, false, &D).value, Value::Text("Desativado".into(), Tone::Dim)); }
    #[test] fn insert_label() { assert_eq!(lock(LockKey::Insert, true, &D).label, "Insert"); }
    #[test] fn fn_lock_texts() { assert_eq!(fn_lock(false, &D).value, Value::Text("Teclas F".into(), Tone::Dim)); assert_eq!(fn_lock(true, &D).value, Value::Text("Teclas de mídia".into(), Tone::Dim)); assert_eq!(fn_lock(true,&D).label, "Fn"); }
    #[test] fn backlight_dots() { let p = backlight(3, 9, &D); assert_eq!(p.label, "Teclado"); assert_eq!(p.value, Value::Dots { on: 3, total: 8 }); }
    #[test] fn battery_low_orange() { let p = battery(BatteryAlert::Low(15), "MX Keys Mini", &D); assert_eq!(p.label, "MX Keys Mini: bateria baixa"); assert_eq!(p.value, Value::Text("15%".into(), Tone::Warning)); assert_eq!(p.duration, ms(1500)); }
    #[test] fn battery_critical_red_4s() { let p = battery(BatteryAlert::Critical(5), "MX Keys Mini", &D); assert_eq!(p.value, Value::Text("5%".into(), Tone::Critical)); assert_eq!(p.duration, ms(4000)); }
    #[test] fn charging_and_charged() { assert_eq!(battery(BatteryAlert::Charging(42), "G602", &D).label, "G602 carregando"); assert_eq!(battery(BatteryAlert::Charging(42), "G602", &D).value, Value::Text("42%".into(), Tone::Dim)); let p = battery(BatteryAlert::Charged, "G602", &D); assert_eq!((p.label.as_str(), &p.value), ("G602 carregado", &Value::None)); }
    #[test] fn connected_with_channel() { let p = connected("MX Keys Mini", Some((3, 0)), &D); assert_eq!(p.label, "MX Keys Mini conectado"); assert_eq!(p.value, Value::Text("Canal 1".into(), Tone::Dim)); assert_eq!(p.icon, Icon::Switch); }
    #[test] fn connected_single_host_has_no_channel() { assert_eq!(connected("G602", Some((1, 0)), &D).value, Value::None); assert_eq!(connected("G602", None, &D).value, Value::None); }
    #[test] fn missing_name_falls_back_to_dispositivo_logitech() { assert_eq!(connected("  ", None, &D).label, "Dispositivo Logitech conectado"); }
}
