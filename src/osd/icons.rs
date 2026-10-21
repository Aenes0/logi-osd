//! Ícones desenhados como paths do tiny-skia (caixa 18×18; teclado 26×18). Sem glifos de fonte.

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform,
};

use crate::message::Icon;

const STROKE: f32 = 1.6;
const RAY_STROKE: f32 = 1.4;

/// Largura lógica do ícone.
pub fn width(icon: Icon) -> f32 {
    if icon == Icon::KeyboardBacklight { 26.0 } else { 18.0 }
}

struct Canvas<'a> {
    pixmap: &'a mut Pixmap,
    paint: Paint<'static>,
    transform: Transform,
}

impl Canvas<'_> {
    fn stroke(&mut self, path: &Path, width: f32, cap: LineCap) {
        let stroke = Stroke { width, line_cap: cap, line_join: LineJoin::Round, ..Stroke::default() };
        self.pixmap.stroke_path(path, &self.paint, &stroke, self.transform, None);
    }

    fn outline(&mut self, path: &Path) {
        self.stroke(path, STROKE, LineCap::Round);
    }

    fn fill(&mut self, path: &Path) {
        self.pixmap.fill_path(path, &self.paint, FillRule::Winding, self.transform, None);
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let k = r * 0.552_284_8; // aproximação de quarto de círculo por cúbica
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Option<Path> {
    Some(PathBuilder::from_rect(Rect::from_xywh(x, y, w, h)?))
}

fn polyline(points: &[(f32, f32)], close: bool) -> Option<Path> {
    let (first, rest) = points.split_first()?;
    let mut pb = PathBuilder::new();
    pb.move_to(first.0, first.1);
    for p in rest {
        pb.line_to(p.0, p.1);
    }
    if close {
        pb.close();
    }
    pb.finish()
}

fn battery_body(c: &mut Canvas) -> Option<()> {
    c.outline(&rounded_rect(1.0, 5.0, 14.0, 8.0, 1.5)?);
    c.fill(&rect(15.0, 7.0, 2.0, 4.0)?);
    Some(())
}

fn keyboard(c: &mut Canvas) -> Option<()> {
    c.outline(&rounded_rect(1.0, 5.0, 24.0, 12.0, 2.0)?);
    for (x, y, w) in [(4.0, 8.0, 3.0), (9.0, 8.0, 3.0), (14.0, 8.0, 3.0), (19.0, 8.0, 3.0), (7.0, 12.0, 12.0)] {
        c.fill(&rect(x, y, w, 2.0)?);
    }
    for ((x1, y1), (x2, y2)) in [((13.0, 0.8), (13.0, 3.0)), ((7.0, 1.5), (8.0, 3.3)), ((19.0, 1.5), (18.0, 3.3))] {
        c.stroke(&polyline(&[(x1, y1), (x2, y2)], false)?, RAY_STROKE, LineCap::Round);
    }
    Some(())
}

fn caps_lock(c: &mut Canvas) -> Option<()> {
    let arrow = [(9.0, 2.0), (16.0, 9.0), (12.0, 9.0), (12.0, 12.0), (6.0, 12.0), (6.0, 9.0), (2.0, 9.0)];
    c.outline(&polyline(&arrow, true)?);
    c.stroke(&polyline(&[(6.0, 15.0), (12.0, 15.0)], false)?, STROKE, LineCap::Butt);
    Some(())
}

fn num_lock(c: &mut Canvas) -> Option<()> {
    c.outline(&rounded_rect(2.0, 2.0, 14.0, 14.0, 3.0)?);
    c.outline(&polyline(&[(7.0, 7.2), (9.3, 5.4), (9.3, 12.6)], false)?);
    Some(())
}

fn insert(c: &mut Canvas) -> Option<()> {
    c.outline(&rounded_rect(2.0, 4.0, 14.0, 10.0, 1.5)?);
    c.stroke(&polyline(&[(6.0, 6.0), (6.0, 12.0)], false)?, STROKE, LineCap::Butt);
    Some(())
}

fn fn_lock(c: &mut Canvas) -> Option<()> {
    let mut pb = PathBuilder::new();
    pb.move_to(6.0, 8.0);
    pb.line_to(6.0, 6.0);
    pb.cubic_to(6.0, 3.0, 12.0, 3.0, 12.0, 6.0);
    pb.line_to(12.0, 8.0);
    c.outline(&pb.finish()?);
    c.outline(&rounded_rect(3.5, 8.0, 11.0, 8.0, 1.5)?);
    let mut dot = PathBuilder::new();
    dot.push_circle(9.0, 12.0, 1.1);
    c.fill(&dot.finish()?);
    Some(())
}

fn charging(c: &mut Canvas) -> Option<()> {
    battery_body(c)?;
    let bolt = [(9.8, 6.2), (6.4, 9.4), (8.5, 9.4), (7.6, 11.9), (11.0, 8.6), (8.9, 8.6)];
    c.fill(&polyline(&bolt, true)?);
    Some(())
}

fn switch(c: &mut Canvas) -> Option<()> {
    c.outline(&polyline(&[(3.0, 6.0), (15.0, 6.0)], false)?);
    c.outline(&polyline(&[(12.5, 3.5), (15.0, 6.0), (12.5, 8.5)], false)?);
    c.outline(&polyline(&[(15.0, 12.0), (3.0, 12.0)], false)?);
    c.outline(&polyline(&[(5.5, 9.5), (3.0, 12.0), (5.5, 14.5)], false)?);
    Some(())
}

/// Desenha `icon` com o canto superior esquerdo da caixa em `(x, y)` (pixels), multiplicado por `scale`.
pub fn draw(pixmap: &mut Pixmap, icon: Icon, x: f32, y: f32, scale: f32, color: Color) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let transform = Transform::from_scale(scale, scale).post_translate(x, y);
    let mut canvas = Canvas { pixmap, paint, transform };
    // `None` só ocorre com geometria degenerada, impossível com estas constantes.
    let _ = match icon {
        Icon::KeyboardBacklight => keyboard(&mut canvas),
        Icon::CapsLock => caps_lock(&mut canvas),
        Icon::NumLock => num_lock(&mut canvas),
        Icon::Insert => insert(&mut canvas),
        Icon::FnLock => fn_lock(&mut canvas),
        Icon::Battery => battery_body(&mut canvas),
        Icon::Charging => charging(&mut canvas),
        Icon::Switch => switch(&mut canvas),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 8] = [
        Icon::CapsLock, Icon::NumLock, Icon::Insert, Icon::FnLock,
        Icon::KeyboardBacklight, Icon::Battery, Icon::Charging, Icon::Switch,
    ];

    #[test] fn widths() {
        assert_eq!(width(Icon::KeyboardBacklight), 26.0);
        assert_eq!(width(Icon::Battery), 18.0);
    }

    #[test] fn every_icon_draws_something_inside_its_box() {
        for icon in ALL {
            let mut pm = Pixmap::new(40, 24).unwrap();
            draw(&mut pm, icon, 4.0, 3.0, 1.0, Color::WHITE);
            let inked = pm.pixels().iter().filter(|p| p.alpha() > 0).count();
            assert!(inked > 20, "{icon:?} quase vazio ({inked})");
            let w = width(icon) as u32;
            for (i, p) in pm.pixels().iter().enumerate() {
                let (px, py) = (i as u32 % 40, i as u32 / 40);
                if p.alpha() > 0 {
                    assert!(px + 1 >= 4 && px <= 4 + w + 1 && py + 1 >= 3 && py <= 3 + 18 + 1, "{icon:?} vaza em ({px},{py})");
                }
            }
        }
    }
}
