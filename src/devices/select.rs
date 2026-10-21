//! Seleção pura de nós hidraw: quais falam HID++ e se são receptor ou dispositivo direto.

pub use crate::hidpp::receiver::RECEIVER_PIDS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind { Receiver, Direct }

pub const VENDOR_LOGITECH: u16 = 0x046D;
pub const BUS_USB: u16 = 0x0003;
pub const BUS_BLUETOOTH: u16 = 0x0005;

const REPORT_ID_SHORT: u32 = 0x10;
const REPORT_ID_LONG: u32 = 0x11;
/// Bits de entrada (sem o byte de report ID) dos relatórios HID++ curto (6 bytes) e longo (19).
const SHORT_INPUT_BITS: u32 = 6 * 8;
const LONG_INPUT_BITS: u32 = 19 * 8;
/// Profundidade máxima de Push (0xA4) seguida; descritores reais usam 0 ou 1.
const GLOBAL_STACK_MAX: usize = 8;
/// Prefixo de item longo.
const LONG_ITEM: u8 = 0xFE;
/// Tag+tipo (prefixo sem os 2 bits de tamanho) dos itens que interessam.
const ITEM_INPUT: u8 = 0x80;
const ITEM_REPORT_SIZE: u8 = 0x74;
const ITEM_REPORT_ID: u8 = 0x84;
const ITEM_REPORT_COUNT: u8 = 0x94;
const ITEM_PUSH: u8 = 0xA4;
const ITEM_POP: u8 = 0xB4;

/// `"0003:0000046D:0000C548"` → `(bus, vendor, product)`.
pub fn parse_hid_id(s: &str) -> Option<(u16, u16, u16)> {
    let mut it = s.split(':');
    let mut field = || -> Option<u16> {
        let v = u32::from_str_radix(it.next()?, 16).ok()?;
        u16::try_from(v).ok()
    };
    let parsed = (field()?, field()?, field()?);
    it.next().is_none().then_some(parsed)
}

#[derive(Clone, Copy, Default)]
struct Globals { id: u32, size: u32, count: u32 }

/// Como o Solaar (`hidapi/udev_impl.py`): input report 0x10 com 6 bytes OU 0x11 com 19 bytes;
/// não olha usage page (BT usa 0xFF43). Percorre os itens curtos somando `size * count` bits de
/// Input por report ID; Push/Pop salvam/restauram os globais; itens longos são pulados.
/// Descritor truncado: o que foi lido até o corte vale, o resto é ignorado.
pub fn is_hidpp_descriptor(desc: &[u8]) -> bool {
    let mut g = Globals::default();
    let mut stack = [Globals::default(); GLOBAL_STACK_MAX];
    let mut depth = 0usize;
    let (mut short_bits, mut long_bits) = (0u32, 0u32);
    let mut i = 0usize;
    while let Some(&prefix) = desc.get(i) {
        if prefix == LONG_ITEM {
            let Some(&len) = desc.get(i + 1) else { break };
            i += 3 + usize::from(len);
            continue;
        }
        let n = [0usize, 1, 2, 4][usize::from(prefix & 0x03)];
        let Some(data) = desc.get(i + 1..i + 1 + n) else { break };
        let value = data.iter().rev().fold(0u32, |acc, &b| acc << 8 | u32::from(b));
        match prefix & 0xFC {
            ITEM_REPORT_ID => g.id = value,
            ITEM_REPORT_SIZE => g.size = value,
            ITEM_REPORT_COUNT => g.count = value,
            ITEM_PUSH if depth < GLOBAL_STACK_MAX => {
                stack[depth] = g;
                depth += 1;
            }
            ITEM_POP if depth > 0 => {
                depth -= 1;
                g = stack[depth];
            }
            ITEM_INPUT => {
                let bits = g.size.saturating_mul(g.count);
                match g.id {
                    REPORT_ID_SHORT => short_bits = short_bits.saturating_add(bits),
                    REPORT_ID_LONG => long_bits = long_bits.saturating_add(bits),
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1 + n;
    }
    short_bits == SHORT_INPUT_BITS || long_bits == LONG_INPUT_BITS
}

/// Regra: outro vendor → `None`; sem HID++ no descritor → `None`; filho DJ (USB cujo pai `hid` tem um
/// pai `hid`, o receptor, que já entrega os frames dele) → `None`; USB com PID de
/// receptor → `Receiver`; outro USB (cabo) ou Bluetooth → `Direct`; outro barramento → `None`.
pub fn select(bus: u16, vendor: u16, product: u16, is_dj_child: bool, desc: &[u8]) -> Option<NodeKind> {
    if vendor != VENDOR_LOGITECH || !is_hidpp_descriptor(desc) {
        return None;
    }
    match bus {
        BUS_USB if is_dj_child => None,
        BUS_USB if RECEIVER_PIDS.contains(&product) => Some(NodeKind::Receiver),
        BUS_USB | BUS_BLUETOOTH => Some(NodeKind::Direct),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C548: &[u8] = include_bytes!("../../tests/fixtures/descriptors/c548_hidpp.bin");
    const C537: &[u8] = include_bytes!("../../tests/fixtures/descriptors/c537_receiver.bin");
    const G602: &[u8] = include_bytes!("../../tests/fixtures/descriptors/g602_dj_child.bin");
    const MOUSE: &[u8] = include_bytes!("../../tests/fixtures/descriptors/c537_mouse.bin");
    /// Sintético: não há nó BT nesta máquina (`resultados.txt: bt_direto = nao`). Página 0xFF43,
    /// relatórios 0x10 (6 bytes) e 0x11 (19 bytes), como os teclados BT da Logitech.
    const BT: &[u8] = include_bytes!("../../tests/fixtures/descriptors/bt_hidpp.bin");

    #[test] fn hid_id_parses() {
        assert_eq!(parse_hid_id("0003:0000046D:0000C548"), Some((3, 0x046D, 0xC548)));
        assert_eq!(parse_hid_id("0005:0000046D:0000B369"), Some((5, 0x046D, 0xB369)));
        assert_eq!(parse_hid_id("lixo"), None);
        assert_eq!(parse_hid_id("0003:0000046D"), None);
        assert_eq!(parse_hid_id("0003:0000046D:0000C548:0"), None);
        assert_eq!(parse_hid_id("0003:0001046D:0000C548"), None, "vendor não cabe em u16");
    }

    #[test] fn hidpp_descriptor_detected() {
        assert!(is_hidpp_descriptor(C548));
        assert!(is_hidpp_descriptor(C537));
        assert!(!is_hidpp_descriptor(MOUSE));
    }

    #[test] fn bolt_receiver_selected() { assert_eq!(select(3, 0x046D, 0xC548, false, C548), Some(NodeKind::Receiver)); }
    #[test] fn dj_receiver_selected() { assert_eq!(select(3, 0x046D, 0xC537, false, C537), Some(NodeKind::Receiver)); }
    #[test] fn dj_child_node_is_ignored() {
        // O filho do G602 também tem 0x10/0x11 no descritor: só a posição na árvore (neto de um hid) o distingue.
        assert!(is_hidpp_descriptor(G602));
        assert_eq!(select(3, 0x046D, 0x402C, true, G602), None);
    }
    #[test] fn wired_device_with_dj_driver_but_no_hid_grandparent_is_direct() {
        // Mesmo driver `logitech-hidpp-device`, mas ligado direto no USB: não é filho DJ.
        assert_eq!(select(3, 0x046D, 0xC080, false, G602), Some(NodeKind::Direct));
    }
    #[test] fn non_hidpp_interface_of_receiver_is_ignored() { assert_eq!(select(3, 0x046D, 0xC537, false, MOUSE), None); }
    #[test] fn bluetooth_device_is_direct() {
        assert!(is_hidpp_descriptor(BT));
        assert_eq!(select(5, 0x046D, 0xB369, false, BT), Some(NodeKind::Direct));
    }
    #[test] fn usb_cable_device_is_direct() { assert_eq!(select(3, 0x046D, 0xC33F, false, C548), Some(NodeKind::Direct)); }
    #[test] fn other_bus_ignored() { assert_eq!(select(0x18, 0x046D, 0xB369, false, BT), None); }

    #[test] fn hidpp_detection_ignores_usage_page() {
        let short_ok = [0x06, 0x43, 0xFF, 0x85, 0x10, 0x75, 0x08, 0x95, 0x06, 0x81, 0x00];
        assert!(is_hidpp_descriptor(&short_ok));
        let short_bad = [0x06, 0x43, 0xFF, 0x85, 0x10, 0x75, 0x08, 0x95, 0x05, 0x81, 0x00];
        assert!(!is_hidpp_descriptor(&short_bad));
        let long_ok = [0x85, 0x11, 0x75, 0x08, 0x95, 0x13, 0x81, 0x00];
        assert!(is_hidpp_descriptor(&long_ok));
    }

    #[test] fn output_report_does_not_count_as_input() {
        assert!(!is_hidpp_descriptor(&[0x85, 0x10, 0x75, 0x08, 0x95, 0x06, 0x91, 0x00]));
    }

    #[test] fn push_pop_restore_globals() {
        // Push salva (id 0x10, 8, 6); o item no meio muda para id 0x20; Pop volta e o Input conta em 0x10.
        let d = [0x85, 0x10, 0x75, 0x08, 0x95, 0x06, 0xA4, 0x85, 0x20, 0x95, 0x01, 0xB4, 0x81, 0x00];
        assert!(is_hidpp_descriptor(&d));
    }

    #[test] fn long_item_is_skipped() {
        // Item longo 0xFE com 2 bytes de dados (tag 0x00) carregando bytes que pareceriam itens curtos.
        let d = [0xFE, 0x02, 0x00, 0x85, 0x11, 0x85, 0x10, 0x75, 0x08, 0x95, 0x06, 0x81, 0x00];
        assert!(is_hidpp_descriptor(&d));
    }

    #[test] fn truncated_or_garbage_descriptor_is_not_hidpp_and_does_not_panic() {
        assert!(!is_hidpp_descriptor(&[]));
        assert!(!is_hidpp_descriptor(&[0x85, 0x10, 0x75, 0x08, 0x96, 0x06]));
        assert!(!is_hidpp_descriptor(&[0xFE]));
        assert!(!is_hidpp_descriptor(&[0xFE, 0xFF, 0x00]));
        assert!(!is_hidpp_descriptor(&[0xB4, 0xB4, 0x81, 0x00]));
        let deep_push = [0xA4u8; 64];
        assert!(!is_hidpp_descriptor(&deep_push));
        // Contagens enormes não estouram.
        assert!(!is_hidpp_descriptor(&[0x85, 0x10, 0x77, 0xFF, 0xFF, 0xFF, 0xFF, 0x97, 0xFF, 0xFF, 0xFF, 0xFF, 0x81, 0x00, 0x81, 0x00]));
    }

    #[test] fn other_vendor_ignored() { assert_eq!(select(3, 0x2DC8, 0x301C, false, C548), None); }
}
