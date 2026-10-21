//! Codec de frames HID++ (relatórios curtos de 7 bytes e longos de 20 bytes).

pub const REPORT_SHORT: u8 = 0x10;
pub const SHORT_LEN: usize = 7;
pub const REPORT_LONG: u8 = 0x11;
pub const LONG_LEN: usize = 20;
/// Software ID que usamos nas requisições (nibble baixo do byte de endereço).
pub const SW_ID: u8 = 0x0C;
pub const DEV_RECEIVER: u8 = 0xFF;

const PARAMS_SHORT: usize = SHORT_LEN - 4;
const PARAMS_LONG: usize = LONG_LEN - 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub long: bool,
    pub device: u8,
    pub sub_id: u8,
    pub address: u8,
    pub params: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Reply,
    Error10 { sub_id: u8, address: u8, code: u8 },
    Error20 { index: u8, address: u8, code: u8 },
    Notification10,
    Event20 { index: u8, event: u8 },
}

impl Frame {
    /// Aceita somente 7 bytes começando em 0x10 ou 20 bytes começando em 0x11.
    pub fn parse(buf: &[u8]) -> Option<Frame> {
        let long = match (buf.first()?, buf.len()) {
            (&REPORT_SHORT, SHORT_LEN) => false,
            (&REPORT_LONG, LONG_LEN) => true,
            _ => return None,
        };
        let mut params = [0u8; 16];
        let tail = &buf[4..];
        params[..tail.len()].copy_from_slice(tail);
        Some(Frame { long, device: buf[1], sub_id: buf[2], address: buf[3], params })
    }

    /// Escreve o frame em `out` e devolve o tamanho (7 ou 20).
    pub fn encode(&self, out: &mut [u8; LONG_LEN]) -> usize {
        let (report, len) = if self.long { (REPORT_LONG, LONG_LEN) } else { (REPORT_SHORT, SHORT_LEN) };
        out.fill(0);
        out[0] = report;
        out[1] = self.device;
        out[2] = self.sub_id;
        out[3] = self.address;
        out[4..len].copy_from_slice(self.payload());
        len
    }

    /// Bytes de parâmetros: 3 (curto) ou 16 (longo).
    pub fn payload(&self) -> &[u8] {
        &self.params[..if self.long { PARAMS_LONG } else { PARAMS_SHORT }]
    }

    /// Classificação na ordem do Solaar.
    pub fn kind(&self) -> FrameKind {
        let p = &self.params;
        if self.sub_id == 0x8F {
            FrameKind::Error10 { sub_id: self.address, address: p[0], code: p[1] }
        } else if self.sub_id == 0xFF {
            FrameKind::Error20 { index: self.address, address: p[0], code: p[1] }
        } else if self.sub_id & 0x80 != 0 {
            FrameKind::Reply
        } else if self.sub_id >= 0x40 {
            FrameKind::Notification10
        } else if self.address & 0x0F == 0 {
            FrameKind::Event20 { index: self.sub_id, event: self.address >> 4 }
        } else {
            FrameKind::Reply
        }
    }

    fn build(long: bool, device: u8, sub_id: u8, address: u8, params: &[u8]) -> Frame {
        let cap = if long { PARAMS_LONG } else { PARAMS_SHORT };
        debug_assert!(params.len() <= cap, "params excedem o frame");
        let n = params.len().min(cap);
        let mut buf = [0u8; 16];
        buf[..n].copy_from_slice(&params[..n]);
        Frame { long, device, sub_id, address, params: buf }
    }

    /// Requisição HID++ 2.0: longo se `params` não cabe nos 3 bytes do curto.
    pub fn feature_request(device: u8, index: u8, function: u8, params: &[u8]) -> Frame {
        let address = (function << 4) | SW_ID;
        Frame::build(params.len() > PARAMS_SHORT, device, index, address, params)
    }

    /// Leitura de registrador: 0x81, ou 0x83 (longo) se `register` > 0xFF.
    pub fn register_read(register: u16, params: &[u8]) -> Frame {
        let sub_id = if register > 0xFF { 0x83 } else { 0x81 };
        Frame::build(false, DEV_RECEIVER, sub_id, register as u8, params)
    }

    /// Escrita de registrador (0x80 curto / 0x82 longo).
    pub fn register_write(register: u16, params: &[u8]) -> Frame {
        let long = register > 0xFF || params.len() > PARAMS_SHORT;
        let sub_id = if long { 0x82 } else { 0x80 };
        Frame::build(long, DEV_RECEIVER, sub_id, register as u8, params)
    }
}

/// Lê uma fixture da sonda: linhas `<tempo> <nó> <bytes hex...>`; ignora `#` e linhas vazias.
#[cfg(test)]
pub(crate) fn load_fixture(text: &str) -> Vec<(String, Vec<u8>)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            it.next()?; // timestamp
            let node = it.next()?.to_string();
            let bytes = it.map(|b| u8::from_str_radix(b, 16)).collect::<Result<Vec<_>, _>>().ok()?;
            Some((node, bytes))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn long(prefix: &[u8]) -> Vec<u8> { let mut v = prefix.to_vec(); v.resize(20, 0); v }

    #[test] fn parse_short_reply() {
        let f = Frame::parse(&[0x10,0xFF,0x81,0x00,0x00,0x09,0x00]).unwrap();
        assert_eq!((f.long, f.device, f.sub_id, f.address), (false, 0xFF, 0x81, 0x00));
        assert_eq!(f.payload(), &[0x00,0x09,0x00]);
        assert!(matches!(f.kind(), FrameKind::Reply));
    }
    #[test] fn parse_rejects_wrong_length() { assert!(Frame::parse(&[0x10,1,2,3]).is_none()); assert!(Frame::parse(&[0x11;7]).is_none()); assert!(Frame::parse(&[0x20;15]).is_none()); assert!(Frame::parse(&[0x10;20]).is_none()); }
    #[test] fn event20_has_zero_sw_id() { let f = Frame::parse(&long(&[0x11,0x01,0x08,0x00])).unwrap(); assert!(matches!(f.kind(), FrameKind::Event20{index:0x08, event:0})); }
    #[test] fn reply20_with_our_sw_id_is_reply() { let f = Frame::parse(&long(&[0x11,0x01,0x08,0x1C])).unwrap(); assert!(matches!(f.kind(), FrameKind::Reply)); }
    #[test] fn connection_notification_is_10() { let f = Frame::parse(&[0x10,0x01,0x41,0x10,0x00,0x69,0xB3]).unwrap(); assert!(matches!(f.kind(), FrameKind::Notification10)); }
    #[test] fn errors_decoded() {
        assert!(matches!(Frame::parse(&[0x10,0x01,0x8F,0x00,0x1C,0x01,0x00]).unwrap().kind(), FrameKind::Error10{sub_id:0x00,address:0x1C,code:0x01}));
        assert!(matches!(Frame::parse(&long(&[0x11,0x01,0xFF,0x08,0x1C,0x02])).unwrap().kind(), FrameKind::Error20{index:0x08,address:0x1C,code:0x02}));
    }
    #[test] fn feature_request_encodes_function_and_sw_id() {
        let mut b = [0u8; LONG_LEN];
        let n = Frame::feature_request(0x01, 0x00, 0x00, &[0x42, 0x20]).encode(&mut b);
        assert_eq!(&b[..n], &[0x10,0x01,0x00,0x0C,0x42,0x20,0x00]);
        let n = Frame::feature_request(0x01, 0x05, 0x03, &[0x01,0x08,0x03,0x00,0x00]).encode(&mut b);
        assert_eq!(n, 20); assert_eq!(&b[..9], &[0x11,0x01,0x05,0x3C,0x01,0x08,0x03,0x00,0x00]);
        assert_eq!(&b[9..], &[0u8; 11]);
    }
    #[test] fn feature_request_length_boundary() {
        let mut b = [0u8; LONG_LEN];
        assert_eq!(Frame::feature_request(1, 5, 0, &[1,2,3]).encode(&mut b), 7);
        assert_eq!(Frame::feature_request(1, 5, 0, &[1,2,3,4]).encode(&mut b), 20);
    }
    #[test] fn short_feature_reply_is_reply() {
        let f = Frame::parse(&[0x10,0x01,0x05,0x1C,0,0,0]).unwrap();
        assert!(matches!(f.kind(), FrameKind::Reply));
    }
    #[test] fn long_payload_is_16() { assert_eq!(Frame::parse(&long(&[0x11,1,5,0x1C])).unwrap().payload().len(), 16); }
    #[test] fn register_write_long_register_uses_82() {
        let mut b = [0u8; LONG_LEN];
        let n = Frame::register_write(0x2B5, &[0x01]).encode(&mut b);
        assert_eq!(n, 20); assert_eq!(&b[..5], &[0x11,0xFF,0x82,0xB5,0x01]);
    }
    #[test] fn register_frames() {
        let mut b = [0u8; LONG_LEN];
        let n = Frame::register_read(0x2B5, &[0x60 + 1, 0x01]).encode(&mut b); assert_eq!(&b[..n], &[0x10,0xFF,0x83,0xB5,0x61,0x01,0x00]);
        let n = Frame::register_write(0x02, &[0x02,0,0]).encode(&mut b);     assert_eq!(&b[..n], &[0x10,0xFF,0x80,0x02,0x02,0x00,0x00]);
    }
    #[test] fn fixture_loader_skips_comments() { let v = load_fixture("# x\n  12 c548 10 ff 81 00 00 09 00\n"); assert_eq!(v, vec![("c548".into(), vec![0x10,0xFF,0x81,0,0,9,0])]); }
}
