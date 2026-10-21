//! Controlador do OSD: uma pílula visível por vez e um único prazo de ocultação.
//! O tempo é injetado (`now`); o chamador mantém um só timer one-shot.

use std::time::Instant;

use crate::message::Pill;

#[derive(Debug, PartialEq)]
pub enum OsdAction {
    Present { pill: Pill, deadline: Instant },
    Hide,
    Nothing,
}

#[derive(Default)]
pub struct OsdController {
    current: Option<(Pill, Instant)>,
}

impl OsdController {
    /// Sempre apresenta; substitui a pílula atual e o prazo anterior.
    pub fn show(&mut self, pill: Pill, now: Instant) -> OsdAction {
        let deadline = now + pill.duration;
        self.current = Some((pill.clone(), deadline));
        OsdAction::Present { pill, deadline }
    }

    /// `Hide` se o prazo atual já chegou; `Nothing` se ainda futuro ou nada visível.
    pub fn expire(&mut self, now: Instant) -> OsdAction {
        match &self.current {
            Some((_, deadline)) if now >= *deadline => {
                self.current = None;
                OsdAction::Hide
            }
            _ => OsdAction::Nothing,
        }
    }

    pub fn visible(&self) -> Option<&Pill> {
        self.current.as_ref().map(|(pill, _)| pill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{lock, LockKey, battery, Durations};
    use crate::battery::BatteryAlert;
    use std::time::Duration;

    const fn ms(n: u64) -> Duration { Duration::from_millis(n) }
    const D: Durations = Durations { normal: ms(1500), critical: ms(4000) };

    #[test] fn show_presents_with_deadline() {
        let t0 = Instant::now(); let mut c = OsdController::default();
        let caps = lock(LockKey::Caps, true, &D);
        assert_eq!(c.show(caps.clone(), t0), OsdAction::Present { pill: caps, deadline: t0 + ms(1500) });
    }
    #[test] fn new_pill_replaces_current() {
        let t0 = Instant::now(); let mut c = OsdController::default();
        let a = lock(LockKey::Caps, true, &D); let b = lock(LockKey::Num, true, &D);
        c.show(a, t0);
        assert_eq!(c.show(b.clone(), t0 + ms(500)), OsdAction::Present { pill: b.clone(), deadline: t0 + ms(500) + b.duration });
        assert_eq!(c.visible(), Some(&b));
    }
    #[test] fn stale_deadline_does_nothing() {
        let t0 = Instant::now(); let mut c = OsdController::default();
        c.show(lock(LockKey::Caps, true, &D), t0);
        c.show(lock(LockKey::Num, true, &D), t0 + ms(500));
        assert_eq!(c.expire(t0 + ms(1500)), OsdAction::Nothing);
        assert_eq!(c.expire(t0 + ms(2000)), OsdAction::Hide);
        assert_eq!(c.visible(), None);
    }
    #[test] fn expire_when_hidden_is_nothing() {
        assert_eq!(OsdController::default().expire(Instant::now()), OsdAction::Nothing);
    }
    #[test] fn critical_battery_lasts_4s() {
        let t0 = Instant::now(); let mut c = OsdController::default();
        c.show(battery(BatteryAlert::Critical(5), "X", &D), t0);
        assert_eq!(c.expire(t0 + ms(3999)), OsdAction::Nothing);
        assert_eq!(c.expire(t0 + ms(4000)), OsdAction::Hide);
    }
}
