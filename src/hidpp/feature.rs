//! IDs de features HID++ 2.0, CIDs de teclas e o mapa id ↔ índice de um dispositivo.

pub mod id {
    pub const ROOT: u16 = 0x0000;
    pub const FEATURE_SET: u16 = 0x0001;
    pub const DEVICE_NAME: u16 = 0x0005;
    pub const BATTERY_STATUS: u16 = 0x1000;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    pub const CHANGE_HOST: u16 = 0x1814;
    pub const HOSTS_INFO: u16 = 0x1815;
    pub const BACKLIGHT2: u16 = 0x1982;
    pub const REPROG_V4: u16 = 0x1B04;
    pub const FN_INVERSION_K375S: u16 = 0x40A3;
    pub const LOCK_KEY_STATE: u16 = 0x4220;

    /// Features consultadas por `getFeature` na descoberta (todas menos ROOT, que é sempre o índice 0).
    pub const QUERIED: [u16; 10] = [
        FEATURE_SET, DEVICE_NAME, BATTERY_STATUS, UNIFIED_BATTERY, CHANGE_HOST,
        HOSTS_INFO, BACKLIGHT2, REPROG_V4, FN_INVERSION_K375S, LOCK_KEY_STATE,
    ];
}

pub mod cid {
    pub const EMOJI: u16 = 0x0108;
}

/// Mapa das features presentes: (id, índice, versão). Poucas entradas → busca linear.
#[derive(Default, Clone, Debug)]
pub struct FeatureMap(Vec<(u16 /*id*/, u8 /*índice*/, u8 /*versão*/)>);

impl FeatureMap {
    /// Reserva espaço de uma vez (a descoberta sabe quantas features vai consultar).
    pub fn with_capacity(n: usize) -> Self { FeatureMap(Vec::with_capacity(n)) }

    pub fn index_of(&self, id: u16) -> Option<u8> {
        self.0.iter().find(|e| e.0 == id).map(|e| e.1)
    }

    pub fn id_at(&self, index: u8) -> Option<u16> {
        self.0.iter().find(|e| e.1 == index).map(|e| e.0)
    }

    /// Insere ou substitui a entrada do `id`.
    pub fn insert(&mut self, id: u16, index: u8, version: u8) {
        match self.0.iter_mut().find(|e| e.0 == id) {
            Some(e) => *e = (id, index, version),
            None => self.0.push((id, index, version)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn lookup_both_ways() {
        let mut m = FeatureMap::default();
        m.insert(id::UNIFIED_BATTERY, 0x07, 2);
        m.insert(id::LOCK_KEY_STATE, 0x0d, 0);
        assert_eq!(m.index_of(id::UNIFIED_BATTERY), Some(0x07));
        assert_eq!(m.id_at(0x0d), Some(id::LOCK_KEY_STATE));
        assert_eq!(m.index_of(id::BACKLIGHT2), None);
        assert_eq!(m.id_at(0x0b), None);
    }

    #[test] fn cids_from_probe() {
        // info.txt: CID da tecla Emoji do MX Keys Mini (as de luz −/+ e Ditado não são usadas).
        assert_eq!(cid::EMOJI, 0x0108);
    }

    #[test] fn insert_replaces_same_id() {
        let mut m = FeatureMap::default();
        m.insert(id::BACKLIGHT2, 0x0b, 3);
        m.insert(id::BACKLIGHT2, 0x0c, 3);
        assert_eq!(m.index_of(id::BACKLIGHT2), Some(0x0c));
        assert_eq!(m.id_at(0x0b), None);
    }
}
