//! `mock-suite` binary.
//!
//! Bare run → interactive TUI (parity with the retired Python
//! `scripts/mock_ethertap.py`): MIDI clock sink panel + mock mixer panel.
//!
//! `--no-tui` → headless test mode: optional JSONL stream of received OSC on
//! stdout, `--expect` assertions evaluated against the mixer state, exit code
//! 0 (all satisfied) / 1 (timeout with unsatisfied expectations).

use std::io::Write;
use std::time::{Duration, Instant};

use clap::Parser;
use mock_suite::{default_slots, parse_slots_spec, MockMixer, SlotState};

mod tui;

#[derive(Parser, Debug)]
#[command(name = "mock-suite", about = "EtherTap mock suite: X32/M32 mixer simulator + MIDI clock sink")]
struct Args {
    /// UDP port for the mock mixer (0 = OS-assigned; real mixer uses 10023).
    #[arg(long, default_value_t = 10023)]
    port: u16,

    /// Slot layout: comma-separated `dly:BPM`, `other:TYPE`, `empty`
    /// (max 8, padded with empty). Default mirrors the test fixture.
    #[arg(long)]
    slots: Option<String>,

    /// Headless mode: no TUI, run until --duration elapses or all --expect
    /// assertions are satisfied.
    #[arg(long)]
    no_tui: bool,

    /// (headless) Print each received OSC message as one JSON line on stdout.
    #[arg(long)]
    jsonl: bool,

    /// (headless) Assertion, repeatable. Forms: `ADDR[:MIN_COUNT]` — OSC
    /// address received at least MIN_COUNT times (default 1) — or
    /// `sync:SLOT[:MIN_COUNT]` — slot received at least MIN_COUNT BPM syncs.
    #[arg(long = "expect")]
    expects: Vec<String>,

    /// (headless) Seconds to run before giving up on --expect (or total run
    /// time when no --expect given). 0 = run until Ctrl-C.
    #[arg(long, default_value_t = 0.0)]
    duration: f64,

    /// Disable the MIDI clock sink (mixer only).
    #[arg(long)]
    no_midi: bool,
}

#[derive(Debug)]
enum Expect {
    Addr { addr: String, min: usize },
    Sync { slot: u8, min: u64 },
}

fn parse_expect(spec: &str) -> Result<Expect, String> {
    if let Some(rest) = spec.strip_prefix("sync:") {
        let (slot, min) = match rest.split_once(':') {
            Some((s, m)) => (s, m.parse().map_err(|_| format!("--expect: bad count in {spec:?}"))?),
            None => (rest, 1u64),
        };
        let slot: u8 = slot.parse().map_err(|_| format!("--expect: bad slot in {spec:?}"))?;
        if !(1..=8).contains(&slot) {
            return Err(format!("--expect: slot out of range in {spec:?}"));
        }
        return Ok(Expect::Sync { slot, min });
    }
    if !spec.starts_with('/') {
        return Err(format!(
            "--expect: {spec:?} is neither an OSC address (starts with /) nor sync:SLOT[:N]"
        ));
    }
    // ADDR[:MIN] — split on the last ':' only if what follows parses as a count
    // (OSC addresses themselves never contain ':').
    match spec.rsplit_once(':') {
        Some((addr, min)) if min.chars().all(|c| c.is_ascii_digit()) && !min.is_empty() => {
            Ok(Expect::Addr { addr: addr.to_string(), min: min.parse().unwrap() })
        }
        _ => Ok(Expect::Addr { addr: spec.to_string(), min: 1 }),
    }
}

fn expects_satisfied(mock: &MockMixer, expects: &[Expect]) -> bool {
    expects.iter().all(|e| match e {
        Expect::Addr { addr, min } => mock.count_addr(addr) >= *min,
        Expect::Sync { slot, min } => mock.sync_count(*slot) >= *min,
    })
}

fn osc_arg_to_json(arg: &rosc::OscType) -> serde_json::Value {
    use serde_json::json;
    match arg {
        rosc::OscType::Int(i) => json!(i),
        rosc::OscType::Float(f) => json!(f),
        rosc::OscType::String(s) => json!(s),
        other => json!(format!("{other:?}")),
    }
}

fn run_headless(args: &Args, slots: [SlotState; 8]) -> i32 {
    let mock = match MockMixer::start_on(args.port, slots) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mock-suite: failed to bind UDP port {}: {e}", args.port);
            return 2;
        }
    };
    eprintln!("mock-suite: mixer listening on UDP :{}", mock.port());

    #[cfg(unix)]
    let _sink = if args.no_midi {
        None
    } else {
        match mock_suite::MidiClockSink::start() {
            Ok(s) => {
                eprintln!("mock-suite: MIDI clock sink open ({:?})", mock_suite::SINK_PORT_NAME);
                Some(s)
            }
            Err(e) => {
                eprintln!("mock-suite: MIDI clock sink unavailable: {e}");
                None
            }
        }
    };

    let expects: Vec<Expect> = match args.expects.iter().map(|s| parse_expect(s)).collect() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mock-suite: {e}");
            return 2;
        }
    };

    let deadline = if args.duration > 0.0 {
        Some(Instant::now() + Duration::from_secs_f64(args.duration))
    } else {
        None
    };

    let stdout = std::io::stdout();
    let mut jsonl_cursor = 0usize;
    loop {
        if args.jsonl {
            let msgs = mock.received_msgs.lock().clone();
            // The log is capped; under test volumes we never hit the cap, so a
            // simple cursor over the Vec is sufficient.
            for m in msgs.iter().skip(jsonl_cursor) {
                let line = serde_json::json!({
                    "ts_ms": m.ts_ms,
                    "peer": m.peer,
                    "addr": m.addr,
                    "args": m.args.iter().map(osc_arg_to_json).collect::<Vec<_>>(),
                });
                let mut lock = stdout.lock();
                let _ = writeln!(lock, "{line}");
            }
            jsonl_cursor = msgs.len();
        }

        if !expects.is_empty() && expects_satisfied(&mock, &expects) {
            eprintln!("mock-suite: all {} expectation(s) satisfied", expects.len());
            return 0;
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                if expects.is_empty() {
                    return 0;
                }
                for e in &expects {
                    let ok = expects_satisfied(&mock, std::slice::from_ref(e));
                    eprintln!("mock-suite: {} {e:?}", if ok { "OK  " } else { "FAIL" });
                }
                return 1;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn main() {
    let args = Args::parse();

    let slots = match &args.slots {
        Some(spec) => match parse_slots_spec(spec) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mock-suite: {e}");
                std::process::exit(2);
            }
        },
        None => default_slots(),
    };

    if args.no_tui || args.jsonl || !args.expects.is_empty() {
        let code = run_headless(&args, slots);
        std::process::exit(code);
    }

    if let Err(e) = tui::run(args.port, slots, args.no_midi) {
        eprintln!("mock-suite: TUI error: {e}");
        std::process::exit(2);
    }
}
