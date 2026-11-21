//! The shared terminal menu (N11) for every settings-style command of the
//! node and the hub: grouped, numbered entries, arrow keys or j/k, number
//! keys, Enter, q. Styled after the Nightgrid app (its NIGHTGRID theme
//! tokens: accent yellow, cyan, magenta, ok green, warn orange, danger red;
//! uppercase section labels; ◆ ◇ ● ▲ ▸ glyphs).
//!
//! It degrades on its own: no colour with `NO_COLOR`, `TERM=dumb` or when
//! output isn't a terminal; 256 colours unless `COLORTERM` says truecolor;
//! and a plain numbered list read line by line when either end isn't a
//! terminal (or `NIGHTGRID_PLAIN=1`), so it scripts and pipes cleanly.

use std::io::{BufRead, IsTerminal, Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Accent,
    Cyan,
    Magenta,
    Ok,
    Warn,
    Danger,
    Dim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Colors {
    None,
    Ansi256,
    True,
}

fn colors() -> Colors {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    if no_color || std::env::var("TERM").is_ok_and(|t| t == "dumb") || !std::io::stdout().is_terminal() {
        return Colors::None;
    }
    match std::env::var("COLORTERM").unwrap_or_default().as_str() {
        "truecolor" | "24bit" => Colors::True,
        _ => Colors::Ansi256,
    }
}

impl Tone {
    /// The app's NIGHTGRID theme value and its nearest 256-colour index.
    fn rgb(self) -> Option<((u8, u8, u8), u8)> {
        Some(match self {
            Tone::Plain => return None,
            Tone::Accent => ((0xF3, 0xE6, 0x00), 226),
            Tone::Cyan => ((0x00, 0xE5, 0xFF), 45),
            Tone::Magenta => ((0xFF, 0x2A, 0x6D), 204),
            Tone::Ok => ((0x2B, 0xFF, 0x88), 48),
            Tone::Warn => ((0xFF, 0x9F, 0x1C), 214),
            Tone::Danger => ((0xFF, 0x00, 0x3C), 197),
            Tone::Dim => ((0x8B, 0x98, 0xA9), 103),
        })
    }

    /// The glyph the app uses for this state.
    pub fn glyph(self) -> &'static str {
        match self {
            Tone::Ok => "●",
            Tone::Warn => "▲",
            Tone::Danger => "✕",
            Tone::Dim => "○",
            Tone::Accent | Tone::Magenta => "◆",
            Tone::Cyan | Tone::Plain => "◇",
        }
    }
}

/// Paints text in a tone (and bold if asked), or leaves it plain.
pub fn paint(text: &str, tone: Tone, bold: bool) -> String {
    paint_with(colors(), text, tone, bold)
}

fn paint_with(c: Colors, text: &str, tone: Tone, bold: bool) -> String {
    if c == Colors::None || text.is_empty() {
        return text.to_owned();
    }
    let mut codes = Vec::new();
    if bold {
        codes.push("1".to_owned());
    }
    if let Some(((r, g, b), idx)) = tone.rgb() {
        codes.push(match c {
            Colors::True => format!("38;2;{r};{g};{b}"),
            _ => format!("38;5;{idx}"),
        });
    }
    if codes.is_empty() {
        return text.to_owned();
    }
    format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
}

/// `◆ NIGHTGRID NODE · FIX`, the menu's title line.
pub fn banner(product: &str, page: &str) -> String {
    let left = paint(&format!("◆ NIGHTGRID {}", product.to_uppercase()), Tone::Accent, true);
    if page.is_empty() {
        left
    } else {
        format!("{left} {} {}", paint("·", Tone::Dim, false), paint(&page.to_uppercase(), Tone::Cyan, true))
    }
}

/// A state pill: glyph and word in the state's tone.
pub fn pill(tone: Tone, word: &str) -> String {
    paint(&format!("{} {word}", tone.glyph()), tone, false)
}

/// One pickable entry.
#[derive(Debug, Clone, Default)]
pub struct Item {
    /// What scripts type (`fix journal`); shown in the plain list.
    pub key: String,
    pub label: String,
    /// A short state on the right, like `● on`.
    pub state: Option<(Tone, String)>,
    /// One or two sentences shown under the list for the focused entry.
    pub about: String,
}

impl Item {
    pub fn new(key: impl Into<String>, label: impl Into<String>, about: impl Into<String>) -> Self {
        Self { key: key.into(), label: label.into(), about: about.into(), state: None }
    }

    pub fn state(mut self, tone: Tone, word: impl Into<String>) -> Self {
        self.state = Some((tone, word.into()));
        self
    }
}

#[derive(Debug, Clone)]
pub enum Row {
    Section(String),
    Item(Item),
    /// A line of text that isn't pickable (empty states, notes).
    Note(String),
}

#[derive(Debug, Clone, Default)]
pub struct Menu {
    pub product: String,
    pub page: String,
    /// Shown under the title (the host, who you are).
    pub subtitle: String,
    pub rows: Vec<Row>,
}

impl Menu {
    pub fn new(product: &str, page: &str) -> Self {
        Self { product: product.into(), page: page.into(), ..Default::default() }
    }

    pub fn section(&mut self, s: &str) {
        self.rows.push(Row::Section(s.to_owned()));
    }

    pub fn item(&mut self, i: Item) {
        self.rows.push(Row::Item(i));
    }

    pub fn note(&mut self, s: &str) {
        self.rows.push(Row::Note(s.to_owned()));
    }

    pub fn items(&self) -> Vec<&Item> {
        self.rows
            .iter()
            .filter_map(|r| match r {
                Row::Item(i) => Some(i),
                _ => None,
            })
            .collect()
    }

    /// The menu as plain numbered text (non-terminals, `fix list`).
    pub fn plain(&self) -> String {
        self.text(None, usize::MAX, Colors::None)
    }

    /// Lines of the menu; `focus` marks the focused item.
    fn text(&self, focus: Option<usize>, width: usize, c: Colors) -> String {
        let mut out = String::new();
        let mut n = 0usize;
        let key_w = self.items().iter().map(|i| i.key.chars().count()).max().unwrap_or(0).min(24);
        // States line up in one column on the right.
        let state_max =
            self.items().iter().filter_map(|i| i.state.as_ref()).map(|(_, w)| w.chars().count() + 2).max().unwrap_or(0);
        for r in &self.rows {
            match r {
                Row::Section(s) => {
                    out.push('\n');
                    out.push_str(&format!("  {}\n", paint_with(c, &s.to_uppercase(), Tone::Cyan, true)));
                }
                Row::Note(s) => out.push_str(&format!(
                    "     {}\n",
                    paint_with(c, &clip(s, width.saturating_sub(5)), Tone::Dim, false)
                )),
                Row::Item(i) => {
                    n += 1;
                    let here = focus == Some(n - 1);
                    let marker = if here { paint_with(c, "▸", Tone::Magenta, true) } else { " ".into() };
                    let num = paint_with(c, &format!("{n:>2}"), if here { Tone::Accent } else { Tone::Dim }, false);
                    let key = format!("{:<key_w$}", clip(&i.key, key_w));
                    let key = paint_with(c, &key, if here { Tone::Accent } else { Tone::Plain }, here);
                    let state = i.state.as_ref().map(|(t, w)| paint_with(c, &format!("{} {w}", t.glyph()), *t, false));
                    let room = width.saturating_sub(10 + key_w + state_max + 2);
                    let label = clip(&i.label, room);
                    let pad = room.saturating_sub(label.chars().count());
                    let label = paint_with(c, &label, Tone::Plain, here);
                    match state {
                        Some(s) if width != usize::MAX => {
                            out.push_str(&format!(" {marker} {num}  {key}  {label}{} {s}\n", " ".repeat(pad)))
                        }
                        Some(s) => out.push_str(&format!(" {marker} {num}  {key}  {label}  {s}\n")),
                        None => out.push_str(&format!(" {marker} {num}  {key}  {label}\n")),
                    }
                }
            }
        }
        out
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else if max == 0 {
        String::new()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

/// Whether the full-screen picker can run here.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && !std::env::var("NIGHTGRID_PLAIN").is_ok_and(|v| !v.is_empty() && v != "0")
        && !std::env::var("TERM").is_ok_and(|t| t == "dumb")
}

/// Puts the console in raw mode with VT input and output until dropped.
#[cfg(windows)]
struct Raw {
    saved_in: u32,
    saved_out: u32,
}

#[cfg(windows)]
impl Raw {
    fn on() -> Option<Self> {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
            ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let (mut i, mut o) = (0u32, 0u32);
        // SAFETY: console handles of this process; plain mode words.
        unsafe {
            let (hi, ho) = (GetStdHandle(STD_INPUT_HANDLE), GetStdHandle(STD_OUTPUT_HANDLE));
            if GetConsoleMode(hi, &mut i) == 0 || GetConsoleMode(ho, &mut o) == 0 {
                return None;
            }
            let raw_in =
                (i & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT;
            if SetConsoleMode(hi, raw_in) == 0 || SetConsoleMode(ho, o | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                SetConsoleMode(hi, i);
                return None;
            }
        }
        print!("\x1b[?1049h\x1b[?25l");
        let _ = std::io::stdout().flush();
        Some(Self { saved_in: i, saved_out: o })
    }
}

#[cfg(windows)]
impl Drop for Raw {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        print!("\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
        // SAFETY: restoring the modes read in `on`.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.saved_in);
            SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), self.saved_out);
        }
    }
}

#[cfg(windows)]
fn term_size() -> (usize, usize) {
    use windows_sys::Win32::System::Console::{GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE};
    // SAFETY: a plain out-struct.
    let mut i = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut i) } != 0 {
        let w = (i.srWindow.Right - i.srWindow.Left + 1).max(20) as usize;
        let h = (i.srWindow.Bottom - i.srWindow.Top + 1).max(10) as usize;
        (w, h)
    } else {
        (80, 24)
    }
}

/// Whether more bytes follow an Esc right away (an arrow, not a lone Esc).
#[cfg(windows)]
fn stdin_ready() -> bool {
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    // SAFETY: waits on this process's console input handle.
    unsafe { WaitForSingleObject(GetStdHandle(STD_INPUT_HANDLE), 40) == 0 }
}

/// Puts the terminal in raw mode until dropped.
#[cfg(unix)]
struct Raw {
    saved: libc::termios,
}

#[cfg(unix)]
impl Raw {
    fn on() -> Option<Self> {
        // SAFETY: an all-zero termios is a valid out-buffer for tcgetattr.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd 0 is stdin; t is writable.
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            return None;
        }
        let saved = t;
        t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        t.c_iflag &= !(libc::IXON | libc::ICRNL);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        // SAFETY: t is a valid termios from tcgetattr.
        if unsafe { libc::tcsetattr(0, libc::TCSAFLUSH, &t) } != 0 {
            return None;
        }
        print!("\x1b[?1049h\x1b[?25l");
        let _ = std::io::stdout().flush();
        Some(Self { saved })
    }
}

#[cfg(unix)]
impl Drop for Raw {
    fn drop(&mut self) {
        print!("\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
        // SAFETY: restoring the attributes read in `on`.
        unsafe { libc::tcsetattr(0, libc::TCSAFLUSH, &self.saved) };
    }
}

#[cfg(unix)]
fn term_size() -> (usize, usize) {
    // SAFETY: an all-zero winsize is a valid out-buffer for TIOCGWINSZ.
    let mut w: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: fd 1 is stdout; w is writable.
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut w) } == 0 && w.ws_col > 0 {
        (w.ws_col as usize, w.ws_row.max(10) as usize)
    } else {
        (80, 24)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Home,
    End,
    Enter,
    Quit,
    Digit(u8),
    Other,
}

/// Whether more bytes follow an Esc right away (an arrow, not a lone Esc).
#[cfg(unix)]
fn stdin_ready() -> bool {
    let mut fds = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut fds, 1, 40) > 0 }
}

/// Reads stdin with no user-space buffer, so `stdin_ready` (which asks the
/// kernel or console) sees every byte not yet handed to `Keys`. Rust's
/// `Stdin` buffers 8 KiB: after it swallowed `ESC [ B` in one read, a poll on
/// fd 0 found nothing and every arrow looked like a lone Esc.
struct RawIn;

#[cfg(unix)]
impl Read for RawIn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // SAFETY: fd 0 is stdin; buf is writable for buf.len() bytes.
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

#[cfg(windows)]
impl Read for RawIn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
        let mut n = 0u32;
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: this process's console input handle; buf is writable for len bytes.
        let ok =
            unsafe { ReadFile(GetStdHandle(STD_INPUT_HANDLE), buf.as_mut_ptr(), len, &mut n, std::ptr::null_mut()) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

/// Turns raw input into keys. Reads up to 16 bytes at a time and parses
/// escape sequences from its own buffer; `more` (is input waiting outside
/// this buffer?) is asked only when an Esc is the last byte it holds.
struct Keys<R> {
    input: R,
    buf: [u8; 16],
    pos: usize,
    len: usize,
}

impl<R: Read> Keys<R> {
    fn new(input: R) -> Self {
        Self { input, buf: [0; 16], pos: 0, len: 0 }
    }

    /// Refills the buffer when it is empty; false at end of input or on error.
    fn fill(&mut self) -> bool {
        if self.pos < self.len {
            return true;
        }
        self.pos = 0;
        self.len = self.input.read(&mut self.buf).unwrap_or(0);
        self.len > 0
    }

    fn byte(&mut self) -> Option<u8> {
        if !self.fill() {
            return None;
        }
        self.pos += 1;
        Some(self.buf[self.pos - 1])
    }

    fn next(&mut self, more: impl Fn() -> bool) -> Key {
        let Some(b) = self.byte() else {
            return Key::Quit;
        };
        match b {
            b'\r' | b'\n' | b' ' => Key::Enter,
            b'q' | b'Q' | 3 | 4 => Key::Quit,
            b'k' | 16 => Key::Up,
            b'j' | 14 => Key::Down,
            b'g' => Key::Home,
            b'G' => Key::End,
            d @ b'0'..=b'9' => Key::Digit(d - b'0'),
            0x1b => {
                // A lone Esc quits; Esc [ A and friends are arrows. The rest of
                // a sequence usually came in the same read, so only wait when
                // the Esc ended what we have.
                if self.pos == self.len && !more() {
                    return Key::Quit;
                }
                match self.byte() {
                    Some(b'[' | b'O') => {}
                    _ => return Key::Quit,
                }
                match self.byte() {
                    None => Key::Other,
                    Some(b'A') => Key::Up,
                    Some(b'B') => Key::Down,
                    Some(b'H') => Key::Home,
                    Some(b'F') => Key::End,
                    Some(b'1'..=b'8') => {
                        // Esc [ 5 ~ and the like: drop the rest up to '~'.
                        while let Some(t) = self.byte() {
                            if t == b'~' || t.is_ascii_alphabetic() {
                                break;
                            }
                        }
                        Key::Other
                    }
                    Some(_) => Key::Other,
                }
            }
            _ => Key::Other,
        }
    }
}

/// Shows the menu and returns the index of the picked item (among items),
/// or `None` when the user backs out. Falls back to the plain list.
pub fn pick(menu: &Menu) -> Option<usize> {
    let n = menu.items().len();
    if n == 0 {
        if !menu.rows.is_empty() {
            println!("{}", menu.plain());
        }
        return None;
    }
    if !interactive() {
        return pick_plain(menu);
    }
    let Some(_raw) = Raw::on() else {
        return pick_plain(menu);
    };
    let c = colors();
    let mut focus = 0usize;
    let mut typed: Option<usize> = None;
    let mut keys = Keys::new(RawIn);
    loop {
        let (w, h) = term_size();
        let body = menu.text(Some(focus), w.saturating_sub(1), c);
        let lines: Vec<&str> = body.lines().collect();
        // Keep the focused line in view.
        let focus_line = lines.iter().position(|l| l.contains('▸')).unwrap_or(0);
        let room = h.saturating_sub(8).max(3);
        let start = focus_line.saturating_sub(room.saturating_sub(2)).min(lines.len().saturating_sub(room));
        let mut out = String::from("\x1b[H\x1b[2J");
        out.push_str(&format!(" {}\n", banner(&menu.product, &menu.page)));
        if !menu.subtitle.is_empty() {
            out.push_str(&format!(
                " {}\n",
                paint_with(c, &clip(&menu.subtitle, w.saturating_sub(2)), Tone::Dim, false)
            ));
        }
        out.push_str(&paint_with(c, &"─".repeat(w.saturating_sub(1).min(78)), Tone::Dim, false));
        out.push('\n');
        for l in lines.iter().skip(start).take(room) {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str(&paint_with(c, &"─".repeat(w.saturating_sub(1).min(78)), Tone::Dim, false));
        out.push('\n');
        let about = menu.items()[focus].about.as_str();
        out.push_str(&format!(" {}\n", clip(about, w.saturating_sub(2))));
        let hint = match typed {
            Some(t) => format!("go to {t}… enter to pick"),
            None => "↑↓ j k move · 1-9 jump · enter pick · q back".to_owned(),
        };
        out.push_str(&format!(" {}", paint_with(c, &hint, Tone::Dim, false)));
        print!("{out}");
        let _ = std::io::stdout().flush();
        match keys.next(stdin_ready) {
            Key::Up => focus = focus.checked_sub(1).unwrap_or(n - 1),
            Key::Down => focus = (focus + 1) % n,
            Key::Home => focus = 0,
            Key::End => focus = n - 1,
            Key::Digit(d) => {
                let v = typed.unwrap_or(0) * 10 + usize::from(d);
                if (1..=n).contains(&v) {
                    focus = v - 1;
                    // Jump at once when no longer number could follow.
                    typed = (v * 10 <= n).then_some(v);
                } else {
                    typed = None;
                }
                continue;
            }
            Key::Enter => return Some(focus),
            Key::Quit => return None,
            Key::Other => {}
        }
        typed = None;
    }
}

/// Reads a choice by number or key from a plain line.
fn pick_plain(menu: &Menu) -> Option<usize> {
    print!("{}\n\n", menu.plain_with_title());
    print!("Pick a number or name (enter to go back): ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    choose(menu, line.trim())
}

/// Which item a typed answer names: its number or its key.
pub fn choose(menu: &Menu, answer: &str) -> Option<usize> {
    let items = menu.items();
    if let Ok(v) = answer.parse::<usize>() {
        return (1..=items.len()).contains(&v).then(|| v - 1);
    }
    items.iter().position(|i| !answer.is_empty() && i.key == answer)
}

impl Menu {
    /// The banner, subtitle and the plain list.
    pub fn plain_with_title(&self) -> String {
        let mut s = format!("{}\n", banner(&self.product, &self.page));
        if !self.subtitle.is_empty() {
            s.push_str(&format!("{}\n", paint(&self.subtitle, Tone::Dim, false)));
        }
        let c = colors();
        s.push_str(self.text(None, usize::MAX, c).trim_end());
        s
    }
}

/// Asks a yes/no question on a line; Enter takes `default`. A closed or
/// non-terminal stdin answers no unless `default` is yes.
pub fn confirm(question: &str, default: bool) -> bool {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{} {question} {} ", paint("?", Tone::Accent, true), paint(hint, Tone::Dim, false));
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
        println!();
        return false;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}

/// Asks for a choice of single letters, like `[y]es, [p]review, [N]o`.
/// Returns the lowercase letter, or `default` on Enter.
pub fn ask(question: &str, choices: &str, default: char) -> char {
    print!("{} {question} {} ", paint("?", Tone::Accent, true), paint(choices, Tone::Dim, false));
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
        println!();
        return 'n';
    }
    line.trim().chars().next().map_or(default, |c| c.to_ascii_lowercase())
}

/// Reads one line of free text (trimmed; empty on end of input).
pub fn read_line(prompt: &str) -> String {
    print!("{} {prompt} ", paint("?", Tone::Accent, true));
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line.trim().to_owned()
}

/// For steps that can't be undone: the user types the word itself.
pub fn type_to_confirm(word: &str) -> bool {
    print!("{} Type {} to go ahead: ", paint("!", Tone::Danger, true), paint(word, Tone::Accent, true));
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap_or(0) > 0 && line.trim() == word
}

/// Waits for Enter before going back to the menu (terminals only).
pub fn pause() {
    if interactive() {
        print!("{}", paint("\nPress enter to go back.", Tone::Dim, false));
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Menu {
        let mut m = Menu::new("node", "fix");
        m.section("Detected");
        m.item(Item::new("restart", "Restart nginx.service", "It failed.").state(Tone::Warn, "failed"));
        m.section("Disk & logs");
        m.item(Item::new("journal", "Vacuum the journal", "Keeps 500M."));
        m.note("nothing else");
        m
    }

    #[test]
    fn plain_text_has_no_escapes_and_numbers_items() {
        let t = menu().plain();
        assert!(!t.contains('\x1b'), "{t}");
        assert!(t.contains("  DETECTED") && t.contains("DISK & LOGS"), "{t}");
        assert!(t.contains(" 1  restart  Restart nginx.service  ▲ failed"), "{t}");
        assert!(t.contains(" 2  journal  Vacuum the journal"), "{t}");
        assert!(t.contains("nothing else"));
    }

    #[test]
    fn choices_by_number_or_key() {
        let m = menu();
        assert_eq!(choose(&m, "2"), Some(1));
        assert_eq!(choose(&m, "journal"), Some(1));
        assert_eq!(choose(&m, "0"), None);
        assert_eq!(choose(&m, "3"), None);
        assert_eq!(choose(&m, ""), None);
    }

    #[test]
    fn colours_follow_the_apps_theme() {
        assert_eq!(paint_with(Colors::None, "x", Tone::Accent, true), "x");
        assert_eq!(paint_with(Colors::True, "x", Tone::Accent, false), "\x1b[38;2;243;230;0mx\x1b[0m");
        assert_eq!(paint_with(Colors::Ansi256, "x", Tone::Danger, true), "\x1b[1;38;5;197mx\x1b[0m");
        let t = menu().text(Some(0), 60, Colors::Ansi256);
        assert!(t.contains("▸"));
    }

    #[test]
    fn keys() {
        let k = |b: &[u8]| Keys::new(std::io::Cursor::new(b.to_vec())).next(|| true);
        assert_eq!(k(b"j"), Key::Down);
        assert_eq!(k(b"\x1b[A"), Key::Up);
        assert_eq!(k(b"\r"), Key::Enter);
        assert_eq!(k(b"7"), Key::Digit(7));
        assert_eq!(k(b"q"), Key::Quit);
        assert_eq!(k(b""), Key::Quit);
    }

    /// The sequence bytes are already read; nothing more waits outside the
    /// buffer. This was the bug: every arrow quit the menu.
    #[test]
    fn arrows_already_read_are_not_a_lone_esc() {
        let k = |b: &[u8]| Keys::new(std::io::Cursor::new(b.to_vec())).next(|| false);
        assert_eq!(k(b"\x1b[B"), Key::Down);
        assert_eq!(k(b"\x1b[A"), Key::Up);
        assert_eq!(k(b"\x1bOB"), Key::Down);
        assert_eq!(k(b"\x1b[H"), Key::Home);
        assert_eq!(k(b"\x1b"), Key::Quit);
    }

    /// Several keys in one read (a paste, key repeat) come out one at a time,
    /// and a long sequence (PgDn) does not leak bytes into the next key.
    #[test]
    fn many_keys_in_one_read() {
        let mut keys = Keys::new(std::io::Cursor::new(b"\x1b[B\x1b[6~j\x1b[A\r\x1b".to_vec()));
        let got: Vec<Key> = (0..6).map(|_| keys.next(|| false)).collect();
        assert_eq!(got, [Key::Down, Key::Other, Key::Down, Key::Up, Key::Enter, Key::Quit]);
    }

    /// A sequence split across reads still parses when more input is waiting.
    #[test]
    fn sequence_split_across_reads() {
        struct Chunks(Vec<Vec<u8>>);
        impl Read for Chunks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let c = self.0.remove(0);
                buf[..c.len()].copy_from_slice(&c);
                Ok(c.len())
            }
        }
        let mut keys = Keys::new(Chunks(vec![b"\x1b".to_vec(), b"[B".to_vec()]));
        assert_eq!(keys.next(|| true), Key::Down);
    }

    #[test]
    fn clips_long_text() {
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
    }
}
