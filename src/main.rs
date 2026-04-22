use std::collections::{HashMap, VecDeque};
use std::io::{Write, stdout};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags, poll, read,
};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, size,
};
use midir::os::unix::VirtualOutput;
use midir::{MidiOutput, MidiOutputConnection};

const NOTE_NAMES: [&str; 31] = [
    "C", "C+", "C#", "Db", "D-", "D", "D+", "D#", "Eb", "E-", "E", "E+", "E#", "F", "F+", "F#",
    "Gb", "G-", "G", "G+", "G#", "Ab", "A-", "A", "A+", "A#", "Bb", "B-", "B", "B+", "B#",
];

const ROWS: [[char; 10]; 4] = [
    ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'],
    ['q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p'],
    ['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', ';'],
    ['z', 'x', 'c', 'v', 'b', 'n', 'm', ',', '.', '/'],
];

fn step_for(row: usize, col: usize, offset: i32) -> i32 {
    26 + (3 - row) as i32 + 4 * col as i32 + offset
}

fn octave_of_step(step: i32, base: u8) -> i32 {
    base as i32 / 12 + step.div_euclid(31) - 1
}

fn key_to_step(k: char, offset: i32) -> Option<i32> {
    for (r, row) in ROWS.iter().enumerate() {
        for (c, &ch) in row.iter().enumerate() {
            if ch == k {
                return Some(step_for(r, c, offset));
            }
        }
    }
    None
}

fn step_to_midi(step: i32, base: u8, bend_range_semitones: u8) -> (u8, u16) {
    let cents = step as f64 * 1200.0 / 31.0;
    let nearest = (cents / 100.0).round() as i32;
    let note = (base as i32 + nearest).clamp(0, 127) as u8;
    let bend_cents = cents - (nearest as f64) * 100.0;
    let range_cents = bend_range_semitones as f64 * 100.0;
    let bend = (8192.0 + (bend_cents / range_cents) * 8192.0)
        .round()
        .clamp(0.0, 16383.0) as u16;
    (note, bend)
}

#[derive(Parser, Debug)]
#[command(name = "utm", about = "31-EDO microtonal QWERTY MIDI controller")]
struct Args {
    /// Connect to existing MIDI output whose name contains this substring.
    /// If omitted, creates a virtual port named "utm".
    #[arg(long)]
    port: Option<String>,

    /// Full MPE: channel 1 is master, notes play on 2..=16.
    #[arg(long)]
    mpe: bool,

    /// MIDI note for 31-EDO step 0 (default 48 = C3, so the first C in the layout is middle C).
    #[arg(long, default_value_t = 48)]
    base: u8,

    /// Pitch bend range in semitones (set via RPN at startup).
    #[arg(long = "bend-range", default_value_t = 2)]
    bend_range: u8,
}

struct Voices {
    free: VecDeque<u8>,
    order: VecDeque<char>,
    active: HashMap<char, (u8, u8)>,
    base: u8,
    bend_range: u8,
}

impl Voices {
    fn new(channels: &[u8], base: u8, bend_range: u8) -> Self {
        Self {
            free: channels.iter().copied().collect(),
            order: VecDeque::new(),
            active: HashMap::new(),
            base,
            bend_range,
        }
    }

    fn on(&mut self, conn: &mut MidiOutputConnection, key: char, step: i32) -> Result<()> {
        if self.active.contains_key(&key) {
            return Ok(());
        }
        let ch = if let Some(c) = self.free.pop_front() {
            c
        } else if let Some(oldest) = self.order.pop_front() {
            let (c, n) = self
                .active
                .remove(&oldest)
                .ok_or_else(|| anyhow!("voice bookkeeping out of sync"))?;
            conn.send(&[0x80 | c, n, 0])?;
            c
        } else {
            return Ok(());
        };
        let (note, bend) = step_to_midi(step, self.base, self.bend_range);
        let lsb = (bend & 0x7F) as u8;
        let msb = ((bend >> 7) & 0x7F) as u8;
        conn.send(&[0xE0 | ch, lsb, msb])?;
        conn.send(&[0x90 | ch, note, 100])?;
        self.active.insert(key, (ch, note));
        self.order.push_back(key);
        Ok(())
    }

    fn off(&mut self, conn: &mut MidiOutputConnection, key: char) -> Result<()> {
        if let Some((ch, note)) = self.active.remove(&key) {
            conn.send(&[0x80 | ch, note, 0])?;
            self.free.push_back(ch);
            if let Some(pos) = self.order.iter().position(|&k| k == key) {
                self.order.remove(pos);
            }
        }
        Ok(())
    }

    fn all_off(&mut self, conn: &mut MidiOutputConnection) {
        let keys: Vec<char> = self.active.keys().copied().collect();
        for k in keys {
            let _ = self.off(conn, k);
        }
    }
}

fn open_midi(args: &Args) -> Result<MidiOutputConnection> {
    let midi = MidiOutput::new("utm")?;
    match &args.port {
        Some(substr) => {
            let needle = substr.to_lowercase();
            let ports = midi.ports();
            let port = ports.iter().find(|p| {
                midi.port_name(p)
                    .map(|n| n.to_lowercase().contains(&needle))
                    .unwrap_or(false)
            });
            match port {
                Some(p) => {
                    let conn = midi.connect(p, "utm-out").map_err(|e| anyhow!("{e}"))?;
                    Ok(conn)
                }
                None => {
                    let names: Vec<String> = ports
                        .iter()
                        .filter_map(|p| midi.port_name(p).ok())
                        .collect();
                    Err(anyhow!(
                        "no MIDI output port matching '{substr}'. available: {names:?}"
                    ))
                }
            }
        }
        None => {
            let conn = midi.create_virtual("utm").map_err(|e| anyhow!("{e}"))?;
            Ok(conn)
        }
    }
}

fn setup_channels(
    conn: &mut MidiOutputConnection,
    channels: &[u8],
    bend_range: u8,
    mpe: bool,
) -> Result<()> {
    if mpe {
        conn.send(&[0xB0, 101, 0])?;
        conn.send(&[0xB0, 100, 6])?;
        conn.send(&[0xB0, 6, 15])?;
        conn.send(&[0xB0, 101, 127])?;
        conn.send(&[0xB0, 100, 127])?;
    }
    for &ch in channels {
        conn.send(&[0xB0 | ch, 101, 0])?;
        conn.send(&[0xB0 | ch, 100, 0])?;
        conn.send(&[0xB0 | ch, 6, bend_range])?;
        conn.send(&[0xB0 | ch, 38, 0])?;
        conn.send(&[0xB0 | ch, 101, 127])?;
        conn.send(&[0xB0 | ch, 100, 127])?;
    }
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            Hide,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags, Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn build_border(pressed: bool, is_top: bool, annotation: Option<&str>, w: usize) -> String {
    let (l, r, mid) = if pressed {
        if is_top {
            ('╔', '╗', '═')
        } else {
            ('╚', '╝', '═')
        }
    } else if is_top {
        ('╭', '╮', '─')
    } else {
        ('╰', '╯', '─')
    };
    let inner = match annotation {
        Some(s) => {
            let s_width = s.chars().count();
            let pad = w.saturating_sub(s_width);
            let (pl, pr) = if is_top { (0, pad) } else { (pad, 0) };
            format!(
                "{}{}{}",
                mid.to_string().repeat(pl),
                s,
                mid.to_string().repeat(pr)
            )
        }
        None => mid.to_string().repeat(w),
    };
    format!("{l}{inner}{r}")
}

fn print_layout(active: &HashMap<char, (u8, u8)>, base: u8, offset: i32, show_numbers: bool) {
    let w = 5;
    let key_cells: u16 = 7;
    let layout_cols: u16 = 2 * 3 + 10 * key_cells;
    let layout_rows: u16 = 16;
    let (term_cols, term_rows) = size().unwrap_or((80, 24));
    let x0 = term_cols.saturating_sub(layout_cols) / 2;
    let y0 = term_rows.saturating_sub(layout_rows) / 2;
    let mut out = stdout();
    let _ = execute!(out, Clear(ClearType::All));
    let mut line: u16 = 0;
    for r in 0..4 {
        let pad = " ".repeat(2 * r);
        let mut tops = String::new();
        let mut notes = String::new();
        let mut keys = String::new();
        let mut bots = String::new();
        for c in 0..10 {
            let step = step_for(r, c, offset);
            let pc = step.rem_euclid(31) as usize;
            let k = ROWS[r][c];
            let name: String = if show_numbers {
                pc.to_string()
            } else {
                NOTE_NAMES[pc].to_string()
            };
            let pressed = active.contains_key(&k);

            let top_annot = if pc == 30 {
                Some(format!("↖{}", octave_of_step(step + 1, base)))
            } else {
                None
            };
            let bot_annot = if pc == 0 {
                Some(format!("{}↘", octave_of_step(step - 1, base)))
            } else {
                None
            };

            tops.push_str(&build_border(pressed, true, top_annot.as_deref(), w));
            bots.push_str(&build_border(pressed, false, bot_annot.as_deref(), w));
            if pressed {
                notes.push_str(&format!("║\x1b[1m{name:^w$}\x1b[0m║"));
                keys.push_str(&format!("║\x1b[1m{k:^w$}\x1b[0m║"));
            } else {
                notes.push_str(&format!("│{name:^w$}│"));
                keys.push_str(&format!("│{k:^w$}│"));
            }
        }
        for row_str in [&tops, &notes, &keys, &bots] {
            let _ = execute!(out, MoveTo(x0, y0 + line));
            let _ = write!(out, "{pad}{row_str}");
            line += 1;
        }
    }
    let _ = out.flush();
}

fn redraw_layout(
    active: &HashMap<char, (u8, u8)>,
    base: u8,
    offset: i32,
    show_numbers: bool,
) -> Result<()> {
    print_layout(active, base, offset, show_numbers);
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    let channels: Vec<u8> = if args.mpe {
        (1..=15).collect()
    } else {
        (0..=15).collect()
    };

    let mut conn = open_midi(&args)?;
    setup_channels(&mut conn, &channels, args.bend_range, args.mpe)?;

    let mut voices = Voices::new(&channels, args.base, args.bend_range);
    let mut offset: i32 = 0;
    let mut show_numbers = false;

    let _guard = TerminalGuard::enter()?;
    print_layout(&voices.active, args.base, offset, show_numbers);

    'outer: loop {
        if !poll(Duration::from_millis(50))? {
            continue;
        }
        let ev = read()?;
        if let Event::Resize(_, _) = ev {
            let _ = redraw_layout(&voices.active, args.base, offset, show_numbers);
            continue;
        }
        let Event::Key(KeyEvent {
            code,
            kind,
            modifiers,
            ..
        }) = ev
        else {
            continue;
        };

        if kind == KeyEventKind::Press
            && (code == KeyCode::Esc
                || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL)))
        {
            break 'outer;
        }

        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            continue;
        }

        if kind == KeyEventKind::Press {
            if code == KeyCode::Tab {
                show_numbers = !show_numbers;
                let _ = redraw_layout(&voices.active, args.base, offset, show_numbers);
                continue;
            }
            let shift = modifiers.contains(KeyModifiers::SHIFT);
            let delta: Option<i32> = match code {
                KeyCode::Left if shift => Some(-31),
                KeyCode::Right if shift => Some(31),
                KeyCode::Left => Some(-4),
                KeyCode::Right => Some(4),
                KeyCode::Up => Some(-1),
                KeyCode::Down => Some(1),
                _ => None,
            };
            if let Some(d) = delta {
                offset += d;
                let _ = redraw_layout(&voices.active, args.base, offset, show_numbers);
                continue;
            }
        }

        let KeyCode::Char(c) = code else { continue };
        let c = c.to_ascii_lowercase();
        let Some(step) = key_to_step(c, offset) else {
            continue;
        };
        match kind {
            KeyEventKind::Press => {
                let _ = voices.on(&mut conn, c, step);
                let _ = redraw_layout(&voices.active, args.base, offset, show_numbers);
            }
            KeyEventKind::Release => {
                let _ = voices.off(&mut conn, c);
                let _ = redraw_layout(&voices.active, args.base, offset, show_numbers);
            }
            _ => {}
        }
    }

    voices.all_off(&mut conn);
    Ok(())
}
