//! Bateria: leituras normalizadas, conversões e máquina de estados de avisos.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeStatus { Discharging, Charging, Full, Unknown }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading { pub percent: u8, pub status: ChargeStatus }

/// Status comum a UNIFIED_BATTERY (0x1004) e BATTERY_STATUS (0x1000).
fn charge_status(status: u8) -> ChargeStatus {
    match status {
        0 => ChargeStatus::Discharging,
        1 | 2 | 4 => ChargeStatus::Charging,
        3 => ChargeStatus::Full,
        _ => ChargeStatus::Unknown,
    }
}

/// UNIFIED_BATTERY: `soc` se > 0; senão pelo bit de nível mais alto (8→90, 4→50, 2→20, 1→5).
pub fn reading_from_unified(soc: u8, level_flags: u8, status: u8) -> Option<BatteryReading> {
    let percent = if soc > 0 {
        soc
    } else if level_flags & 0x08 != 0 {
        90
    } else if level_flags & 0x04 != 0 {
        50
    } else if level_flags & 0x02 != 0 {
        20
    } else if level_flags & 0x01 != 0 {
        5
    } else {
        return None;
    };
    Some(BatteryReading { percent, status: charge_status(status) })
}

/// BATTERY_STATUS: `level == 0` = sem leitura.
pub fn reading_from_status(level: u8, status: u8) -> Option<BatteryReading> {
    (level > 0).then(|| BatteryReading { percent: level, status: charge_status(status) })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds { pub low: u8, pub critical: u8 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryAlert { Low(u8), Critical(u8), Charging(u8), Charged }

/// Máquina de estados pura (sem alocação): avisos únicos de baixo/crítico com re-arme
/// após carregar acima do limite, e avisos de carregando/carregado.
pub struct BatteryTracker {
    t: Thresholds,
    armed_low: bool,
    armed_critical: bool,
    charged_announced: bool,
    /// Status da leitura anterior; `None` = ainda não houve leitura.
    prev: Option<ChargeStatus>,
}

impl BatteryTracker {
    pub fn new(t: Thresholds) -> Self {
        Self { t, armed_low: true, armed_critical: true, charged_announced: false, prev: None }
    }

    pub fn update(&mut self, r: BatteryReading) -> Option<BatteryAlert> {
        match r.status {
            ChargeStatus::Charging | ChargeStatus::Full => {
                let prev = self.prev.replace(r.status);
                self.on_charging(r, prev)
            }
            ChargeStatus::Discharging => {
                self.prev = Some(r.status);
                self.charged_announced = false;
                self.check_thresholds(r.percent)
            }
            // Status desconhecido = "status inalterado": não mexe em `prev` nem em
            // `charged_announced` (evita reanunciar a cada oscilação Full/Unknown).
            ChargeStatus::Unknown => self.check_thresholds(r.percent),
        }
    }

    fn on_charging(&mut self, r: BatteryReading, prev: Option<ChargeStatus>) -> Option<BatteryAlert> {
        if r.percent > self.t.low { self.armed_low = true; }
        if r.percent > self.t.critical { self.armed_critical = true; }
        let is_full = r.status == ChargeStatus::Full || r.percent >= 100;
        let Some(prev) = prev else {
            // Primeira leitura: silenciosa; Full já conta como anunciado.
            self.charged_announced = is_full;
            return None;
        };
        if is_full {
            if self.charged_announced { return None; }
            self.charged_announced = true;
            return Some(BatteryAlert::Charged);
        }
        let was_charging = matches!(prev, ChargeStatus::Charging | ChargeStatus::Full);
        (!was_charging).then_some(BatteryAlert::Charging(r.percent))
    }

    fn check_thresholds(&mut self, percent: u8) -> Option<BatteryAlert> {
        if percent <= self.t.critical && self.armed_critical {
            self.armed_critical = false;
            self.armed_low = false;
            Some(BatteryAlert::Critical(percent))
        } else if percent <= self.t.low && self.armed_low {
            self.armed_low = false;
            Some(BatteryAlert::Low(percent))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const T: Thresholds = Thresholds { low: 15, critical: 5 };
    #[test] fn first_reading_above_low_is_silent()      { let mut b = BatteryTracker::new(T); assert_eq!(b.update(d(80)), None); }
    #[test] fn crossing_low_alerts_once()               { let mut b = BatteryTracker::new(T); b.update(d(20)); assert_eq!(b.update(d(15)), Some(BatteryAlert::Low(15))); assert_eq!(b.update(d(14)), None); assert_eq!(b.update(d(13)), None); }
    #[test] fn crossing_critical_after_low()            { let mut b = BatteryTracker::new(T); b.update(d(16)); b.update(d(15)); assert_eq!(b.update(d(5)), Some(BatteryAlert::Critical(5))); assert_eq!(b.update(d(4)), None); }
    #[test] fn first_reading_below_critical_only_critical() { let mut b = BatteryTracker::new(T); assert_eq!(b.update(d(4)), Some(BatteryAlert::Critical(4))); assert_eq!(b.update(d(3)), None); }
    #[test] fn first_reading_between_thresholds_alerts_low() { let mut b = BatteryTracker::new(T); assert_eq!(b.update(d(12)), Some(BatteryAlert::Low(12))); }
    #[test] fn jitter_without_charge_does_not_rearm()   { let mut b = BatteryTracker::new(T); b.update(d(15)); b.update(d(16)); assert_eq!(b.update(d(15)), None); }
    #[test] fn charge_above_low_rearms()                { let mut b = BatteryTracker::new(T); b.update(d(15)); assert_eq!(b.update(c(20)), Some(BatteryAlert::Charging(20))); b.update(d(16)); assert_eq!(b.update(d(15)), Some(BatteryAlert::Low(15))); }
    #[test] fn charge_below_low_rearms_only_critical()  { let mut b = BatteryTracker::new(T); b.update(d(15)); b.update(d(5)); b.update(c(10)); b.update(d(10)); assert_eq!(b.update(d(14)), None); assert_eq!(b.update(d(5)), Some(BatteryAlert::Critical(5))); }
    #[test] fn plug_cable_emits_charging()              { let mut b = BatteryTracker::new(T); b.update(d(42)); assert_eq!(b.update(c(42)), Some(BatteryAlert::Charging(42))); assert_eq!(b.update(c(43)), None); }
    #[test] fn reaching_full_emits_charged_once()       { let mut b = BatteryTracker::new(T); b.update(d(90)); b.update(c(99)); assert_eq!(b.update(full()), Some(BatteryAlert::Charged)); assert_eq!(b.update(full()), None); }
    #[test] fn charged_again_after_unplug()             { let mut b = BatteryTracker::new(T); b.update(d(90)); b.update(c(99)); b.update(full()); b.update(d(100)); b.update(c(99)); assert_eq!(b.update(full()), Some(BatteryAlert::Charged)); }
    #[test] fn discharging_to_full_directly_is_charged() { let mut b = BatteryTracker::new(T); b.update(d(98)); assert_eq!(b.update(full()), Some(BatteryAlert::Charged)); }
    #[test] fn charging_at_startup_is_silent()          { let mut b = BatteryTracker::new(T); assert_eq!(b.update(c(50)), None); assert_eq!(b.update(full()), Some(BatteryAlert::Charged)); }
    fn u(p: u8) -> BatteryReading { BatteryReading { percent: p, status: ChargeStatus::Unknown } }
    #[test] fn unknown_flicker_at_full_does_not_repeat_charged() { let mut b = BatteryTracker::new(T); b.update(d(98)); assert_eq!(b.update(full()), Some(BatteryAlert::Charged)); assert_eq!(b.update(u(100)), None); assert_eq!(b.update(full()), None); }
    #[test] fn unknown_between_charging_does_not_repeat_charging() { let mut b = BatteryTracker::new(T); b.update(d(40)); assert_eq!(b.update(c(50)), Some(BatteryAlert::Charging(50))); assert_eq!(b.update(u(50)), None); assert_eq!(b.update(c(51)), None); }
    // Dados reais do MX Keys Mini: `5a 08 01 01` (plugar) e `5a 08 00 00` (desplugar), 90%.
    #[test] fn real_mx_keys_plug_unplug() {
        let mut b = BatteryTracker::new(T);
        b.update(reading_from_unified(90, 0, 0).unwrap());
        assert_eq!(b.update(reading_from_unified(90, 0, 1).unwrap()), Some(BatteryAlert::Charging(90)));
        assert_eq!(b.update(reading_from_unified(90, 0, 0).unwrap()), None);
    }
    fn d(p: u8) -> BatteryReading { BatteryReading { percent: p, status: ChargeStatus::Discharging } }
    fn c(p: u8) -> BatteryReading { BatteryReading { percent: p, status: ChargeStatus::Charging } }
    fn full() -> BatteryReading { BatteryReading { percent: 100, status: ChargeStatus::Full } }

    // Copiados das Interfaces da Tarefa 6.
    #[test] fn unified_conversions() { assert_eq!(reading_from_unified(42,0,1), Some(c(42))); assert_eq!(reading_from_unified(0,2,0), Some(d(20))); assert_eq!(reading_from_unified(0,0,0), None); assert_eq!(reading_from_unified(100,8,3), Some(full())); }
    #[test] fn status_conversions()  { assert_eq!(reading_from_status(30,0), Some(d(30))); assert_eq!(reading_from_status(0,0), None); assert_eq!(reading_from_status(50,4), Some(c(50))); }
}
