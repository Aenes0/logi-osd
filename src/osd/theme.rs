//! Cores do tema do COSMIC (RON mínimo). Relido a cada aviso: sem inotify, sem polling.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme { pub dark: bool, pub background: Rgba, pub text: Rgba, pub accent: Rgba }

pub const WARNING: Rgba = Rgba(0xF0, 0xA3, 0x5E, 0xFF);
pub const CRITICAL: Rgba = Rgba(0xE5, 0x53, 0x4B, 0xFF);
pub const FALLBACK: Theme = Theme {
    dark: true,
    background: Rgba(0x1B, 0x1B, 0x1B, 0xFF),
    text: Rgba(0xF5, 0xF5, 0xF5, 0xFF),
    accent: Rgba(0x63, 0xD0, 0xDF, 0xFF),
};

/// `#RRGGBBAA` ou `#RRGGBB` (alfa FF). Sem `#` ou outro tamanho: `None`.
pub fn parse_hex(s: &str) -> Option<Rgba> {
    let digits = s.strip_prefix('#')?;
    if !digits.is_ascii() || !matches!(digits.len(), 6 | 8) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
    let alpha = if digits.len() == 8 { byte(6)? } else { 0xFF };
    Some(Rgba(byte(0)?, byte(2)?, byte(4)?, alpha))
}

/// Valor entre aspas do campo `key:` na profundidade 1 de parênteses (ignora structs aninhadas).
pub fn top_level_field<'a>(ron: &'a str, key: &str) -> Option<&'a str> {
    let bytes = ron.as_bytes();
    let (mut depth, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'"' => {
                let end = ron[i + 1..].find('"')? + i + 1;
                i = end;
            }
            c if depth == 1 && (c.is_ascii_alphabetic() || c == b'_') => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let ident = &ron[start..i];
                let rest = ron[i..].trim_start();
                if let Some(after) = rest.strip_prefix(':')
                    && let Some(value) = after.trim_start().strip_prefix('"')
                    && ident == key
                {
                    return value.split('"').next();
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Subdiretório `vN` de maior N numérico.
pub fn latest_version_dir(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let n: u32 = entry.file_name().to_str()?.strip_prefix('v')?.parse().ok()?;
            Some((n, entry.path()))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, path)| path)
}

fn read_color(dir: &Path, file: &str, key: &str) -> Option<Rgba> {
    let text = std::fs::read_to_string(dir.join(file)).ok()?;
    parse_hex(top_level_field(&text, key)?)
}

/// `cosmic_dir` = `~/.config/cosmic`. Qualquer falha cai no `FALLBACK`, campo a campo.
pub fn load(cosmic_dir: &Path) -> Theme {
    let dark = latest_version_dir(&cosmic_dir.join("com.system76.CosmicTheme.Mode"))
        .and_then(|dir| std::fs::read_to_string(dir.join("is_dark")).ok())
        .and_then(|text| text.trim().parse::<bool>().ok())
        .unwrap_or(FALLBACK.dark);
    let name = if dark { "com.system76.CosmicTheme.Dark" } else { "com.system76.CosmicTheme.Light" };
    let dir = latest_version_dir(&cosmic_dir.join(name));
    let color = |file: &str, key: &str, fallback: Rgba| {
        dir.as_deref().and_then(|d| read_color(d, file, key)).unwrap_or(fallback)
    };
    Theme {
        dark,
        background: color("background", "base", FALLBACK.background),
        text: color("background", "on", FALLBACK.text),
        accent: color("accent", "base", FALLBACK.accent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const DARK_ACCENT: &str = include_str!("../../tests/fixtures/theme/dark_accent");
    const DARK_BG: &str = include_str!("../../tests/fixtures/theme/dark_background");
    const LIGHT_ACCENT: &str = include_str!("../../tests/fixtures/theme/light_accent");

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("logi-osd-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test] fn hex_parses_rgba() { assert_eq!(parse_hex("#63D0DFFF"), Some(Rgba(0x63,0xD0,0xDF,0xFF))); assert_eq!(parse_hex("#FFFFFF33"), Some(Rgba(255,255,255,0x33))); assert_eq!(parse_hex("63D0DF"), None); }
    #[test] fn hex_rgb_defaults_alpha_and_rejects_garbage() { assert_eq!(parse_hex("#63D0DF"), Some(Rgba(0x63,0xD0,0xDF,0xFF))); assert_eq!(parse_hex("#GGGGGG"), None); assert_eq!(parse_hex("#é1234"), None); assert_eq!(parse_hex("#12345"), None); }
    #[test] fn accent_base_from_fixture() { assert_eq!(top_level_field(DARK_ACCENT, "base"), Some("#63D0DFFF")); }
    #[test] fn background_base_is_top_level_not_component() { assert_eq!(top_level_field(DARK_BG, "base"), Some("#1B1B1BFF")); }
    #[test] fn background_on_is_top_level_not_component() { assert_eq!(top_level_field(DARK_BG, "on"), Some("#F5F5F5FF")); }
    #[test] fn light_accent() { assert_eq!(top_level_field(LIGHT_ACCENT, "base"), Some("#00525AFF")); }
    #[test] fn nested_only_key_is_not_found() { assert_eq!(top_level_field(DARK_BG, "hover"), None); }

    #[test] fn picks_highest_version_dir() {
        let d = tempdir("ver");
        for v in ["v1", "v2", "v10", "vx"] { fs::create_dir(d.join(v)).unwrap(); }
        assert_eq!(latest_version_dir(&d), Some(d.join("v10")));
        let _ = fs::remove_dir_all(&d);
    }
    #[test] fn missing_dir_is_fallback() { assert_eq!(load(Path::new("/nao/existe")), FALLBACK); }
    #[test] fn light_mode_uses_light_theme() {
        let d = tempdir("light");
        fs::create_dir_all(d.join("com.system76.CosmicTheme.Mode/v1")).unwrap();
        fs::write(d.join("com.system76.CosmicTheme.Mode/v1/is_dark"), "false\n").unwrap();
        let light = d.join("com.system76.CosmicTheme.Light/v2");
        fs::create_dir_all(&light).unwrap();
        fs::write(light.join("accent"), LIGHT_ACCENT).unwrap();
        let t = load(&d);
        assert!(!t.dark);
        assert_eq!(t.accent, Rgba(0x00, 0x52, 0x5A, 0xFF));
        assert_eq!(t.background, FALLBACK.background); // arquivo ausente: campo a campo
        let _ = fs::remove_dir_all(&d);
    }
}
