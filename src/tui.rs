//! A small full-screen terminal toolkit in the ncurses / dialog tradition,
//! with no library underneath: raw mode through termios, drawing through
//! ANSI escape sequences.  That keeps `dyode setup` working on a box that
//! has nothing but the dyode binary (the offline bundle), on any terminal
//! that speaks VT100 -- a serial console, ssh, the Linux console.
//!
//! Everything is drawn into a [`Canvas`] first.  The canvas is then either
//! painted onto the terminal or, for tests and screenshots, kept as is.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

// --------------------------------------------------------------------------
// Cells and canvas
// --------------------------------------------------------------------------

/// The 8 standard terminal colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
}

impl Color {
    fn code(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
}

impl Style {
    pub const fn new(fg: Color, bg: Color) -> Self {
        Self {
            fg,
            bg,
            bold: false,
        }
    }
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub cells: Vec<Cell>,
    /// Where the terminal cursor should sit (text input), if visible.
    pub cursor: Option<(usize, usize)>,
}

impl Canvas {
    pub fn new(w: usize, h: usize, style: Style) -> Self {
        Self {
            w,
            h,
            cells: vec![Cell { ch: ' ', style }; w * h],
            cursor: None,
        }
    }

    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, style: Style) {
        for row in y..(y + h).min(self.h) {
            for col in x..(x + w).min(self.w) {
                self.cells[row * self.w + col] = Cell { ch: ' ', style };
            }
        }
    }

    pub fn put(&mut self, x: usize, y: usize, ch: char, style: Style) {
        if x < self.w && y < self.h {
            self.cells[y * self.w + x] = Cell { ch, style };
        }
    }

    /// Text clipped to `max` columns.  Returns the number of columns used.
    pub fn text(&mut self, x: usize, y: usize, s: &str, style: Style, max: usize) -> usize {
        let mut n = 0;
        for ch in s.chars() {
            if n >= max {
                break;
            }
            let ch = if ch.is_control() { ' ' } else { ch };
            self.put(x + n, y, ch, style);
            n += 1;
        }
        n
    }

    /// Recolour cells without changing their characters (drop shadows).
    pub fn shade(&mut self, x: usize, y: usize, w: usize, h: usize, style: Style) {
        for row in y..(y + h).min(self.h) {
            for col in x..(x + w).min(self.w) {
                self.cells[row * self.w + col].style = style;
            }
        }
    }

    /// Plain text rendering, one line per row (tests, screenshots).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for row in 0..self.h {
            let line: String = self.cells[row * self.w..(row + 1) * self.w]
                .iter()
                .map(|c| c.ch)
                .collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// HTML rendering with colours, for reviewing screens in a browser.
    pub fn to_html(&self) -> String {
        let css = |c: Color| match c {
            Color::Black => "#000000",
            Color::Red => "#aa0000",
            Color::Green => "#00aa00",
            Color::Yellow => "#aa5500",
            Color::Blue => "#0000aa",
            Color::Magenta => "#aa00aa",
            Color::Cyan => "#00aaaa",
            Color::White => "#aaaaaa",
        };
        let bright = |c: Color| match c {
            Color::White => "#ffffff",
            Color::Yellow => "#ffff55",
            Color::Cyan => "#55ffff",
            Color::Red => "#ff5555",
            Color::Green => "#55ff55",
            other => css(other),
        };
        let mut out = String::from("<pre class=\"term\">");
        for row in 0..self.h {
            let mut col = 0;
            while col < self.w {
                let st = self.cells[row * self.w + col].style;
                let mut run = String::new();
                while col < self.w && self.cells[row * self.w + col].style == st {
                    let ch = self.cells[row * self.w + col].ch;
                    match ch {
                        '<' => run.push_str("&lt;"),
                        '>' => run.push_str("&gt;"),
                        '&' => run.push_str("&amp;"),
                        c => run.push(c),
                    }
                    col += 1;
                }
                let fg = if st.bold { bright(st.fg) } else { css(st.fg) };
                out.push_str(&format!(
                    "<span style=\"color:{fg};background:{}{}\">{run}</span>",
                    css(st.bg),
                    if st.bold { ";font-weight:bold" } else { "" }
                ));
            }
            out.push('\n');
        }
        out.push_str("</pre>");
        out
    }
}

// --------------------------------------------------------------------------
// Keys
// --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    CtrlC,
    CtrlU,
    Char(char),
    /// The terminal changed size (or nothing happened before the timeout).
    Redraw,
}

/// Decode one key from the start of `buf`; returns it and the bytes used.
/// `more` says whether further bytes may still arrive (a lone ESC is only
/// Escape if nothing follows it quickly).
pub fn decode_key(buf: &[u8]) -> Option<(Key, usize)> {
    let b = *buf.first()?;
    let simple = match b {
        b'\r' | b'\n' => Some(Key::Enter),
        b'\t' => Some(Key::Tab),
        127 | 8 => Some(Key::Backspace),
        3 => Some(Key::CtrlC),
        21 => Some(Key::CtrlU),
        _ => None,
    };
    if let Some(k) = simple {
        return Some((k, 1));
    }
    if b == 0x1b {
        if buf.len() == 1 {
            return Some((Key::Esc, 1));
        }
        if buf[1] == b'[' || buf[1] == b'O' {
            // CSI / SS3: parameters then a final byte in 0x40..=0x7e.
            let mut i = 2;
            while i < buf.len() && !(0x40..=0x7e).contains(&buf[i]) {
                i += 1;
            }
            if i >= buf.len() {
                return Some((Key::Esc, 1)); // incomplete: treat ESC alone
            }
            let params = &buf[2..i];
            let key = match (buf[i], params) {
                (b'A', _) => Key::Up,
                (b'B', _) => Key::Down,
                (b'C', _) => Key::Right,
                (b'D', _) => Key::Left,
                (b'H', _) => Key::Home,
                (b'F', _) => Key::End,
                (b'Z', _) => Key::BackTab,
                (b'~', b"1" | b"7") => Key::Home,
                (b'~', b"4" | b"8") => Key::End,
                (b'~', b"3") => Key::Delete,
                (b'~', b"5") => Key::PageUp,
                (b'~', b"6") => Key::PageDown,
                _ => Key::Redraw,
            };
            return Some((key, i + 1));
        }
        return Some((Key::Esc, 1));
    }
    if b < 32 {
        return Some((Key::Redraw, 1));
    }
    // UTF-8
    let len = match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    };
    if buf.len() < len {
        return None;
    }
    let ch = std::str::from_utf8(&buf[..len])
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('?');
    Some((Key::Char(ch), len))
}

// --------------------------------------------------------------------------
// Backends
// --------------------------------------------------------------------------

/// Where frames go and keys come from.
pub trait Backend {
    fn size(&self) -> (usize, usize);
    fn present(&mut self, canvas: &Canvas) -> io::Result<()>;
    fn key(&mut self) -> io::Result<Key>;
    /// Leave full-screen mode (before printing to the normal screen).
    fn suspend(&mut self) {}
    fn resume(&mut self) {}
}

/// The real terminal: raw mode, alternate screen, restored on drop.
pub struct Terminal {
    fd: i32,
    saved: Option<libc::termios>,
    pending: Vec<u8>,
    color: bool,
    active: bool,
}

impl Terminal {
    pub fn open() -> io::Result<Self> {
        let fd = io::stdin().as_raw_fd();
        // SAFETY: plain libc calls on our own stdin.
        if unsafe { libc::isatty(fd) } != 1
            || unsafe { libc::isatty(io::stdout().as_raw_fd()) } != 1
        {
            return Err(io::Error::other("not a terminal"));
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.is_empty() || term == "dumb" {
            return Err(io::Error::other("TERM is dumb or unset"));
        }
        let color = std::env::var_os("NO_COLOR").is_none();
        let mut t = Self {
            fd,
            saved: None,
            pending: Vec::new(),
            color,
            active: false,
        };
        t.resume();
        if t.saved.is_none() {
            return Err(io::Error::other("cannot switch the terminal to raw mode"));
        }
        Ok(t)
    }

    fn raw_on(&mut self) {
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd and termios pointer.
        if unsafe { libc::tcgetattr(self.fd, &mut tio) } != 0 {
            return;
        }
        self.saved.get_or_insert(tio);
        let mut raw = tio;
        // SAFETY: cfmakeraw only writes into raw.
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_oflag |= libc::OPOST | libc::ONLCR; // keep \n -> \r\n for safety
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: valid fd and termios.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &raw) };
    }

    fn raw_off(&mut self) {
        if let Some(saved) = self.saved {
            // SAFETY: restoring what tcgetattr returned.
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &saved) };
        }
    }

    fn wait_input(&self, ms: i32) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
    }

    fn read_more(&mut self) {
        let mut b = [0u8; 64];
        if let Ok(n) = io::stdin().lock().read(&mut b) {
            self.pending.extend_from_slice(&b[..n]);
        }
    }
}

fn sgr(st: Style, color: bool) -> String {
    if color {
        format!(
            "\x1b[0;{}3{};4{}m",
            if st.bold { "1;" } else { "" },
            st.fg.code(),
            st.bg.code()
        )
    } else {
        // Monochrome: anything on a light background is shown in reverse.
        let reverse = matches!(
            st.bg,
            Color::White | Color::Cyan | Color::Yellow | Color::Green
        );
        format!(
            "\x1b[0{}{}m",
            if st.bold { ";1" } else { "" },
            if reverse { ";7" } else { "" }
        )
    }
}

impl Backend for Terminal {
    fn size(&self) -> (usize, usize) {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ fills a winsize.
        let ok = unsafe { libc::ioctl(io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } == 0;
        if ok && ws.ws_col >= 20 && ws.ws_row >= 8 {
            (usize::from(ws.ws_col), usize::from(ws.ws_row))
        } else {
            (80, 24)
        }
    }

    fn present(&mut self, c: &Canvas) -> io::Result<()> {
        let mut out = String::with_capacity(c.w * c.h * 4);
        out.push_str("\x1b[?25l\x1b[H");
        let mut last: Option<Style> = None;
        for row in 0..c.h {
            out.push_str(&format!("\x1b[{};1H", row + 1));
            for col in 0..c.w {
                let cell = c.cells[row * c.w + col];
                if last != Some(cell.style) {
                    out.push_str(&sgr(cell.style, self.color));
                    last = Some(cell.style);
                }
                out.push(cell.ch);
            }
        }
        out.push_str("\x1b[0m");
        if let Some((x, y)) = c.cursor {
            out.push_str(&format!("\x1b[{};{}H\x1b[?25h", y + 1, x + 1));
        }
        let mut so = io::stdout().lock();
        so.write_all(out.as_bytes())?;
        so.flush()
    }

    fn key(&mut self) -> io::Result<Key> {
        loop {
            if self.pending.is_empty() {
                if !self.wait_input(250) {
                    return Ok(Key::Redraw); // lets the caller notice resizes
                }
                self.read_more();
                continue;
            }
            // A lone ESC: give a following sequence 30 ms to arrive.
            if self.pending == [0x1b] && self.wait_input(30) {
                self.read_more();
            }
            if let Some((k, n)) = decode_key(&self.pending) {
                self.pending.drain(..n);
                return Ok(k);
            }
            if !self.wait_input(30) {
                self.pending.clear();
                continue;
            }
            self.read_more();
        }
    }

    fn suspend(&mut self) {
        if self.active {
            let _ = io::stdout().write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
            let _ = io::stdout().flush();
            self.raw_off();
            self.active = false;
        }
    }

    fn resume(&mut self) {
        if !self.active {
            self.raw_on();
            let _ = io::stdout().write_all(b"\x1b[?1049h\x1b[?25l");
            let _ = io::stdout().flush();
            self.active = true;
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.suspend();
    }
}

/// A scripted backend: keys from a list, every frame kept.  Used by the
/// tests and to produce screenshots of a whole wizard run.
pub struct Virtual {
    pub w: usize,
    pub h: usize,
    pub keys: std::collections::VecDeque<Key>,
    pub frames: Vec<String>,
    pub html: Vec<String>,
    /// The screen as it stood each time Enter, Esc or Ctrl-U was pressed:
    /// one picture per answered question, for reviews.
    pub shots: Vec<(String, String)>,
}

impl Virtual {
    pub fn new(w: usize, h: usize, keys: Vec<Key>) -> Self {
        Self {
            w,
            h,
            keys: keys.into(),
            frames: Vec::new(),
            html: Vec::new(),
            shots: Vec::new(),
        }
    }
}

impl Backend for Virtual {
    fn size(&self) -> (usize, usize) {
        (self.w, self.h)
    }
    fn present(&mut self, c: &Canvas) -> io::Result<()> {
        // Compare with colours: moving a highlight changes no characters.
        let h = c.to_html();
        if self.html.last() != Some(&h) {
            self.frames.push(c.to_text());
            self.html.push(h);
        }
        Ok(())
    }
    fn key(&mut self) -> io::Result<Key> {
        let k = self
            .keys
            .pop_front()
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "script ended"))?;
        if matches!(k, Key::Enter | Key::Esc | Key::CtrlU | Key::PageDown) {
            if let (Some(t), Some(h)) = (self.frames.last(), self.html.last()) {
                if self.shots.last().map(|s| &s.1) != Some(h) {
                    self.shots.push((t.clone(), h.clone()));
                }
            }
        }
        Ok(k)
    }
}

/// Parse a key script: "down down enter 'text' esc" (words, or quoted text
/// typed character by character).
pub fn parse_keys(script: &str) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut chars = script.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '\'' {
            chars.next();
            for ch in chars.by_ref() {
                if ch == '\'' {
                    break;
                }
                keys.push(Key::Char(ch));
            }
            continue;
        }
        let word: String = std::iter::from_fn(|| chars.next_if(|c| !c.is_whitespace())).collect();
        let (name, times) = match word.split_once('*') {
            Some((n, t)) => (n.to_string(), t.parse().unwrap_or(1)),
            None => (word.clone(), 1),
        };
        let k = match name.as_str() {
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "enter" => Key::Enter,
            "tab" => Key::Tab,
            "esc" => Key::Esc,
            "bs" => Key::Backspace,
            "ctrl-u" => Key::CtrlU,
            "pgdn" => Key::PageDown,
            "pgup" => Key::PageUp,
            "home" => Key::Home,
            "end" => Key::End,
            other => panic!("unknown key {other:?} in script"),
        };
        for _ in 0..times {
            keys.push(k);
        }
    }
    keys
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_decode() {
        assert_eq!(decode_key(b"\x1b[A"), Some((Key::Up, 3)));
        assert_eq!(decode_key(b"\x1b[6~"), Some((Key::PageDown, 4)));
        assert_eq!(decode_key(b"\x1bOB"), Some((Key::Down, 3)));
        assert_eq!(decode_key(b"\x1b"), Some((Key::Esc, 1)));
        assert_eq!(decode_key(b"\r"), Some((Key::Enter, 1)));
        assert_eq!(decode_key("é".as_bytes()), Some((Key::Char('é'), 2)));
        assert_eq!(decode_key(b"\x1b[Z"), Some((Key::BackTab, 3)));
    }

    #[test]
    fn script() {
        assert_eq!(
            parse_keys("down*2 'ab' enter"),
            vec![
                Key::Down,
                Key::Down,
                Key::Char('a'),
                Key::Char('b'),
                Key::Enter
            ]
        );
    }
}
