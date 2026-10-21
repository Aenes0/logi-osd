//! Transporte HID++: envia uma requisição e espera a resposta que casa com ela.

use std::io;
use std::time::{Duration, Instant};

use super::frame::{DEV_RECEIVER, Frame, FrameKind, LONG_LEN};

pub trait HidIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Ok(None) = timeout. Ok(Some(n)) = n bytes em buf.
    fn read(&mut self, buf: &mut [u8; 32], timeout: Duration) -> io::Result<Option<usize>>;
    /// Relógio usado por toda lógica de prazo. Real: `Instant::now()`. FakeIo: relógio virtual.
    fn now(&self) -> Instant;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidppError {
    Timeout,
    Hidpp10(u8),
    Hidpp20(u8),
    Io(io::ErrorKind),
}

/// O G602 leva até ~1,7 s para responder à primeira requisição depois de ocioso; 4 s é o valor do Solaar.
pub const DEVICE_TIMEOUT: Duration = Duration::from_millis(4000);
/// Registradores do receptor respondem rápido. ×2 em leitura longa 0x83 0xB5 (e 0x81 0xF1).
pub const RECEIVER_TIMEOUT: Duration = Duration::from_millis(1000);
/// Teto absoluto da espera, em múltiplos do timeout.
pub const HARD_CAP_FACTOR: u32 = 3;
/// Máximo de eventos guardados em `pending` durante uma requisição.
pub const PENDING_MAX: usize = 64;

/// Registradores cuja resposta também tem que casar o primeiro parâmetro (sub-registrador)
/// e que o hardware demora mais para responder (regra do `base.request` do Solaar).
const SLOW_SUBREGISTER: [(u8, u8); 2] = [(0x83, 0xB5), (0x81, 0xF1)];

/// Envia `req` e devolve o payload da resposta. Frames que não casam vão para `pending`
/// (notificações a processar depois, na ordem).
pub fn request(io: &mut impl HidIo, req: &Frame, pending: &mut Vec<Frame>) -> Result<[u8; 16], HidppError> {
    let (res, dropped) = exchange(io, req, pending);
    if dropped > 0 {
        eprintln!("logi-osd: fila de eventos cheia, {dropped} descartados durante uma requisição HID++");
    }
    res
}

/// Igual a `request`, mas devolve também quantos eventos não couberam em `pending`.
pub(crate) fn exchange(io: &mut impl HidIo, req: &Frame, pending: &mut Vec<Frame>) -> (Result<[u8; 16], HidppError>, usize) {
    let mut out = [0u8; LONG_LEN];
    let len = req.encode(&mut out);
    if let Err(e) = io.write(&out[..len]) {
        return (Err(HidppError::Io(e.kind())), 0);
    }

    let timeout = timeout_for(req);
    let start = io.now();
    let hard_limit = start + timeout * HARD_CAP_FACTOR;
    let mut deadline = start + timeout;
    let mut dropped = 0usize;
    let mut buf = [0u8; 32];

    loop {
        let now = io.now();
        let limit = deadline.min(hard_limit);
        if now >= limit {
            return (Err(HidppError::Timeout), dropped);
        }
        let n = match io.read(&mut buf, limit - now) {
            Ok(Some(n)) => n.min(buf.len()),
            Ok(None) => continue, // o topo do laço decide se o prazo acabou
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return (Err(HidppError::Io(e.kind())), dropped),
        };
        let Some(frame) = Frame::parse(&buf[..n]) else { continue };

        if is_same_device(req, &frame) {
            if let Some(res) = match_reply(req, &frame) {
                return (res, dropped);
            }
            // Como o Solaar: tráfego do mesmo dispositivo mostra que ele está vivo. O teto absoluto
            // impede que uma rajada de eventos prenda a espera para sempre.
            deadline = io.now() + timeout;
        }
        keep_if_event(frame, pending, &mut dropped);
    }
}

fn timeout_for(req: &Frame) -> Duration {
    let is_receiver_register = req.device == DEV_RECEIVER && req.sub_id & 0x80 != 0;
    let base = if is_receiver_register { RECEIVER_TIMEOUT } else { DEVICE_TIMEOUT };
    if is_slow_subregister(req) { base * 2 } else { base }
}

fn is_slow_subregister(req: &Frame) -> bool { SLOW_SUBREGISTER.contains(&(req.sub_id, req.address)) }

/// Dispositivos Bluetooth diretos respondem com 0x00 a uma requisição para 0xFF (e vice-versa).
fn is_same_device(req: &Frame, frame: &Frame) -> bool {
    frame.device == req.device || frame.device == req.device ^ 0xFF
}

/// `Some` se o frame é a resposta (ou o erro) desta requisição.
fn match_reply(req: &Frame, frame: &Frame) -> Option<Result<[u8; 16], HidppError>> {
    let p = &frame.params;
    // Erros: `address` traz o sub_id que falhou e `params[0]` o address que falhou.
    let names_req = frame.address == req.sub_id && p[0] == req.address;
    match frame.sub_id {
        0x8F => return names_req.then_some(Err(HidppError::Hidpp10(p[1]))),
        0xFF => return names_req.then_some(Err(HidppError::Hidpp20(p[1]))),
        _ => {}
    }
    if frame.sub_id != req.sub_id || frame.address != req.address {
        return None;
    }
    if is_slow_subregister(req) && p[0] != req.params[0] {
        return None;
    }
    Some(Ok(frame.params))
}

/// Só eventos 2.0 e notificações 1.0 seguem para `pending`. Respostas e erros que não casaram
/// (outro software no mesmo hidraw, ou resposta atrasada nossa) e o no-op `sub_id 0` são descartados.
fn keep_if_event(frame: Frame, pending: &mut Vec<Frame>, dropped: &mut usize) {
    let keep = match frame.kind() {
        FrameKind::Event20 { index, .. } => index != 0x00,
        FrameKind::Notification10 => true,
        FrameKind::Reply | FrameKind::Error10 { .. } | FrameKind::Error20 { .. } => false,
    };
    if !keep {
        return;
    }
    if pending.len() < PENDING_MAX {
        pending.push(frame);
    } else {
        *dropped += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::fake::{FakeDevice, FakeIo, Step};

    fn long(prefix: &[u8]) -> Vec<u8> { let mut v = prefix.to_vec(); v.resize(20, 0); v }
    fn enc(f: &Frame) -> Vec<u8> { let mut b = [0u8; LONG_LEN]; let n = f.encode(&mut b); b[..n].to_vec() }
    /// Requisição 2.0 típica: dev 1, feature index 0x08, função 1 → address 0x1C.
    fn req20() -> Frame { Frame::feature_request(0x01, 0x08, 0x01, &[]) }
    fn reply20() -> Vec<u8> { long(&[0x11, 0x01, 0x08, 0x1C, 0xAA, 0xBB]) }
    fn caps_event() -> Vec<u8> { long(&[0x11, 0x01, 0x0D, 0x00, 0x02]) }

    #[test] fn returns_matching_reply_payload() {
        let req = req20();
        let mut io = FakeIo::new([Step::Reply(reply20())]);
        let mut pending = Vec::new();
        let p = request(&mut io, &req, &mut pending).unwrap();
        assert_eq!(&p[..2], &[0xAA, 0xBB]);
        assert_eq!(io.written, vec![enc(&req)]);
        assert!(pending.is_empty());
    }

    #[test] fn notification_before_reply_goes_to_pending() {
        let mut io = FakeIo::new([Step::Reply(caps_event()), Step::Reply(reply20())]);
        let mut pending = Vec::new();
        assert!(request(&mut io, &req20(), &mut pending).is_ok());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0], Frame::parse(&caps_event()).unwrap());
        assert!(matches!(pending[0].kind(), FrameKind::Event20 { index: 0x0D, event: 0 }));
    }

    #[test] fn connection_notification10_goes_to_pending() {
        let conn = vec![0x10, 0x01, 0x41, 0x10, 0x01, 0x69, 0xB3];
        let mut io = FakeIo::new([Step::Reply(conn.clone()), Step::Reply(reply20())]);
        let mut pending = Vec::new();
        assert!(request(&mut io, &req20(), &mut pending).is_ok());
        assert_eq!(pending, vec![Frame::parse(&conn).unwrap()]);
    }

    #[test] fn reply_with_other_sw_id_is_not_ours() {
        let solaar = long(&[0x11, 0x01, 0x08, 0x1B, 0x99]);
        let mut io = FakeIo::new([Step::Reply(solaar), Step::Reply(reply20())]);
        let mut pending = Vec::new();
        let p = request(&mut io, &req20(), &mut pending).unwrap();
        assert_eq!(p[0], 0xAA);
        assert!(pending.is_empty());
    }

    #[test] fn bt_device_answering_with_dev_00_matches_ff() {
        let req = Frame::feature_request(0xFF, 0x08, 0x01, &[]);
        let mut io = FakeIo::new([Step::Reply(long(&[0x11, 0x00, 0x08, 0x1C, 0x55]))]);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req, &mut pending).unwrap()[0], 0x55);
    }

    #[test] fn reply_from_other_device_is_not_ours() {
        let other = long(&[0x11, 0x02, 0x08, 0x1C, 0x77]);
        let mut io = FakeIo::new([Step::Reply(other), Step::Reply(reply20())]);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req20(), &mut pending).unwrap()[0], 0xAA);
        assert!(pending.is_empty());
    }

    #[test] fn hidpp10_error_maps() {
        // Requisição dev 1, sub_id 0x00, address 0x1C (root, função 1).
        let req = Frame::feature_request(0x01, 0x00, 0x01, &[]);
        let mut io = FakeIo::new([Step::Reply(vec![0x10, 0x01, 0x8F, 0x00, 0x1C, 0x01, 0x00])]);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req, &mut pending), Err(HidppError::Hidpp10(0x01)));
    }

    #[test] fn hidpp10_register_error_maps() {
        let req = Frame::register_read(0x00, &[]);
        let mut io = FakeIo::new([Step::Reply(vec![0x10, 0xFF, 0x8F, 0x81, 0x00, 0x02, 0x00])]);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req, &mut pending), Err(HidppError::Hidpp10(0x02)));
    }

    #[test] fn hidpp20_error_maps() {
        let mut io = FakeIo::new([Step::Reply(long(&[0x11, 0x01, 0xFF, 0x08, 0x1C, 0x02]))]);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req20(), &mut pending), Err(HidppError::Hidpp20(0x02)));
    }

    #[test] fn timeout_when_nothing_arrives() {
        let mut io = FakeIo::new([Step::Timeout]);
        let t0 = io.now();
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req20(), &mut pending), Err(HidppError::Timeout));
        assert_eq!(io.now() - t0, DEVICE_TIMEOUT);
        assert_eq!(io.written.len(), 1);
    }

    #[test] fn receiver_register_uses_receiver_timeout() {
        let mut io = FakeIo::new([Step::Timeout]);
        let t0 = io.now();
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &Frame::register_read(0x02, &[]), &mut pending), Err(HidppError::Timeout));
        assert_eq!(io.now() - t0, RECEIVER_TIMEOUT);
    }

    #[test] fn long_register_read_83b5_doubles_timeout() {
        let mut io = FakeIo::new([Step::Timeout]);
        let t0 = io.now();
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &Frame::register_read(0x2B5, &[0x61]), &mut pending), Err(HidppError::Timeout));
        assert_eq!(io.now() - t0, RECEIVER_TIMEOUT * 2);
    }

    #[test] fn foreign_error20_is_discarded() {
        let kernel_err = long(&[0x11, 0x01, 0xFF, 0x08, 0x11, 0x05]);
        let mut io = FakeIo::new([Step::Reply(kernel_err), Step::Reply(reply20())]);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req20(), &mut pending).unwrap()[0], 0xAA);
        assert!(pending.is_empty());
    }

    #[test] fn error20_for_other_function_is_discarded() {
        // Mesmo software id (0x0C) mas outra função (0x2C): é de outra requisição nossa, não desta.
        let stale = long(&[0x11, 0x01, 0xFF, 0x08, 0x2C, 0x05]);
        let mut io = FakeIo::new([Step::Reply(stale), Step::Reply(reply20())]);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req20(), &mut pending).unwrap()[0], 0xAA);
        assert!(pending.is_empty());
    }

    #[test] fn unmatched_register_reply_is_discarded() {
        let req = Frame::register_read(0x00, &[]);
        let mut io = FakeIo::new([
            Step::Reply(vec![0x10, 0xFF, 0x81, 0x02, 0x00, 0x03, 0x00]),
            Step::Reply(vec![0x10, 0xFF, 0x81, 0x00, 0x00, 0x09, 0x00]),
        ]);
        let mut pending = Vec::new();
        assert_eq!(&request(&mut io, &req, &mut pending).unwrap()[..3], &[0x00, 0x09, 0x00]);
        assert!(pending.is_empty());
    }

    #[test] fn noop_notification_is_discarded() {
        let mut io = FakeIo::new([Step::Reply(vec![0x10, 0x01, 0x00, 0x00, 0, 0, 0]), Step::Timeout]);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req20(), &mut pending), Err(HidppError::Timeout));
        assert!(pending.is_empty());
    }

    #[test] fn garbage_report_is_ignored() {
        // Relatório DJ (0x20) e frame curto truncado: nem retorno, nem pending.
        let mut io = FakeIo::new([
            Step::Reply(vec![0x20, 0x01, 0x02, 0x00, 0x00]),
            Step::Reply(vec![0x10, 0x01, 0x08]),
            Step::Reply(reply20()),
        ]);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req20(), &mut pending).unwrap()[0], 0xAA);
        assert!(pending.is_empty());
    }

    #[test] fn long_register_read_must_match_subregister() {
        let req = Frame::register_read(0x2B5, &[0x61]);
        let mut io = FakeIo::new([
            Step::Reply(long(&[0x11, 0xFF, 0x83, 0xB5, 0x62, 0x01])),
            Step::Reply(long(&[0x11, 0xFF, 0x83, 0xB5, 0x61, 0x02])),
        ]);
        let mut pending = Vec::new();
        let p = request(&mut io, &req, &mut pending).unwrap();
        assert_eq!(&p[..2], &[0x61, 0x02]);
        assert!(pending.is_empty());
    }

    #[test] fn event_storm_cannot_extend_wait_forever() {
        let mut io = FakeIo::new([Step::Forever(caps_event())]);
        let t0 = io.now();
        let mut pending = Vec::new();
        let (res, dropped) = exchange(&mut io, &req20(), &mut pending);
        let waited = io.now() - t0;
        assert_eq!(res, Err(HidppError::Timeout));
        // O prazo reiniciou (passou de 1×) mas parou no teto de 3×.
        assert!(waited > DEVICE_TIMEOUT, "esperou {waited:?}");
        assert!(waited <= DEVICE_TIMEOUT * HARD_CAP_FACTOR, "esperou {waited:?}");
        assert_eq!(pending.len(), PENDING_MAX);
        let reads = waited.as_millis() as usize;
        assert_eq!(dropped, reads - PENDING_MAX);
    }

    #[test] fn other_device_events_do_not_restart_deadline() {
        // Requisição ao receptor; eventos do dev 1 não renovam o prazo.
        let mut io = FakeIo::new([Step::Forever(caps_event())]);
        let t0 = io.now();
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &Frame::register_read(0x02, &[]), &mut pending), Err(HidppError::Timeout));
        assert_eq!(io.now() - t0, RECEIVER_TIMEOUT);
    }

    #[test] fn pending_already_full_drops_new_events() {
        let mut pending = vec![Frame::parse(&caps_event()).unwrap(); PENDING_MAX];
        let mut io = FakeIo::new([Step::Reply(caps_event()), Step::Reply(reply20())]);
        let (res, dropped) = exchange(&mut io, &req20(), &mut pending);
        assert!(res.is_ok());
        assert_eq!((pending.len(), dropped), (PENDING_MAX, 1));
    }

    #[test] fn write_error_maps_to_io() {
        let mut io = FakeIo::new([Step::Reply(reply20())]);
        io.fail_write = Some(io::ErrorKind::BrokenPipe);
        let mut pending = Vec::new();
        assert_eq!(request(&mut io, &req20(), &mut pending), Err(HidppError::Io(io::ErrorKind::BrokenPipe)));
    }

    #[test] fn read_error_maps_to_io_but_eintr_retries() {
        let mut io = FakeIo::new([Step::Fail(io::ErrorKind::Interrupted), Step::Reply(reply20())]);
        let mut pending = Vec::new();
        assert!(request(&mut io, &req20(), &mut pending).is_ok());
        let mut io = FakeIo::new([Step::Fail(io::ErrorKind::NotConnected)]);
        assert_eq!(request(&mut io, &req20(), &mut pending), Err(HidppError::Io(io::ErrorKind::NotConnected)));
    }

    #[test] fn fake_device_answers_by_rule() {
        let mut dev = FakeDevice::new(|f| {
            if f.sub_id == 0x08 { vec![long(&[0x11, f.device, f.sub_id, f.address, 0x42])] } else { vec![] }
        });
        dev.push(caps_event());
        let mut pending = Vec::new();
        assert_eq!(request(&mut dev, &req20(), &mut pending).unwrap()[0], 0x42);
        assert_eq!(pending.len(), 1);
        let other = Frame::feature_request(0x01, 0x09, 0x00, &[]);
        assert_eq!(request(&mut dev, &other, &mut pending), Err(HidppError::Timeout));
        assert_eq!(dev.written.len(), 2);
    }
}
