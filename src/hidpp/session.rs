//! Descoberta de um dispositivo HID++ 2.0 e leituras de estado da sessão.
//!
//! Erros: `Timeout`/`Io`/`Hidpp10` sempre sobem (dispositivo sumiu ou não é 2.0). Um erro 2.0
//! (`Hidpp20`) numa feature **opcional** quer dizer "essa função não serve neste aparelho" e vira
//! `None`; a descoberta segue para as outras features.

use super::events::LockState;
use super::feature::{FeatureMap, cid, id};
use super::frame::Frame;
use super::receiver::Codename;
use super::transport::{HidIo, HidppError, request};
use crate::battery::{BatteryReading, reading_from_status, reading_from_unified};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryKind { Unified, Status, None }

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub index: u8,
    pub wpid: u16,
    pub name: String,
    pub features: FeatureMap,
    pub battery: BatteryKind,
    pub backlight_levels: Option<u8>,
    pub hosts: Option<(u8 /*total*/, u8 /*atual, 0-based*/)>,
    pub emoji_divertable: bool,
    /// getCidReporting(0x0108) da descoberta já mostrou a tecla desviada (flags1 bit0).
    pub emoji_diverted: bool,
}

/// Valor de ping devolvido no byte 2 da resposta (qualquer um serve; o casamento é por função).
const PING_MARK: u8 = 0x5A;
const ROOT_GET_FEATURE: u8 = 0;
const ROOT_PING: u8 = 1;
/// Registrador longo de informação do receptor (codinome do dispositivo pareado).
const REG_RECEIVER_INFO: u16 = 0x2B5;
const BOLT_CODENAME_BASE: u8 = 0x60;
const UNIFYING_CODENAME_BASE: u8 = 0x40;
/// Payload de 16 bytes: Bolt guarda o texto a partir do byte 3, Unifying a partir do 2.
const BOLT_CODENAME_MAX: usize = 13;
const UNIFYING_CODENAME_MAX: usize = 14;
const NAME_CHUNK: usize = 16;
/// REPROG V4: fn2 getCidReporting, fn3 setCidReporting. flags1 bit0 = desviada, bit1 = "bit0 vale".
const REPROG_GET_REPORTING: u8 = 2;
const REPROG_SET_REPORTING: u8 = 3;
const FLAG_DIVERTED: u8 = 0x01;
const DIVERT_ON: u8 = 0x03;
/// Prefixo do 40A3 quando não há HOSTS_INFO (como o Solaar).
const NO_HOST_PREFIX: u8 = 0xFF;

fn call(io: &mut impl HidIo, dev: u8, index: u8, function: u8, params: &[u8], pending: &mut Vec<Frame>) -> Result<[u8; 16], HidppError> {
    request(io, &Frame::feature_request(dev, index, function, params), pending)
}

/// Feature opcional: erro 2.0 → `None`, o resto sobe.
fn optional<T>(r: Result<T, HidppError>) -> Result<Option<T>, HidppError> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(HidppError::Hidpp20(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Lê a função `function` da feature `feature`, se o dispositivo a tiver.
fn read_feature(io: &mut impl HidIo, dev: u8, features: &FeatureMap, feature: u16, function: u8, params: &[u8], pending: &mut Vec<Frame>) -> Result<Option<[u8; 16]>, HidppError> {
    let Some(index) = features.index_of(feature) else { return Ok(None) };
    optional(call(io, dev, index, function, params, pending))
}

/// `codename`: layout do codinome do receptor (`receiver::codename_for_receiver(product)`), ou
/// `Codename::None` em nó direto.
pub fn discover(io: &mut impl HidIo, index: u8, wpid: u16, codename: Codename, pending: &mut Vec<Frame>) -> Result<DeviceInfo, HidppError> {
    let version = call(io, index, 0x00, ROOT_PING, &[0, 0, PING_MARK], pending)?;
    if version[0] < 2 {
        // Dispositivos HID++ 1.0 ficam fora do escopo (mesmo código do erro 0x8F que eles devolvem).
        return Err(HidppError::Hidpp10(0x01));
    }

    let mut features = FeatureMap::with_capacity(id::QUERIED.len() + 1);
    features.insert(id::ROOT, 0x00, 0);
    for fid in id::QUERIED {
        let p = call(io, index, 0x00, ROOT_GET_FEATURE, &fid.to_be_bytes(), pending)?;
        if p[0] != 0 {
            features.insert(fid, p[0], p[2]);
        }
    }

    // resultados.txt: nome_0005_g602 = "Wireless Gaming Mouse" (genérico) → em receptor, o codinome
    // vem primeiro (MX: igual ao DEVICE_NAME, G602: "G602"); DEVICE_NAME é a reserva.
    let codename = receiver_codename(io, index, codename, pending)?;
    let name = match codename {
        Some(n) => n,
        None => device_name(io, index, &features, pending)?.unwrap_or_default(),
    };

    let backlight_levels = read_feature(io, index, &features, id::BACKLIGHT2, 2, &[], pending)?.map(|p| p[0]);
    let hosts = read_feature(io, index, &features, id::CHANGE_HOST, 0, &[], pending)?.map(|p| (p[0], p[1]));
    let battery = if features.index_of(id::UNIFIED_BATTERY).is_some() {
        BatteryKind::Unified
    } else if features.index_of(id::BATTERY_STATUS).is_some() {
        BatteryKind::Status
    } else {
        BatteryKind::None
    };
    // Uma leitura serve às duas perguntas: a CID existe (respondeu sem erro) e já está desviada?
    let emoji = read_feature(io, index, &features, id::REPROG_V4, REPROG_GET_REPORTING, &cid::EMOJI.to_be_bytes(), pending)?;
    let emoji_divertable = emoji.is_some();
    let emoji_diverted = emoji.is_some_and(|p| p[2] & FLAG_DIVERTED != 0);

    Ok(DeviceInfo { index, wpid, name, features, battery, backlight_levels, hosts, emoji_divertable, emoji_diverted })
}


/// Codinome guardado no receptor: uma leitura do 0x2B5 no layout do receptor. Recusa (erro 1.0)
/// ou texto ilegível → `None` (o chamador cai no DEVICE_NAME); timeout e E/S sobem.
fn receiver_codename(io: &mut impl HidIo, index: u8, kind: Codename, pending: &mut Vec<Frame>) -> Result<Option<String>, HidppError> {
    // `wrapping_*`: índice fora de 1..=6 só faz o receptor recusar, nunca estoura.
    let (req, len_at, text_at, max) = match kind {
        Codename::Bolt => (
            Frame::register_read(REG_RECEIVER_INFO, &[BOLT_CODENAME_BASE.wrapping_add(index), 0x01]),
            2, 3, BOLT_CODENAME_MAX,
        ),
        Codename::Unifying => (
            Frame::register_read(REG_RECEIVER_INFO, &[UNIFYING_CODENAME_BASE.wrapping_add(index).wrapping_sub(1)]),
            1, 2, UNIFYING_CODENAME_MAX,
        ),
        Codename::None => return Ok(None),
    };
    let p = match request(io, &req, pending) {
        Ok(p) => p,
        Err(HidppError::Hidpp10(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = usize::from(p[len_at]).min(max);
    Ok(printable(&p[text_at..text_at + len]))
}

/// Texto ASCII imprimível sem os NULs finais; vazio ou com lixo → `None`.
fn printable(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let text = &bytes[..end];
    if text.is_empty() || !text.iter().all(|b| (0x20..=0x7E).contains(b)) {
        return None;
    }
    Some(text.iter().map(|&b| char::from(b)).collect())
}

/// DEVICE_NAME: fn0 → `[len]`, fn1 `[offset]` → até 16 bytes do nome.
fn device_name(io: &mut impl HidIo, dev: u8, features: &FeatureMap, pending: &mut Vec<Frame>) -> Result<Option<String>, HidppError> {
    let Some(index) = features.index_of(id::DEVICE_NAME) else { return Ok(None) };
    let Some(head) = optional(call(io, dev, index, 0, &[], pending))? else { return Ok(None) };
    let len = usize::from(head[0]);
    let mut raw = Vec::with_capacity(len);
    while raw.len() < len {
        let Some(chunk) = optional(call(io, dev, index, 1, &[raw.len() as u8], pending))? else { break };
        let take = (len - raw.len()).min(NAME_CHUNK);
        raw.extend_from_slice(&chunk[..take]);
    }
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Ok(Some(String::from_utf8_lossy(&raw[..end]).trim().to_string()))
}

pub fn read_battery(io: &mut impl HidIo, dev: &DeviceInfo, pending: &mut Vec<Frame>) -> Result<Option<BatteryReading>, HidppError> {
    let f = &dev.features;
    Ok(match dev.battery {
        BatteryKind::Unified => read_feature(io, dev.index, f, id::UNIFIED_BATTERY, 1, &[], pending)?
            .and_then(|p| reading_from_unified(p[0], p[1], p[2])),
        BatteryKind::Status => read_feature(io, dev.index, f, id::BATTERY_STATUS, 0, &[], pending)?
            .and_then(|p| reading_from_status(p[0], p[2])),
        BatteryKind::None => None,
    })
}

pub fn read_locks(io: &mut impl HidIo, dev: &DeviceInfo, pending: &mut Vec<Frame>) -> Result<Option<LockState>, HidppError> {
    Ok(read_feature(io, dev.index, &dev.features, id::LOCK_KEY_STATE, 0, &[], pending)?.map(|p| LockState::from_mask(p[0])))
}

/// `Some(true)` = F1..F12 dão a função de mídia sem Fn (`fn_inv_semantica`: estado 1).
pub fn read_fn_inversion(io: &mut impl HidIo, dev: &DeviceInfo, pending: &mut Vec<Frame>) -> Result<Option<bool>, HidppError> {
    if dev.features.index_of(id::FN_INVERSION_K375S).is_none() {
        return Ok(None);
    }
    let prefix = read_feature(io, dev.index, &dev.features, id::HOSTS_INFO, 0, &[], pending)?
        .map_or(NO_HOST_PREFIX, |p| p[3]);
    Ok(read_feature(io, dev.index, &dev.features, id::FN_INVERSION_K375S, 0, &[prefix], pending)?.map(|p| p[1] != 0))
}

/// Desvia a tecla Emoji para HID++ (o desvio se perde a cada reconexão). Usa o estado lido em
/// `discover` (`emoji_diverted`), sem reler. `Ok(true)` = já estava ou ficou desviada;
/// `Ok(false)` = sem REPROG V4/CID, ou o eco do dispositivo não confirmou.
pub fn divert_emoji(io: &mut impl HidIo, dev: &DeviceInfo, pending: &mut Vec<Frame>) -> Result<bool, HidppError> {
    let Some(index) = dev.features.index_of(id::REPROG_V4) else { return Ok(false) };
    if !dev.emoji_divertable {
        return Ok(false);
    }
    if dev.emoji_diverted {
        return Ok(true);
    }
    let [hi, lo] = cid::EMOJI.to_be_bytes();
    let echo = call(io, dev.index, index, REPROG_SET_REPORTING, &[hi, lo, DIVERT_ON, 0x00, 0x00], pending)?;
    Ok(echo[0] == hi && echo[1] == lo && echo[2] & FLAG_DIVERTED != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::{BatteryReading, ChargeStatus};
    use crate::hidpp::fake::FakeDevice;
    use crate::hidpp::frame::LONG_LEN;

    /// Dispositivo simulado a partir de `info.txt` / `resultados.txt`.
    #[derive(Clone)]
    struct Spec {
        index: u8,
        /// `None` = dispositivo HID++ 1.0 (ping responde erro 0x8F/0x01).
        version: Option<(u8, u8)>,
        features: Vec<(u16, u8)>,
        name: &'static str,
        /// Resposta do receptor à leitura 0x83 0xB5 com params[0] = chave.
        codename: Vec<(u8, Vec<u8>)>,
        /// Tipo de receptor passado a `discover`.
        kind: Codename,
        /// (índice da feature, função) → payload.
        replies: Vec<(u8, u8, Vec<u8>)>,
    }

    fn long(dev: u8, sub: u8, addr: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x11, dev, sub, addr];
        v.extend_from_slice(payload);
        v.resize(LONG_LEN, 0);
        v
    }

    fn device(spec: Spec) -> FakeDevice {
        FakeDevice::new(move |f| {
            let p = &f.params;
            if f.device == 0xFF && f.sub_id == 0x83 && f.address == 0xB5 {
                return match spec.codename.iter().find(|c| c.0 == p[0]) {
                    Some((_, b)) if b.is_empty() => vec![], // receptor mudo
                    Some((_, b)) => vec![long(0xFF, 0x83, 0xB5, b)],
                    None => vec![vec![0x10, 0xFF, 0x8F, 0x83, 0xB5, 0x03, 0x00]],
                };
            }
            if f.device != spec.index {
                return vec![];
            }
            let function = f.address >> 4;
            if f.sub_id == 0x00 {
                return match (function, spec.version) {
                    (1, Some((maj, min))) => vec![long(f.device, 0, f.address, &[maj, min, p[2]])],
                    (1, None) => vec![vec![0x10, f.device, 0x8F, 0x00, f.address, 0x01, 0x00]],
                    (0, _) => {
                        let fid = u16::from(p[0]) << 8 | u16::from(p[1]);
                        let idx = spec.features.iter().find(|e| e.0 == fid).map_or(0, |e| e.1);
                        vec![long(f.device, 0, f.address, &[idx, 0, if idx == 0 { 0 } else { 1 }])]
                    }
                    _ => vec![long(f.device, 0xFF, 0x00, &[f.address, 0x07])],
                };
            }
            let name_idx = spec.features.iter().find(|e| e.0 == id::DEVICE_NAME).map(|e| e.1);
            if Some(f.sub_id) == name_idx {
                let name = spec.name.as_bytes();
                return match function {
                    0 => vec![long(f.device, f.sub_id, f.address, &[name.len() as u8])],
                    1 => {
                        let off = usize::from(p[0]).min(name.len());
                        let end = (off + 16).min(name.len());
                        vec![long(f.device, f.sub_id, f.address, &name[off..end])]
                    }
                    _ => vec![long(f.device, 0xFF, f.sub_id, &[f.address, 0x07])],
                };
            }
            match spec.replies.iter().find(|r| r.0 == f.sub_id && r.1 == function) {
                Some((_, _, payload)) => vec![long(f.device, f.sub_id, f.address, payload)],
                None => vec![long(f.device, 0xFF, f.sub_id, &[f.address, 0x07])],
            }
        })
    }

    /// MX Keys Mini no c548, dev 1 (`indices_mx`, respostas de `info.txt`).
    fn mx() -> Spec {
        Spec {
            index: 1,
            version: Some((4, 5)),
            features: vec![
                (0x0001, 0x01), (0x0005, 0x03), (0x1d4b, 0x04), (0x1004, 0x07), (0x1b04, 0x08),
                (0x1814, 0x09), (0x1815, 0x0a), (0x1982, 0x0b), (0x40a3, 0x0c), (0x4220, 0x0d), (0x4521, 0x0e),
            ],
            name: "MX Keys Mini",
            kind: Codename::Bolt,
            codename: vec![(0x61, vec![0x61, 0x01, 0x0c, 0x4d, 0x58, 0x20, 0x4b, 0x65, 0x79, 0x73, 0x20, 0x4d, 0x69, 0x6e, 0x69, 0x00])],
            replies: vec![
                (0x0b, 2, vec![0x08, 0x02, 0x02, 0x00, 0x03, 0x00, 0x06, 0x00, 0x3c]),
                (0x09, 0, vec![0x03, 0x00]),
                (0x0a, 0, vec![0x13, 0x08, 0x03, 0x00]),
                (0x0c, 0, vec![0x00, 0x00, 0x01, 0x01]),
                (0x08, 2, vec![0x01, 0x08, 0x00, 0x00, 0x00]),
                (0x08, 3, vec![0x01, 0x08, 0x01, 0x00, 0x00]),
                (0x07, 1, vec![0x5a, 0x08, 0x00, 0x00]),
                (0x0d, 0, vec![0x00]),
            ],
        }
    }

    /// G602 no c537, dev 1 (`indices_g602`): Unifying responde o codinome em 0x40, não em 0x61.
    fn g602() -> Spec {
        Spec {
            index: 1,
            version: Some((2, 0)),
            features: vec![(0x0001, 0x01), (0x0005, 0x03), (0x1000, 0x05), (0x1d4b, 0x06)],
            name: "Wireless Gaming Mouse",
            kind: Codename::Unifying,
            codename: vec![(0x40, vec![0x40, 0x04, 0x47, 0x36, 0x30, 0x32, 0x00])],
            replies: vec![(0x05, 0, vec![0x32, 0x14, 0x00])],
        }
    }

    fn enc(f: &Frame) -> Vec<u8> { let mut b = [0u8; LONG_LEN]; let n = f.encode(&mut b); b[..n].to_vec() }

    fn discovered(spec: Spec) -> (FakeDevice, DeviceInfo) {
        let (index, kind) = (spec.index, spec.kind);
        let mut io = device(spec);
        let mut pending = Vec::new();
        let info = discover(&mut io, index, 0xB369, kind, &mut pending).unwrap();
        io.written.clear();
        (io, info)
    }

    #[test] fn discover_mx_keys_mini_from_probe() {
        let (_, d) = discovered(mx());
        assert_eq!((d.index, d.wpid), (1, 0xB369));
        assert_eq!(d.name, "MX Keys Mini"); // nome_0005_mx = codinome_mx
        assert_eq!(d.battery, BatteryKind::Unified);
        assert_eq!(d.backlight_levels, Some(8)); // backlight_niveis
        assert_eq!(d.hosts, Some((3, 0)));
        assert!(d.emoji_divertable);
        assert!(!d.emoji_diverted); // info.txt: getCidReporting(0x0108) flags 0x00
        assert_eq!(d.features.index_of(id::UNIFIED_BATTERY), Some(0x07));
        assert_eq!(d.features.index_of(id::REPROG_V4), Some(0x08));
        assert_eq!(d.features.index_of(id::CHANGE_HOST), Some(0x09));
        assert_eq!(d.features.index_of(id::HOSTS_INFO), Some(0x0a));
        assert_eq!(d.features.index_of(id::BACKLIGHT2), Some(0x0b));
        assert_eq!(d.features.index_of(id::FN_INVERSION_K375S), Some(0x0c));
        assert_eq!(d.features.id_at(0x0d), Some(id::LOCK_KEY_STATE));
        assert_eq!(d.features.index_of(id::BATTERY_STATUS), None);
    }

    #[test] fn discover_g602_from_probe() {
        let mut io = device(g602());
        let mut pending = Vec::new();
        let d = discover(&mut io, 1, 0x402C, Codename::Unifying, &mut pending).unwrap();
        let codename_reads: Vec<_> = io.written.iter().filter(|w| w[2] == 0x83).collect();
        assert_eq!(codename_reads, vec![&enc(&Frame::register_read(0x2B5, &[0x40]))], "uma leitura, layout Unifying, sem 0x61");
        // nome_0005_g602 = "Wireless Gaming Mouse" não é nome de modelo → codinome do receptor.
        assert_eq!(d.name, "G602");
        assert_eq!(d.battery, BatteryKind::Status);
        assert_eq!(d.backlight_levels, None);
        assert_eq!(d.hosts, None);
        assert!(!d.emoji_divertable);
        assert!(!d.emoji_diverted);
        assert_eq!(d.features.index_of(id::BATTERY_STATUS), Some(0x05));
    }

    #[test] fn bolt_codename_is_one_read() {
        let mut io = device(mx());
        let mut pending = Vec::new();
        discover(&mut io, 1, 0xB369, Codename::Bolt, &mut pending).unwrap();
        let codename_reads: Vec<_> = io.written.iter().filter(|w| w[2] == 0x83).collect();
        assert_eq!(codename_reads, vec![&enc(&Frame::register_read(0x2B5, &[0x61, 0x01]))]);
    }

    #[test] fn silent_receiver_on_codename_is_an_error() {
        // Receptor que não responde ao 0x2B5 não é mascarado: o timeout sobe.
        let mut spec = g602();
        spec.codename = vec![(0x40, vec![])];
        let mut io = device(spec);
        let mut pending = Vec::new();
        assert_eq!(discover(&mut io, 1, 0x402C, Codename::Unifying, &mut pending).unwrap_err(), HidppError::Timeout);
    }

    #[test] fn discover_queries_every_feature_but_root() {
        let mut io = device(mx());
        let mut pending = Vec::new();
        discover(&mut io, 1, 0xB369, Codename::Bolt, &mut pending).unwrap();
        for fid in id::QUERIED {
            let [hi, lo] = fid.to_be_bytes();
            assert!(io.written.contains(&enc(&Frame::feature_request(1, 0, 0, &[hi, lo]))), "faltou getFeature {fid:#06x}");
        }
        assert!(!io.written.contains(&enc(&Frame::feature_request(1, 0, 0, &[0, 0]))));
    }

    #[test] fn codename_skips_device_name_reads() {
        let mut io = device(mx());
        let mut pending = Vec::new();
        discover(&mut io, 1, 0xB369, Codename::Bolt, &mut pending).unwrap();
        assert!(io.written.iter().all(|w| w[2] != 0x03), "DEVICE_NAME lido sem necessidade");
    }

    #[test] fn direct_device_uses_device_name() {
        let mut spec = mx();
        spec.index = 0xFF;
        spec.codename.clear();
        let mut io = device(spec);
        let mut pending = Vec::new();
        let d = discover(&mut io, 0xFF, 0xB369, Codename::None, &mut pending).unwrap();
        assert_eq!(d.name, "MX Keys Mini");
        assert!(io.written.iter().all(|w| w[2] != 0x83), "nó direto não tem receptor para codinome");
    }

    #[test] fn receiver_without_codename_falls_back_to_device_name() {
        let mut spec = g602();
        spec.codename.clear();
        let (_, d) = discovered(spec);
        assert_eq!(d.name, "Wireless Gaming Mouse");
    }

    #[test] fn long_device_name_is_read_in_chunks() {
        let mut spec = g602();
        spec.codename.clear();
        spec.name = "Nome Bem Comprido de Dispositivo X";
        let (_, d) = discovered(spec);
        assert_eq!(d.name, "Nome Bem Comprido de Dispositivo X");
    }

    #[test] fn no_name_anywhere_is_empty() {
        let mut spec = g602();
        spec.codename.clear();
        spec.features.retain(|e| e.0 != id::DEVICE_NAME);
        let (_, d) = discovered(spec);
        assert_eq!(d.name, "");
    }

    #[test] fn hidpp10_device_is_rejected() {
        let mut spec = g602();
        spec.version = None;
        let mut io = device(spec);
        let mut pending = Vec::new();
        assert_eq!(discover(&mut io, 1, 0x402C, Codename::Unifying, &mut pending).unwrap_err(), HidppError::Hidpp10(0x01));
        assert_eq!(io.written.len(), 1);
    }

    #[test] fn version_below_2_is_rejected() {
        let mut spec = g602();
        spec.version = Some((1, 0));
        let mut io = device(spec);
        let mut pending = Vec::new();
        assert_eq!(discover(&mut io, 1, 0x402C, Codename::Unifying, &mut pending).unwrap_err(), HidppError::Hidpp10(0x01));
    }

    #[test] fn silent_device_times_out() {
        let mut spec = g602();
        spec.index = 2; // ninguém responde no dev 1
        let mut io = device(spec);
        let mut pending = Vec::new();
        assert_eq!(discover(&mut io, 1, 0x402C, Codename::Unifying, &mut pending).unwrap_err(), HidppError::Timeout);
    }

    #[test] fn battery_reads_from_probe() {
        let mut pending = Vec::new();
        let (mut io, d) = discovered(mx());
        // UNIFIED_BATTERY fn1 = 5a 08 00 00 → 90 %, descarregando.
        assert_eq!(read_battery(&mut io, &d, &mut pending), Ok(Some(BatteryReading { percent: 90, status: ChargeStatus::Discharging })));
        assert_eq!(io.written, vec![enc(&Frame::feature_request(1, 0x07, 1, &[]))]);
        let (mut io, d) = discovered(g602());
        // BATTERY_STATUS fn0 = 32 14 00 → 50 %, descarregando.
        assert_eq!(read_battery(&mut io, &d, &mut pending), Ok(Some(BatteryReading { percent: 50, status: ChargeStatus::Discharging })));
        assert_eq!(io.written, vec![enc(&Frame::feature_request(1, 0x05, 0, &[]))]);
    }

    #[test] fn locks_from_probe() {
        let mut pending = Vec::new();
        let (mut io, d) = discovered(mx());
        assert_eq!(read_locks(&mut io, &d, &mut pending), Ok(Some(LockState { caps: Some(false), num: Some(false), insert: None })));
        let (mut io, d) = discovered(g602());
        assert_eq!(read_locks(&mut io, &d, &mut pending), Ok(None));
        assert_eq!(read_fn_inversion(&mut io, &d, &mut pending), Ok(None));
        assert!(io.written.is_empty(), "feature ausente não gera requisição");
    }

    #[test] fn fn_inversion_uses_hosts_info_prefix() {
        let mut spec = mx();
        for r in spec.replies.iter_mut().filter(|r| r.0 == 0x0c) {
            r.2 = vec![0x00, 0x01, 0x01, 0x01];
        }
        let (mut io, d) = discovered(spec);
        let mut pending = Vec::new();
        // fn_inv_semantica: estado 1 = F1..F12 dão mídia sem Fn.
        assert_eq!(read_fn_inversion(&mut io, &d, &mut pending), Ok(Some(true)));
        assert_eq!(io.written, vec![
            enc(&Frame::feature_request(1, 0x0a, 0, &[])),
            enc(&Frame::feature_request(1, 0x0c, 0, &[0x00])), // hosts_info_byte3 = 0x00
        ]);
        // Valor real da sonda: 40A3 fn0 [0x00] = 00 00 01 01 → estado 0.
        let (mut io, d) = discovered(mx());
        assert_eq!(read_fn_inversion(&mut io, &d, &mut pending), Ok(Some(false)));
    }

    #[test] fn fn_inversion_without_hosts_info_uses_ff() {
        let mut spec = mx();
        spec.features.retain(|e| e.0 != id::HOSTS_INFO);
        let (mut io, d) = discovered(spec);
        let mut pending = Vec::new();
        assert_eq!(read_fn_inversion(&mut io, &d, &mut pending), Ok(Some(false)));
        assert_eq!(io.written, vec![enc(&Frame::feature_request(1, 0x0c, 0, &[0xFF]))]);
    }

    #[test] fn divert_emoji_skips_write_when_already_diverted() {
        let mut spec = mx();
        for r in spec.replies.iter_mut().filter(|r| (r.0, r.1) == (0x08, 2)) {
            r.2 = vec![0x01, 0x08, 0x01, 0x00, 0x00];
        }
        let (mut io, d) = discovered(spec);
        let mut pending = Vec::new();
        assert_eq!(divert_emoji(&mut io, &d, &mut pending), Ok(true));
        assert!(d.emoji_diverted);
        assert!(io.written.is_empty(), "estado já lido na descoberta: nem fn2 de novo, nem fn3");
    }

    #[test] fn divert_emoji_writes_exact_bytes() {
        let (mut io, d) = discovered(mx());
        let mut pending = Vec::new();
        assert_eq!(divert_emoji(&mut io, &d, &mut pending), Ok(true));
        assert_eq!(io.written.len(), 1, "só o setCidReporting; o getCidReporting foi feito na descoberta");
        assert_eq!(io.written.last(), Some(&enc(&Frame::feature_request(1, 0x08, 3, &[0x01, 0x08, 0x03, 0x00, 0x00]))));
    }

    #[test] fn divert_emoji_reports_ineffective_echo() {
        let mut spec = mx();
        for r in spec.replies.iter_mut().filter(|r| (r.0, r.1) == (0x08, 3)) {
            r.2 = vec![0x01, 0x08, 0x00, 0x00, 0x00];
        }
        let (mut io, d) = discovered(spec);
        let mut pending = Vec::new();
        assert_eq!(divert_emoji(&mut io, &d, &mut pending), Ok(false));
    }

    #[test] fn divert_emoji_without_reprog_does_nothing() {
        let (mut io, d) = discovered(g602());
        let mut pending = Vec::new();
        assert_eq!(divert_emoji(&mut io, &d, &mut pending), Ok(false));
        assert!(io.written.is_empty());
    }
}
