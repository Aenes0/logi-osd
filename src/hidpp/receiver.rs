//! Receptor (Bolt/Unifying/Nano): flags de notificação, gatilho de chegada e notificações 1.0.

use super::frame::Frame;
use super::transport::{HidIo, HidppError, request};

/// Registrador 0x00: bits de notificação do receptor (3 bytes, big-endian).
pub const REG_NOTIFICATION_FLAGS: u16 = 0x00;
/// Registrador 0x02: escrever `[0x02, 0, 0]` faz o receptor emitir um 0x41 por dispositivo pareado.
pub const REG_CONNECTION_STATE: u16 = 0x02;
pub const FLAG_WIRELESS: u32 = 0x00_01_00;

const SUB_LINK: u8 = 0x41;
const SUB_POWER: u8 = 0x4B;
const POWER_ON: u8 = 0x01;
/// `p0 & 0x40` = link **não** estabelecido.
const LINK_NOT_ESTABLISHED: u8 = 0x40;
const ARRIVAL_TRIGGER: [u8; 3] = [0x02, 0x00, 0x00];

/// Liga o bit WIRELESS (notificações 0x41) sem nunca desligar outro bit (o Solaar pode ter ligado
/// SOFTWARE_PRESENT etc.). Devolve `true` se precisou escrever.
pub fn ensure_wireless_notifications(io: &mut impl HidIo, pending: &mut Vec<Frame>) -> Result<bool, HidppError> {
    let p = request(io, &Frame::register_read(REG_NOTIFICATION_FLAGS, &[]), pending)?;
    let flags = u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]);
    if flags & FLAG_WIRELESS != 0 {
        return Ok(false);
    }
    let new = flags | FLAG_WIRELESS;
    let bytes = [(new >> 16) as u8, (new >> 8) as u8, new as u8];
    request(io, &Frame::register_write(REG_NOTIFICATION_FLAGS, &bytes), pending)?;
    Ok(true)
}

/// Pede ao receptor uma notificação 0x41 por dispositivo pareado (chegam em `pending` ou depois).
pub fn trigger_arrival(io: &mut impl HidIo, pending: &mut Vec<Frame>) -> Result<(), HidppError> {
    request(io, &Frame::register_write(REG_CONNECTION_STATE, &ARRIVAL_TRIGGER), pending).map(|_| ())
}


/// Layout do codinome do dispositivo pareado (registrador longo 0x2B5) conforme o receptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codename {
    /// `[0x60 + n, 0x01]` → texto nos bytes 3.. (comprimento no byte 2).
    Bolt,
    /// Unifying/Nano/Lightspeed: `[0x40 + n - 1]` → texto nos bytes 2.. (comprimento no byte 1).
    Unifying,
    /// Nó direto (Bluetooth/cabo): sem receptor, só DEVICE_NAME.
    None,
}

/// Receptores Bolt (`base_usb.py` do Solaar 1.1.20).
pub const BOLT_PIDS: &[u16] = &[0xC548];
/// Todos os receptores Logitech HID++ (`base_usb.py` do Solaar 1.1.20; o Lenovo 0x6042 é de
/// outro vendor e fica de fora).
pub const RECEIVER_PIDS: &[u16] = &[
    0xC517, 0xC518, 0xC51A, 0xC51B, 0xC521, 0xC525, 0xC526, 0xC52B, 0xC52E, 0xC52F, 0xC531, 0xC532,
    0xC534, 0xC535, 0xC537, 0xC539, 0xC53A, 0xC53D, 0xC53F, 0xC541, 0xC545, 0xC547, 0xC548, 0xC54D,
];

/// Layout do codinome para o receptor com este PID USB; PID que não é receptor → `None`.
pub fn codename_for_receiver(product: u16) -> Codename {
    if BOLT_PIDS.contains(&product) {
        Codename::Bolt
    } else if RECEIVER_PIDS.contains(&product) {
        Codename::Unifying
    } else {
        Codename::None
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkNotice {
    pub index: u8,
    pub online: bool,
    pub wpid: u16,
}

/// Notificação 0x41: `device` = índice, `address` = protocolo, `p0 & 0x40` = sem link,
/// wpid = `p2 << 8 | p1`. `address 0x00` não é anúncio de dispositivo e é ignorado.
pub fn parse_link(frame: &Frame) -> Option<LinkNotice> {
    if frame.sub_id != SUB_LINK || frame.address == 0x00 {
        return None;
    }
    let p = &frame.params;
    Some(LinkNotice {
        index: frame.device,
        online: p[0] & LINK_NOT_ESTABLISHED == 0,
        wpid: u16::from(p[2]) << 8 | u16::from(p[1]),
    })
}

/// Notificação 0x4B com `address 0x01` = dispositivo ligou/acordou. Devolve o índice.
pub fn parse_power_on(frame: &Frame) -> Option<u8> {
    (frame.sub_id == SUB_POWER && frame.address == POWER_ON).then_some(frame.device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::fake::FakeDevice;

    /// Receptor com o registrador 0x00 valendo `flags`; ecoa escritas como o hardware.
    fn receiver(flags: [u8; 3]) -> FakeDevice {
        FakeDevice::new(move |f| match (f.device, f.sub_id, f.address) {
            (0xFF, 0x81, 0x00) => vec![vec![0x10, 0xFF, 0x81, 0x00, flags[0], flags[1], flags[2]]],
            (0xFF, 0x80, reg) => vec![vec![0x10, 0xFF, 0x80, reg, 0x00, 0x00, 0x00]],
            _ => vec![],
        })
    }

    #[test] fn receiver_flags_are_read_modify_write_never_cleared() {
        let mut io = receiver([0x00, 0x08, 0x00]);
        let mut pending = Vec::new();
        assert_eq!(ensure_wireless_notifications(&mut io, &mut pending), Ok(true));
        assert_eq!(io.written, vec![
            vec![0x10, 0xFF, 0x81, 0x00, 0x00, 0x00, 0x00],
            vec![0x10, 0xFF, 0x80, 0x00, 0x00, 0x09, 0x00],
        ]);

        // Valor da sonda (c548 e c537): já ligado → só a leitura.
        let mut io = receiver([0x00, 0x09, 0x00]);
        assert_eq!(ensure_wireless_notifications(&mut io, &mut pending), Ok(false));
        assert_eq!(io.written.len(), 1);
    }

    #[test] fn receiver_flags_keep_every_other_bit() {
        let mut io = receiver([0xAB, 0xFE, 0xCD]);
        let mut pending = Vec::new();
        assert_eq!(ensure_wireless_notifications(&mut io, &mut pending), Ok(true));
        assert_eq!(io.written[1], vec![0x10, 0xFF, 0x80, 0x00, 0xAB, 0xFF, 0xCD]);
    }

    #[test] fn receiver_flags_read_error_is_returned() {
        let mut io = FakeDevice::new(|_| vec![vec![0x10, 0xFF, 0x8F, 0x81, 0x00, 0x02, 0x00]]);
        let mut pending = Vec::new();
        assert_eq!(ensure_wireless_notifications(&mut io, &mut pending), Err(HidppError::Hidpp10(0x02)));
        assert_eq!(io.written.len(), 1);
    }

    #[test] fn arrival_trigger_writes_register_02() {
        let mut io = receiver([0x00, 0x09, 0x00]);
        let mut pending = Vec::new();
        assert_eq!(trigger_arrival(&mut io, &mut pending), Ok(()));
        assert_eq!(io.written, vec![vec![0x10, 0xFF, 0x80, 0x02, 0x02, 0x00, 0x00]]);
    }

    #[test] fn arrival_notifications_land_in_pending() {
        // Sonda c548: o 0x41 do MX chega junto com a resposta do gatilho.
        let mut io = FakeDevice::new(|_| vec![
            vec![0x10, 0x01, 0x41, 0x10, 0x01, 0x69, 0xB3],
            vec![0x10, 0xFF, 0x80, 0x02, 0x00, 0x00, 0x00],
        ]);
        let mut pending = Vec::new();
        assert_eq!(trigger_arrival(&mut io, &mut pending), Ok(()));
        assert_eq!(pending.len(), 1);
        assert_eq!(parse_link(&pending[0]), Some(LinkNotice { index: 1, online: true, wpid: 0xB369 }));
    }

    #[test] fn link_notice_online_and_offline() {
        let on = Frame::parse(&[0x10, 0x01, 0x41, 0x10, 0x00, 0x69, 0xB3]).unwrap();
        assert_eq!(parse_link(&on), Some(LinkNotice { index: 1, online: true, wpid: 0xB369 }));
        let off = Frame::parse(&[0x10, 0x01, 0x41, 0x10, 0x40, 0x69, 0xB3]).unwrap();
        assert_eq!(parse_link(&off), Some(LinkNotice { index: 1, online: false, wpid: 0xB369 }));
        // Sonda: desligar/Easy-Switch = p0 0x41 (sem link); G602 no c537 = p0 0x12 (com link).
        let easy = Frame::parse(&[0x10, 0x01, 0x41, 0x10, 0x41, 0x69, 0xB3]).unwrap();
        assert_eq!(parse_link(&easy).map(|n| n.online), Some(false));
        let g602 = Frame::parse(&[0x10, 0x01, 0x41, 0x07, 0x12, 0x2C, 0x40]).unwrap();
        assert_eq!(parse_link(&g602), Some(LinkNotice { index: 1, online: true, wpid: 0x402C }));
    }

    #[test] fn link_notice_ignores_address_zero() {
        let f = Frame::parse(&[0x10, 0x01, 0x41, 0x00, 0x00, 0x69, 0xB3]).unwrap();
        assert_eq!(parse_link(&f), None);
        let other = Frame::parse(&[0x10, 0x01, 0x40, 0x10, 0x00, 0x69, 0xB3]).unwrap();
        assert_eq!(parse_link(&other), None);
    }

    #[test] fn receiver_kind_from_product() {
        assert_eq!(codename_for_receiver(0xC548), Codename::Bolt);
        assert_eq!(codename_for_receiver(0xC537), Codename::Unifying); // Nano do G602
        assert_eq!(codename_for_receiver(0xC52B), Codename::Unifying);
        assert_eq!(codename_for_receiver(0xC547), Codename::Unifying); // Lightspeed
        assert_eq!(codename_for_receiver(0xB369), Codename::None);     // MX Keys Mini direto (BT)
        assert_eq!(codename_for_receiver(0x0000), Codename::None);
    }

    #[test] fn power_on_notice() {
        let f = Frame::parse(&[0x10, 0x01, 0x4B, 0x01, 0, 0, 0]).unwrap();
        assert_eq!(parse_power_on(&f), Some(1));
        let f = Frame::parse(&[0x10, 0x01, 0x4B, 0x00, 0, 0, 0]).unwrap();
        assert_eq!(parse_power_on(&f), None);
        let f = Frame::parse(&[0x10, 0x01, 0x41, 0x01, 0, 0, 0]).unwrap();
        assert_eq!(parse_power_on(&f), None);
    }
}
