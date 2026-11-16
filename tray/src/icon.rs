//! Картинка значка: плашка цвета срочности — квадрат со скруглёнными углами — и число
//! на ней. Рисуется в памяти, без файлов и шрифтов: цифры — точечный шрифт 5×7.

use crate::model::Level;

/// Сторона значка в пикселях. Панель GNOME и строка меню macOS сами уменьшают его до
/// своего размера; 44 — это 22 точки строки меню на экране Retina.
pub const SIZE: u32 = 44;

/// Цвет плашки и цифр.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Paint {
    /// Цвет плашки.
    fill: [u8; 3],
    /// Цвет цифр. Подобран так, чтобы цифры читались на плашке и в 16 пикселях.
    ink: [u8; 3],
}

/// Цвета значка для уровня срочности.
fn paint(level: Level) -> Paint {
    const WHITE: [u8; 3] = [255, 255, 255];
    match level {
        Level::Attention => Paint {
            fill: [245, 166, 35],
            ink: [32, 32, 32],
        },
        Level::Busy => Paint {
            fill: [33, 118, 199],
            ink: WHITE,
        },
        Level::Done => Paint {
            fill: [30, 135, 60],
            ink: WHITE,
        },
        Level::Calm => Paint {
            fill: [117, 117, 117],
            ink: WHITE,
        },
    }
}

/// Цифры 0–9 и знак «+», 5×7 точек; старший бит строки — левый столбец.
const GLYPHS: [[u8; 7]; 11] = [
    [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
    [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
    [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
    [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
    [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
    [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
    [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
    [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
    [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
    [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
];
/// Номер знака «+» в `GLYPHS`.
const PLUS: usize = 10;
const GLYPH_W: u32 = 5;
const GLYPH_H: u32 = 7;

/// Радиус скругления углов плашки в пикселях: четверть стороны, в панели GNOME — 4 точки
/// из 16. Цветного места так примерно на 20% больше, чем у круга во весь значок.
const CORNER: f32 = SIZE as f32 / 4.0;
/// Толщина рамки пустого значка в пикселях.
const FRAME: f32 = 4.5;
/// Размер точки шрифта в пикселях: для одной цифры и для двух знаков («9+»). Подобраны
/// так, чтобы цифры были крупными и не задевали край плашки.
const DOT_ONE: f32 = 4.6;
const DOT_TWO: f32 = 3.3;

/// Значок: `SIZE`×`SIZE` пикселей RGBA, построчно сверху вниз. С числом — залитая
/// плашка во весь значок с цифрами (больше девяти — «9+»), без числа — пустая рамка той
/// же формы.
///
/// Панель GNOME показывает значки в квадрате 16×16 точек, строка меню macOS — высотой
/// 18 точек. Круг во весь квадрат оставлял углы пустыми; плашка со скруглёнными углами
/// закрашивает больше, а крупнее значок делает только сама панель.
pub fn render(level: Level, count: usize) -> Vec<u8> {
    let paint = paint(level);
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let covered = coverage(x, y, |px, py| {
                rounded_square(px, py, 0.0, CORNER)
                    && (count > 0 || !rounded_square(px, py, FRAME, CORNER - FRAME))
            });
            if covered > 0.0 {
                blend(&mut rgba, x, y, paint.fill, covered);
            }
        }
    }
    match count {
        0 => {}
        1..=9 => draw_text(&mut rgba, &[count], DOT_ONE, paint.ink),
        _ => draw_text(&mut rgba, &[9, PLUS], DOT_TWO, paint.ink),
    }
    rgba
}

/// Лежит ли точка (`px`, `py`) внутри квадрата со скруглёнными углами радиуса `radius`,
/// отстоящего от краёв значка на `inset` пикселей.
fn rounded_square(px: f32, py: f32, inset: f32, radius: f32) -> bool {
    let (low, high) = (inset, SIZE as f32 - inset);
    if px < low || px > high || py < low || py > high {
        return false;
    }
    // Насколько точка зашла за центр ближайшего скругления по каждой оси; вне углов — 0.
    let dx = (low + radius - px).max(px - (high - radius)).max(0.0);
    let dy = (low + radius - py).max(py - (high - radius)).max(0.0);
    dx.hypot(dy) <= radius
}

/// Доля пикселя (`x`, `y`) внутри фигуры: 16 проб на пиксель сглаживают край.
/// `inside` получает координаты пробы в пикселях значка.
fn coverage(x: u32, y: u32, inside: impl Fn(f32, f32) -> bool) -> f32 {
    let mut hits = 0;
    for sy in 0..4 {
        for sx in 0..4 {
            let px = x as f32 + (sx as f32 + 0.5) / 4.0;
            let py = y as f32 + (sy as f32 + 0.5) / 4.0;
            if inside(px, py) {
                hits += 1;
            }
        }
    }
    hits as f32 / 16.0
}

/// Кладёт цвет `color` с непрозрачностью `amount` (0–1) поверх пикселя: обычное
/// наложение «поверх» для неумноженной альфы. На прозрачном пикселе цвет остаётся
/// чистым, меняется только альфа — сглаженный край не темнеет.
fn blend(rgba: &mut [u8], x: u32, y: u32, color: [u8; 3], amount: f32) {
    let at = ((y * SIZE + x) * 4) as usize;
    let under = f32::from(rgba[at + 3]) / 255.0;
    let alpha = amount + under * (1.0 - amount);
    if alpha <= 0.0 {
        return;
    }
    for (channel, value) in color.iter().enumerate() {
        let old = f32::from(rgba[at + channel]) * under * (1.0 - amount);
        rgba[at + channel] = ((f32::from(*value) * amount + old) / alpha).round() as u8;
    }
    rgba[at + 3] = (alpha * 255.0).round() as u8;
}

/// Рисует строку знаков по центру значка: каждая точка шрифта — квадрат `dot`×`dot`
/// пикселей, между знаками — одна пустая точка. Край сглажен.
fn draw_text(rgba: &mut [u8], glyphs: &[usize], dot: f32, ink: [u8; 3]) {
    let cells = glyphs.len() as u32 * (GLYPH_W + 1) - 1;
    let left = (SIZE as f32 - cells as f32 * dot) / 2.0;
    let top = (SIZE as f32 - GLYPH_H as f32 * dot) / 2.0;
    let lit = |px: f32, py: f32| -> bool {
        let (column, row) = ((px - left) / dot, (py - top) / dot);
        if column < 0.0 || row < 0.0 || row >= GLYPH_H as f32 {
            return false;
        }
        let (cell, row) = (column as u32, row as usize);
        let (index, column) = ((cell / (GLYPH_W + 1)) as usize, cell % (GLYPH_W + 1));
        match glyphs.get(index) {
            Some(&glyph) if column < GLYPH_W => {
                GLYPHS[glyph][row] & (1 << (GLYPH_W - 1 - column)) != 0
            }
            _ => false,
        }
    };
    for y in 0..SIZE {
        for x in 0..SIZE {
            let covered = coverage(x, y, lit);
            if covered > 0.0 {
                blend(rgba, x, y, ink, covered);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
        let at = ((y * SIZE + x) * 4) as usize;
        [rgba[at], rgba[at + 1], rgba[at + 2], rgba[at + 3]]
    }

    fn opaque_with(rgba: &[u8], color: [u8; 3]) -> usize {
        rgba.chunks(4)
            .filter(|p| p[3] == 255 && p[..3] == color)
            .count()
    }

    #[test]
    fn icon_is_a_square_of_rgba_pixels() {
        assert_eq!(render(Level::Calm, 0).len(), (SIZE * SIZE * 4) as usize);
    }

    #[test]
    fn corners_are_transparent_and_tile_is_filled() {
        let rgba = render(Level::Attention, 1);
        assert_eq!(pixel(&rgba, 0, 0)[3], 0);
        assert_eq!(pixel(&rgba, SIZE - 1, SIZE - 1)[3], 0);
        // Над цифрой — сама плашка.
        assert_eq!(pixel(&rgba, SIZE / 2, 4), [245, 166, 35, 255]);
        // Край сглажен: есть полупрозрачные пиксели.
        assert!(rgba.chunks(4).any(|p| p[3] > 0 && p[3] < 255));
    }

    #[test]
    fn every_level_with_a_count_is_a_tile_and_without_is_a_frame() {
        let fills = [
            (Level::Attention, [245, 166, 35]),
            (Level::Busy, [33, 118, 199]),
            (Level::Done, [30, 135, 60]),
            (Level::Calm, [117, 117, 117]),
        ];
        for (level, [r, g, b]) in fills {
            let tile = render(level, 2);
            assert_eq!(pixel(&tile, SIZE / 2, 4), [r, g, b, 255], "{level:?}");
            assert_eq!(pixel(&tile, 8, SIZE / 2), [r, g, b, 255], "{level:?}");
            let frame = render(level, 0);
            assert_eq!(pixel(&frame, SIZE / 2, SIZE / 2)[3], 0, "{level:?}");
            assert_eq!(pixel(&frame, SIZE / 2, 2), [r, g, b, 255], "{level:?}");
            // Рамка повторяет скругление угла.
            assert_eq!(pixel(&frame, 4, 4), [r, g, b, 255], "{level:?}");
            assert_eq!(pixel(&frame, 9, 9)[3], 0, "{level:?}");
        }
    }

    #[test]
    fn tile_fills_the_whole_icon() {
        // Плашка доходит до краёв значка: стороны закрашены целиком, кроме скруглений.
        let rgba = render(Level::Busy, 3);
        for at in [CORNER as u32, SIZE / 2, SIZE - 1 - CORNER as u32] {
            for (x, y) in [(0, at), (SIZE - 1, at), (at, 0), (at, SIZE - 1)] {
                assert_eq!(pixel(&rgba, x, y)[3], 255, "({x}, {y})");
            }
        }
        assert_eq!(pixel(&rgba, 1, 1)[3], 0);
        // Углы закрашены дальше, чем у круга во весь значок: этой точки круг не касается.
        assert_eq!(pixel(&rgba, 4, 4)[3], 255);
        let painted = rgba.chunks(4).map(|p| f32::from(p[3]) / 255.0).sum::<f32>();
        let circle = std::f32::consts::PI * (SIZE as f32 / 2.0).powi(2);
        assert!(painted > circle * 1.15, "закрашено {painted}, у круга {circle}");
    }

    #[test]
    fn count_is_drawn_in_ink() {
        let ink = [32, 32, 32];
        let ink_pixels = |count| opaque_with(&render(Level::Attention, count), ink);
        assert_eq!(ink_pixels(0), 0);
        // У «8» точек шрифта больше, чем у «1»: 20 против 10.
        assert!(ink_pixels(1) > 100);
        assert!(ink_pixels(8) > ink_pixels(1) * 3 / 2);
        assert!(ink_pixels(12) > 0);
        assert!(opaque_with(&render(Level::Busy, 3), [255, 255, 255]) > 100);
    }

    #[test]
    fn digits_take_two_thirds_of_the_icon_height() {
        // Высота цифры — от первой до последней строки с краской.
        let rgba = render(Level::Done, 8);
        let inked: Vec<u32> = (0..SIZE)
            .filter(|&y| (0..SIZE).any(|x| pixel(&rgba, x, y)[..3] == [255, 255, 255]))
            .collect();
        let height = inked.last().unwrap_or(&0) - inked.first().unwrap_or(&0) + 1;
        assert!(height * 3 > SIZE * 2, "высота цифры {height} из {SIZE}");
    }

    #[test]
    fn digits_stay_inside_the_tile() {
        // Всё, что не цвет плашки, лежит заметно внутри круга: цифры не задевают край.
        for count in [1, 2, 7, 8, 10] {
            let rgba = render(Level::Done, count);
            let fill = [30, 135, 60];
            for y in 0..SIZE {
                for x in 0..SIZE {
                    let p = pixel(&rgba, x, y);
                    if p[3] > 0 && p[..3] != fill {
                        let dx = x as f32 + 0.5 - SIZE as f32 / 2.0;
                        let dy = y as f32 + 0.5 - SIZE as f32 / 2.0;
                        let r = dx.hypot(dy);
                        assert!(r < SIZE as f32 / 2.0 - 1.0, "{count}: ({x}, {y}) r={r}");
                    }
                }
            }
        }
    }
}
