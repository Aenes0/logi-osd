//! Agregador de regras: transforma `NodeEvent` em `Action`. Puro: sem I/O, `Instant` injetado.
//!
//! Ramos fixados no portão da T1 (`resultados.txt`, aprovados pelo usuário), sem código dos
//! ramos descartados:
//! - **Luz, ramo A:** todo evento BACKLIGHT2 vem de tecla manual → `Show` se `manual == Some(true)`.
//! - **Conectado, ramo C1:** `Online { initial: false }` mostra se o `DeviceKey` estava `Offline`
//!   ou nunca foi visto (nó Direct novo). `easyswitch_saida` = evento 0x41 (o receptor avisa a
//!   saída), então `Online` sobre `Online` é silencioso. `PowerOn` sempre mostra.
//!
//! Orçamento: estado em `Vec` pequenos (poucos dispositivos, busca linear); nenhuma alocação por
//! evento exceto a da `Pill` quando um OSD é de fato mostrado (e a cópia do nome uma única vez,
//! quando um `BatteryKey` novo aparece).

use std::time::{Duration, Instant};

use crate::battery::{BatteryTracker, Thresholds};
use crate::hidpp::events::{FeatureEvent, LockState};
use crate::message::{self, Durations, LockKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceKey { pub node: u32, pub index: u8 }

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BatteryKey { pub wpid: u16, pub name: String }

#[derive(Debug, Clone)]
pub struct DeviceSummary { pub name: String, pub wpid: u16, pub hosts: Option<(u8, u8)>, pub backlight_levels: Option<u8> }

#[derive(Debug, Clone)]
pub enum NodeEvent {
    Online { dev: DeviceKey, summary: DeviceSummary, initial: bool },
    Offline { dev: DeviceKey },
    /// Notificação 0x4B/0x01.
    PowerOn { dev: DeviceKey },
    /// `baseline` = leitura ativa logo após conectar.
    Feature { dev: DeviceKey, event: FeatureEvent, baseline: bool },
    NodeGone { node: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action { Show(crate::message::Pill), RunEmojiCommand }

/// Pós-`Online`/`PowerOn`: o host reescreve os LEDs ao reconectar; locks/Fn só atualizam estado.
pub const RECONNECT_GRACE: Duration = Duration::from_millis(1000);

/// CID da tecla Emoji (REPROG V4). A de Ditado (0x0103) não gera ação.
const CID_EMOJI: u16 = 0x0108;

/// Estado de link/teclas de um `DeviceKey` (apagado em `NodeGone`).
struct Device {
    key: DeviceKey,
    summary: Option<DeviceSummary>,
    online: bool,
    /// Último `Online`/`PowerOn`; abre a janela de `RECONNECT_GRACE`.
    last_online: Option<Instant>,
    locks: LockState,
    fn_media_keys: Option<bool>,
    emoji_down: bool,
}

impl Device {
    fn new(key: DeviceKey) -> Self {
        Self { key, summary: None, online: false, last_online: None, locks: LockState::default(), fn_media_keys: None, emoji_down: false }
    }

    fn in_grace(&self, now: Instant) -> bool {
        self.last_online.is_some_and(|t| now.saturating_duration_since(t) <= RECONNECT_GRACE)
    }

    /// Identidade da bateria sem alocar; `None` sem `Online` anterior.
    fn battery_id(&self) -> Option<(u16, &str)> {
        self.summary.as_ref().map(|s| (s.wpid, s.name.as_str()))
    }

    fn connected_pill(&self, d: &Durations) -> message::Pill {
        let (name, hosts) = self.summary.as_ref().map_or(("", None), |s| (s.name.as_str(), s.hosts));
        message::connected(name, hosts, d)
    }
}

pub struct App {
    durations: Durations,
    thresholds: Thresholds,
    /// `emoji_command` configurado: só então a tecla Emoji gera `RunEmojiCommand`.
    emoji_enabled: bool,
    devices: Vec<Device>,
    /// Sobrevive a Easy-Switch, troca de nó, BT e `NodeGone`.
    batteries: Vec<(BatteryKey, BatteryTracker)>,
}

impl App {
    pub fn new(cfg: &crate::config::Config) -> Self {
        Self {
            durations: Durations { normal: cfg.duration, critical: cfg.duration_critical },
            thresholds: Thresholds { low: cfg.battery_low, critical: cfg.battery_critical },
            emoji_enabled: cfg.emoji_command.is_some(),
            devices: Vec::new(),
            batteries: Vec::new(),
        }
    }

    pub fn handle(&mut self, ev: NodeEvent, now: Instant) -> Option<Action> {
        match ev {
            NodeEvent::Online { dev, summary, initial } => self.on_online(dev, summary, initial, now),
            NodeEvent::Offline { dev } => {
                let d = self.device(dev);
                d.online = false;
                d.emoji_down = false; // a soltura da tecla se perde com o link
                None
            }
            NodeEvent::PowerOn { dev } => {
                let durations = self.durations;
                let d = self.device(dev);
                // Logo após um `Online` o "conectado" já saiu (ou era silencioso): só renova a carência.
                let duplicate = d.online && d.in_grace(now);
                d.online = true;
                d.last_online = Some(now);
                (!duplicate).then(|| Action::Show(d.connected_pill(&durations)))
            }
            NodeEvent::Feature { dev, event, baseline } => self.on_feature(dev, event, baseline, now),
            NodeEvent::NodeGone { node } => {
                self.devices.retain(|d| d.key.node != node);
                None
            }
        }
    }

    fn device(&mut self, key: DeviceKey) -> &mut Device {
        let pos = match self.devices.iter().position(|d| d.key == key) {
            Some(pos) => pos,
            None => {
                self.devices.push(Device::new(key));
                self.devices.len() - 1
            }
        };
        &mut self.devices[pos]
    }

    fn on_online(&mut self, dev: DeviceKey, summary: DeviceSummary, initial: bool, now: Instant) -> Option<Action> {
        let durations = self.durations;
        let seen = self.devices.iter().any(|d| d.key == dev);
        let d = self.device(dev);
        let was_offline = !seen || !d.online;
        d.summary = Some(summary);
        d.online = true;
        d.last_online = Some(now);
        (!initial && was_offline).then(|| Action::Show(d.connected_pill(&durations)))
    }

    fn on_feature(&mut self, dev: DeviceKey, event: FeatureEvent, baseline: bool, now: Instant) -> Option<Action> {
        let durations = self.durations;
        match event {
            FeatureEvent::Locks(new) => {
                let d = self.device(dev);
                let quiet = baseline || d.in_grace(now);
                let key = changed_lock(&d.locks, &new);
                merge_locks(&mut d.locks, &new);
                let (key, on) = key.filter(|_| !quiet)?;
                Some(Action::Show(message::lock(key, on, &durations)))
            }
            FeatureEvent::FnInversion { media_keys } => {
                let d = self.device(dev);
                let quiet = baseline || d.in_grace(now);
                let prev = d.fn_media_keys.replace(media_keys);
                (!quiet && prev != Some(media_keys)).then(|| Action::Show(message::fn_lock(media_keys, &durations)))
            }
            FeatureEvent::Backlight { level, manual } => {
                if baseline || manual != Some(true) { return None; }
                // Sem nº de níveis não há como desenhar as bolinhas.
                let levels = self.device(dev).summary.as_ref()?.backlight_levels?;
                Some(Action::Show(message::backlight(level, levels, &durations)))
            }
            FeatureEvent::Battery(reading) => {
                let thresholds = self.thresholds;
                // Sem `Online` anterior não há identidade estável: descarta (não mistura dispositivos).
                let d = self.devices.iter().find(|d| d.key == dev)?;
                let (wpid, name) = d.battery_id()?;
                let idx = match self.batteries.iter().position(|(k, _)| k.wpid == wpid && k.name == name) {
                    Some(i) => i,
                    None => {
                        let key = BatteryKey { wpid, name: name.to_owned() };
                        self.batteries.push((key, BatteryTracker::new(thresholds)));
                        self.batteries.len() - 1
                    }
                };
                let (key, tracker) = &mut self.batteries[idx];
                let alert = tracker.update(reading)?;
                Some(Action::Show(message::battery(alert, &key.name, &durations)))
            }
            FeatureEvent::DivertedKeys(cids) => {
                let down = cids.contains(&CID_EMOJI);
                let d = self.device(dev);
                let rising = down && !d.emoji_down;
                d.emoji_down = down; // borda rastreada sempre, com ou sem comando configurado
                (rising && self.emoji_enabled).then_some(Action::RunEmojiCommand)
            }
        }
    }
}

/// Tecla de maior prioridade que mudou (Caps > Num > Insert). Caps conta como mudança também a
/// partir de estado desconhecido (a notificação só vem de tecla). Num/Insert exigem estado
/// anterior conhecido: o MX Keys Mini não tem Num Lock e sempre manda `num = Some(false)`, que não
/// pode virar OSD; em teclados que têm, a primeira leitura (baseline) torna o estado conhecido.
fn changed_lock(prev: &LockState, new: &LockState) -> Option<(LockKey, bool)> {
    if let Some(on) = new.caps.filter(|&on| prev.caps != Some(on)) {
        return Some((LockKey::Caps, on));
    }
    let known_change = |p: Option<bool>, n: Option<bool>| n.filter(|&on| p.is_some_and(|p| p != on));
    if let Some(on) = known_change(prev.num, new.num) {
        return Some((LockKey::Num, on));
    }
    known_change(prev.insert, new.insert).map(|on| (LockKey::Insert, on))
}

/// Campos `None` não mudam o estado.
fn merge_locks(state: &mut LockState, new: &LockState) {
    state.caps = new.caps.or(state.caps);
    state.num = new.num.or(state.num);
    state.insert = new.insert.or(state.insert);
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::battery::{BatteryReading, ChargeStatus};
    use crate::config::Config;
    use crate::hidpp::events::{FeatureEvent, LockState};
    use crate::message::{self, Durations, LockKey, Value};

    const KB: DeviceKey = DeviceKey { node: 1, index: 1 };

    fn durations() -> Durations {
        let c = Config::default();
        Durations { normal: c.duration, critical: c.duration_critical }
    }

    fn summary() -> DeviceSummary {
        DeviceSummary { name: "MX Keys Mini".into(), wpid: 0xb369, hosts: Some((3, 0)), backlight_levels: Some(8) }
    }

    fn online(dev: DeviceKey, initial: bool) -> NodeEvent {
        NodeEvent::Online { dev, summary: summary(), initial }
    }

    fn feat(dev: DeviceKey, event: FeatureEvent, baseline: bool) -> NodeEvent {
        NodeEvent::Feature { dev, event, baseline }
    }

    fn caps(on: bool) -> FeatureEvent {
        FeatureEvent::Locks(LockState { caps: Some(on), num: Some(false), insert: None })
    }

    fn battery(percent: u8) -> FeatureEvent {
        FeatureEvent::Battery(BatteryReading { percent, status: ChargeStatus::Discharging })
    }

    fn new_app() -> App {
        App::new(&Config::default())
    }

    fn after_grace(t: Instant) -> Instant {
        t + RECONNECT_GRACE + Duration::from_millis(500)
    }

    fn connected_pill() -> Action {
        Action::Show(message::connected("MX Keys Mini", Some((3, 0)), &durations()))
    }

    #[test]
    fn lock_change_shows_caps_on() {
        let mut app = new_app();
        let t = Instant::now();
        assert_eq!(app.handle(online(KB, true), t), None);
        assert_eq!(app.handle(feat(KB, caps(false), true), t), None);
        let got = app.handle(feat(KB, caps(true), false), after_grace(t));
        assert_eq!(got, Some(Action::Show(message::lock(LockKey::Caps, true, &durations()))));
    }

    #[test]
    fn lock_state_after_reconnect_is_baseline_not_osd() {
        for (delay, expect_show) in [(100, false), (1500, true)] {
            let mut app = new_app();
            let t = Instant::now();
            app.handle(online(KB, true), t);
            app.handle(NodeEvent::Offline { dev: KB }, t);
            let t1 = t + Duration::from_secs(10);
            app.handle(online(KB, false), t1);
            assert_eq!(app.handle(feat(KB, caps(false), true), t1), None);
            let got = app.handle(feat(KB, caps(true), false), t1 + Duration::from_millis(delay));
            let want = expect_show.then(|| Action::Show(message::lock(LockKey::Caps, true, &durations())));
            assert_eq!(got, want, "delay {delay} ms");
        }
    }

    #[test]
    fn grace_change_is_recorded_so_later_repeat_is_silent() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        app.handle(feat(KB, caps(false), true), t);
        // O host reescreve os LEDs dentro da carência: só grava.
        assert_eq!(app.handle(feat(KB, caps(true), false), t + Duration::from_millis(200)), None);
        // Mesmo valor depois da carência: não é mudança.
        assert_eq!(app.handle(feat(KB, caps(true), false), after_grace(t)), None);
    }

    #[test]
    fn power_on_shows_connected() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        assert_eq!(app.handle(NodeEvent::PowerOn { dev: KB }, after_grace(t)), Some(connected_pill()));
    }

    #[test]
    fn direct_node_new_after_start_shows_connected() {
        let mut app = new_app();
        let bt = DeviceKey { node: 7, index: 0xff };
        assert_eq!(app.handle(online(bt, false), Instant::now()), Some(connected_pill()));
    }

    #[test]
    fn online_over_online_is_silent() {
        // C1 com easyswitch_saida = 0x41 (o receptor avisa a saída): Online sobre Online não mostra.
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        assert_eq!(app.handle(online(KB, false), after_grace(t)), None);
    }

    #[test]
    fn duplicate_lock_event_is_ignored() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        app.handle(feat(KB, caps(false), true), t);
        let t1 = after_grace(t);
        assert!(app.handle(feat(KB, caps(true), false), t1).is_some());
        assert_eq!(app.handle(feat(KB, caps(true), false), t1 + Duration::from_millis(50)), None);
    }

    #[test]
    fn num_lock_is_never_shown_from_unknown_state() {
        // O MX Keys Mini não tem Num Lock: sem estado anterior conhecido, `num` nunca vira OSD.
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        let ev = FeatureEvent::Locks(LockState { caps: None, num: Some(false), insert: None });
        assert_eq!(app.handle(feat(KB, ev, false), after_grace(t)), None);
    }

    #[test]
    fn num_lock_real_change_after_grace_shows() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        app.handle(feat(KB, caps(false), true), t);
        let ev = FeatureEvent::Locks(LockState { caps: Some(false), num: Some(true), insert: None });
        let got = app.handle(feat(KB, ev, false), after_grace(t));
        assert_eq!(got, Some(Action::Show(message::lock(LockKey::Num, true, &durations()))));
    }

    #[test]
    fn caps_has_priority_over_num_when_both_change() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        app.handle(feat(KB, caps(false), true), t);
        let ev = FeatureEvent::Locks(LockState { caps: Some(true), num: Some(true), insert: None });
        let got = app.handle(feat(KB, ev, false), after_grace(t));
        assert_eq!(got, Some(Action::Show(message::lock(LockKey::Caps, true, &durations()))));
    }

    #[test]
    fn fn_inversion_change_shows_after_baseline() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        assert_eq!(app.handle(feat(KB, FeatureEvent::FnInversion { media_keys: true }, true), t), None);
        let t1 = after_grace(t);
        let got = app.handle(feat(KB, FeatureEvent::FnInversion { media_keys: false }, false), t1);
        assert_eq!(got, Some(Action::Show(message::fn_lock(false, &durations()))));
        assert_eq!(app.handle(feat(KB, FeatureEvent::FnInversion { media_keys: false }, false), t1), None);
    }

    #[test]
    fn backlight_auto_change_is_silent() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        let ev = FeatureEvent::Backlight { level: 3, manual: Some(false) };
        assert_eq!(app.handle(feat(KB, ev, false), after_grace(t)), None);
    }

    #[test]
    fn backlight_after_key_press_shows_dots() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        let ev = FeatureEvent::Backlight { level: 3, manual: Some(true) };
        let Some(Action::Show(pill)) = app.handle(feat(KB, ev, false), after_grace(t)) else {
            panic!("esperava Show");
        };
        assert_eq!(pill.value, Value::Dots { on: 3, total: 7 });
    }

    #[test]
    fn battery_alert_keyed_by_device_not_node() {
        let mut app = new_app();
        let t = Instant::now();
        let node1 = DeviceKey { node: 1, index: 1 };
        let node2 = DeviceKey { node: 2, index: 0xff };
        app.handle(online(node1, true), t);
        let got = app.handle(feat(node1, battery(10), false), t);
        let want = message::battery(crate::battery::BatteryAlert::Low(10), "MX Keys Mini", &durations());
        assert_eq!(got, Some(Action::Show(want)));
        app.handle(NodeEvent::Offline { dev: node1 }, t);
        app.handle(online(node2, false), t);
        assert_eq!(app.handle(feat(node2, battery(10), false), t), None);
    }

    #[test]
    fn battery_survives_node_gone() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        assert!(app.handle(feat(KB, battery(10), false), t).is_some());
        app.handle(NodeEvent::NodeGone { node: KB.node }, t);
        app.handle(online(KB, true), t);
        assert_eq!(app.handle(feat(KB, battery(9), false), t), None);
    }

    #[test]
    fn node_gone_clears_lock_and_link_state() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        app.handle(feat(KB, caps(true), true), t);
        app.handle(NodeEvent::NodeGone { node: KB.node }, t);
        // Estado apagado: o mesmo DeviceKey volta como nunca visto.
        assert_eq!(app.handle(online(KB, false), after_grace(t)), Some(connected_pill()));
    }

    #[test]
    fn reconnect_after_offline_shows_connected_with_channel() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        assert_eq!(app.handle(NodeEvent::Offline { dev: KB }, t), None);
        let Some(Action::Show(pill)) = app.handle(online(KB, false), after_grace(t)) else {
            panic!("esperava Show");
        };
        assert_eq!(pill.label, "MX Keys Mini conectado");
        assert_eq!(pill.value, Value::Text("Canal 1".into(), message::Tone::Dim));
    }

    #[test]
    fn initial_enumeration_is_silent() {
        let mut app = new_app();
        assert_eq!(app.handle(online(KB, true), Instant::now()), None);
    }

    #[test]
    fn offline_never_shows() {
        let mut app = new_app();
        let t = Instant::now();
        assert_eq!(app.handle(NodeEvent::Offline { dev: KB }, t), None);
        app.handle(online(KB, true), t);
        assert_eq!(app.handle(NodeEvent::Offline { dev: KB }, after_grace(t)), None);
    }

    #[test]
    fn emoji_rising_edge_runs_command_once() {
        let mut app = App::new(&Config { emoji_command: Some("emoji-picker".into()), ..Config::default() });
        let t = Instant::now();
        app.handle(online(KB, true), t);
        let down = FeatureEvent::DivertedKeys([0x0108, 0, 0, 0]);
        let up = FeatureEvent::DivertedKeys([0; 4]);
        assert_eq!(app.handle(feat(KB, down, false), t), Some(Action::RunEmojiCommand));
        assert_eq!(app.handle(feat(KB, down, false), t), None);
        assert_eq!(app.handle(feat(KB, up, false), t), None);
        assert_eq!(app.handle(feat(KB, down, false), t), Some(Action::RunEmojiCommand));
    }

    #[test]
    fn dictation_does_nothing() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        assert_eq!(app.handle(feat(KB, FeatureEvent::DivertedKeys([0x0103, 0, 0, 0]), false), t), None);
    }

    #[test]
    fn emoji_without_command_is_silent() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        let down = FeatureEvent::DivertedKeys([0x0108, 0, 0, 0]);
        assert_eq!(app.handle(feat(KB, down, false), t), None);
    }

    #[test]
    fn battery_before_online_is_dropped() {
        let mut app = new_app();
        let t = Instant::now();
        assert_eq!(app.handle(feat(KB, battery(10), false), t), None);
        // Nada foi rastreado sob chave compartilhada: o aviso real ainda aparece depois do Online.
        app.handle(online(KB, true), t);
        assert!(app.handle(feat(KB, battery(10), false), t).is_some());
    }

    #[test]
    fn power_on_right_after_online_is_silent() {
        let mut app = new_app();
        let t = Instant::now();
        app.handle(online(KB, true), t);
        app.handle(NodeEvent::Offline { dev: KB }, t);
        let t1 = t + Duration::from_secs(10);
        assert_eq!(app.handle(online(KB, false), t1), Some(connected_pill()));
        let t2 = t1 + Duration::from_millis(300);
        assert_eq!(app.handle(NodeEvent::PowerOn { dev: KB }, t2), None);
        // O PowerOn renovou a carência: lock em t2 + 900 ms ainda só grava.
        app.handle(feat(KB, caps(false), true), t2);
        assert_eq!(app.handle(feat(KB, caps(true), false), t2 + Duration::from_millis(900)), None);
    }
}
