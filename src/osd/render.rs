//! Desenho da pílula num `Pixmap` do tiny-skia e conversão para ARGB8888 do wl_shm.
//! A fonte é mapeada com mmap (page cache compartilhado), nunca copiada para um `Vec`.

use std::fs::File;
use std::io;
use std::path::Path;
use std::ptr::NonNull;

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, ScaleFont};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::message::{Pill, Tone, Value};
use crate::osd::icons;
use crate::osd::theme::{CRITICAL, Rgba, Theme, WARNING};

pub const HEIGHT: u32 = 38;
pub const MIN_WIDTH: u32 = 260;
pub const MAX_WIDTH: u32 = 420;
pub const PAD_X: f32 = 18.0;
pub const GAP: f32 = 12.0;
pub const FONT_PX: f32 = 13.0;
pub const ICON: f32 = 18.0;
pub const DOT: f32 = 9.0;
pub const DOT_GAP: f32 = 6.0;
pub const MARGIN_BOTTOM: i32 = 24;

const FONT_PATHS: [&str; 8] = [
    "/usr/share/fonts/TTF/OpenSans-SemiBold.ttf",
    "/usr/share/fonts/TTF/OpenSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/open-sans/OpenSans-SemiBold.ttf",
    "/usr/share/fonts/open-sans/OpenSans-Regular.ttf",
    "/usr/share/fonts/truetype/open-sans/OpenSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
];
const ELLIPSIS: char = '…';
const BORDER_ALPHA: f32 = 0.08;
const DIM_ALPHA: f32 = 0.80;
const UNLIT_ALPHA: f32 = 0.18;

/// Mapeamento somente leitura de um arquivo; desfeito no `Drop`.
struct Mapping { ptr: NonNull<u8>, len: usize }

impl Mapping {
    fn open(path: &Path) -> io::Result<Mapping> {
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "fonte grande demais"))?;
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "arquivo de fonte vazio"));
        }
        // SAFETY: mapeamento novo (addr nulo), PRIVATE+READ de um fd válido com `len` bytes;
        // o resultado só é lido e liberado uma vez no Drop.
        let raw = unsafe { mmap(std::ptr::null_mut(), len, ProtFlags::READ, MapFlags::PRIVATE, &file, 0) }
            .map_err(io::Error::from)?;
        let ptr = NonNull::new(raw.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap devolveu ponteiro nulo"))?;
        Ok(Mapping { ptr, len })
    }

    /// Bytes mapeados com tempo de vida emprestado ao `Mapping`.
    fn bytes(&self) -> &[u8] {
        // SAFETY: região válida de `len` bytes, imutável (PROT_READ) enquanto o Mapping existir.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr/len vieram de um mmap bem-sucedido e ninguém mais referencia a região
        // (a fonte emprestada é declarada antes no `Renderer` e cai primeiro).
        let _ = unsafe { munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

pub struct Renderer {
    // Ordem importa: a fonte empresta de `_map` e precisa cair antes dele.
    font: FontRef<'static>,
    _map: Mapping,
}

/// Layout resolvido em coordenadas lógicas.
struct Layout { width: u32, label: String }

impl Renderer {
    /// Usa `path_override` se dado; senão a primeira fonte existente da lista padrão.
    pub fn load(path_override: Option<&Path>) -> io::Result<Renderer> {
        let map = match path_override {
            Some(path) => Mapping::open(path)?,
            None => {
                let path = FONT_PATHS.iter().map(Path::new).find(|p| p.exists()).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("nenhuma fonte encontrada em {FONT_PATHS:?}; defina `fonte =` na configuração com o caminho de um .ttf"))
                })?;
                Mapping::open(path)?
            }
        };
        // SAFETY: estende o empréstimo para 'static; seguro porque `font` é campo anterior a `_map`
        // (cai antes) e `Renderer` nunca expõe a fonte nem o slice.
        let bytes: &'static [u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(map.bytes()) };
        let font = FontRef::try_from_slice(bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("fonte inválida: {e}")))?;
        Ok(Renderer { font, _map: map })
    }

    fn text_width(&self, text: &str, px: f32) -> f32 {
        let scaled = self.font.as_scaled(PxScale::from(px));
        let mut width = 0.0;
        let mut prev: Option<GlyphId> = None;
        for ch in text.chars() {
            let id = scaled.glyph_id(ch);
            if let Some(p) = prev {
                width += scaled.kern(p, id);
            }
            width += scaled.h_advance(id);
            prev = Some(id);
        }
        width
    }

    fn value_width(&self, value: &Value) -> f32 {
        match value {
            Value::None => 0.0,
            Value::Text(text, _) => self.text_width(text, FONT_PX),
            Value::Dots { total, .. } => dots_width(*total),
        }
    }

    fn layout(&self, pill: &Pill) -> Layout {
        let value_w = self.value_width(&pill.value);
        let has_value = value_w > 0.0;
        let fixed = PAD_X * 2.0 + icons::width(pill.icon) + GAP + if has_value { GAP + value_w } else { 0.0 };
        let max_label = MAX_WIDTH as f32 - fixed;
        let full_w = self.text_width(&pill.label, FONT_PX);
        if full_w <= max_label {
            let width = ((fixed + full_w).ceil() as u32).clamp(MIN_WIDTH, MAX_WIDTH);
            return Layout { width, label: pill.label.clone() };
        }
        // Não cabe: trunca por caractere com reticências; a pílula fica na largura máxima.
        let chars: Vec<char> = pill.label.chars().collect();
        for keep in (0..chars.len()).rev() {
            let mut label: String = chars[..keep].iter().collect();
            label.push(ELLIPSIS);
            let label_w = self.text_width(&label, FONT_PX);
            if label_w <= max_label {
                return Layout { width: MAX_WIDTH, label };
            }
        }
        Layout { width: MAX_WIDTH, label: ELLIPSIS.to_string() }
    }

    /// Largura lógica e rótulo (já truncado, se preciso). Só os testes precisam: o `render` já
    /// devolve o pixmap no tamanho certo.
    #[cfg(test)]
    pub fn measure(&self, pill: &Pill) -> (u32, String) {
        let layout = self.layout(pill);
        (layout.width, layout.label)
    }

    /// Pixmap `(largura*scale)×(HEIGHT*scale)`, RGBA pré-multiplicado.
    pub fn render(&self, pill: &Pill, theme: &Theme, scale: u32) -> Pixmap {
        let layout = self.layout(pill);
        let s = scale.max(1) as f32;
        let (w, h) = (layout.width as f32 * s, HEIGHT as f32 * s);
        // Dimensões sempre > 0 e pequenas: falha só por falta de memória, tratada como bug.
        let mut pm = Pixmap::new(layout.width * scale.max(1), HEIGHT * scale.max(1))
            .expect("dimensões do pixmap são positivas e limitadas");

        draw_background(&mut pm, theme, w, h, s);

        let icon_x = PAD_X * s;
        let icon_y = (h - ICON * s) / 2.0;
        icons::draw(&mut pm, pill.icon, icon_x, icon_y, s, color(tone_rgba(pill.icon_tone, theme)));

        let label_x = (PAD_X + icons::width(pill.icon) + GAP) * s;
        self.draw_text(&mut pm, &layout.label, label_x, h / 2.0, FONT_PX * s, theme.text);

        let right = w - PAD_X * s;
        match &pill.value {
            Value::None => {}
            Value::Text(text, tone) => {
                let color = value_color(*tone, theme);
                let tw = self.text_width(text, FONT_PX * s);
                self.draw_text(&mut pm, text, right - tw, h / 2.0, FONT_PX * s, color);
            }
            Value::Dots { on, total } => draw_dots(&mut pm, *on, *total, theme, right, h / 2.0, s),
        }
        pm
    }

    /// Compõe `text` com a linha de base centrada verticalmente em `cy`.
    fn draw_text(&self, pm: &mut Pixmap, text: &str, x: f32, cy: f32, px: f32, rgba: Rgba) {
        let scaled = self.font.as_scaled(PxScale::from(px));
        let baseline = cy + (scaled.ascent() + scaled.descent()) / 2.0;
        let (width, height) = (pm.width() as i32, pm.height() as i32);
        let alpha = f32::from(rgba.3) / 255.0;
        let data = pm.data_mut();
        let mut pen = x;
        let mut prev: Option<GlyphId> = None;
        for ch in text.chars() {
            let id = scaled.glyph_id(ch);
            if let Some(p) = prev {
                pen += scaled.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(px), point(pen, baseline));
            pen += scaled.h_advance(id);
            prev = Some(id);
            let Some(outlined) = self.font.outline_glyph(glyph) else { continue };
            let bounds = outlined.px_bounds();
            let (ox, oy) = (bounds.min.x as i32, bounds.min.y as i32);
            outlined.draw(|gx, gy, coverage| {
                let (px_x, px_y) = (ox + gx as i32, oy + gy as i32);
                if px_x < 0 || px_y < 0 || px_x >= width || px_y >= height {
                    return;
                }
                blend(&mut data[((px_y * width + px_x) * 4) as usize..][..4], rgba, coverage * alpha);
            });
        }
    }
}

/// Source-over de `rgba` com alfa efetivo `a` sobre um pixel RGBA pré-multiplicado.
fn blend(dst: &mut [u8], rgba: Rgba, a: f32) {
    let inv = 1.0 - a;
    let src = [rgba.0, rgba.1, rgba.2];
    for (d, c) in dst[..3].iter_mut().zip(src) {
        *d = (f32::from(c) * a + f32::from(*d) * inv).round().min(255.0) as u8;
    }
    dst[3] = (255.0 * a + f32::from(dst[3]) * inv).round().min(255.0) as u8;
}

fn dots_width(total: u8) -> f32 {
    if total == 0 { 0.0 } else { f32::from(total) * DOT + f32::from(total - 1) * DOT_GAP }
}

fn color(rgba: Rgba) -> Color {
    Color::from_rgba8(rgba.0, rgba.1, rgba.2, rgba.3)
}

fn with_alpha(rgba: Rgba, factor: f32) -> Rgba {
    Rgba(rgba.0, rgba.1, rgba.2, (f32::from(rgba.3) * factor).round() as u8)
}

fn tone_rgba(tone: Tone, theme: &Theme) -> Rgba {
    match tone {
        Tone::Accent => theme.accent,
        Tone::Dim => with_alpha(theme.text, DIM_ALPHA),
        Tone::Warning => WARNING,
        Tone::Critical => CRITICAL,
    }
}

fn value_color(tone: Tone, theme: &Theme) -> Rgba {
    tone_rgba(tone, theme)
}

fn fill_paint(rgba: Rgba) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color(rgba));
    paint.anti_alias = true;
    paint
}

fn pill_path(x: f32, y: f32, w: f32, h: f32) -> Option<tiny_skia::Path> {
    let r = h / 2.0;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r * 0.4477, y, x + w, y + r * 0.4477, x + w, y + r);
    pb.cubic_to(x + w, y + h - r * 0.4477, x + w - r * 0.4477, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r * 0.4477, y + h, x, y + h - r * 0.4477, x, y + r);
    pb.cubic_to(x, y + r * 0.4477, x + r * 0.4477, y, x + r, y);
    pb.close();
    pb.finish()
}

fn draw_background(pm: &mut Pixmap, theme: &Theme, w: f32, h: f32, s: f32) {
    if let Some(path) = pill_path(0.0, 0.0, w, h) {
        pm.fill_path(&path, &fill_paint(theme.background), FillRule::Winding, Transform::identity(), None);
    }
    // Borda de 1 px lógico, desenhada para dentro (traço centrado a meio px da margem).
    let half = 0.5 * s;
    if let Some(path) = pill_path(half, half, w - 2.0 * half, h - 2.0 * half) {
        let stroke = Stroke { width: s, ..Stroke::default() };
        pm.stroke_path(&path, &fill_paint(with_alpha(theme.text, BORDER_ALPHA)), &stroke, Transform::identity(), None);
    }
}

fn draw_dots(pm: &mut Pixmap, on: u8, total: u8, theme: &Theme, right: f32, cy: f32, s: f32) {
    let left = right - dots_width(total) * s;
    for i in 0..total {
        let cx = left + (f32::from(i) * (DOT + DOT_GAP) + DOT / 2.0) * s;
        let fill = if i < on { theme.accent } else { with_alpha(theme.text, UNLIT_ALPHA) };
        let mut pb = PathBuilder::new();
        pb.push_circle(cx, cy, DOT / 2.0 * s);
        if let Some(path) = pb.finish() {
            pm.fill_path(&path, &fill_paint(fill), FillRule::Winding, Transform::identity(), None);
        }
    }
}

/// tiny-skia é RGBA pré-multiplicado; wl_shm Argb8888 little-endian é B,G,R,A. Troca R↔B.
pub fn to_argb8888(rgba_premul: &[u8], out: &mut [u8]) {
    let (src, _) = rgba_premul.as_chunks::<4>();
    let (dst, _) = out.as_chunks_mut::<4>();
    for (s, d) in src.iter().zip(dst) {
        *d = [s[2], s[1], s[0], s[3]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Icon, Pill};
    use crate::osd::theme::FALLBACK;
    use std::time::Duration;

    fn renderer() -> Renderer {
        Renderer::load(None).expect("nenhuma fonte do sistema encontrada (OpenSans/DejaVu em /usr/share/fonts/TTF)")
    }

    fn text_pill(label: &str, value: &str, tone: Tone) -> Pill {
        Pill {
            icon: Icon::CapsLock, icon_tone: Tone::Accent, label: label.into(),
            value: Value::Text(value.into(), tone), duration: Duration::from_millis(1500),
        }
    }

    fn caps_on() -> Pill { text_pill("Caps Lock", "Ativado", Tone::Accent) }

    /// Pixel (r,g,b,a) pré-multiplicado.
    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * pm.width() + x) * 4) as usize;
        pm.data()[i..i + 4].try_into().unwrap()
    }

    fn near(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
        a.iter().zip(b).all(|(x, y)| (i32::from(*x) - i32::from(y)).abs() <= tol)
    }

    #[test] fn width_is_at_least_260() { assert_eq!(renderer().measure(&caps_on()).0, 260); }

    #[test] fn long_label_is_truncated_with_ellipsis_within_max_width() {
        let pill = text_pill("Teclado Mecânico Sem Fio Ultra Super Mega Longo de Teste Para Truncar: bateria baixa", "10%", Tone::Warning);
        let (width, label) = renderer().measure(&pill);
        assert_eq!(width, 420);
        assert!(label.ends_with('…'), "{label}");
        assert!(label.chars().count() < pill.label.chars().count());
    }

    #[test] fn render_size_scales() {
        let r = renderer();
        let pm = r.render(&caps_on(), &FALLBACK, 2);
        assert_eq!((pm.width(), pm.height()), (2 * r.measure(&caps_on()).0, 76));
    }

    #[test] fn outside_rounded_corner_is_transparent() {
        assert_eq!(px(&renderer().render(&caps_on(), &FALLBACK, 1), 0, 0)[3], 0);
    }

    #[test] fn pill_center_has_background_color() {
        let pm = renderer().render(&caps_on(), &FALLBACK, 1);
        let bg = FALLBACK.background;
        assert!(near(px(&pm, 150, HEIGHT / 2), [bg.0, bg.1, bg.2, bg.3], 2));
    }

    #[test] fn dots_lit_and_unlit() {
        let r = renderer();
        let pill = Pill {
            icon: Icon::KeyboardBacklight, icon_tone: Tone::Accent, label: "Teclado".into(),
            value: Value::Dots { on: 3, total: 8 }, duration: Duration::from_millis(1500),
        };
        let (width, _) = r.measure(&pill);
        let pm = r.render(&pill, &FALLBACK, 1);
        let left = width as f32 - PAD_X - dots_width(8);
        let (a, t, b) = (FALLBACK.accent, FALLBACK.text, FALLBACK.background);
        let unlit = |c: u8, bgc: u8| (f32::from(c) * UNLIT_ALPHA + f32::from(bgc) * (1.0 - UNLIT_ALPHA)).round() as u8;
        for i in 0..8u32 {
            let x = (left + i as f32 * (DOT + DOT_GAP) + DOT / 2.0) as u32;
            let got = px(&pm, x, HEIGHT / 2);
            if i < 3 {
                assert!(near(got, [a.0, a.1, a.2, 255], 2), "dot {i} aceso: {got:?}");
            } else {
                assert!(near(got, [unlit(t.0, b.0), unlit(t.1, b.1), unlit(t.2, b.2), 255], 2), "dot {i} apagado: {got:?}");
            }
        }
    }

    #[test] fn warning_value_is_orange() {
        let r = renderer();
        let pill = text_pill("G602: bateria baixa", "15%", Tone::Warning);
        let (width, _) = r.measure(&pill);
        let pm = r.render(&pill, &FALLBACK, 1);
        let target = [0xF0, 0xA3, 0x5E];
        let value_start = width - PAD_X as u32 - 32;
        let best = (value_start..width - PAD_X as u32)
            .flat_map(|x| (0..HEIGHT).map(move |y| (x, y)))
            .map(|(x, y)| {
                let p = px(&pm, x, y);
                (0..3).map(|k| (i32::from(p[k]) - target[k]).abs()).max().unwrap()
            })
            .min()
            .unwrap();
        assert!(best <= 12, "nenhum pixel perto de #F0A35E (melhor desvio {best})");
    }

    #[test] fn argb_swaps_red_and_blue() { let mut o = [0u8; 4]; to_argb8888(&[1, 2, 3, 4], &mut o); assert_eq!(o, [3, 2, 1, 4]); }

    #[test] fn missing_override_font_is_an_error() {
        assert!(Renderer::load(Some(Path::new("/nao/existe.ttf"))).is_err());
    }

    #[test] fn garbage_font_is_invalid_data() {
        let path = std::env::temp_dir().join(format!("logi-osd-fonte-lixo-{}.ttf", std::process::id()));
        std::fs::write(&path, b"isto nao e uma fonte, so lixo").expect("grava arquivo temporario");
        let res = Renderer::load(Some(&path));
        let _ = std::fs::remove_file(&path);
        let err = res.err().expect("deveria falhar");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test] #[ignore] fn dump_pngs() {
        use crate::message::{backlight, battery, connected, fn_lock, lock, Durations, LockKey};
        use crate::battery::BatteryAlert;
        let d = Durations { normal: Duration::from_millis(1500), critical: Duration::from_millis(4000) };
        let pills = [
            ("caps_on", lock(LockKey::Caps, true, &d)),
            ("num_off", lock(LockKey::Num, false, &d)),
            ("insert_on", lock(LockKey::Insert, true, &d)),
            ("fn_media", fn_lock(true, &d)),
            ("backlight_3_of_8", backlight(3, 9, &d)),
            ("battery_low", battery(BatteryAlert::Low(15), "MX Keys Mini", &d)),
            ("battery_critical", battery(BatteryAlert::Critical(5), "MX Keys Mini", &d)),
            ("charging", battery(BatteryAlert::Charging(42), "G602", &d)),
            ("charged", battery(BatteryAlert::Charged, "G602", &d)),
            ("connected", connected("MX Keys Mini", Some((3, 0)), &d)),
            ("truncated", text_pill("Teclado Mecânico Sem Fio Ultra Super Mega Longo de Teste Para Truncar: bateria baixa", "10%", Tone::Warning)),
        ];
        let r = renderer();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/osd-preview");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, pill) in &pills {
            let light = Theme { dark: false, background: Rgba(0xF2, 0xF2, 0xF2, 0xFF), text: Rgba(0x1B, 0x1B, 0x1B, 0xFF), accent: Rgba(0x00, 0x52, 0x5A, 0xFF) };
            for (suffix, theme, canvas) in [("dark", FALLBACK, (0x70, 0x70, 0x70)), ("light", light, (0xB0, 0xB0, 0xB0))] {
                let pm = r.render(pill, &theme, 3);
                let mut sheet = Pixmap::new(pm.width() + 60, pm.height() + 60).unwrap();
                sheet.fill(Color::from_rgba8(canvas.0, canvas.1, canvas.2, 255));
                sheet.draw_pixmap(30, 30, pm.as_ref(), &tiny_skia::PixmapPaint::default(), Transform::identity(), None);
                sheet.save_png(dir.join(format!("{name}-{suffix}.png"))).unwrap();
            }
        }
    }
}
