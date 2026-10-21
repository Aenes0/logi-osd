//! Decodificação de eventos HID++ 2.0 (lock keys, backlight, bateria, Fn, teclas desviadas).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LockState { pub caps: Option<bool>, pub num: Option<bool>, pub insert: Option<bool> }

const LOCK_NUM: u8 = 0x01;
const LOCK_CAPS: u8 = 0x02;

impl LockState {
    /// LOCK_KEY_STATE (0x4220), `resultados.txt: lock_layout`: p0 bit1 = caps (observado),
    /// bit0 = num (convenção de LED HID, **nunca observado**: o MX Keys Mini não tem tecla Num Lock,
    /// `lock_num_notifica = sem_tecla`). O bit é decodificado, mas `num` deste teclado sempre vem
    /// `Some(false)`; silenciar ou não esse valor é decisão da T5/T7. Insert não é reportado → `None`.
    /// Mesmo layout no evento 0 e na fn0 (`lock_fn0_layout`).
    pub fn from_mask(mask: u8) -> LockState {
        LockState { caps: Some(mask & LOCK_CAPS != 0), num: Some(mask & LOCK_NUM != 0), insert: None }
    }
}

use crate::battery::{reading_from_status, reading_from_unified, BatteryReading};
use crate::hidpp::feature::id;

/// Evento de feature já decodificado. Todos os variantes são `Copy` (caminho quente sem alocação).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureEvent {
    Locks(LockState),
    /// `manual = Some(true)` sempre: ramo A (`resultados.txt: backlight_criterio`), todo evento
    /// BACKLIGHT2 recebido vem de tecla manual.
    Backlight { level: u8, manual: Option<bool> },
    Battery(BatteryReading),
    FnInversion { media_keys: bool },
    /// REPROG V4 evento 0: até 4 CIDs pressionadas (big-endian), 0 = vazio.
    DivertedKeys([u16; 4]),
}

fn cids(payload: &[u8]) -> Option<[u16; 4]> {
    let b = payload.get(..8)?;
    Some([
        u16::from_be_bytes([b[0], b[1]]),
        u16::from_be_bytes([b[2], b[3]]),
        u16::from_be_bytes([b[4], b[5]]),
        u16::from_be_bytes([b[6], b[7]]),
    ])
}

/// Decodifica o evento `event` da feature `feature` (id). Layouts conforme `resultados.txt`.
/// Qualquer combinação desconhecida ou payload curto demais → `None`.
pub fn decode(feature: u16, event: u8, payload: &[u8]) -> Option<FeatureEvent> {
    match (feature, event) {
        // lock_evento = 0, lock_layout: p0 = máscara.
        (id::LOCK_KEY_STATE, 0) => Some(FeatureEvent::Locks(LockState::from_mask(*payload.first()?))),
        // Solaar: level = p1; ramo A.
        (id::BACKLIGHT2, 0) => Some(FeatureEvent::Backlight { level: *payload.get(1)?, manual: Some(true) }),
        // unified_layout = soc, nível, status, ext.
        (id::UNIFIED_BATTERY, 0) => {
            reading_from_unified(*payload.first()?, *payload.get(1)?, *payload.get(2)?).map(FeatureEvent::Battery)
        }
        (id::BATTERY_STATUS, 0) => {
            reading_from_status(*payload.first()?, *payload.get(2)?).map(FeatureEvent::Battery)
        }
        // fn_inv_evento = 0, fn_inv_layout: p1 = estado; fn_inv_semantica: 1 = teclas de mídia sem Fn.
        (id::FN_INVERSION_K375S, 0) => Some(FeatureEvent::FnInversion { media_keys: *payload.get(1)? != 0 }),
        (id::REPROG_V4, 0) => cids(payload).map(FeatureEvent::DivertedKeys),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn lock_mask_layout_from_probe() {
        // caps.hex: 11 01 0d 00 02.. (ligado) / 11 01 0d 00 00.. (desligado).
        assert_eq!(LockState::from_mask(0x02), LockState { caps: Some(true), num: Some(false), insert: None });
        assert_eq!(LockState::from_mask(0x00), LockState { caps: Some(false), num: Some(false), insert: None });
        assert_eq!(LockState::from_mask(0x03).num, Some(true));
    }

    use crate::battery::ChargeStatus;
    use crate::hidpp::frame::{load_fixture, Frame, FrameKind};

    /// Decodifica os eventos HID++ 2.0 do teclado (nó c548) da fixture que pertencem a `feature`,
    /// cujo índice vem de `resultados.txt: indices_mx`.
    fn events_of(text: &str, feature: u16, index: u8) -> Vec<FeatureEvent> {
        load_fixture(text)
            .into_iter()
            .filter(|(node, _)| node == "c548")
            .filter_map(|(_, bytes)| Frame::parse(&bytes))
            .filter_map(|f| match f.kind() {
                FrameKind::Event20 { index: i, event } if i == index => decode(feature, event, f.payload()),
                _ => None,
            })
            .collect()
    }

    fn caps_seq(evs: &[FeatureEvent]) -> Vec<Option<bool>> {
        let mut out: Vec<Option<bool>> = Vec::new();
        for e in evs {
            if let FeatureEvent::Locks(l) = e && out.last() != Some(&l.caps) {
                out.push(l.caps);
            }
        }
        out
    }

    #[test] fn caps_fixture_alternates_on_off() {
        // indices_mx: 0x4220 -> 0x0d
        let evs = events_of(include_str!("../../tests/fixtures/probe/caps.hex"), id::LOCK_KEY_STATE, 0x0d);
        assert_eq!(evs.len(), 5);
        assert_eq!(caps_seq(&evs), [Some(true), Some(false), Some(true), Some(false), Some(true)]);
    }

    #[test]
    #[ignore = "resultados.txt: lock_num_notifica = sem_tecla"]
    fn num_fixture_reports_num() {
        // MX Keys Mini não tem Num Lock: nenhuma fixture exercita o bit 0 de verdade.
        let evs = events_of(include_str!("../../tests/fixtures/probe/num.hex"), id::LOCK_KEY_STATE, 0x0d);
        assert!(evs.iter().any(|e| matches!(e, FeatureEvent::Locks(LockState { num: Some(true), .. }))));
    }

    #[test]
    #[ignore = "resultados.txt: lock_insert_reportado = nao"]
    fn insert_fixture_reports_insert() {
        let evs = events_of(include_str!("../../tests/fixtures/probe/insert.hex"), id::LOCK_KEY_STATE, 0x0d);
        assert!(evs.iter().any(|e| matches!(e, FeatureEvent::Locks(LockState { insert: Some(true), .. }))));
    }

    #[test] fn fn_esc_fixture_toggles_media_keys() {
        // fn_inv_notifica = sim; dados do apêndice de fn-esc.hex (copiados da tela, t_ms sintéticos).
        // indices_mx: 0x40a3 -> 0x0c. Estado 1 = mídia sem Fn; 0 = F1..F12 padrão.
        let evs = events_of(include_str!("../../tests/fixtures/probe/fn-esc.hex"), id::FN_INVERSION_K375S, 0x0c);
        assert_eq!(evs, [FeatureEvent::FnInversion { media_keys: true }, FeatureEvent::FnInversion { media_keys: false }]);
    }

    #[test] fn backlight_manual_levels_follow_presses() {
        // indices_mx: 0x1982 -> 0x0b; backlight_niveis = 8. luz-manual.hex só capturou luz + (3->4->5);
        // luz - não foi capturada (resultados.txt: backlight_faixa_observada), então só a subida é provada.
        let evs = events_of(include_str!("../../tests/fixtures/probe/luz-manual.hex"), id::BACKLIGHT2, 0x0b);
        let levels: Vec<u8> = evs.iter().map(|e| match e { FeatureEvent::Backlight { level, .. } => *level, _ => 255 }).collect();
        assert_eq!(levels, [3, 4, 5]);
        assert!(levels.iter().all(|&l| l < 8));
    }

    #[test] fn backlight_manual_flag_branch_a() {
        // backlight_criterio (ramo A): todo evento BACKLIGHT2 é de tecla manual.
        let evs = events_of(include_str!("../../tests/fixtures/probe/luz-manual.hex"), id::BACKLIGHT2, 0x0b);
        assert_eq!(evs.len(), 3);
        assert!(evs.iter().all(|e| matches!(e, FeatureEvent::Backlight { manual: Some(true), .. })));
    }

    #[test]
    #[ignore = "resultados.txt: backlight_evento_auto = nada (luz-auto.hex tem 0 frames; o ramo A não notifica mudança automática)"]
    fn backlight_auto_flag_branch_a() {
        let evs = events_of(include_str!("../../tests/fixtures/probe/luz-auto.hex"), id::BACKLIGHT2, 0x0b);
        assert!(evs.iter().all(|e| matches!(e, FeatureEvent::Backlight { manual: Some(false), .. })));
    }

    #[test] fn unified_battery_fixture_shows_charging() {
        // indices_mx: 0x1004 -> 0x07. carga.hex: 5a 08 01 (carregando), depois 5a 08 00 (descarregando).
        let evs = events_of(include_str!("../../tests/fixtures/probe/carga.hex"), id::UNIFIED_BATTERY, 0x07);
        let charging = BatteryReading { percent: 90, status: ChargeStatus::Charging };
        let discharging = BatteryReading { percent: 90, status: ChargeStatus::Discharging };
        assert_eq!(evs, [FeatureEvent::Battery(charging), FeatureEvent::Battery(discharging)]);
    }

    #[test] fn battery_status_uses_level_and_status_bytes() {
        // BATTERY_STATUS (G602): p0 = nível, p2 = status (layout de reading_from_status).
        assert_eq!(
            decode(id::BATTERY_STATUS, 0, &[30, 0, 0]),
            Some(FeatureEvent::Battery(BatteryReading { percent: 30, status: ChargeStatus::Discharging }))
        );
        assert_eq!(decode(id::BATTERY_STATUS, 0, &[0, 0, 0]), None);
    }

    #[test] fn diverted_emoji_press_and_release() {
        // indices_mx: 0x1b04 -> 0x08. Só os dois primeiros pares (antes do Easy-Switch) são
        // do índice 0x08; a fixture inteira tem 4 eventos.
        let evs = events_of(include_str!("../../tests/fixtures/probe/emoji-desviado.hex"), id::REPROG_V4, 0x08);
        let press = FeatureEvent::DivertedKeys([0x0108, 0, 0, 0]);
        let release = FeatureEvent::DivertedKeys([0, 0, 0, 0]);
        assert_eq!(evs, [press, release, press, release]);
    }

    #[test] fn unknown_feature_is_none() {
        assert_eq!(decode(0x1234, 0, &[0; 16]), None);
        assert_eq!(decode(id::LOCK_KEY_STATE, 1, &[0; 16]), None);
    }

    #[test] fn short_payload_is_none_not_panic() {
        assert_eq!(decode(id::BACKLIGHT2, 0, &[8]), None);
        assert_eq!(decode(id::LOCK_KEY_STATE, 0, &[]), None);
        assert_eq!(decode(id::REPROG_V4, 0, &[1, 8, 0]), None);
    }
}
