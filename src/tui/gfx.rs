//! Картинки в панели терминала по протоколу Kitty graphics.
//!
//! Работает в терминалах, которые знают протокол (Ghostty, kitty), и сквозь zellij
//! начиная с 0.45: он сам следит за картинками панели и обрезает их по её границам.
//! Каждая лента — отдельная небольшая картинка с прозрачным фоном; номер картинки
//! равен номеру строки на экране, поэтому новая лента просто заменяет прежнюю.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::time::{Duration, Instant};

use miniz_oxide::deflate::compress_to_vec_zlib;
use ratatui::crossterm::terminal;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use super::app::LaneBox;
use crate::timeline::Phase;

/// Сколько ждать ответа терминала на вопрос о поддержке протокола.
const PROBE_WAIT: Duration = Duration::from_secs(1);
/// Наибольший размер одного куска данных в командe протокола.
const CHUNK: usize = 4096;
/// Номер, которым помечен вопрос о поддержке; по нему узнаётся ответ.
const PROBE_ID: u32 = 31;

/// Размер знакоместа в пикселях.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CellSize {
    /// Ширина.
    pub width: u16,
    /// Высота.
    pub height: u16,
}

impl CellSize {
    /// Размер знакоместа по размеру окна терминала. `None`, если терминал не сообщает
    /// размер в пикселях.
    pub fn detect() -> Option<CellSize> {
        let size = terminal::window_size().ok()?;
        if size.columns == 0 || size.rows == 0 {
            return None;
        }
        let cell = CellSize {
            width: size.width / size.columns,
            height: size.height / size.rows,
        };
        (cell.width > 0 && cell.height > 0).then_some(cell)
    }
}

/// Спрашивает терминал, знает ли он протокол. Терминал должен быть в «сыром» режиме,
/// а чтение клавиш ещё не начато: ответ читается прямо со стандартного ввода.
///
/// Вслед за вопросом уходит запрос атрибутов устройства, на который отвечает любой
/// терминал: по его ответу понятно, что ждать больше нечего.
pub fn probe() -> bool {
    let mut out = io::stdout();
    let query = format!("\x1b_Gi={PROBE_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c");
    if out
        .write_all(query.as_bytes())
        .and_then(|()| out.flush())
        .is_err()
    {
        return false;
    }
    let stdin = io::stdin();
    let deadline = Instant::now() + PROBE_WAIT;
    let mut reply: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    while !has_device_attributes(&reply) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let timeout = Timespec {
            tv_sec: left.as_secs() as i64,
            tv_nsec: left.subsec_nanos().into(),
        };
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        match rustix::io::read(&stdin, &mut chunk[..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => reply.extend_from_slice(&chunk[..count]),
        }
    }
    let accepted = format!("\x1b_Gi={PROBE_ID};OK");
    contains(&reply, accepted.as_bytes())
}

/// Пришёл ли ответ на запрос атрибутов устройства: `ESC [ ? … c`.
fn has_device_attributes(reply: &[u8]) -> bool {
    reply
        .windows(3)
        .position(|window| window == b"\x1b[?")
        .is_some_and(|at| reply[at..].contains(&b'c'))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Картинка: пиксели RGBA по строкам.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Image {
    /// Ширина в пикселях.
    pub width: usize,
    /// Высота в пикселях.
    pub height: usize,
    /// Пиксели: по четыре байта (красный, зелёный, синий, непрозрачность).
    pub rgba: Vec<u8>,
}

impl Image {
    /// Полностью прозрачная картинка.
    fn transparent(width: usize, height: usize) -> Self {
        Image {
            width,
            height,
            rgba: vec![0; width * height * 4],
        }
    }

    /// Закрашивает прямоугольник; всё, что выходит за края картинки, отбрасывается.
    fn fill(&mut self, x: (usize, usize), y: (usize, usize), colour: [u8; 4]) {
        for row in y.0..y.1.min(self.height) {
            for column in x.0..x.1.min(self.width) {
                let at = (row * self.width + column) * 4;
                self.rgba[at..at + 4].copy_from_slice(&colour);
            }
        }
    }

    /// Пиксель в точке; за краями картинки — прозрачный.
    #[cfg(test)]
    fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        let at = (y * self.width + x) * 4;
        match self.rgba.get(at..at + 4) {
            Some(&[r, g, b, a]) if x < self.width => [r, g, b, a],
            _ => [0; 4],
        }
    }
}

/// Цвет состояния на ленте.
fn colour(phase: Phase) -> [u8; 4] {
    match phase {
        Phase::Idle => [140, 140, 150, 200],
        Phase::Done => [96, 196, 110, 255],
        Phase::Quiet => [160, 120, 190, 255],
        Phase::Working => [70, 190, 215, 255],
        Phase::Error => [235, 85, 85, 255],
        Phase::Waiting => [248, 196, 56, 255],
    }
}

/// Верхняя и нижняя границы полосы состояния в знакоместе высотой `height`.
/// Чем важнее состояние, тем полоса выше: то, что требует человека, — самая высокая,
/// работа — средняя, готовый результат — узкая, простой — тонкая линия.
fn band(phase: Phase, height: usize) -> (usize, usize) {
    let inset = match phase {
        Phase::Waiting | Phase::Error => height * 3 / 20,
        Phase::Working | Phase::Quiet => height * 3 / 10,
        Phase::Done => height * 2 / 5,
        Phase::Idle => (height / 2).saturating_sub(1),
    };
    (inset, height.saturating_sub(inset).max(inset + 1))
}

/// Рисует ленту: тонкая ось во всю ширину, деления шкалы и отрезки состояний поверх.
pub fn rasterize(lane: &LaneBox, cell: CellSize) -> Image {
    let width = usize::from(lane.cells) * usize::from(cell.width);
    let height = usize::from(cell.height);
    let mut image = Image::transparent(width, height);
    if width == 0 || height == 0 {
        return image;
    }
    let middle = height / 2;
    image.fill((0, width), (middle, middle + 1), [150, 150, 160, 70]);
    for tick in &lane.ticks {
        let x = lane.window.slot(*tick, width);
        if (0..width as i64).contains(&x) {
            let x = x as usize;
            image.fill(
                (x, x + 1),
                (height / 5, height - height / 5),
                [150, 150, 160, 90],
            );
        }
    }
    // Важное рисуется последним и перекрывает остальное.
    let mut segments = lane.segments.clone();
    segments.sort_by_key(|segment| segment.phase);
    for segment in segments {
        let from = lane.window.slot(segment.from, width);
        let to = lane.window.slot(segment.to, width);
        if to < 0 || from >= width as i64 {
            continue;
        }
        let from = from.max(0) as usize;
        // Короткое ожидание не должно пропасть: у отрезка есть наименьшая ширина.
        let least = match segment.phase {
            Phase::Waiting | Phase::Error => 2,
            _ => 1,
        };
        let to = (to.max(0) as usize).max(from + least).min(width);
        image.fill(
            (from, to),
            band(segment.phase, height),
            colour(segment.phase),
        );
    }
    image
}

/// Кодирует данные в base64 со стандартным алфавитом и дополнением.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let bytes = [
            group[0],
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let bits = u32::from(bytes[0]) << 16 | u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
        for index in 0..4 {
            if index <= group.len() {
                let sextet = (bits >> (18 - 6 * index)) & 0x3f;
                out.push(char::from(ALPHABET[sextet as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Команда терминалу: передать картинку под номером `id` и показать её там, где стоит
/// курсор. Курсор не сдвигается, ответ терминалу запрещён: он пришёл бы как нажатия клавиш.
fn transmit(image: &Image, id: usize) -> String {
    let payload = base64(&compress_to_vec_zlib(&image.rgba, 6));
    let keys = format!(
        "a=T,q=2,f=32,o=z,s={},v={},i={id},C=1",
        image.width, image.height
    );
    let chunks: Vec<&str> = payload
        .as_bytes()
        .chunks(CHUNK)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect();
    let mut out = String::with_capacity(payload.len() + chunks.len() * 16 + keys.len());
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        if index == 0 {
            out.push_str(&format!("\x1b_G{keys},m={more};{chunk}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }
    out
}

/// Команда терминалу: убрать картинку под номером `id` и забыть её данные.
fn delete(id: usize) -> String {
    format!("\x1b_Ga=d,q=2,d=I,i={id}\x1b\\")
}

/// То, что сейчас показано на месте одной ленты.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shown {
    /// Колонка левого края.
    x: u16,
    /// Строка.
    y: u16,
    /// Отпечаток картинки.
    print: u64,
}

/// Ленты на экране: помнит, что уже показано, и отправляет терминалу только изменения.
#[derive(Debug)]
pub struct Gfx {
    /// Размер знакоместа в пикселях.
    pub cell: CellSize,
    /// Что показано в каждой строке; номер картинки на единицу больше номера элемента.
    shown: Vec<Option<Shown>>,
}

impl Gfx {
    /// Графика для терминала с известным размером знакоместа.
    pub fn new(cell: CellSize) -> Self {
        Gfx {
            cell,
            shown: Vec::new(),
        }
    }

    /// Команды терминалу, которые приводят экран к лентам `lanes`: новые и изменившиеся
    /// картинки передаются, лишние убираются. Пустая строка — менять нечего.
    pub fn sync(&mut self, lanes: &[LaneBox]) -> String {
        let mut out = String::new();
        if self.shown.len() < lanes.len() {
            self.shown.resize(lanes.len(), None);
        }
        for (slot, lane) in lanes.iter().enumerate() {
            let image = rasterize(lane, self.cell);
            let mut hasher = DefaultHasher::new();
            image.hash(&mut hasher);
            let now = Shown {
                x: lane.x,
                y: lane.y,
                print: hasher.finish(),
            };
            if self.shown[slot] != Some(now) {
                out.push_str(&format!("\x1b[{};{}H", lane.y + 1, lane.x + 1));
                out.push_str(&transmit(&image, slot + 1));
                self.shown[slot] = Some(now);
            }
        }
        for slot in lanes.len()..self.shown.len() {
            if self.shown[slot].take().is_some() {
                out.push_str(&delete(slot + 1));
            }
        }
        self.shown.truncate(lanes.len());
        wrap_cursor(out)
    }

    /// Команды, убирающие с экрана все ленты.
    pub fn clear(&mut self) -> String {
        let out: String = (0..self.shown.len())
            .filter(|slot| self.shown[*slot].is_some())
            .map(|slot| delete(slot + 1))
            .collect();
        self.shown.clear();
        out
    }

    /// Экран очищен или изменил размер: терминал мог потерять картинки, а знакоместо —
    /// стать другим. Убирает всё показанное; следующая сверка отправит ленты заново.
    pub fn reset(&mut self) -> String {
        if let Some(cell) = CellSize::detect() {
            self.cell = cell;
        }
        self.clear()
    }
}

/// Окружает команды сохранением и возвратом курсора: панель не должна заметить,
/// что он куда-то ходил. Всё вместе идёт одним синхронным обновлением экрана,
/// чтобы замена картинки не мигала.
fn wrap_cursor(commands: String) -> String {
    if commands.is_empty() {
        commands
    } else {
        format!("\x1b[?2026h\x1b7{commands}\x1b8\x1b[?2026l")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{Segment, Window};

    const CELL: CellSize = CellSize {
        width: 10,
        height: 20,
    };

    fn lane(segments: &[(i64, i64, Phase)]) -> LaneBox {
        LaneBox {
            x: 4,
            y: 2,
            cells: 10,
            segments: segments
                .iter()
                .map(|(from, to, phase)| Segment {
                    from: *from,
                    to: *to,
                    phase: *phase,
                })
                .collect(),
            window: Window {
                start: 0,
                span: 100,
            },
            ticks: vec![50],
        }
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd, 0xfc]), "//79/A==");
    }

    #[test]
    fn lane_is_drawn_to_scale() {
        // Окно 100 секунд на 100 пикселей: секунда — пиксель.
        let image = rasterize(
            &lane(&[(10, 40, Phase::Working), (40, 42, Phase::Waiting)]),
            CELL,
        );
        assert_eq!((image.width, image.height), (100, 20));
        assert_eq!(image.pixel(9, 10), [150, 150, 160, 70], "ось до отрезка");
        assert_eq!(image.pixel(10, 10), colour(Phase::Working));
        assert_eq!(image.pixel(39, 10), colour(Phase::Working));
        assert_eq!(image.pixel(40, 10), colour(Phase::Waiting));
        assert_eq!(
            image.pixel(42, 10),
            [150, 150, 160, 70],
            "ось после отрезка"
        );
        // Ожидание выше работы, выше и ниже полосы — прозрачно.
        assert_eq!(image.pixel(40, 3), colour(Phase::Waiting));
        assert_eq!(image.pixel(20, 3), [0; 4]);
        assert_eq!(image.pixel(20, 6), colour(Phase::Working));
        assert_eq!(image.pixel(20, 14), [0; 4]);
        assert_eq!(image.pixel(50, 5), [150, 150, 160, 90], "деление шкалы");
    }

    #[test]
    fn short_and_outside_segments() {
        // Мгновенное ожидание всё равно видно: два пикселя.
        let image = rasterize(&lane(&[(60, 60, Phase::Waiting)]), CELL);
        assert_eq!(image.pixel(60, 10), colour(Phase::Waiting));
        assert_eq!(image.pixel(61, 10), colour(Phase::Waiting));
        assert_eq!(image.pixel(62, 10), [150, 150, 160, 70]);
        // Отрезок, начавшийся до окна, обрезан слева; отрезок вне окна не рисуется.
        let image = rasterize(
            &lane(&[(-500, 5, Phase::Done), (200, 300, Phase::Error)]),
            CELL,
        );
        assert_eq!(image.pixel(0, 10), colour(Phase::Done));
        assert_eq!(image.pixel(4, 10), colour(Phase::Done));
        assert_eq!(image.pixel(99, 10), [150, 150, 160, 70]);
    }

    #[test]
    fn image_is_sent_once_and_replaced_on_change() {
        let mut gfx = Gfx::new(CELL);
        let first = lane(&[(10, 40, Phase::Working)]);
        let sent = gfx.sync(std::slice::from_ref(&first));
        assert!(sent.starts_with(
            "\x1b[?2026h\x1b7\x1b[3;5H\x1b_Ga=T,q=2,f=32,o=z,s=100,v=20,i=1,C=1,m=0;"
        ));
        assert!(sent.ends_with("\x1b\\\x1b8\x1b[?2026l"));
        assert_eq!(
            gfx.sync(std::slice::from_ref(&first)),
            "",
            "то же самое не шлётся"
        );

        let grown = lane(&[(10, 55, Phase::Working)]);
        assert!(gfx.sync(std::slice::from_ref(&grown)).contains("i=1,"));
        let second = LaneBox { y: 3, ..first };
        let sent = gfx.sync(&[grown.clone(), second]);
        assert!(sent.contains("\x1b[4;5H") && sent.contains("i=2,") && !sent.contains("i=1,"));

        // Строк стало меньше: лишняя картинка убирается.
        assert_eq!(
            gfx.sync(&[grown]),
            "\x1b[?2026h\x1b7\x1b_Ga=d,q=2,d=I,i=2\x1b\\\x1b8\x1b[?2026l"
        );
        assert_eq!(gfx.clear(), "\x1b_Ga=d,q=2,d=I,i=1\x1b\\");
        assert_eq!(gfx.clear(), "");
    }

    #[test]
    fn large_payload_is_split_into_chunks() {
        // Шум не сжимается: данных больше одного куска.
        let mut noise = Image::transparent(64, 64);
        let mut state = 1u32;
        for byte in &mut noise.rgba {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (state >> 24) as u8;
        }
        let sent = transmit(&noise, 9);
        let parts: Vec<&str> = sent
            .split("\x1b\\")
            .filter(|part| !part.is_empty())
            .collect();
        assert!(parts.len() > 1);
        assert!(parts[0].starts_with("\x1b_Ga=T,q=2,f=32,o=z,s=64,v=64,i=9,C=1,m=1;"));
        assert!(
            parts[1..parts.len() - 1]
                .iter()
                .all(|part| part.starts_with("\x1b_Gm=1;"))
        );
        assert!(parts[parts.len() - 1].starts_with("\x1b_Gm=0;"));
        let longest = parts
            .iter()
            .map(|part| part.rsplit(';').next().map_or(0, str::len))
            .max();
        assert_eq!(longest, Some(CHUNK));
    }

    #[test]
    fn device_attributes_reply_ends_the_probe() {
        assert!(!has_device_attributes(b"\x1b_Gi=31;OK\x1b\\"));
        assert!(!has_device_attributes(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;"));
        assert!(has_device_attributes(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;4c"));
        assert!(has_device_attributes(b"\x1b[?1;2c"));
    }
}
