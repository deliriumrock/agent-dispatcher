//! Картинка значка: кружок цвета срочности и число на нём. Рисуется в памяти, без файлов
//! и шрифтов: цифры — точечный шрифт 5×7.

use crate::model::Level;

/// Сторона значка в пикселях. Панель GNOME и строка меню macOS сами уменьшают его до
/// своего размера; 44 — это 22 точки строки меню на экране Retina.
pub const SIZE: u32 = 44;

/// Цвет кружка и цифр.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Paint {
    /// Цвет кружка.
    fill: [u8; 3],
    /// Цвет цифр. Подобран так, чтобы цифры читались на кружке и в 16 пикселях.
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

/// Значок: `SIZE`×`SIZE` пикселей RGBA, построчно сверху вниз. С числом — залитый
/// кружок с цифрами (больше девяти — «9+»), без числа — пустое кольцо.
pub fn render(level: Level, count: usize) -> Vec<u8> {
    let paint = paint(level);
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = SIZE as f32 / 2.0;
    let outer = center - 1.0;
    let inner = if count > 0 { 0.0 } else { outer - 4.5 };
    for y in 0..SIZE {
        for x in 0..SIZE {
            let covered = coverage(x, y, |dx, dy| {
                let r = (dx * dx + dy * dy).sqrt();
                r <= outer && r >= inner
            });
            if covered > 0.0 {
                put(&mut rgba, x, y, paint.fill, covered);
            }
        }
    }
    let glyphs: Vec<usize> = match count {
        0 => Vec::new(),
        1..=9 => vec![count],
        _ => vec![9, PLUS],
    };
    if !glyphs.is_empty() {
        // Одна цифра крупнее, две — мельче, чтобы поместиться в круг.
        let scale = if glyphs.len() == 1 { 4 } else { 3 };
        let count = glyphs.len() as u32;
        let width = (count * GLYPH_W + (count - 1)) * scale;
        let height = GLYPH_H * scale;
        let left = (SIZE - width) / 2;
        let top = (SIZE - height) / 2;
        for (index, &glyph) in glyphs.iter().enumerate() {
            let origin = left + index as u32 * (GLYPH_W + 1) * scale;
            draw_glyph(&mut rgba, glyph, origin, top, scale, paint.ink);
        }
    }
    rgba
}

/// Доля пикселя внутри фигуры: 16 проб на пиксель сглаживают край круга.
fn coverage(x: u32, y: u32, inside: impl Fn(f32, f32) -> bool) -> f32 {
    let center = SIZE as f32 / 2.0;
    let mut hits = 0;
    for sy in 0..4 {
        for sx in 0..4 {
            let dx = x as f32 + (sx as f32 + 0.5) / 4.0 - center;
            let dy = y as f32 + (sy as f32 + 0.5) / 4.0 - center;
            if inside(dx, dy) {
                hits += 1;
            }
        }
    }
    hits as f32 / 16.0
}

/// Закрашивает пиксель цветом `color` с непрозрачностью `alpha` (0–1).
fn put(rgba: &mut [u8], x: u32, y: u32, color: [u8; 3], alpha: f32) {
    let at = ((y * SIZE + x) * 4) as usize;
    rgba[at..at + 3].copy_from_slice(&color);
    rgba[at + 3] = (alpha * 255.0).round() as u8;
}

/// Рисует знак `glyph` с левым верхним углом в (`left`, `top`), каждая точка —
/// квадрат `scale`×`scale`.
fn draw_glyph(rgba: &mut [u8], glyph: usize, left: u32, top: u32, scale: u32, ink: [u8; 3]) {
    for (row, bits) in GLYPHS[glyph].iter().enumerate() {
        for column in 0..GLYPH_W {
            if bits & (1 << (GLYPH_W - 1 - column)) == 0 {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = left + column * scale + dx;
                    let y = top + row as u32 * scale + dy;
                    put(rgba, x, y, ink, 1.0);
                }
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
    fn corners_are_transparent_and_disc_is_filled() {
        let rgba = render(Level::Attention, 1);
        assert_eq!(pixel(&rgba, 0, 0)[3], 0);
        assert_eq!(pixel(&rgba, SIZE - 1, SIZE - 1)[3], 0);
        // Над цифрой — сам кружок.
        assert_eq!(pixel(&rgba, SIZE / 2, 4), [245, 166, 35, 255]);
        // Край сглажен: есть полупрозрачные пиксели.
        assert!(rgba.chunks(4).any(|p| p[3] > 0 && p[3] < 255));
    }

    #[test]
    fn every_level_with_a_count_is_a_disc_and_without_is_a_ring() {
        let fills = [
            (Level::Attention, [245, 166, 35]),
            (Level::Busy, [33, 118, 199]),
            (Level::Done, [30, 135, 60]),
            (Level::Calm, [117, 117, 117]),
        ];
        for (level, [r, g, b]) in fills {
            let disc = render(level, 2);
            assert_eq!(pixel(&disc, SIZE / 2, 4), [r, g, b, 255], "{level:?}");
            assert_eq!(pixel(&disc, 8, SIZE / 2), [r, g, b, 255], "{level:?}");
            let ring = render(level, 0);
            assert_eq!(pixel(&ring, SIZE / 2, SIZE / 2)[3], 0, "{level:?}");
            assert_eq!(pixel(&ring, SIZE / 2, 2), [r, g, b, 255], "{level:?}");
        }
    }

    #[test]
    fn working_count_is_white_on_blue() {
        // «3» — 14 точек шрифта по 4×4 пикселя.
        assert_eq!(opaque_with(&render(Level::Busy, 3), [255, 255, 255]), 14 * 16);
    }

    #[test]
    fn count_is_drawn_in_ink() {
        let ink = [32, 32, 32];
        assert_eq!(opaque_with(&render(Level::Attention, 0), ink), 0);
        // «1» — 10 точек шрифта, каждая 4×4 пикселя.
        assert_eq!(opaque_with(&render(Level::Attention, 1), ink), 10 * 16);
        // «9+» — 15 + 9 точек по 3×3.
        assert_eq!(opaque_with(&render(Level::Attention, 12), ink), (15 + 9) * 9);
    }

    #[test]
    fn digits_stay_inside_the_disc() {
        // Каждый пиксель цифр лежит на кружке: вокруг них нет прозрачных дыр.
        for count in [1, 8, 10] {
            let rgba = render(Level::Done, count);
            let ink = [255, 255, 255];
            for y in 0..SIZE {
                for x in 0..SIZE {
                    let p = pixel(&rgba, x, y);
                    if p[..3] == ink && p[3] == 255 {
                        let dx = x as f32 + 0.5 - SIZE as f32 / 2.0;
                        let dy = y as f32 + 0.5 - SIZE as f32 / 2.0;
                        assert!((dx * dx + dy * dy).sqrt() < SIZE as f32 / 2.0 - 1.5);
                    }
                }
            }
        }
    }
}
