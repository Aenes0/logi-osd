//! Lógica de um nó hidraw, sem thread: frames e prazos entram, `NodeEvent`s saem.
//!
//! Contrato com `app.rs`:
//! - `Online` só com o link de pé (0x41 sem o bit 0x40, ou nó Direct que respondeu) e sempre
//!   antes de qualquer `Feature` daquele dispositivo; as leituras logo depois são
//!   `Feature { baseline: true }`.
//! - `Offline` em todo 0x41 de link perdido.
//! - `initial` dos `Online` gerados pelo `start` = `initial_enumeration || kind == Receiver`;
//!   fora da janela do gatilho de chegada, `initial: false`.
//! - O desvio da tecla Emoji é tentado uma vez por conexão (o teclado o esquece a cada
//!   reconexão). Se alguém o desfizer (Solaar, `solaar_desfaz_desvio = sim`), não insiste.
//! - Comandos (tecla Emoji) não rodam aqui.
//! - Luz no ramo A (`backlight_criterio`): o analytics do ramo B não é ligado.

use std::time::{Duration, Instant};

use super::select::NodeKind;
use crate::app::{DeviceKey, DeviceSummary, NodeEvent};
use crate::hidpp::events::{FeatureEvent, decode};
use crate::hidpp::frame::{DEV_RECEIVER, Frame, FrameKind};
use crate::hidpp::receiver::{self, Codename, LinkNotice};
use crate::hidpp::session::{self, BatteryKind, DeviceInfo};
use crate::hidpp::transport::{HidIo, HidppError, PENDING_MAX};

/// Notificações 0x41 que chegam até 2 s depois do gatilho de chegada são respostas a ele.
pub const ARRIVAL_WINDOW: Duration = Duration::from_secs(2);
/// Índice do dispositivo num nó Direct (Bluetooth/cabo).
const DEV_DIRECT: u8 = DEV_RECEIVER;

/// `resultados.txt: unified_push = sim` (o MX Keys Mini avisa a bateria sozinho) e
/// `g602_bateria_push = inconclusivo` (o G602 nunca avisou): só BATTERY_STATUS é lida de tempos em tempos.
fn needs_battery_polling(kind: BatteryKind) -> bool { kind == BatteryKind::Status }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerKind {
    /// Leitura periódica de bateria de dispositivo sem push.
    Battery,
    /// A descoberta estourou o prazo com o link de pé: nova tentativa no mesmo intervalo (sem
    /// retry rápido). `initial` é o do `Online` que a descoberta teria emitido.
    Discover { wpid: u16, initial: bool },
}

#[derive(Debug, Clone, Copy)]
struct Timer { index: u8, kind: TimerKind, at: Instant }

pub struct NodeWorker {
    node: u32,
    kind: NodeKind,
    product: u16,
    initial_enumeration: bool,
    legacy_interval: Duration,
    /// `comando_emoji` configurado: só então a tecla Emoji é desviada (senão fica nativa).
    divert_emoji: bool,
    /// Dispositivos online e descobertos (até 6 num receptor): busca linear.
    devices: Vec<DeviceInfo>,
    timers: Vec<Timer>,
    /// Eventos guardados por `request` durante uma requisição; processados depois, na ordem.
    pending: Vec<Frame>,
    /// Fim da janela de respostas ao gatilho de chegada (só receptor).
    arrival_until: Option<Instant>,
}

impl NodeWorker {
    /// `product` = PID do HID_ID; em nó Direct vira o `wpid` do dispositivo (no BT da Logitech
    /// PID == wpid, ex.: 0xB369), para a `BatteryKey` coincidir com a do mesmo teclado no Bolt.
    /// `initial_enumeration` = nó veio da enumeração da partida do daemon.
    /// `divert_emoji` = há `comando_emoji`; sem ele a tecla Emoji não é desviada.
    pub fn new(node: u32, kind: NodeKind, product: u16, initial_enumeration: bool, legacy_interval: Duration, divert_emoji: bool) -> Self {
        NodeWorker {
            node,
            kind,
            product,
            initial_enumeration,
            legacy_interval,
            divert_emoji,
            devices: Vec::new(),
            timers: Vec::new(),
            pending: Vec::with_capacity(PENDING_MAX),
            arrival_until: None,
        }
    }

    /// Receptor: liga as notificações 0x41 (sem desligar bit nenhum) e pede uma por dispositivo
    /// pareado. Direct: descobre o dispositivo 0xFF.
    pub fn start(&mut self, io: &mut impl HidIo, now: Instant) -> Vec<NodeEvent> {
        let mut out = Vec::new();
        match self.kind {
            NodeKind::Receiver => {
                if let Err(e) = receiver::ensure_wireless_notifications(io, &mut self.pending) {
                    self.log(format_args!("não consegui ligar as notificações do receptor: {e:?}"));
                }
                self.arrival_until = Some(now + ARRIVAL_WINDOW);
                if let Err(e) = receiver::trigger_arrival(io, &mut self.pending) {
                    self.log(format_args!("gatilho de chegada falhou: {e:?}"));
                }
            }
            NodeKind::Direct => {
                let initial = self.start_initial();
                self.connect(io, DEV_DIRECT, self.product, initial, now, &mut out);
            }
        }
        self.drain_pending(io, now, &mut out);
        out
    }

    pub fn on_frame(&mut self, io: &mut impl HidIo, f: Frame, now: Instant) -> Vec<NodeEvent> {
        let mut out = Vec::new();
        self.handle(io, f, now, &mut out);
        self.drain_pending(io, now, &mut out);
        out
    }

    /// Leituras de bateria e novas descobertas vencidas. Erros e timeouts reagendam igual.
    pub fn on_timer(&mut self, io: &mut impl HidIo, now: Instant) -> Vec<NodeEvent> {
        let mut out = Vec::new();
        let next = now + self.legacy_interval;
        let NodeWorker { node, devices, timers, pending, .. } = self;
        for t in timers.iter_mut().filter(|t| t.kind == TimerKind::Battery && t.at <= now) {
            t.at = next;
            let Some(info) = devices.iter().find(|d| d.index == t.index) else { continue };
            match session::read_battery(io, info, pending) {
                Ok(Some(r)) => out.push(feature(*node, t.index, FeatureEvent::Battery(r), false)),
                Ok(None) => {}
                Err(e) => eprintln!("logi-osd: hidraw-{node}: leitura de bateria do dispositivo {} falhou: {e:?}", t.index),
            }
        }
        while let Some(pos) = self.timers.iter().position(|t| matches!(t.kind, TimerKind::Discover { .. }) && t.at <= now) {
            let t = self.timers.remove(pos);
            if let TimerKind::Discover { wpid, initial } = t.kind {
                self.connect(io, t.index, wpid, initial, now, &mut out);
            }
        }
        self.drain_pending(io, now, &mut out);
        out
    }

    /// `None` se não há leitura de bateria nem nova descoberta agendada (repouso sem prazo).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.iter().map(|t| t.at).min()
    }

    fn start_initial(&self) -> bool { self.initial_enumeration || self.kind == NodeKind::Receiver }

    fn drain_pending(&mut self, io: &mut impl HidIo, now: Instant, out: &mut Vec<NodeEvent>) {
        while !self.pending.is_empty() {
            let f = self.pending.remove(0);
            self.handle(io, f, now, out);
        }
    }

    fn handle(&mut self, io: &mut impl HidIo, f: Frame, now: Instant, out: &mut Vec<NodeEvent>) {
        match f.kind() {
            FrameKind::Notification10 => {
                if let Some(link) = receiver::parse_link(&f) {
                    if self.kind == NodeKind::Receiver {
                        self.on_link(io, link, now, out);
                    }
                } else if let Some(index) = receiver::parse_power_on(&f) {
                    out.push(NodeEvent::PowerOn { dev: self.key(index) });
                }
            }
            FrameKind::Event20 { index, event } => {
                let dev = self.device_index(f.device);
                let Some(info) = self.devices.iter().find(|d| d.index == dev) else { return };
                let Some(fid) = info.features.id_at(index) else { return };
                if let Some(ev) = decode(fid, event, f.payload()) {
                    out.push(feature(self.node, dev, ev, false));
                }
            }
            // Respostas atrasadas ou de outro software (kernel, Solaar) no mesmo nó.
            FrameKind::Reply | FrameKind::Error10 { .. } | FrameKind::Error20 { .. } => {}
        }
    }

    fn on_link(&mut self, io: &mut impl HidIo, link: LinkNotice, now: Instant, out: &mut Vec<NodeEvent>) {
        if !link.online {
            self.forget(link.index);
            out.push(NodeEvent::Offline { dev: self.key(link.index) });
            return;
        }
        // 0x41 repetido sem queda no meio (outro software disparou o gatilho): mesma conexão,
        // nada a redescobrir nem a desviar de novo.
        if self.devices.iter().any(|d| d.index == link.index && d.wpid == link.wpid) {
            return;
        }
        self.forget(link.index);
        let initial = self.arrival_until.is_some_and(|t| now < t) && self.start_initial();
        self.connect(io, link.index, link.wpid, initial, now, out);
    }

    fn forget(&mut self, index: u8) {
        self.devices.retain(|d| d.index != index);
        self.timers.retain(|t| t.index != index);
    }

    /// Link de pé: descobre, desvia a Emoji (uma vez), lê o estado inicial e emite `Online` +
    /// `Feature { baseline: true }`.
    fn connect(&mut self, io: &mut impl HidIo, index: u8, wpid: u16, initial: bool, now: Instant, out: &mut Vec<NodeEvent>) {
        let codename = match self.kind {
            NodeKind::Receiver => receiver::codename_for_receiver(self.product),
            NodeKind::Direct => Codename::None,
        };
        let info = match session::discover(io, index, wpid, codename, &mut self.pending) {
            Ok(info) => info,
            Err(HidppError::Timeout) => {
                self.log(format_args!("dispositivo {index} não respondeu à descoberta; nova tentativa em {:?}", self.legacy_interval));
                let at = now + self.legacy_interval;
                self.timers.push(Timer { index, kind: TimerKind::Discover { wpid, initial }, at });
                return;
            }
            Err(e) => {
                self.log(format_args!("descoberta do dispositivo {index} falhou: {e:?}"));
                return;
            }
        };

        if self.divert_emoji {
            match session::divert_emoji(io, &info, &mut self.pending) {
                Ok(false) if info.emoji_divertable => self.log(format_args!("desvio da tecla Emoji não confirmado pelo dispositivo {index}")),
                Ok(_) => {}
                Err(e) => self.log(format_args!("desvio da tecla Emoji do dispositivo {index} falhou: {e:?}")),
            }
        }

        let node = self.node;
        let battery = baseline(node, session::read_battery(io, &info, &mut self.pending), "bateria");
        let locks = baseline(node, session::read_locks(io, &info, &mut self.pending), "locks");
        let fn_inv = baseline(node, session::read_fn_inversion(io, &info, &mut self.pending), "Fn");

        let summary = DeviceSummary { name: info.name.clone(), wpid: info.wpid, hosts: info.hosts, backlight_levels: info.backlight_levels };
        out.push(NodeEvent::Online { dev: self.key(index), summary, initial });
        let baseline = [
            battery.map(FeatureEvent::Battery),
            locks.map(FeatureEvent::Locks),
            fn_inv.map(|media_keys| FeatureEvent::FnInversion { media_keys }),
        ];
        out.extend(baseline.into_iter().flatten().map(|ev| feature(self.node, index, ev, true)));

        if needs_battery_polling(info.battery) {
            self.timers.push(Timer { index, kind: TimerKind::Battery, at: now + self.legacy_interval });
        }
        self.devices.push(info);
    }

    /// Em nó Direct todo frame é do único dispositivo (o BT pode responder com 0x00 ou 0xFF).
    fn device_index(&self, device: u8) -> u8 {
        match self.kind {
            NodeKind::Direct => DEV_DIRECT,
            NodeKind::Receiver => device,
        }
    }

    fn key(&self, index: u8) -> DeviceKey { DeviceKey { node: self.node, index: self.device_index(index) } }

    fn log(&self, msg: std::fmt::Arguments<'_>) {
        eprintln!("logi-osd: hidraw-{}: {msg}", self.node);
    }
}

/// Leitura de estado inicial: a falha só loga (o `Online` sai mesmo assim).
fn baseline<T>(node: u32, r: Result<Option<T>, HidppError>, what: &str) -> Option<T> {
    r.unwrap_or_else(|e| {
        eprintln!("logi-osd: hidraw-{node}: leitura inicial de {what} falhou: {e:?}");
        None
    })
}

fn feature(node: u32, index: u8, event: FeatureEvent, baseline: bool) -> NodeEvent {
    NodeEvent::Feature { dev: DeviceKey { node, index }, event, baseline }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::battery::{BatteryReading, ChargeStatus};
    use crate::hidpp::events::{FeatureEvent, LockState};
    use crate::hidpp::fake::FakeDevice;
    use crate::hidpp::feature::id;
    use crate::hidpp::frame::{LONG_LEN, load_fixture};
    use crate::hidpp::transport::{DEVICE_TIMEOUT, HARD_CAP_FACTOR};

    const MIN15: Duration = Duration::from_secs(15 * 60);
    const NODE: u32 = 7;

    /// Dispositivo simulado (valores de `info.txt` / `resultados.txt`).
    #[derive(Clone)]
    struct Dev {
        index: u8,
        wpid: u16,
        features: Vec<(u16, u8)>,
        /// Chave do 0x83 0xB5 → payload do codinome.
        codename: (u8, Vec<u8>),
        /// (índice da feature, função) → payload.
        replies: Vec<(u8, u8, Vec<u8>)>,
        /// `Some(c)`: enquanto `c` for `true`, o ping não é respondido e chega um Caps do dev 1.
        silent: Option<Rc<Cell<bool>>>,
    }

    fn long(dev: u8, sub: u8, addr: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x11, dev, sub, addr];
        v.extend_from_slice(payload);
        v.resize(LONG_LEN, 0);
        v
    }
    fn link(index: u8, online: bool, wpid: u16) -> Vec<u8> {
        let [hi, lo] = wpid.to_be_bytes();
        vec![0x10, index, 0x41, 0x10, if online { 0x00 } else { 0x40 }, lo, hi]
    }
    fn frame(bytes: &[u8]) -> Frame { Frame::parse(bytes).unwrap() }
    fn caps_on() -> Vec<u8> { long(0x01, 0x0d, 0x00, &[0x02]) }

    /// MX Keys Mini (`indices_mx`).
    fn mx(index: u8) -> Dev {
        Dev {
            index,
            wpid: 0xB369,
            features: vec![
                (0x0001, 0x01), (0x0005, 0x03), (0x1d4b, 0x04), (0x1004, 0x07), (0x1b04, 0x08),
                (0x1814, 0x09), (0x1815, 0x0a), (0x1982, 0x0b), (0x40a3, 0x0c), (0x4220, 0x0d), (0x4521, 0x0e),
            ],
            codename: (0x60 + index, vec![0x60 + index, 0x01, 0x0c, b'M', b'X', b' ', b'K', b'e', b'y', b's', b' ', b'M', b'i', b'n', b'i']),
            replies: vec![
                (0x03, 0, vec![12]),
                (0x03, 1, b"MX Keys Mini".to_vec()),
                (0x0b, 2, vec![0x08, 0x02, 0x02, 0x00, 0x03, 0x00, 0x06, 0x00, 0x3c]),
                (0x09, 0, vec![0x03, 0x00]),
                (0x0a, 0, vec![0x13, 0x08, 0x03, 0x00]),
                (0x0c, 0, vec![0x00, 0x00, 0x01, 0x01]),
                (0x08, 2, vec![0x01, 0x08, 0x00, 0x00, 0x00]),
                (0x08, 3, vec![0x01, 0x08, 0x01, 0x00, 0x00]),
                (0x07, 1, vec![0x5a, 0x08, 0x00, 0x00]),
                (0x0d, 0, vec![0x00]),
            ],
            silent: None,
        }
    }

    /// G602 no c537 (`indices_g602`): codinome Unifying em 0x40 + índice − 1.
    fn g602(index: u8) -> Dev {
        Dev {
            index,
            wpid: 0x402C,
            features: vec![(0x0001, 0x01), (0x0005, 0x03), (0x1000, 0x05), (0x1d4b, 0x06)],
            codename: (0x40 + index - 1, vec![0x40 + index - 1, 0x04, b'G', b'6', b'0', b'2']),
            replies: vec![(0x05, 0, vec![0x32, 0x14, 0x00])],
            silent: None,
        }
    }

    fn answer(d: &Dev, f: &Frame) -> Vec<Vec<u8>> {
        let p = &f.params;
        let function = f.address >> 4;
        if f.sub_id == 0x00 {
            if let Some(s) = &d.silent && s.get() {
                return vec![caps_on()];
            }
            return match function {
                1 => vec![long(f.device, 0, f.address, &[4, 5, p[2]])],
                0 => {
                    let fid = u16::from_be_bytes([p[0], p[1]]);
                    let idx = d.features.iter().find(|e| e.0 == fid).map_or(0, |e| e.1);
                    vec![long(f.device, 0, f.address, &[idx, 0, u8::from(idx != 0)])]
                }
                _ => vec![long(f.device, 0xFF, 0x00, &[f.address, 0x07])],
            };
        }
        match d.replies.iter().find(|r| r.0 == f.sub_id && r.1 == function) {
            Some((_, _, payload)) => vec![long(f.device, f.sub_id, f.address, payload)],
            None => vec![long(f.device, 0xFF, f.sub_id, &[f.address, 0x07])],
        }
    }

    /// Receptor com flags 00 09 00; o gatilho 0x02 devolve um 0x41 online por dispositivo.
    fn receiver(devs: Vec<Dev>) -> FakeDevice {
        FakeDevice::new(move |f| match (f.device, f.sub_id, f.address) {
            (0xFF, 0x81, 0x00) => vec![vec![0x10, 0xFF, 0x81, 0x00, 0x00, 0x09, 0x00]],
            (0xFF, 0x80, 0x02) => {
                let mut v: Vec<_> = devs.iter().map(|d| link(d.index, true, d.wpid)).collect();
                v.push(vec![0x10, 0xFF, 0x80, 0x02, 0x00, 0x00, 0x00]);
                v
            }
            (0xFF, 0x83, 0xB5) => match devs.iter().find(|d| d.codename.0 == f.params[0]) {
                Some(d) => vec![long(0xFF, 0x83, 0xB5, &d.codename.1)],
                None => vec![vec![0x10, 0xFF, 0x8F, 0x83, 0xB5, 0x03, 0x00]],
            },
            (dev, _, _) => devs.iter().find(|d| d.index == dev).map_or_else(Vec::new, |d| answer(d, f)),
        })
    }

    /// Nó Direct: o dispositivo responde em 0xFF.
    fn direct(mut d: Dev) -> FakeDevice {
        d.index = 0xFF;
        FakeDevice::new(move |f| if f.device == 0xFF { answer(&d, f) } else { vec![] })
    }

    fn worker(kind: NodeKind, product: u16, initial_enumeration: bool) -> NodeWorker {
        NodeWorker::new(NODE, kind, product, initial_enumeration, MIN15, true)
    }

    fn onlines(evs: &[NodeEvent]) -> Vec<(DeviceKey, u16, bool)> {
        evs.iter()
            .filter_map(|e| match e {
                NodeEvent::Online { dev, summary, initial } => Some((*dev, summary.wpid, *initial)),
                _ => None,
            })
            .collect()
    }
    fn dev(index: u8) -> DeviceKey { DeviceKey { node: NODE, index } }
    /// Escritas `setCidReporting` (fn3) na REPROG V4 do MX (índice 0x08).
    fn divert_writes(io: &FakeDevice) -> usize { io.written.iter().filter(|w| w[2] == 0x08 && w[3] == 0x3C).count() }

    #[test] fn start_on_receiver_marks_initial_online() {
        let mut io = receiver(vec![mx(1)]);
        let now = io.now();
        let evs = worker(NodeKind::Receiver, 0xC548, true).start(&mut io, now);
        assert_eq!(onlines(&evs), vec![(dev(1), 0xB369, true)]);
        let online_at = evs.iter().position(|e| matches!(e, NodeEvent::Online { .. })).unwrap();
        let locks_at = evs.iter().position(|e| matches!(e,
            NodeEvent::Feature { event: FeatureEvent::Locks(LockState { caps: Some(false), .. }), baseline: true, .. })).unwrap();
        assert!(online_at < locks_at, "Online antes de qualquer Feature");
        let NodeEvent::Online { summary, .. } = &evs[online_at] else { unreachable!() };
        assert_eq!(summary.name, "MX Keys Mini");
        assert_eq!(summary.hosts, Some((3, 0)));
        assert_eq!(summary.backlight_levels, Some(8));
        let battery = BatteryReading { percent: 90, status: ChargeStatus::Discharging };
        assert!(evs.iter().any(|e| matches!(e, NodeEvent::Feature { event: FeatureEvent::Battery(b), baseline: true, .. } if *b == battery)));
        assert!(evs.iter().any(|e| matches!(e, NodeEvent::Feature { event: FeatureEvent::FnInversion { media_keys: false }, baseline: true, .. })));
        // receptor_flags_antes = 00 09 00: só a leitura do registrador 0x00, nenhuma escrita nele.
        assert!(!io.written.iter().any(|w| w[..4] == [0x10, 0xFF, 0x80, 0x00]));
    }

    #[test] fn reconnect_emits_non_initial_online() {
        let mut io = receiver(vec![mx(1)]);
        let t0 = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        w.start(&mut io, t0);
        let later = t0 + Duration::from_secs(60);
        let evs = w.on_frame(&mut io, frame(&link(1, false, 0xB369)), later);
        assert!(matches!(evs.as_slice(), [NodeEvent::Offline { dev: d }] if *d == dev(1)));
        let evs = w.on_frame(&mut io, frame(&link(1, true, 0xB369)), later);
        assert_eq!(onlines(&evs), vec![(dev(1), 0xB369, false)]);
        assert!(evs.iter().skip(1).all(|e| matches!(e, NodeEvent::Feature { baseline: true, .. })));
    }

    #[test] fn offline_is_emitted_even_for_unknown_device() {
        let mut io = receiver(vec![]);
        let now = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        let evs = w.on_frame(&mut io, frame(&link(3, false, 0xB369)), now);
        assert!(matches!(evs.as_slice(), [NodeEvent::Offline { dev: d }] if *d == dev(3)));
    }

    #[test] fn arrival_inside_window_is_initial_outside_is_not() {
        // 0x41 que chega depois do `start` mas dentro dos 2 s ainda é resposta ao gatilho.
        let t0 = receiver(vec![]).now();
        let mut io = receiver(vec![mx(1)]);
        let mut w = worker(NodeKind::Receiver, 0xC548, false);
        assert!(w.start(&mut receiver(vec![]), t0).is_empty());
        let evs = w.on_frame(&mut io, frame(&link(1, true, 0xB369)), t0 + Duration::from_millis(1500));
        assert_eq!(onlines(&evs), vec![(dev(1), 0xB369, true)]);

        let mut io = receiver(vec![mx(1)]);
        let mut w = worker(NodeKind::Receiver, 0xC548, false);
        w.start(&mut receiver(vec![]), t0);
        let evs = w.on_frame(&mut io, frame(&link(1, true, 0xB369)), t0 + ARRIVAL_WINDOW);
        assert_eq!(onlines(&evs), vec![(dev(1), 0xB369, false)]);
    }

    #[test] fn repeated_online_without_offline_is_the_same_connection() {
        let mut io = receiver(vec![mx(1)]);
        let t0 = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        w.start(&mut io, t0);
        let writes = io.written.len();
        let evs = w.on_frame(&mut io, frame(&link(1, true, 0xB369)), t0 + Duration::from_secs(60));
        assert!(evs.is_empty());
        assert_eq!(io.written.len(), writes, "nem nova descoberta, nem novo desvio");
    }

    #[test] fn receiver_hotplug_is_silent() {
        let mut io = receiver(vec![mx(1)]);
        let now = io.now();
        let evs = worker(NodeKind::Receiver, 0xC548, false).start(&mut io, now);
        assert_eq!(onlines(&evs), vec![(dev(1), 0xB369, true)]);
    }

    #[test] fn direct_node_hotplug_shows_connected() {
        let mut io = direct(mx(1));
        let now = io.now();
        let evs = worker(NodeKind::Direct, 0xB369, false).start(&mut io, now);
        assert_eq!(onlines(&evs), vec![(dev(0xFF), 0xB369, false)]);
        let mut io = direct(mx(1));
        let evs = worker(NodeKind::Direct, 0xB369, true).start(&mut io, now);
        assert_eq!(onlines(&evs), vec![(dev(0xFF), 0xB369, true)]);
        // Nó direto: nada de registradores de receptor.
        assert!(io.written.iter().all(|w| w[2] & 0x80 == 0));
    }

    #[test] fn direct_node_uses_product_as_wpid() {
        let mut io = direct(mx(1));
        let now = io.now();
        let evs = worker(NodeKind::Direct, 0xB369, true).start(&mut io, now);
        let Some(NodeEvent::Online { summary, .. }) = evs.first() else { panic!("sem Online: {evs:?}") };
        assert_eq!(summary.wpid, 0xB369);
        assert_eq!(summary.name, "MX Keys Mini", "sem receptor: DEVICE_NAME");
    }

    #[test] fn direct_node_events_use_device_ff() {
        let mut io = direct(mx(1));
        let now = io.now();
        let mut w = worker(NodeKind::Direct, 0xB369, true);
        w.start(&mut io, now);
        // BT pode mandar o evento com dev 0x00 ou 0xFF.
        for d in [0x00, 0xFF] {
            let evs = w.on_frame(&mut io, frame(&long(d, 0x0d, 0x00, &[0x02])), now);
            assert!(matches!(evs.as_slice(), [NodeEvent::Feature { dev: k, event: FeatureEvent::Locks(_), baseline: false }] if *k == dev(0xFF)));
        }
    }

    #[test] fn power_on_frame_emits_power_on() {
        let mut io = receiver(vec![]);
        let now = io.now();
        let evs = worker(NodeKind::Receiver, 0xC548, true).on_frame(&mut io, frame(&[0x10, 0x01, 0x4B, 0x01, 0, 0, 0]), now);
        assert!(matches!(evs.as_slice(), [NodeEvent::PowerOn { dev: d }] if *d == dev(1)));
    }

    #[test] fn emoji_divert_is_attempted_once_per_connection() {
        let mut io = receiver(vec![mx(1)]);
        let t0 = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        w.start(&mut io, t0);
        assert_eq!(divert_writes(&io), 1);
        let later = t0 + Duration::from_secs(60);
        let battery = long(0x01, 0x07, 0x00, &[0x5a, 0x08, 0x00, 0x00]);
        for _ in 0..2 {
            let evs = w.on_frame(&mut io, frame(&battery), later);
            assert!(matches!(evs.as_slice(), [NodeEvent::Feature { event: FeatureEvent::Battery(_), baseline: false, .. }]));
        }
        assert_eq!(divert_writes(&io), 1, "evento não gera novo desvio");
        w.on_frame(&mut io, frame(&link(1, false, 0xB369)), later);
        w.on_frame(&mut io, frame(&link(1, true, 0xB369)), later);
        assert_eq!(divert_writes(&io), 2, "reconexão: exatamente um desvio");
    }

    #[test] fn emoji_is_not_diverted_without_command() {
        // Sem `comando_emoji` a tecla mantém o comportamento nativo: nenhum setCidReporting.
        let mut io = receiver(vec![mx(1)]);
        let t0 = io.now();
        let mut w = NodeWorker::new(NODE, NodeKind::Receiver, 0xC548, true, MIN15, false);
        let evs = w.start(&mut io, t0);
        assert_eq!(onlines(&evs).len(), 1, "o dispositivo ainda é descoberto");
        assert_eq!(divert_writes(&io), 0);
    }

    #[test] fn event_frame_is_decoded_with_feature_map() {
        let mut io = receiver(vec![mx(1)]);
        let t0 = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        w.start(&mut io, t0);
        let fixture = load_fixture(include_str!("../../tests/fixtures/probe/caps.hex"));
        let (_, bytes) = &fixture[0];
        let evs = w.on_frame(&mut io, frame(bytes), t0 + Duration::from_secs(60));
        assert!(matches!(evs.as_slice(), [NodeEvent::Feature { dev: d, event: FeatureEvent::Locks(LockState { caps: Some(true), .. }), baseline: false }] if *d == dev(1)));
    }

    #[test] fn event_of_unknown_device_or_feature_is_dropped() {
        let mut io = receiver(vec![mx(1)]);
        let t0 = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        w.start(&mut io, t0);
        assert!(w.on_frame(&mut io, frame(&long(0x02, 0x0d, 0x00, &[0x02])), t0).is_empty());
        assert!(w.on_frame(&mut io, frame(&long(0x01, 0x2F, 0x00, &[0x02])), t0).is_empty());
        // Resposta alheia (SW ID do kernel) não vira evento.
        assert!(w.on_frame(&mut io, frame(&long(0x01, 0x0d, 0x01, &[0x02])), t0).is_empty());
    }

    #[test] fn legacy_device_has_15_min_deadline() {
        let mut io = receiver(vec![g602(1)]);
        let t0 = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC537, true);
        let evs = w.start(&mut io, t0);
        assert_eq!(onlines(&evs), vec![(dev(1), 0x402C, true)]);
        let NodeEvent::Online { summary, .. } = &evs[0] else { panic!("{evs:?}") };
        assert_eq!(summary.name, "G602");
        assert_eq!(w.next_deadline(), Some(t0 + MIN15));

        let writes = io.written.len();
        assert!(w.on_timer(&mut io, t0 + MIN15 - Duration::from_millis(1)).is_empty());
        assert_eq!(io.written.len(), writes, "antes do prazo nada é lido");

        let evs = w.on_timer(&mut io, t0 + MIN15);
        assert_eq!(io.written[writes..], [vec![0x10, 0x01, 0x05, 0x0C, 0x00, 0x00, 0x00]], "uma leitura BATTERY_STATUS fn0");
        let reading = BatteryReading { percent: 50, status: ChargeStatus::Discharging };
        assert!(matches!(evs.as_slice(), [NodeEvent::Feature { dev: d, event: FeatureEvent::Battery(r), baseline: false }] if *d == dev(1) && *r == reading));
        assert_eq!(w.next_deadline(), Some(t0 + 2 * MIN15));
    }

    #[test] fn legacy_read_error_reschedules_without_fast_retry() {
        let mut g = g602(1);
        g.replies.clear(); // BATTERY_STATUS passa a responder erro 2.0
        let mut io = receiver(vec![g]);
        let t0 = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC537, true);
        w.start(&mut io, t0);
        assert!(w.on_timer(&mut io, t0 + MIN15).is_empty());
        assert_eq!(w.next_deadline(), Some(t0 + 2 * MIN15));
    }

    #[test] fn offline_cancels_legacy_deadline() {
        let mut io = receiver(vec![g602(1)]);
        let t0 = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC537, true);
        w.start(&mut io, t0);
        w.on_frame(&mut io, frame(&link(1, false, 0x402C)), t0);
        assert_eq!(w.next_deadline(), None);
    }

    #[test] fn no_deadline_without_legacy_device() {
        let mut io = receiver(vec![mx(1)]);
        let now = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        w.start(&mut io, now);
        assert_eq!(w.next_deadline(), None);
    }

    #[test] fn discovery_timeout_does_not_block_other_frames() {
        // MX no 1; G602 no 2 não responde, e durante a espera chega um Caps do MX.
        let silent = Rc::new(Cell::new(true));
        let mut g = g602(2);
        g.silent = Some(silent.clone());
        let mut io = receiver(vec![mx(1), g]);
        let t0 = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        let evs = w.start(&mut io, t0);
        let waited = io.now() - t0;
        assert!(waited >= DEVICE_TIMEOUT, "esperou {waited:?}");
        assert!(waited <= DEVICE_TIMEOUT * HARD_CAP_FACTOR, "uma requisição estourou; esperou {waited:?}");
        assert_eq!(onlines(&evs), vec![(dev(1), 0xB369, true)], "sem Online para quem não respondeu");
        let caps = evs.iter().position(|e| matches!(e,
            NodeEvent::Feature { dev: d, event: FeatureEvent::Locks(LockState { caps: Some(true), .. }), baseline: false } if *d == dev(1)));
        assert!(caps.is_some(), "Caps enfileirado foi entregue: {evs:?}");

        // Nova tentativa só no prazo da bateria antiga, com o `initial` original.
        assert_eq!(w.next_deadline(), Some(t0 + MIN15));
        silent.set(false);
        let evs = w.on_timer(&mut io, t0 + MIN15);
        assert_eq!(onlines(&evs), vec![(dev(2), 0x402C, true)]);
        assert_eq!(w.next_deadline(), Some(t0 + 2 * MIN15), "agora agenda a bateria do G602");
    }

    #[test] fn hidpp10_device_is_not_retried() {
        let mut io = FakeDevice::new(|f| match (f.device, f.sub_id) {
            (0xFF, 0x81) => vec![vec![0x10, 0xFF, 0x81, 0x00, 0x00, 0x09, 0x00]],
            (0xFF, 0x80) => vec![link(1, true, 0x1234), vec![0x10, 0xFF, 0x80, 0x02, 0, 0, 0]],
            (0x01, 0x00) => vec![vec![0x10, 0x01, 0x8F, 0x00, f.address, 0x01, 0x00]],
            _ => vec![],
        });
        let now = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        assert!(onlines(&w.start(&mut io, now)).is_empty());
        assert_eq!(w.next_deadline(), None);
    }

    #[test] fn worker_fits_in_node_thread_stack() {
        // Descoberta completa (MX + G602), eventos e leitura periódica numa pilha de NODE_STACK
        // (build de teste, sem otimização: pior caso de uso de pilha).
        let t = std::thread::Builder::new().stack_size(crate::devices::node::NODE_STACK).spawn(|| {
            let mut io = receiver(vec![mx(1), g602(2)]);
            let t0 = io.now();
            let mut w = worker(NodeKind::Receiver, 0xC548, true);
            let mut n = w.start(&mut io, t0).len();
            n += w.on_frame(&mut io, frame(&caps_on()), t0).len();
            n += w.on_timer(&mut io, t0 + MIN15).len();
            n
        });
        assert!(t.unwrap().join().unwrap() >= 3);
    }

    #[test] fn feature_map_kept_per_device() {
        let mut io = receiver(vec![mx(1)]);
        let now = io.now();
        let mut w = worker(NodeKind::Receiver, 0xC548, true);
        w.start(&mut io, now);
        assert_eq!(w.devices[0].features.id_at(0x0d), Some(id::LOCK_KEY_STATE));
    }
}
