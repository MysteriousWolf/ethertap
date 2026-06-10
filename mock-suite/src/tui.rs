//! Interactive TUI — parity with the retired Python `mock_ethertap.py`.
//!
//! Keys: `c` toggle MIDI clock sink, `m` toggle mixer, `k`/`j` scroll the
//! mixer log (older/newer), `r` follow latest, `q` quit.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use mock_suite::{type_name, MockMixer, SlotState, DLY, EMPTY};

const SPARK: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        .constraints([Constraint::Length(1), Constraint::Min(5), Constraint::Length(1)])
        .split(f.area());

    draw_header(f, chunks[0], app);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[1]);
    draw_midi_panel(f, body[0], app);
    draw_mixer_panel(f, body[1], app);

    let footer = Line::from(vec![
        Span::styled("  c", Style::default().add_modifier(Modifier::DIM)),
        Span::raw(" MIDI   "),
        Span::styled("m", Style::default().add_modifier(Modifier::DIM)),
        Span::raw(" Mixer   "),
        Span::styled("k/j", Style::default().add_modifier(Modifier::DIM)),
        Span::raw(" scroll   "),
        Span::styled("r", Style::default().add_modifier(Modifier::DIM)),
        Span::raw(" latest   "),
        Span::styled("q", Style::default().add_modifier(Modifier::DIM)),
        Span::raw(" quit"),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[2]);
}

fn badge<'a>(label: &'a str, running: bool, error: &'a str) -> Vec<Span<'a>> {
    if !error.is_empty() {
        return vec![Span::styled(format!("✗ {label}: {error}  "), Style::default().fg(Color::Red))];
    }
    vec![
        Span::styled(
            if running { "● " } else { "○ " },
            Style::default().fg(if running { Color::Green } else { Color::DarkGray }),
        ),
        Span::styled(format!("{label} "), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            if running { "on   " } else { "off  " },
            Style::default().fg(if running { Color::Green } else { Color::DarkGray }),
        ),
    ]
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    #[cfg(unix)]
    let sink_running = app.sink.is_some();
    #[cfg(not(unix))]
    let sink_running = false;

    let mixer_label = format!("Mixer :{}", app.mixer.as_ref().map_or(app.port, |m| m.port()));
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
                Span::styled(label, Style::default().fg(color).add_modifier(Modifier::BOLD)),
            ]
        }
        _ => vec![Span::styled(
            "BPM cross-check: waiting for both sources…",
            Style::default().add_modifier(Modifier::DIM),
        )],
    }
}

fn sparkline(values: &[f64], width: usize) -> String {
    if values.len() < 2 {
        return String::new();
    }
    let lo = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let spread = hi - lo;
    let tail = &values[values.len().saturating_sub(width)..];
    if spread < 0.5 {
        return SPARK[4].to_string().repeat(tail.len());
    }
    tail.iter()
        .map(|v| SPARK[(((v - lo) / spread * 8.0) as usize).min(7)])
        .collect()
}

fn draw_midi_panel(f: &mut Frame, area: Rect, app: &App) {
    #[cfg(unix)]
    let (running, stats) = (
        app.sink.is_some(),
        app.sink.as_ref().and_then(|s| s.stats()),
    );
    #[cfg(not(unix))]
    let (running, stats): (bool, Option<mock_suite::SinkStats>) = (false, None);

    let mut lines: Vec<Line> = Vec::new();
    if !running {
        lines.push(Line::from(Span::styled(
            if app.no_midi { "disabled (--no-midi)" } else { "stopped — c to start" },
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else if let Some(s) = stats {
        let gap_s = (now_ms().saturating_sub(s.last_clock_ts_ms)) as f64 / 1000.0;
        let (status, color) = if gap_s > 8.0 {
            (format!("✗ clock dead  ({gap_s:.1}s gap)"), Color::Red)
        } else if gap_s > 2.0 {
            (format!("⚠ clock gap  {gap_s:.1}s"), Color::Yellow)
        } else {
            ("● flowing".to_string(), Color::Green)
        };
        let pct_of = |v: f64| if s.mean_us > 0.0 { format!(" ({:.1}%)", v / s.mean_us * 100.0) } else { String::new() };

        lines.push(Line::from(vec![
            Span::raw("BPM           "),
            Span::styled(format!("{:.2}", s.bpm), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(Span::styled(status, Style::default().fg(color))));
        lines.push(Line::from(format!("Total clocks  {}", s.total_clocks)));
        lines.push(Line::from(format!("Other msgs    {}", s.other_msgs)));
        lines.push(Line::from(format!("Window        {} ivl", s.sample_count)));
        lines.push(Line::from(format!("Mean interval {:.1} µs", s.mean_us)));
        lines.push(Line::from(format!("Std dev       {:.1} µs{}", s.std_us, pct_of(s.std_us))));
        lines.push(Line::from(format!("Jitter p50    {:.1} µs{}", s.p50_us, pct_of(s.p50_us))));
        lines.push(Line::from(format!("Jitter p75    {:.1} µs{}", s.p75_us, pct_of(s.p75_us))));
        lines.push(Line::from(format!("Jitter p95    {:.1} µs{}", s.p95_us, pct_of(s.p95_us))));
        lines.push(Line::from(format!("Jitter p99    {:.1} µs{}", s.p99_us, pct_of(s.p99_us))));
        lines.push(Line::from(format!("Jitter max    {:.1} µs{}", s.max_us, pct_of(s.max_us))));
        lines.push(Line::from(format!("Last message  {}", s.last_hex)));
        let spark = sparkline(&s.bpm_history, area.width.saturating_sub(16) as usize);
        if !spark.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("BPM history   {spark}"),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    } else {
        lines.push(Line::from("Waiting for 0xF8 clock bytes…"));
    }

    let block = Block::default().borders(Borders::ALL).title(" MIDI Clock Sink ");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_mixer_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" Mock Mixer ");

    let Some(mixer) = &app.mixer else {
        let msg = if app.mixer_error.is_empty() {
            "stopped — m to start".to_string()
        } else {
            app.mixer_error.clone()
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().add_modifier(Modifier::DIM))).block(block),
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

    let slots = *mixer.slots.lock();
    let rows: Vec<Row> = slots
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let is_dly = s.type_id == DLY;
            let name_style = if is_dly {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else if s.type_id == EMPTY {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            let rx = match s.rx_bpm {
                Some(b) => format!("{b:.1}"),
                None => "—".to_string(),
            };
            let ago = if s.rx_ts_ms > 0 {
                format!("{}s ago", now_ms().saturating_sub(s.rx_ts_ms) / 1000)
            } else {
                "—".to_string()
            };
            Row::new(vec![
                Cell::from(format!("{}", i + 1)),
                Cell::from(Span::styled(type_name(s.type_id), name_style)),
                Cell::from(s.init_bpm.map_or("—".to_string(), |b| format!("{b:.1}"))),
                Cell::from(rx),
                Cell::from(format!("{}", s.sync_count)),
                Cell::from(ago),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(9),
        ],
    )
    .header(
        Row::new(vec!["Slot", "Name", "Init", "Rx BPM", "Syncs", "Last"])
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    );
    f.render_widget(table, parts[0]);

    // Message log with k/j scroll-back.
    let log = mixer.received_msgs.lock().clone();
    let visible_rows = parts[1].height.saturating_sub(1) as usize;
    let n = log.len();
    let scroll = app.log_scroll.min(n.saturating_sub(visible_rows));
    let end = n - scroll;
    let start = end.saturating_sub(visible_rows);

    let mut lines: Vec<Line> = Vec::new();
    let total = mixer.total_msgs.load(std::sync::atomic::Ordering::Relaxed);
    lines.push(Line::from(Span::styled(
        format!(
            "log  {total} msgs total{}",
            if scroll > 0 { format!("  ↓ {scroll} newer (r to follow)") } else { String::new() }
        ),
        Style::default().add_modifier(Modifier::DIM),
    )));
    for m in &log[start..end] {
        let args = m
            .args
            .iter()
            .map(|a| match a {
                rosc::OscType::Float(v) => format!("{v:.5}"),
                rosc::OscType::Int(v) => format!("{v}"),
                rosc::OscType::String(s) => format!("{s:?}"),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("  ");
        // Highlight delay-time sets with the decoded BPM.
        let bpm_note = m
            .is_set_delay()
            .map(|(_, v)| if v > 0.0 { format!("  → {:.2} BPM", 20.0 / v) } else { String::new() })
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}  ", fmt_clock(m.ts_ms)),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::raw(format!("{}  {}", m.addr, args)),
            Span::styled(bpm_note, Style::default().fg(Color::Green).bold()),
        ]));
    }
    f.render_widget(Paragraph::new(lines), parts[1]);
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
