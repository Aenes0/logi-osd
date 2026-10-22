//! Arquivo de configuração opcional: `chave = valor`, uma por linha.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub battery_low: u8,
    pub battery_critical: u8,
    pub duration: Duration,
    pub duration_critical: Duration,
    pub legacy_battery_interval: Duration,
    /// Invariante: `None` ou texto não vazio e aparado (é o único predicado de "há comando":
    /// `App`, o desvio da tecla e `main` usam `is_some()`).
    pub emoji_command: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            battery_low: 15,
            battery_critical: 5,
            duration: Duration::from_millis(1500),
            duration_critical: Duration::from_millis(4000),
            legacy_battery_interval: Duration::from_secs(15 * 60),
            emoji_command: None,
        }
    }
}

/// Lê o texto da configuração. Linhas inválidas mantêm o padrão e geram um aviso com o nº da linha.
pub fn parse(text: &str) -> (Config, Vec<String>) {
    let mut cfg = Config::default();
    let mut warnings = Vec::new();
    let mut low_line = None;
    let mut critical_line = None;

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!("linha {line_no}: esperado `chave = valor`"));
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match apply(&mut cfg, key, value) {
            Ok(()) if key == "bateria_baixa" => low_line = Some(line_no),
            Ok(()) if key == "bateria_critica" => critical_line = Some(line_no),
            Ok(()) => {}
            Err(reason) => warnings.push(format!("linha {line_no}: {reason}")),
        }
    }
    settle_battery_pair(&mut cfg, low_line, critical_line, &mut warnings);
    (cfg, warnings)
}

fn apply(cfg: &mut Config, key: &str, value: &str) -> Result<(), String> {
    match key {
        "bateria_baixa" => cfg.battery_low = in_range(value, 1, 99)? as u8,
        "bateria_critica" => cfg.battery_critical = in_range(value, 1, 99)? as u8,
        "duracao_ms" => cfg.duration = Duration::from_millis(in_range(value, 200, 20_000)?),
        "duracao_critica_ms" => cfg.duration_critical = Duration::from_millis(in_range(value, 200, 20_000)?),
        "intervalo_bateria_min" => cfg.legacy_battery_interval = Duration::from_secs(in_range(value, 1, 1440)? * 60),
        "comando_emoji" => cfg.emoji_command = non_empty(value),
        "fonte" => return Err("chave obsoleta \"fonte\" (o texto agora é desenhado pelo daemon de notificações)".into()),
        _ => return Err(format!("chave desconhecida `{key}`")),
    }
    Ok(())
}

fn in_range(value: &str, min: u64, max: u64) -> Result<u64, String> {
    let n: u64 = value.parse().map_err(|_| format!("valor inválido `{value}`"))?;
    if (min..=max).contains(&n) {
        Ok(n)
    } else {
        Err(format!("valor {n} fora da faixa {min}..={max}"))
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// Garante `bateria_critica < bateria_baixa`. Crítica explícita em conflito volta ao padrão;
/// se o padrão ainda não couber, a crítica desce para `baixa - 1`; baixa = 1 não tem espaço e volta ao padrão.
fn settle_battery_pair(cfg: &mut Config, low_line: Option<usize>, critical_line: Option<usize>, warnings: &mut Vec<String>) {
    if cfg.battery_critical < cfg.battery_low {
        return;
    }
    let defaults = Config::default();
    if let Some(line) = critical_line {
        warnings.push(format!(
            "linha {line}: bateria_critica ({}) deve ser menor que bateria_baixa ({}); usando o padrão",
            cfg.battery_critical, cfg.battery_low
        ));
        cfg.battery_critical = defaults.battery_critical;
    }
    if cfg.battery_critical < cfg.battery_low {
        return;
    }
    // Aqui a baixa veio do arquivo e é <= 5 (crítica padrão): a baixa é a única linha a apontar.
    let Some(line) = low_line else { return };
    if cfg.battery_low >= 2 {
        cfg.battery_critical = cfg.battery_low - 1;
        warnings.push(format!("linha {line}: bateria_critica ajustada para {} (abaixo de bateria_baixa)", cfg.battery_critical));
    } else {
        cfg.battery_low = defaults.battery_low;
        warnings.push(format!("linha {line}: bateria_baixa deve ser entre 2 e 99; usando o padrão"));
    }
}

/// Carrega a configuração. Arquivo ausente = padrões sem aviso; erros e avisos vão para stderr.
pub fn load() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let (cfg, warnings) = parse(&text);
            for warning in &warnings {
                eprintln!("logi-osd: {}: {warning}", path.display());
            }
            cfg
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(err) => {
            eprintln!("logi-osd: {}: {err}; usando os padrões", path.display());
            Config::default()
        }
    }
}

/// `$XDG_CONFIG_HOME/logi-osd/config`, com fallback para `~/.config/logi-osd/config`.
pub fn config_path() -> PathBuf {
    path_from(std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME"))
}

/// `$XDG_CONFIG_HOME` (se absoluto), senão `~/.config`.
fn home_from(xdg_config_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    match xdg_config_home.filter(|v| !v.is_empty()).map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => PathBuf::from(home.unwrap_or_default()).join(".config"),
    }
}

fn path_from(xdg_config_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    home_from(xdg_config_home, home).join("logi-osd").join("config")
}

#[cfg(test)]
mod tests {
    use super::*;
    const fn ms(n: u64) -> Duration { Duration::from_millis(n) }

    #[test] fn empty_text_is_default()        { assert_eq!(parse("").0, Config::default()); }
    #[test] fn values_override_defaults()     { let (c, w) = parse("bateria_baixa = 20\nduracao_ms=2000\ncomando_emoji = gnome-characters\n"); assert!(w.is_empty()); assert_eq!(c.battery_low, 20); assert_eq!(c.duration, ms(2000)); assert_eq!(c.emoji_command.as_deref(), Some("gnome-characters")); }
    #[test] fn comments_and_blank_lines()     { assert!(parse("# x\n\n   # y\n").1.is_empty()); }
    #[test] fn invalid_value_keeps_default_and_warns() { let (c, w) = parse("bateria_baixa = muito\n"); assert_eq!(c.battery_low, 15); assert_eq!(w.len(), 1); assert!(w[0].contains("linha 1")); }
    #[test] fn critical_must_be_below_low()   { let (c, w) = parse("bateria_baixa = 10\nbateria_critica = 12\n"); assert_eq!((c.battery_low, c.battery_critical), (10, 5)); assert_eq!(w.len(), 1); }
    #[test] fn unknown_key_warns()            { assert_eq!(parse("cor = azul\n").1.len(), 1); }
    #[test] fn empty_emoji_command_is_none()  { assert_eq!(parse("comando_emoji =\n").0.emoji_command, None); }
    #[test] fn blank_emoji_command_is_none()  { assert_eq!(parse("comando_emoji = \t  \n").0.emoji_command, None); assert_eq!(parse("comando_emoji =  x y \n").0.emoji_command.as_deref(), Some("x y")); }

    #[test] fn critical_below_low_from_file_is_kept() { let (c, w) = parse("bateria_baixa = 20\nbateria_critica = 8\n"); assert!(w.is_empty()); assert_eq!((c.battery_low, c.battery_critical), (20, 8)); }
    #[test] fn low_below_default_critical_clamps_critical() { let (c, w) = parse("bateria_baixa = 3\n"); assert_eq!((c.battery_low, c.battery_critical), (3, 2)); assert_eq!(w.len(), 1); assert!(w[0].contains("linha 1")); }
    #[test] fn low_of_one_has_no_room_and_resets() { let (c, w) = parse("bateria_baixa = 1\n"); assert_eq!((c.battery_low, c.battery_critical), (15, 5)); assert_eq!(w.len(), 1); }
    #[test] fn out_of_range_values_warn_and_keep_default() { let (c, w) = parse("bateria_baixa = 100\nduracao_ms = 100\nintervalo_bateria_min = 0\n"); assert_eq!(c, Config::default()); assert_eq!(w.len(), 3); }
    #[test] fn duration_and_interval_units() { let (c, _) = parse("duracao_critica_ms = 5000\nintervalo_bateria_min = 30\n"); assert_eq!(c.duration_critical, ms(5000)); assert_eq!(c.legacy_battery_interval, Duration::from_secs(30 * 60)); }
    #[test] fn line_without_equals_warns() { assert_eq!(parse("bateria_baixa 20\n").1.len(), 1); }
    #[test] fn font_key_is_obsolete_and_warns() { let (c, w) = parse("fonte = /x.ttf\n"); assert_eq!(c, Config::default()); assert_eq!(w.len(), 1); assert!(w[0].contains("obsoleta"), "{w:?}"); assert!(w[0].contains("linha 1")); }
    #[test] fn config_path_prefers_absolute_xdg() { assert_eq!(path_from(Some("/x/cfg".into()), Some("/h".into())), PathBuf::from("/x/cfg/logi-osd/config")); }
    #[test] fn config_path_falls_back_to_home() { assert_eq!(path_from(Some("rel".into()), Some("/h".into())), PathBuf::from("/h/.config/logi-osd/config")); assert_eq!(path_from(Some("".into()), Some("/h".into())), PathBuf::from("/h/.config/logi-osd/config")); }
}
