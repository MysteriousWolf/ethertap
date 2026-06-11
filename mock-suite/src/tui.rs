//! Interactive TUI — parity with the retired Python `mock_ethertap.py`.
//!
//! Tabbed layout: `1` Overview, `2` MIDI Clock, `3` Mixer, `4` Log
//! (`Tab` cycles). Keys: `c` toggle MIDI clock sink, `m` toggle mixer,
//! `k`/`j` scroll the mixer log (older/newer), `r` follow latest, `q` quit.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Cell, Chart, Dataset, GraphType, Paragraph, Row, Sparkline, Table,
    Tabs,
};
use ratatui::Frame;

use mock_suite::{type_name, MockMixer, SlotState, DLY, EMPTY};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Midi,
    Mixer,
    Log,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Overview, Tab::Midi, Tab::Mixer, Tab::Log];

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn next(self) -> Tab {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }
}

struct App {
    port: u16,
    slots: [SlotState; 8],
    no_midi: bool,
    mixer: Option<MockMixer>,
    #[cfg(unix)]
    sink: Option<mock_suite::MidiClockSink>,
    sink_error: String,
    mixer_error: String,
    /// 0 = follow latest; >0 = lines scrolled back.
    log_scroll: usize,
    tab: Tab,
}

/// Rounded-corner bordered block — the house style for every panel.
fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "))
}

impl App {
    fn toggle_mixer(&mut self) {
        if self.mixer.take().is_none() {
            // take() already stopped it if it was running (Drop).
            match MockMixer::start_on(self.port, self.slots) {
                Ok(m) => {
                    self.mixer = Some(m);
                    self.mixer_error.clear();
                }
                Err(e) => self.mixer_error = e.to_string(),
            }
        }
    }

    fn toggle_sink(&mut self) {
        #[cfg(unix)]
        {
            if self.no_midi {
                return;
            }
            if self.sink.take().is_none() {
                match mock_suite::MidiClockSink::start() {
                    Ok(s) => {
                        self.sink = Some(s);
                        self.sink_error.clear();
                    }
                    Err(e) => self.sink_error = e,
                }
            }
        }
    }
}

pub fn run(port: u16, slots: [SlotState; 8], no_midi: bool) -> std::io::Result<()> {
    let mut app = App {
        port,
        slots,
        no_midi,
        mixer: None,
        #[cfg(unix)]
        sink: None,
        sink_error: String::new(),
        mixer_error: String::new(),
        log_scroll: 0,
        tab: Tab::Overview,
    };
    // Both services start automatically, like the Python tool.
    app.toggle_mixer();
    if !no_midi {
        app.toggle_sink();
    }

    let mut terminal = ratatui::init();
    let result = loop {
        if let Err(e) = terminal.draw(|f| draw(f, &app)) {
            break Err(e);
        }
        // Poll keyboard at the refresh cadence.
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => break Ok(()),
                    KeyCode::Char('c') | KeyCode::Char('C') => app.toggle_sink(),
                    KeyCode::Char('m') | KeyCode::Char('M') => app.toggle_mixer(),
                    KeyCode::Char('k') | KeyCode::Char('K') => app.log_scroll += 1,
                    KeyCode::Char('j') | KeyCode::Char('J') => {
                        app.log_scroll = app.log_scroll.saturating_sub(1)
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => app.log_scroll = 0,
                    KeyCode::Char('1') => app.tab = Tab::Overview,
                    KeyCode::Char('2') => app.tab = Tab::Midi,
                    KeyCode::Char('3') => app.tab = Tab::Mixer,
                    KeyCode::Char('4') => app.tab = Tab::Log,
                    KeyCode::Tab => app.tab = app.tab.next(),
                    _ => {}
                }
            }
        }
    };
    ratatui::restore();
    result
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar
            Constraint::Length(1), // status badges
            Constraint::Min(5),    // tab body
            Constraint::Length(1), // footer keymap
        ])
        .split(f.area());

    let tabs = Tabs::new(["1 Overview", "2 MIDI Clock", "3 Mixer", "4 Log"])
        .select(app.tab.index())
        .style(Style::default().add_modifier(Modifier::DIM))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .remove_modifier(Modifier::DIM)
                .add_modifier(Modifier::BOLD),
        )
        .divider("│");
    f.render_widget(tabs, chunks[0]);

    draw_header(f, chunks[1], app);

    match app.tab {
        Tab::Overview => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(chunks[2]);
            draw_midi_panel(f, body[0], app);
            draw_mixer_panel(f, body[1], app);
        }
        Tab::Midi => draw_midi_tab(f, chunks[2], app),
        Tab::Mixer => draw_mixer_panel(f, chunks[2], app),
        Tab::Log => draw_log_tab(f, chunks[2], app),
    }

    let key = |k: &'static str| Span::styled(k, Style::default().fg(Color::Cyan));
    let footer = Line::from(vec![
        Span::raw("  "),
        key("1-4/Tab"),
        Span::raw(" tabs   "),
        key("c"),
        Span::raw(" MIDI   "),
        key("m"),
        Span::raw(" Mixer   "),
        key("k/j"),
        Span::raw(" scroll   "),
        key("r"),
        Span::raw(" latest   "),
        key("q"),
        Span::raw(" quit"),
    ]);
    f.render_widget(
        Paragraph::new(footer).style(Style::default().add_modifier(Modifier::DIM)),
        chunks[3],
    );
}

fn badge<'a>(label: &'a str, running: bool, error: &'a str) -> Vec<Span<'a>> {
    if !error.is_empty() {
        return vec![Span::styled(
            format!("✗ {label}: {error}  "),
            Style::default().fg(Color::Red),
        )];
    }
    vec![
        Span::styled(
            if running { "● " } else { "○ " },
            Style::default().fg(if running {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled(
            format!("{label} "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if running { "on   " } else { "off  " },
            Style::default().fg(if running {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
    ]
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    #[cfg(unix)]
    let sink_running = app.sink.is_some();
    #[cfg(not(unix))]
    let sink_running = false;

    let mixer_label = format!(
        "Mixer :{}",
        app.mixer.as_ref().map_or(app.port, |m| m.port())
    );
    let mut spans = vec![Span::raw("  ")];
    spans.extend(badge("MIDI sink", sink_running, &app.sink_error));
    spans.extend(badge(&mixer_label, app.mixer.is_some(), &app.mixer_error));
    spans.extend(bpm_crosscheck(app));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Compare live MIDI clock BPM against the most recently OSC-synced BPM —
/// drift means the plugin's two tempo paths fell out of sync.
fn bpm_crosscheck(app: &App) -> Vec<Span<'static>> {
    #[cfg(unix)]
    let midi_bpm = app.sink.as_ref().and_then(|s| s.stats()).map(|s| s.bpm);
    #[cfg(not(unix))]
    let midi_bpm: Option<f64> = None;

    let osc_bpm = app.mixer.as_ref().and_then(|m| {
        m.slots
            .lock()
            .iter()
            .filter(|s| s.rx_bpm.is_some() && s.rx_ts_ms > 0)
            .max_by_key(|s| s.rx_ts_ms)
            .and_then(|s| s.rx_bpm)
    });

    match (midi_bpm, osc_bpm) {
        (Some(m), Some(o)) if m > 0.0 && o > 0.0 => {
            let pct = (m - o).abs() / o * 100.0;
            let (color, label) = if pct > 5.0 {
                (Color::Red, format!("✗ MISMATCH Δ{pct:.1}%"))
            } else if pct > 2.0 {
                (Color::Yellow, format!("⚠ drift Δ{pct:.1}%"))
            } else {
                (Color::Green, "✓ match".to_string())
            };
            vec![
                Span::raw(format!("BPM cross-check  MIDI {m:.1}  OSC {o:.1}  ")),
                Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]
        }
        _ => vec![Span::styled(
            "BPM cross-check: waiting for both sources…",
            Style::default().add_modifier(Modifier::DIM),
        )],
    }
}

/// (lo, hi) of a BPM history, when there are at least two samples.
fn bpm_range(hist: &[f64]) -> Option<(f64, f64)> {
    if hist.len() < 2 {
        return None;
    }
    let lo = hist.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = hist.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    Some((lo, hi))
}

/// Normalize a BPM history tail into 0..=8 levels for the Sparkline widget,
/// which scales bars from zero — raw BPM values (~120 ± 1) would render flat.
fn spark_levels(hist: &[f64], width: usize) -> Vec<u64> {
    let Some((lo, hi)) = bpm_range(hist) else {
        return Vec::new();
    };
    let tail = &hist[hist.len().saturating_sub(width)..];
    let spread = hi - lo;
    if spread < 0.5 {
        return vec![4; tail.len()];
    }
    tail.iter()
        .map(|v| (((v - lo) / spread) * 8.0).round() as u64)
        .collect()
}

/// " (+2.0 / +1.7%)" delta suffix for an Rx BPM against the slot's init BPM.
fn rx_delta(rx: f64, init: Option<f64>) -> String {
    match init {
        Some(i) if i > 0.0 => {
            let d = rx - i;
            format!(" ({:+.1} / {:+.1}%)", d, d / i * 100.0)
        }
        _ => String::new(),
    }
}

/// Clamp a scroll-back offset and slice the log window.
/// Returns (start, end, above, below): the visible slice bounds plus the
/// hidden-line counts for the ↑ older / ↓ newer indicators.
fn scroll_window(n: usize, visible: usize, scroll: usize) -> (usize, usize, usize, usize) {
    let clamped = scroll.min(n.saturating_sub(visible));
    let end = n - clamped;
    let start = end.saturating_sub(visible);
    (start, end, start, clamped)
}

/// Color OSC args like the Python tool: cyan floats, yellow ints, quoted strings.
fn arg_spans(args: &[rosc::OscType]) -> Vec<Span<'static>> {
    if args.is_empty() {
        return vec![Span::styled(
            "—",
            Style::default().add_modifier(Modifier::DIM),
        )];
    }
    let mut spans = Vec::with_capacity(args.len() * 2);
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(match a {
            rosc::OscType::Float(v) => {
                Span::styled(format!("{v:.5}"), Style::default().fg(Color::Cyan))
            }
            rosc::OscType::Int(v) => {
                Span::styled(format!("{v}"), Style::default().fg(Color::Yellow))
            }
            rosc::OscType::String(s) => Span::raw(format!("{s:?}")),
            other => Span::raw(format!("{other:?}")),
        });
    }
    spans
}

/// Snapshot the sink state in a platform-neutral shape.
fn sink_snapshot(app: &App) -> (bool, Option<mock_suite::SinkStats>) {
    #[cfg(unix)]
    return (
        app.sink.is_some(),
        app.sink.as_ref().and_then(|s| s.stats()),
    );
    #[cfg(not(unix))]
    {
        let _ = app;
        (false, None)
    }
}

/// Tier the gap since the last 0xF8 byte into status text + color.
fn clock_tier(gap_s: f64) -> (String, Color) {
    if gap_s > 8.0 {
        (format!("✗ clock dead  ({gap_s:.1}s gap)"), Color::Red)
    } else if gap_s > 2.0 {
        (format!("⚠ clock gap  {gap_s:.1}s"), Color::Yellow)
    } else {
        ("● flowing".to_string(), Color::Green)
    }
}

/// Panel must be at least this wide for the 3-column grouped stat layout.
const MIDI_GROUPED_MIN_WIDTH: u16 = 100;

fn draw_midi_panel(f: &mut Frame, area: Rect, app: &App) {
    let (running, stats) = sink_snapshot(app);

    let block = panel("MIDI Clock Sink");
    if !running {
        f.render_widget(
            Paragraph::new(Span::styled(
                if app.no_midi {
                    "disabled (--no-midi)"
                } else {
                    "stopped — c to start"
                },
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block),
            area,
        );
        return;
    }
    let Some(s) = stats else {
        f.render_widget(
            Paragraph::new("Waiting for 0xF8 clock bytes…").block(block),
            area,
        );
        return;
    };

    let inner = block.inner(area);
    let border_color = clock_tier((now_ms().saturating_sub(s.last_clock_ts_ms)) as f64 / 1000.0).1;
    f.render_widget(block.border_style(Style::default().fg(border_color)), area);

    // Stats above, labelled sparkline pinned to the bottom.
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    if inner.width >= MIDI_GROUPED_MIN_WIDTH {
        draw_midi_stats_grouped(f, parts[0], &s);
    } else {
        f.render_widget(Paragraph::new(midi_stats_compact(&s)), parts[0]);
    }

    let range_label = bpm_range(&s.bpm_history)
        .map(|(lo, hi)| format!("{lo:.1}–{hi:.1} BPM"))
        .unwrap_or_default();
    let levels = spark_levels(&s.bpm_history, inner.width.saturating_sub(2) as usize);
    if !levels.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("BPM history  {range_label}"),
                Style::default().add_modifier(Modifier::DIM),
            )),
            parts[1],
        );
        f.render_widget(
            Sparkline::default()
                .data(&levels)
                .max(8)
                .style(Style::default().fg(Color::Cyan)),
            parts[2],
        );
    }
}

/// One metric line: dim label column + value spans. The label is formatted
/// into an owned String, so the line's lifetime follows the value spans only.
fn metric<'a>(label: &str, value: Vec<Span<'a>>) -> Line<'a> {
    let mut spans = vec![Span::styled(
        format!("{label:<14}"),
        Style::default().add_modifier(Modifier::DIM),
    )];
    spans.extend(value);
    Line::from(spans)
}

fn pct_of(v: f64, mean_us: f64) -> String {
    if mean_us > 0.0 {
        format!(" ({:.1}%)", v / mean_us * 100.0)
    } else {
        String::new()
    }
}

fn jitter_line(label: &str, v: f64, mean_us: f64, color: Color) -> Line<'static> {
    metric(
        label,
        vec![Span::styled(
            format!("{:.1} µs{}", v, pct_of(v, mean_us)),
            Style::default().fg(color),
        )],
    )
}

/// Narrow layout: single vertical metric list (Python compact mode).
fn midi_stats_compact(s: &mock_suite::SinkStats) -> Vec<Line<'static>> {
    let gap_s = (now_ms().saturating_sub(s.last_clock_ts_ms)) as f64 / 1000.0;
    let (status, color) = clock_tier(gap_s);
    vec![
        metric(
            "BPM",
            vec![Span::styled(
                format!("{:.2}", s.bpm),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )],
        ),
        metric(
            "Clock",
            vec![Span::styled(status, Style::default().fg(color))],
        ),
        metric("Total clocks", vec![Span::raw(s.total_clocks.to_string())]),
        metric("Other msgs", vec![Span::raw(s.other_msgs.to_string())]),
        metric("Window", vec![Span::raw(format!("{} ivl", s.sample_count))]),
        metric(
            "Mean interval",
            vec![Span::styled(
                format!("{:.1} µs", s.mean_us),
                Style::default().fg(Color::Cyan),
            )],
        ),
        metric(
            "Std dev",
            vec![Span::styled(
                format!("{:.1} µs{}", s.std_us, pct_of(s.std_us, s.mean_us)),
                Style::default().fg(Color::Cyan),
            )],
        ),
        jitter_line("Jitter p50", s.p50_us, s.mean_us, Color::Yellow),
        jitter_line("Jitter p95", s.p95_us, s.mean_us, Color::Yellow),
        jitter_line("Jitter max", s.max_us, s.mean_us, Color::Red),
        metric("Last message", vec![Span::raw(s.last_hex.clone())]),
    ]
}

/// Wide layout: CLOCK / TIMING / JITTER groups side by side (Python grouped mode).
fn draw_midi_stats_grouped(f: &mut Frame, area: Rect, s: &mock_suite::SinkStats) {
    let gap_s = (now_ms().saturating_sub(s.last_clock_ts_ms)) as f64 / 1000.0;
    let (status, color) = clock_tier(gap_s);
    let header = |t: &'static str| {
        Line::from(Span::styled(
            t,
            Style::default()
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                .add_modifier(Modifier::DIM),
        ))
    };

    let range = bpm_range(&s.bpm_history)
        .map(|(lo, hi)| format!("{lo:.1}–{hi:.1}"))
        .unwrap_or_else(|| "—".to_string());
    let clock_grp = vec![
        header("CLOCK"),
        metric(
            "BPM",
            vec![Span::styled(
                format!("{:.2}", s.bpm),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )],
        ),
        metric("Range", vec![Span::raw(range)]),
        metric(
            "Status",
            vec![Span::styled(status, Style::default().fg(color))],
        ),
        metric("Clocks", vec![Span::raw(s.total_clocks.to_string())]),
        metric("Window", vec![Span::raw(format!("{} ivl", s.sample_count))]),
        metric("Other", vec![Span::raw(s.other_msgs.to_string())]),
    ];
    let ago_s = (now_ms().saturating_sub(s.last_ts_ms)) as f64 / 1000.0;
    let timing_grp = vec![
        header("TIMING"),
        metric(
            "Mean",
            vec![Span::styled(
                format!("{:.1} µs", s.mean_us),
                Style::default().fg(Color::Cyan),
            )],
        ),
        metric(
            "Std dev",
            vec![Span::styled(
                format!("{:.1} µs{}", s.std_us, pct_of(s.std_us, s.mean_us)),
                Style::default().fg(Color::Cyan),
            )],
        ),
        metric("Last msg", vec![Span::raw(s.last_hex.clone())]),
        metric("Received", vec![Span::raw(format!("{ago_s:.1}s ago"))]),
    ];
    let jitter_grp = vec![
        header("JITTER"),
        jitter_line("p50", s.p50_us, s.mean_us, Color::Yellow),
        jitter_line("p75", s.p75_us, s.mean_us, Color::Yellow),
        jitter_line("p95", s.p95_us, s.mean_us, Color::Yellow),
        jitter_line("p99", s.p99_us, s.mean_us, Color::Red),
        jitter_line("max", s.max_us, s.mean_us, Color::Red),
    ];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(area);
    f.render_widget(Paragraph::new(clock_grp), cols[0]);
    f.render_widget(Paragraph::new(timing_grp), cols[1]);
    f.render_widget(Paragraph::new(jitter_grp), cols[2]);
}

/// Full MIDI tab: stats panel on top, BPM-over-time chart below.
fn draw_midi_tab(f: &mut Frame, area: Rect, app: &App) {
    let (_, stats) = sink_snapshot(app);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(12), Constraint::Min(6)])
        .split(area);
    draw_midi_panel(f, parts[0], app);
    draw_bpm_chart(
        f,
        parts[1],
        stats.as_ref().map(|s| s.bpm_history.as_slice()),
    );
}

fn draw_bpm_chart(f: &mut Frame, area: Rect, hist: Option<&[f64]>) {
    let block = panel("BPM over time");
    let Some((hist, (lo, hi))) = hist.and_then(|h| bpm_range(h).map(|r| (h, r))) else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "collecting BPM history…",
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block),
            area,
        );
        return;
    };

    // Pad the y axis so a steady tempo doesn't hug the frame.
    let pad = ((hi - lo) * 0.2).max(0.5);
    let (y_lo, y_hi) = (lo - pad, hi + pad);
    let points: Vec<(f64, f64)> = hist
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v))
        .collect();
    let dataset = Dataset::default()
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(&points);
    let axis_style = Style::default().add_modifier(Modifier::DIM);
    let chart = Chart::new(vec![dataset])
        .block(block)
        .x_axis(
            Axis::default()
                .bounds([0.0, (points.len().saturating_sub(1)) as f64])
                .style(axis_style),
        )
        .y_axis(
            Axis::default()
                .bounds([y_lo, y_hi])
                .labels([
                    format!("{y_lo:.1}"),
                    format!("{:.1}", (y_lo + y_hi) / 2.0),
                    format!("{y_hi:.1}"),
                ])
                .style(axis_style),
        );
    f.render_widget(chart, area);
}

fn draw_mixer_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = panel("Mock Mixer");

    let Some(mixer) = &app.mixer else {
        let msg = if app.mixer_error.is_empty() {
            "stopped — m to start".to_string()
        } else {
            app.mixer_error.clone()
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                msg,
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block),
            area,
        );
        return;
    };

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Slot table (10 rows incl. header + spacer) on top, log below.
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(3)])
        .split(inner);

    // Shed the Last/Compat columns when the panel is too narrow for them.
    let narrow = parts[0].width < 64;
    let dim = Style::default().add_modifier(Modifier::DIM);

    let slots = *mixer.slots.lock();
    let rows: Vec<Row> = slots
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let is_dly = s.type_id == DLY;
            let name_style = if is_dly {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if s.type_id == EMPTY {
                dim
            } else {
                Style::default()
            };
            let init = match s.init_bpm {
                Some(b) => Span::styled(format!("{b:.1}"), Style::default().fg(Color::Cyan)),
                None => Span::styled("—", dim),
            };
            let rx = match s.rx_bpm {
                Some(b) => Line::from(vec![
                    Span::styled(
                        format!("{b:.1}"),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(rx_delta(b, s.init_bpm), dim),
                ]),
                None => Line::from(Span::styled("—", dim)),
            };
            let mut cells = vec![
                Cell::from(format!("{}", i + 1)),
                Cell::from(Span::styled(type_name(s.type_id), name_style)),
                Cell::from(init),
                Cell::from(rx),
                Cell::from(format!("{}", s.sync_count)),
            ];
            if !narrow {
                let ago = if s.rx_ts_ms > 0 {
                    format!("{}s ago", now_ms().saturating_sub(s.rx_ts_ms) / 1000)
                } else {
                    "—".to_string()
                };
                cells.push(Cell::from(Span::styled(ago, dim)));
                cells.push(Cell::from(if is_dly {
                    Span::styled("✓", Style::default().fg(Color::Green))
                } else {
                    Span::styled("—", dim)
                }));
            }
            Row::new(cells)
        })
        .collect();

    let mut widths = vec![
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Length(if narrow { 14 } else { 20 }),
        Constraint::Length(6),
    ];
    let mut headers = vec!["Slot", "Name", "Init", "Rx BPM", "Syncs"];
    if !narrow {
        widths.extend([Constraint::Length(9), Constraint::Length(6)]);
        headers.extend(["Last", "Compat"]);
    }
    let table = Table::new(rows, widths).header(
        Row::new(headers).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    );
    f.render_widget(table, parts[0]);
    draw_log(f, parts[1], mixer, app.log_scroll);
}

/// Full-screen log tab — same renderer as the mixer panel's embedded log,
/// wrapped in its own rounded panel.
fn draw_log_tab(f: &mut Frame, area: Rect, app: &App) {
    let block = panel("OSC Log");
    let Some(mixer) = &app.mixer else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "mixer stopped — m to start",
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block),
            area,
        );
        return;
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    draw_log(f, inner, mixer, app.log_scroll);
}

/// Message log with k/j scroll-back; shared by the mixer panel and Log tab.
fn draw_log(f: &mut Frame, area: Rect, mixer: &MockMixer, log_scroll: usize) {
    let log = mixer.received_msgs.lock().clone();
    // Budget: header line + optional ↑/↓ indicator lines.
    let visible_rows = area.height.saturating_sub(2) as usize;
    let n = log.len();
    let (start, end, above, below) = scroll_window(n, visible_rows, log_scroll);

    let mut lines: Vec<Line> = Vec::new();
    let total = mixer.total_msgs.load(std::sync::atomic::Ordering::Relaxed);
    lines.push(Line::from(Span::styled(
        format!("log  {total} msgs total"),
        Style::default().add_modifier(Modifier::DIM),
    )));
    if above > 0 {
        lines.push(Line::from(Span::styled(
            format!("↑ {above} older"),
            Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    for m in &log[start..end] {
        let mut spans = vec![
            Span::styled(
                format!("{}  ", fmt_clock(m.ts_ms)),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::raw(format!("{}  ", m.addr)),
        ];
        spans.extend(arg_spans(&m.args));
        // Highlight delay-time sets with the decoded BPM.
        if let Some((_, v)) = m.is_set_delay() {
            if v > 0.0 {
                spans.push(Span::styled(
                    format!("  → {:.2} BPM", 20.0 / v),
                    Style::default().fg(Color::Green).bold(),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    if below > 0 {
        lines.push(Line::from(Span::styled(
            format!("↓ {below} newer  (j or r to follow)"),
            Style::default().fg(Color::Yellow),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// HH:MM:SS in local time from ms-since-epoch (good enough for a log column;
/// avoids pulling in chrono).
fn fmt_clock(ts_ms: u64) -> String {
    let secs_today = (ts_ms / 1000) % 86_400;
    // Local offset: derive once from the C library via tm? Keep it UTC and
    // label nothing — relative ordering is what matters in the log.
    let h = secs_today / 3600;
    let m = (secs_today % 3600) / 60;
    let s = secs_today % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_window_follows_latest_when_unscrolled() {
        // 10 msgs, 4 visible, no scroll-back: show 6..10, 6 hidden above.
        assert_eq!(scroll_window(10, 4, 0), (6, 10, 6, 0));
    }

    #[test]
    fn scroll_window_reports_newer_lines_when_scrolled_back() {
        // Scrolled back 3: window shifts to 3..7, 3 newer lines hidden below.
        assert_eq!(scroll_window(10, 4, 3), (3, 7, 3, 3));
    }

    #[test]
    fn scroll_window_clamps_overscroll_at_oldest() {
        // Asking for more scroll-back than exists pins to the oldest window.
        assert_eq!(scroll_window(10, 4, 99), (0, 4, 0, 6));
    }

    #[test]
    fn scroll_window_short_log_fits_entirely() {
        assert_eq!(scroll_window(2, 4, 0), (0, 2, 0, 0));
        assert_eq!(scroll_window(0, 4, 5), (0, 0, 0, 0));
    }

    #[test]
    fn rx_delta_signs_and_percent() {
        assert_eq!(rx_delta(122.0, Some(120.0)), " (+2.0 / +1.7%)");
        assert_eq!(rx_delta(118.0, Some(120.0)), " (-2.0 / -1.7%)");
    }

    #[test]
    fn rx_delta_empty_without_init_bpm() {
        // No init BPM → no delta to compute; suffix must vanish, not show NaN.
        assert_eq!(rx_delta(120.0, None), "");
        assert_eq!(rx_delta(120.0, Some(0.0)), "");
    }

    #[test]
    fn bpm_range_needs_two_samples() {
        assert_eq!(bpm_range(&[]), None);
        assert_eq!(bpm_range(&[120.0]), None);
        assert_eq!(bpm_range(&[118.0, 122.0, 120.0]), Some((118.0, 122.0)));
    }

    #[test]
    fn spark_levels_normalizes_to_full_scale() {
        // Variation must span 0..=8 so small BPM wobble is visible.
        let levels = spark_levels(&[100.0, 110.0, 120.0], 10);
        assert_eq!(levels, vec![0, 4, 8]);
    }

    #[test]
    fn spark_levels_flatlines_steady_tempo_at_midscale() {
        // Sub-0.5 BPM spread is noise; render a steady mid bar, not jumps.
        let levels = spark_levels(&[120.0, 120.1, 120.2], 10);
        assert_eq!(levels, vec![4, 4, 4]);
    }

    #[test]
    fn spark_levels_truncates_to_width() {
        let hist: Vec<f64> = (0..20).map(|i| 100.0 + i as f64).collect();
        assert_eq!(spark_levels(&hist, 5).len(), 5);
    }

    #[test]
    fn clock_tier_thresholds() {
        // Tiers encode the telemetry contract: green ≤2s, yellow ≤8s, red dead.
        assert_eq!(clock_tier(0.5).1, Color::Green);
        assert_eq!(clock_tier(3.0).1, Color::Yellow);
        assert_eq!(clock_tier(9.0).1, Color::Red);
    }
}
