//! Uma thread por nó hidraw (bloqueada em `poll`, sem wakeup periódico) e o hotplug via udev.
//!
//! Repouso: a thread só acorda com um frame do dispositivo ou no prazo da bateria antiga
//! (`NodeWorker::next_deadline`); sem prazo, o `poll` não tem timeout. A remoção do nó é vista
//! pela própria thread (`ENODEV`/EOF na leitura), não pelo udev.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use calloop::channel::Sender;

use super::select::{NodeKind, VENDOR_LOGITECH, parse_hid_id, select};
use super::worker::NodeWorker;
use crate::app::NodeEvent;
use crate::config::Config;
use crate::hidpp::frame::Frame;
use crate::hidpp::transport::HidIo;

/// Pilha da thread de nó (regra de orçamento: 64 KiB).
pub const NODE_STACK: usize = 64 * 1024;
/// Tamanho máximo de um report descriptor (`HID_MAX_DESCRIPTOR_SIZE` do kernel).
const DESCRIPTOR_MAX: usize = 4096;
/// Buffer de um relatório (o `HidIo` fixa 32; HID++ longo tem 20).
const READ_BUF: usize = 32;

/// `HidIo` sobre um descritor de arquivo: `poll` + `read`/`write` (um relatório por chamada).
pub struct FdIo<F> { fd: F }

impl<F: AsFd> FdIo<F> {
    pub fn new(fd: F) -> Self { FdIo { fd } }

    /// `Ok(true)` = há algo para ler (ou erro/HUP, que a leitura vai revelar). `None` = sem prazo.
    fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
        let ts = timeout
            .map(|d| Timespec::try_from(d).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput)))
            .transpose()?;
        let mut fds = [PollFd::new(&self.fd, PollFlags::IN)];
        Ok(poll(&mut fds, ts.as_ref())? > 0)
    }

    /// Um relatório. EOF vira `UnexpectedEof` (nó sumiu).
    fn read_report(&self, buf: &mut [u8; READ_BUF]) -> io::Result<usize> {
        match rustix::io::read(&self.fd, &mut buf[..])? {
            0 => Err(io::ErrorKind::UnexpectedEof.into()),
            n => Ok(n),
        }
    }
}

impl<F: AsFd> HidIo for FdIo<F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let n = rustix::io::write(&self.fd, bytes)?;
        if n == bytes.len() { Ok(()) } else { Err(io::ErrorKind::WriteZero.into()) }
    }

    fn read(&mut self, buf: &mut [u8; READ_BUF], timeout: Duration) -> io::Result<Option<usize>> {
        if !self.wait(Some(timeout))? {
            return Ok(None);
        }
        self.read_report(buf).map(Some)
    }

    fn now(&self) -> Instant { Instant::now() }
}

/// Timeout do `poll`: até o próximo prazo, ou infinito (`None`) sem prazo.
fn poll_timeout(deadline: Option<Instant>, now: Instant) -> Option<Duration> {
    deadline.map(|d| d.saturating_duration_since(now))
}

/// Laço da thread. `send` devolve `false` quando a thread principal sumiu (encerra sem `NodeGone`).
fn run<F: AsFd>(io: &mut FdIo<F>, worker: &mut NodeWorker, node: u32, mut send: impl FnMut(NodeEvent) -> bool) {
    let mut send_all = |evs: Vec<NodeEvent>| evs.into_iter().all(&mut send);
    let now = io.now();
    if !send_all(worker.start(io, now)) {
        return;
    }
    let mut buf = [0u8; READ_BUF];
    loop {
        let ready = match io.wait(poll_timeout(worker.next_deadline(), io.now())) {
            Ok(ready) => ready,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("logi-osd: hidraw-{node}: poll falhou: {e}");
                break;
            }
        };
        if ready {
            match io.read_report(&mut buf) {
                Ok(n) => {
                    if let Some(f) = Frame::parse(&buf[..n]) {
                        let now = io.now();
                        if !send_all(worker.on_frame(io, f, now)) {
                            return;
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    if !is_gone(&e) {
                        eprintln!("logi-osd: hidraw-{node}: leitura falhou: {e}");
                    }
                    break;
                }
            }
        }
        let now = io.now();
        if worker.next_deadline().is_some_and(|d| d <= now) && !send_all(worker.on_timer(io, now)) {
            return;
        }
    }
    send(NodeEvent::NodeGone { node });
}

/// Nó desplugado: `ENODEV` (hidraw) ou EOF.
fn is_gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::UnexpectedEof || e.raw_os_error() == Some(libc::ENODEV)
}

/// Abre o nó (`O_RDWR | O_CLOEXEC`) na thread chamadora e cria a thread `hidraw-{node}`.
pub fn spawn_node(
    devnode: PathBuf,
    node: u32,
    kind: NodeKind,
    product: u16,
    initial_enumeration: bool,
    cfg: &Config,
    tx: Sender<NodeEvent>,
) -> io::Result<()> {
    let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(&devnode)?;
    let interval = cfg.legacy_battery_interval;
    let divert_emoji = cfg.emoji_command.is_some();
    std::thread::Builder::new().name(format!("hidraw-{node}")).stack_size(NODE_STACK).spawn(move || {
        let _guard = GoneOnPanic { tx: tx.clone(), node };
        let mut io = FdIo::new(file);
        let mut worker = NodeWorker::new(node, kind, product, initial_enumeration, interval, divert_emoji);
        run(&mut io, &mut worker, node, |ev| tx.send(ev).is_ok());
    })?;
    Ok(())
}

/// Manda `NodeGone` se a thread do nó morrer em pânico (builds de debug; o release aborta).
/// O fim normal já manda `NodeGone` pelo `run`.
struct GoneOnPanic { tx: Sender<NodeEvent>, node: u32 }

impl Drop for GoneOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // A thread principal pode já ter sumido: nada a fazer então.
            let _ = self.tx.send(NodeEvent::NodeGone { node: self.node });
        }
    }
}

/// `stamp` = `USEC_INITIALIZED` do udev (0 se ausente): distingue um dispositivo novo que
/// reaproveitou o mesmo `/dev/hidrawN`.
#[derive(Debug, Clone)]
pub struct FoundNode { pub devnode: PathBuf, pub kind: NodeKind, pub product: u16, pub stamp: u64 }

/// Nós com thread viva, por devnode: nunca duas threads no mesmo hidraw (o monitor udev é ligado
/// antes da enumeração, então o mesmo nó pode chegar pelas duas vias).
#[derive(Default)]
pub struct LiveNodes { live: Vec<(PathBuf, u32, u64)>, next_id: u32 }

impl LiveNodes {
    /// Id novo se o devnode ainda não tem thread; `None` se já tem.
    pub fn admit(&mut self, n: &FoundNode) -> Option<u32> {
        if self.live.iter().any(|(d, _, _)| *d == n.devnode) {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.live.push((n.devnode.clone(), id, n.stamp));
        Some(id)
    }

    /// Solta o nó (`NodeGone` ou falha ao criar a thread); devolve `(devnode, stamp)`.
    pub fn gone(&mut self, node: u32) -> Option<(PathBuf, u64)> {
        let pos = self.live.iter().position(|(_, id, _)| *id == node)?;
        let (devnode, _, stamp) = self.live.swap_remove(pos);
        Some((devnode, stamp))
    }
}

/// Depois de `NodeGone` o devnode ainda existe: é outro dispositivo (reiniciar) só se o carimbo
/// udev mudou. Mesmo carimbo = a thread caiu por erro com o dispositivo presente; reiniciar daria
/// laço. Sem carimbo, na dúvida, não reinicia.
pub fn is_new_device(old_stamp: u64, found: &FoundNode) -> bool {
    found.stamp != 0 && found.stamp != old_stamp
}

/// Reinspeciona um devnode (`/sys/class/hidraw/<nome>`); `None` se sumiu ou não serve.
pub fn reinspect(devnode: &Path) -> Option<FoundNode> {
    let syspath = Path::new("/sys/class/hidraw").join(devnode.file_name()?);
    if !syspath.exists() {
        return None;
    }
    match udev::Device::from_syspath(&syspath) {
        Ok(dev) => inspect(&dev),
        Err(e) => {
            eprintln!("logi-osd: {}: reinspeção falhou: {e}", devnode.display());
            None
        }
    }
}

/// Monitor udev de `hidraw`. O fd (`AsFd`) entra no calloop como `Generic` (modo `Level`).
pub struct Hotplug { socket: udev::MonitorSocket }

impl Hotplug {
    /// Liga o monitor **antes** de enumerar (nada se perde entre os dois) e devolve os nós HID++
    /// presentes na partida (`initial_enumeration = true`).
    pub fn new() -> io::Result<(Hotplug, Vec<FoundNode>)> {
        let socket = udev::MonitorBuilder::new()?.match_subsystem("hidraw")?.listen()?;
        let mut en = udev::Enumerator::new()?;
        en.match_subsystem("hidraw")?;
        let found = en.scan_devices()?.filter_map(|d| inspect(&d)).collect();
        Ok((Hotplug { socket }, found))
    }

    /// Nós HID++ adicionados desde a última chamada (`initial_enumeration = false`). Remoções são
    /// ignoradas: a thread do nó as vê sozinha. `&self` (não `&mut`): o `Generic` do calloop só
    /// dá acesso compartilhado à fonte sem `unsafe`, e o iterador do udev não precisa de mais.
    pub fn drain(&self) -> Vec<FoundNode> {
        self.socket
            .iter()
            .filter(|e| e.event_type() == udev::EventType::Add)
            .filter_map(|e| inspect(&e))
            .collect()
    }
}

impl AsFd for Hotplug {
    fn as_fd(&self) -> BorrowedFd<'_> { self.socket.as_fd() }
}

/// `HID_ID` do pai `hid` (e se ele é filho de outro `hid`), descritor de `<syspath>/device/report_descriptor`.
fn inspect(dev: &udev::Device) -> Option<FoundNode> {
    let devnode = dev.devnode()?.to_path_buf();
    let hid = match dev.parent_with_subsystem("hid") {
        Ok(Some(hid)) => hid,
        Ok(None) => return None,
        Err(e) => {
            eprintln!("logi-osd: {}: sem pai hid: {e}", devnode.display());
            return None;
        }
    };
    let (bus, vendor, product) = parse_hid_id(hid.property_value("HID_ID")?.to_str()?)?;
    if vendor != VENDOR_LOGITECH {
        return None; // nem lê o descritor de quem não é Logitech
    }
    // Filho DJ: o pai `hid` dele tem outro `hid` como pai (o receptor). Dispositivo com fio não tem.
    let is_dj_child = matches!(hid.parent_with_subsystem("hid"), Ok(Some(_)));
    let mut desc = [0u8; DESCRIPTOR_MAX];
    let len = match read_descriptor(&dev.syspath().join("device/report_descriptor"), &mut desc) {
        Ok(len) => len,
        Err(e) => {
            eprintln!("logi-osd: {}: report_descriptor ilegível: {e}", devnode.display());
            return None;
        }
    };
    let kind = select(bus, vendor, product, is_dj_child, &desc[..len])?;
    let stamp = dev.property_value("USEC_INITIALIZED").and_then(|v| v.to_str()?.parse().ok()).unwrap_or(0);
    Some(FoundNode { devnode, kind, product, stamp })
}

fn read_descriptor(path: &Path, buf: &mut [u8; DESCRIPTOR_MAX]) -> io::Result<usize> {
    let mut f = File::open(path)?;
    let mut len = 0;
    while len < buf.len() {
        match f.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::app::DeviceKey;

    fn found(devnode: &str, stamp: u64) -> FoundNode {
        FoundNode { devnode: PathBuf::from(devnode), kind: NodeKind::Receiver, product: 0xC548, stamp }
    }

    #[test] fn same_devnode_is_admitted_once() {
        // Monitor udev ligado antes da enumeração: o mesmo nó pode chegar pelas duas vias.
        let mut live = LiveNodes::default();
        let a = live.admit(&found("/dev/hidraw3", 7)).expect("primeira vez admite");
        assert_eq!(live.admit(&found("/dev/hidraw3", 7)), None);
        let b = live.admit(&found("/dev/hidraw4", 8)).expect("outro devnode admite");
        assert_ne!(a, b);
    }

    #[test] fn gone_frees_devnode_and_returns_it() {
        let mut live = LiveNodes::default();
        let id = live.admit(&found("/dev/hidraw3", 7)).unwrap();
        assert_eq!(live.gone(id), Some((PathBuf::from("/dev/hidraw3"), 7)));
        assert_eq!(live.gone(id), None, "segundo NodeGone do mesmo id não devolve nada");
        let again = live.admit(&found("/dev/hidraw3", 9)).expect("devnode livre de novo");
        assert_ne!(again, id, "id nunca é reaproveitado");
    }

    #[test] fn restart_only_for_a_new_device_on_the_same_devnode() {
        // hidrawN reaproveitado por outro dispositivo: carimbo udev diferente → reinicia.
        assert!(is_new_device(7, &found("/dev/hidraw3", 9)));
        // Mesmo dispositivo (a thread caiu por erro): não reinicia, senão viraria laço.
        assert!(!is_new_device(7, &found("/dev/hidraw3", 7)));
        // Sem carimbo (udev sem banco): na dúvida não reinicia.
        assert!(!is_new_device(7, &found("/dev/hidraw3", 0)));
    }

    #[test] fn guard_sends_node_gone_only_on_panic() {
        let (tx, rx) = calloop::channel::channel::<NodeEvent>();
        let tx2 = tx.clone();
        let t = std::thread::spawn(move || {
            let _guard = GoneOnPanic { tx: tx2, node: 5 };
            panic!("simulado");
        });
        assert!(t.join().is_err());
        {
            let _guard = GoneOnPanic { tx, node: 6 };
        }
        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(matches!(got.as_slice(), [NodeEvent::NodeGone { node: 5 }]), "{got:?}");
    }

    #[test] fn no_deadline_means_infinite_poll() {
        let now = Instant::now();
        assert_eq!(poll_timeout(None, now), None);
        assert_eq!(poll_timeout(Some(now + Duration::from_secs(900)), now), Some(Duration::from_secs(900)));
        assert_eq!(poll_timeout(Some(now), now + Duration::from_secs(1)), Some(Duration::ZERO), "prazo vencido não espera");
    }

    #[test] fn fd_io_times_out_and_reads_one_report() {
        let (r, mut w) = io::pipe().unwrap();
        let mut io = FdIo::new(r);
        let mut buf = [0u8; READ_BUF];
        assert_eq!(HidIo::read(&mut io, &mut buf, Duration::from_millis(5)).unwrap(), None);
        w.write_all(&[0x10, 0x01, 0x41, 0x10, 0x40, 0x69, 0xB3]).unwrap();
        assert_eq!(HidIo::read(&mut io, &mut buf, Duration::from_millis(5)).unwrap(), Some(7));
        drop(w);
        let e = HidIo::read(&mut io, &mut buf, Duration::from_millis(5)).unwrap_err();
        assert!(is_gone(&e), "EOF = nó sumiu: {e:?}");
    }

    #[test] fn enodev_is_gone() {
        assert!(is_gone(&io::Error::from_raw_os_error(libc::ENODEV)));
        assert!(!is_gone(&io::Error::from_raw_os_error(libc::EIO)));
    }

    #[test] fn loop_forwards_frames_then_reports_node_gone() {
        // Só a ponta de leitura de um pipe: as escritas do `start` falham (logadas) e o laço lê
        // um 0x41 de link perdido e depois EOF.
        let (r, mut w) = io::pipe().unwrap();
        w.write_all(&[0x10, 0x02, 0x41, 0x10, 0x40, 0x69, 0xB3]).unwrap();
        drop(w);
        let mut io = FdIo::new(r);
        let mut worker = NodeWorker::new(9, NodeKind::Receiver, 0xC548, true, Duration::from_secs(900), true);
        let mut got = Vec::new();
        run(&mut io, &mut worker, 9, |ev| { got.push(ev); true });
        assert!(matches!(got.as_slice(), [NodeEvent::Offline { dev: DeviceKey { node: 9, index: 2 } }, NodeEvent::NodeGone { node: 9 }]), "{got:?}");
    }

    #[test] fn loop_stops_quietly_when_main_thread_is_gone() {
        let (r, mut w) = io::pipe().unwrap();
        w.write_all(&[0x10, 0x02, 0x41, 0x10, 0x40, 0x69, 0xB3]).unwrap();
        let mut io = FdIo::new(r);
        let mut worker = NodeWorker::new(9, NodeKind::Receiver, 0xC548, true, Duration::from_secs(900), true);
        let mut calls = 0;
        // `w` continua aberto: se o laço não parasse no primeiro envio recusado, bloquearia no poll.
        run(&mut io, &mut worker, 9, |_| { calls += 1; false });
        assert_eq!(calls, 1);
        drop(w);
    }

    /// Fumaça na máquina real: enumera e abre só para leitura cada nó selecionado. Não escreve nada.
    /// `cargo test real_nodes -- --ignored --nocapture`
    #[test]
    #[ignore = "precisa dos receptores reais"]
    fn real_nodes_enumerate_and_open_read_only() {
        let (_hotplug, found) = Hotplug::new().unwrap();
        for n in &found {
            let opened = File::open(&n.devnode).map(|_| "aberto");
            println!("{} {:?} {:04x} {:?}", n.devnode.display(), n.kind, n.product, opened);
        }
        assert!(!found.is_empty());
    }
}
