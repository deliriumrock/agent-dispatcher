//! Лента времени: чем была занята сессия в последние часы.
//!
//! Строится по журналу смен статуса. Здесь только расчёт: отрезки, окно времени
//! и деления шкалы. Рисует ленту панель — пикселями или символами.

use std::collections::HashMap;

use crate::board::Row;
use crate::model::{Status, View};
use crate::store::JournalEvent;

/// Чем занята сессия на отрезке ленты. Порядок вариантов — порядок важности:
/// если в одно деление ленты попало несколько отрезков, показывается самый важный.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// Сессия открыта, заданий нет.
    Idle,
    /// Ход закончен, результат не принят.
    Done,
    /// Числится работающей, но молчит.
    Quiet,
    /// Идёт ход.
    Working,
    /// Ход оборвался: ошибка или сессию закрыли посреди работы.
    Error,
    /// Агент ждёт человека.
    Waiting,
}

/// Отрезок ленты: с какого по какое время сессия была в одном состоянии.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Segment {
    /// Начало, unix-секунды.
    pub from: i64,
    /// Конец, unix-секунды.
    pub to: i64,
    /// Состояние на отрезке.
    pub phase: Phase,
}

/// Окно времени, которое показывает лента.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Window {
    /// Левый край, unix-секунды.
    pub start: i64,
    /// Длина окна в секундах.
    pub span: i64,
}

impl Window {
    /// Окно длиной `span`, в которое попадает момент `now`, для ленты из `slots` делений.
    ///
    /// Правый край округлён до целого деления: все отрезки сдвигаются одновременно,
    /// раз в деление, и картинку не приходится перерисовывать каждую секунду.
    pub fn ending_at(now: i64, span: i64, slots: usize) -> Window {
        let span = span.max(1);
        let step = (span / slots.max(1) as i64).max(1);
        let end = now.div_euclid(step) * step + step;
        Window {
            start: end - span,
            span,
        }
    }

    /// Правый край окна.
    pub fn end(&self) -> i64 {
        self.start + self.span
    }

    /// Номер деления ленты из `slots` делений, в которое попадает момент `ts`.
    /// Для моментов вне окна номер выходит за границы ленты.
    pub fn slot(&self, ts: i64, slots: usize) -> i64 {
        ((ts - self.start) * slots as i64).div_euclid(self.span)
    }

    /// Шаг делений шкалы в секундах: от пяти минут для получасового окна
    /// до четырёх часов для суточного.
    pub fn tick_step(&self) -> i64 {
        const STEPS: [(i64, i64); 5] = [
            (1_800, 300),
            (3_600, 600),
            (10_800, 1_800),
            (21_600, 3_600),
            (43_200, 7_200),
        ];
        STEPS
            .iter()
            .find(|(span, _)| self.span <= *span)
            .map_or(14_400, |(_, step)| *step)
    }

    /// Моменты делений шкалы внутри окна. Деления стоят на круглом местном времени;
    /// `utc_offset` — смещение местного времени от UTC в секундах.
    pub fn ticks(&self, utc_offset: i64) -> Vec<i64> {
        self.marks(self.tick_step(), utc_offset)
    }

    /// Моменты внутри окна, кратные `step` секундам по местному времени.
    pub fn marks(&self, step: i64, utc_offset: i64) -> Vec<i64> {
        let step = step.max(1);
        let first = (self.start + utc_offset).div_euclid(step) * step + step - utc_offset;
        (0..)
            .map(|index| first + index * step)
            .take_while(|mark| *mark < self.end())
            .collect()
    }

    /// Шаг подписей шкалы для ленты из `cells` знакомест: наименьший круглый шаг,
    /// при котором подписи вида `17:00` не слипаются и попадают на деления.
    pub fn label_step(&self, cells: usize) -> i64 {
        const STEPS: [i64; 9] = [
            300, 600, 1_800, 3_600, 7_200, 10_800, 14_400, 21_600, 43_200,
        ];
        // Подписи нужно семь знакомест: пять цифр с двоеточием и промежуток.
        let needed = 7 * self.span / cells.max(1) as i64;
        let tick = self.tick_step();
        STEPS
            .iter()
            .copied()
            .find(|step| *step >= needed && step % tick == 0)
            .unwrap_or(86_400)
    }
}

/// Смены статуса всех сессий из журнала, разложенные по сессиям.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// Ключ — агент и id сессии, значение — моменты смен и новые статусы по порядку.
    changes: HashMap<String, Vec<(i64, String)>>,
}

impl History {
    /// Раскладывает записи журнала по сессиям.
    pub fn from_events(events: &[JournalEvent]) -> Self {
        let mut changes: HashMap<String, Vec<(i64, String)>> = HashMap::new();
        for event in events {
            changes
                .entry(format!("{}-{}", event.agent, event.sid))
                .or_default()
                .push((event.ts, event.to.clone()));
        }
        History { changes }
    }

    /// Отрезки ленты для строки доски, от старых к новым. Лента заканчивается
    /// в момент `now`, а у принятой сессии — в момент приёмки.
    pub fn lane(&self, row: &Row, now: i64) -> Vec<Segment> {
        let s = &row.session;
        let points = match self.changes.get(&row.key()) {
            Some(changes) if !changes.is_empty() => journal_points(changes),
            _ => session_points(row),
        };
        let end = if s.accepted {
            s.accepted_at.unwrap_or(s.updated).min(now)
        } else {
            now
        };
        let mut lane = Vec::new();
        for (index, (from, phase)) in points.iter().enumerate() {
            let to = points.get(index + 1).map_or(end, |next| next.0).min(end);
            if let Some(phase) = phase
                && to > *from
            {
                lane.push(Segment {
                    from: *from,
                    to,
                    phase: *phase,
                });
            }
        }
        // Молчание в журнал не попадает: о нём известно только по времени последнего события.
        // Сессия, у которой нашлась идущая команда, не молчит, а работает.
        if matches!(row.view, View::Quiet | View::Stuck)
            && let Some(last) = lane.last_mut()
            && last.phase == Phase::Working
            && s.updated > last.from
            && s.updated < last.to
        {
            let quiet = Segment {
                from: s.updated,
                to: last.to,
                phase: Phase::Quiet,
            };
            last.to = s.updated;
            lane.push(quiet);
        }
        lane
    }
}

/// Точки смены состояния по журналу: момент и состояние, наступившее с этого момента.
/// `None` — сессии уже нет и ждать от неё нечего.
fn journal_points(changes: &[(i64, String)]) -> Vec<(i64, Option<Phase>)> {
    let mut current = None;
    changes
        .iter()
        .map(|(ts, to)| {
            current = match to.as_str() {
                "idle" => Some(Phase::Idle),
                "working" => Some(Phase::Working),
                "waiting" => Some(Phase::Waiting),
                "done" => Some(Phase::Done),
                "error" => Some(Phase::Error),
                // Закрытие посреди работы — обрыв; законченный и не принятый ход
                // продолжает ждать приёмки.
                "ended" => match current {
                    Some(Phase::Working | Phase::Waiting | Phase::Quiet) => Some(Phase::Error),
                    Some(Phase::Idle) | None => None,
                    kept => kept,
                },
                _ => current,
            };
            (*ts, current)
        })
        .collect()
}

/// Точки смены состояния для сессии, которой нет в журнале: по временам из её файла.
fn session_points(row: &Row) -> Vec<(i64, Option<Phase>)> {
    let s = &row.session;
    let mut points = vec![(s.started, Some(Phase::Idle))];
    if let Some(turn) = s.turn_started {
        points.push((turn, Some(Phase::Working)));
    }
    let last = match s.status {
        Status::Done => Some((s.done_at.unwrap_or(s.updated), Phase::Done)),
        Status::Waiting => Some((s.updated, Phase::Waiting)),
        Status::Error => Some((s.updated, Phase::Error)),
        Status::Working if s.ended => Some((s.updated, Phase::Error)),
        Status::Working | Status::Idle => None,
    };
    if let Some((ts, phase)) = last {
        points.push((ts, Some(phase)));
    }
    points.sort_by_key(|point| point.0);
    points
}

/// Длина окна ленты словами: `30м`, `3ч`, `24ч`.
pub fn span_label(span: i64) -> String {
    if span < 3_600 {
        format!("{}м", span / 60)
    } else {
        format!("{}ч", span / 3_600)
    }
}

/// Самое важное состояние в каждом из `slots` делений ленты. Так лента рисуется
/// символами: одно деление — одно знакоместо.
pub fn slots(lane: &[Segment], window: Window, slots: usize) -> Vec<Option<Phase>> {
    let mut out = vec![None; slots];
    for segment in lane {
        let first = window.slot(segment.from, slots).max(0);
        // Отрезок занимает деление, в котором заканчивается, только если зашёл в него.
        let last = window.slot(segment.to - 1, slots).min(slots as i64 - 1);
        for slot in first..=last {
            let cell = &mut out[slot as usize];
            *cell = (*cell).max(Some(segment.phase));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::Session;

    fn row(status: Status) -> Row {
        let session = Session {
            agent: "claude".to_string(),
            session_id: "s1".to_string(),
            status,
            started: 1_000,
            updated: 5_000,
            ..Session::default()
        };
        Row::new(PathBuf::from("/nonexistent/s1.json"), session, 5_000, 30)
    }

    fn history(changes: &[(i64, &str)]) -> History {
        let events: Vec<JournalEvent> = changes
            .iter()
            .map(|(ts, to)| JournalEvent {
                ts: *ts,
                agent: "claude".to_string(),
                sid: "s1".to_string(),
                to: to.to_string(),
                ..JournalEvent::default()
            })
            .collect();
        History::from_events(&events)
    }

    fn segment(from: i64, to: i64, phase: Phase) -> Segment {
        Segment { from, to, phase }
    }

    #[test]
    fn journal_becomes_segments() {
        let history = history(&[
            (1_000, "working"),
            (1_600, "waiting"),
            (1_900, "working"),
            (2_500, "done"),
        ]);
        assert_eq!(
            history.lane(&row(Status::Done), 5_000),
            [
                segment(1_000, 1_600, Phase::Working),
                segment(1_600, 1_900, Phase::Waiting),
                segment(1_900, 2_500, Phase::Working),
                segment(2_500, 5_000, Phase::Done),
            ]
        );
    }

    #[test]
    fn accepted_result_ends_the_lane() {
        let history = history(&[(1_000, "working"), (2_500, "done")]);
        let mut accepted = row(Status::Done);
        accepted.session.accepted = true;
        accepted.session.accepted_at = Some(3_000);
        assert_eq!(
            history.lane(&accepted, 5_000).last(),
            Some(&segment(2_500, 3_000, Phase::Done))
        );
    }

    #[test]
    fn closing_mid_turn_is_a_break_and_a_finished_turn_still_waits() {
        let broken = history(&[(1_000, "working"), (2_000, "ended")]);
        assert_eq!(
            broken.lane(&row(Status::Working), 5_000),
            [
                segment(1_000, 2_000, Phase::Working),
                segment(2_000, 5_000, Phase::Error)
            ]
        );
        let finished = history(&[(1_000, "working"), (2_000, "done"), (2_100, "ended")]);
        assert_eq!(
            finished.lane(&row(Status::Done), 5_000),
            [
                segment(1_000, 2_000, Phase::Working),
                segment(2_000, 2_100, Phase::Done),
                segment(2_100, 5_000, Phase::Done)
            ]
        );
        let unused = history(&[(1_000, "idle"), (2_000, "ended")]);
        assert_eq!(
            unused.lane(&row(Status::Idle), 5_000),
            [segment(1_000, 2_000, Phase::Idle)]
        );
    }

    #[test]
    fn silence_is_taken_from_the_last_event_time() {
        let history = history(&[(1_000, "working")]);
        let mut quiet = row(Status::Working);
        quiet.session.updated = 2_000;
        quiet.view = View::Quiet;
        assert_eq!(
            history.lane(&quiet, 5_000),
            [
                segment(1_000, 2_000, Phase::Working),
                segment(2_000, 5_000, Phase::Quiet)
            ]
        );
    }

    #[test]
    fn session_missing_from_the_journal_uses_its_own_times() {
        let mut done = row(Status::Done);
        done.session.turn_started = Some(2_000);
        done.session.done_at = Some(4_000);
        assert_eq!(
            History::default().lane(&done, 5_000),
            [
                segment(1_000, 2_000, Phase::Idle),
                segment(2_000, 4_000, Phase::Working),
                segment(4_000, 5_000, Phase::Done)
            ]
        );
    }

    #[test]
    fn window_moves_in_whole_slots() {
        // Час на 60 делений: деление — минута, край окна — на границе минуты после `now`.
        let window = Window::ending_at(10_030, 3_600, 60);
        assert_eq!(window.end(), 10_080);
        assert_eq!(Window::ending_at(10_079, 3_600, 60), window);
        assert_ne!(Window::ending_at(10_080, 3_600, 60), window);
        assert_eq!(window.slot(window.start, 60), 0);
        assert_eq!(window.slot(10_030, 60), 59);
        assert_eq!(window.slot(window.start - 1, 60), -1);
    }

    #[test]
    fn ticks_stand_on_round_local_time() {
        // Окно три часа, местное время UTC+2: деления каждые полчаса местного времени.
        let window = Window {
            start: 1_794_664_000,
            span: 10_800,
        };
        let ticks = window.ticks(7_200);
        assert_eq!(ticks.len(), 6);
        assert!(ticks.iter().all(|tick| (tick + 7_200) % 1_800 == 0));
        assert!(ticks[0] > window.start && ticks[5] < window.end());
        assert_eq!(
            Window {
                start: 0,
                span: 86_400
            }
            .tick_step(),
            14_400
        );
        // На 33 знакоместа три часа подписываются по часам, на 120 — по получасам.
        assert_eq!(window.label_step(33), 3_600);
        assert_eq!(window.label_step(120), 1_800);
        assert_eq!(window.marks(3_600, 7_200).len(), 3);
    }

    #[test]
    fn slots_keep_the_most_important_phase() {
        let window = Window {
            start: 0,
            span: 100,
        };
        let lane = [
            segment(0, 35, Phase::Working),
            segment(35, 38, Phase::Waiting),
            segment(38, 60, Phase::Working),
            segment(60, 80, Phase::Done),
        ];
        let work = Some(Phase::Working);
        let done = Some(Phase::Done);
        assert_eq!(
            slots(&lane, window, 10),
            [
                work,
                work,
                work,
                Some(Phase::Waiting),
                work,
                work,
                done,
                done,
                None,
                None
            ]
        );
        // Отрезок до начала окна и после его конца обрезается по краям ленты.
        let wide = [segment(-50, 500, Phase::Idle)];
        assert_eq!(slots(&wide, window, 4), [Some(Phase::Idle); 4]);
        assert_eq!(
            slots(&[segment(-50, -10, Phase::Idle)], window, 4),
            [None; 4]
        );
    }
}
