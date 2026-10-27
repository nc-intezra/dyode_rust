//! The two faces of `dyode setup`: dialog boxes in the ncurses / whiptail
//! style, and plain numbered questions for pipes, serial consoles without
//! a usable TERM, or `--plain`.  The wizard only talks to [`Ui`].

use crate::tui::{Backend, Canvas, Color, Key, Style};
use std::io::{BufRead, Write};

/// What the person did on a screen.
#[derive(Debug, PartialEq, Eq)]
pub enum Nav<T> {
    Next(T),
    Back,
    Quit,
}

/// One entry of a menu.
#[derive(Clone, Debug)]
pub struct Opt {
    pub label: String,
    pub desc: String,
    /// Shown as a warning (red): a choice that is probably wrong.
    pub warn: bool,
}

impl Opt {
    pub fn new(label: impl Into<String>, desc: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            desc: desc.into(),
            warn: false,
        }
    }
    pub fn warn(mut self, on: bool) -> Self {
        self.warn = on;
        self
    }
}

/// A line of the review screen.
#[derive(Clone, Debug)]
pub enum Line {
    Text(String),
    Head(String),
    Warn(String),
    Code(String),
}

pub type Check<'a> = &'a dyn Fn(&str) -> Result<(), String>;

pub trait Ui {
    /// Progress shown in the title bar.
    fn step(&mut self, n: usize, total: usize);
    fn info(&mut self, title: &str, body: &[String]) -> Nav<()>;
    fn choose(&mut self, title: &str, body: &[String], opts: &[Opt], default: usize) -> Nav<usize>;
    fn text(
        &mut self,
        title: &str,
        body: &[String],
        label: &str,
        default: &str,
        check: Check,
    ) -> Nav<String>;
    fn confirm(
        &mut self,
        title: &str,
        body: &[String],
        yes: &str,
        no: &str,
        default_yes: bool,
    ) -> Nav<bool>;
    /// A message with a single OK (results, errors).
    fn notice(&mut self, title: &str, body: &[String]) -> Nav<()>;
    /// A long, scrollable page with an accept button (the review screen).
    fn review(&mut self, title: &str, lines: &[Line], accept: &str) -> Nav<()>;
    /// Leave the screen alone and print to the normal terminal (the end).
    fn finish(&mut self) {}
    /// Dialog boxes (true) or plain questions (false).
    fn full_screen(&self) -> bool {
        false
    }
}

/// Word-wrap a paragraph; lines that start with spaces keep them.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let indent: String = para.chars().take_while(|c| *c == ' ').collect();
        let mut line = indent.clone();
        for word in para.split_whitespace() {
            // A word longer than a line (a path) is cut, not lost.
            let wchars: Vec<char> = word.chars().collect();
            if wchars.len() > width.saturating_sub(indent.len()) {
                if line.chars().count() > indent.len() {
                    out.push(std::mem::replace(&mut line, indent.clone()));
                }
                let room = width.saturating_sub(indent.len()).max(1);
                let mut chunks = wchars.chunks(room).peekable();
                while let Some(ch) = chunks.next() {
                    line.push_str(&ch.iter().collect::<String>());
                    if chunks.peek().is_some() {
                        out.push(std::mem::replace(&mut line, indent.clone()));
                    }
                }
                continue;
            }
            let cur = line.chars().count();
            if cur > indent.len() && cur + 1 + word.chars().count() > width {
                out.push(std::mem::replace(&mut line, indent.clone()));
            }
            if line.chars().count() > indent.len() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

// ==========================================================================
// Dialog boxes
// ==========================================================================

struct Theme {
    screen: Style,
    bar: Style,
    dialog: Style,
    title: Style,
    border: Style,
    selected: Style,
    selected_warn: Style,
    warn: Style,
    dim: Style,
    field: Style,
    button: Style,
    button_on: Style,
    shadow: Style,
    head: Style,
    code: Style,
}

const THEME: Theme = Theme {
    screen: Style::new(Color::White, Color::Blue),
    bar: Style::new(Color::White, Color::Blue).bold(),
    dialog: Style::new(Color::Black, Color::White),
    title: Style::new(Color::Blue, Color::White).bold(),
    border: Style::new(Color::Black, Color::White),
    selected: Style::new(Color::White, Color::Blue).bold(),
    selected_warn: Style::new(Color::Yellow, Color::Red).bold(),
    warn: Style::new(Color::Red, Color::White),
    dim: Style::new(Color::Blue, Color::White),
    field: Style::new(Color::Black, Color::Cyan),
    button: Style::new(Color::Black, Color::White),
    button_on: Style::new(Color::White, Color::Blue).bold(),
    shadow: Style::new(Color::Black, Color::Black),
    head: Style::new(Color::Blue, Color::White).bold(),
    code: Style::new(Color::Black, Color::White),
};

struct Glyphs {
    h: char,
    v: char,
    tl: char,
    tr: char,
    bl: char,
    br: char,
    lt: char,
    rt: char,
    up: char,
    down: char,
}

fn glyphs() -> Glyphs {
    let utf8 = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .is_some_and(|s| s.to_uppercase().contains("UTF-8") || s.to_uppercase().contains("UTF8"));
    if utf8 {
        Glyphs {
            h: '─',
            v: '│',
            tl: '┌',
            tr: '┐',
            bl: '└',
            br: '┘',
            lt: '┤',
            rt: '├',
            up: '↑',
            down: '↓',
        }
    } else {
        Glyphs {
            h: '-',
            v: '|',
            tl: '+',
            tr: '+',
            bl: '+',
            br: '+',
            lt: '[',
            rt: ']',
            up: '^',
            down: 'v',
        }
    }
}

/// The geometry of one drawn dialog.
struct Frame {
    x: usize,
    /// Inner left column and width (inside the border and padding).
    ix: usize,
    iw: usize,
    /// First row for the content (after the body text).
    cy: usize,
    /// Rows available for content.
    ch: usize,
    /// Row of the buttons.
    by: usize,
}

pub struct DialogUi<B: Backend> {
    pub backend: B,
    step: (usize, usize),
    g: Glyphs,
    hint: String,
}

impl<B: Backend> DialogUi<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            step: (0, 0),
            g: glyphs(),
            hint: String::new(),
        }
    }

    fn canvas(&self) -> Canvas {
        let (w, h) = self.backend.size();
        let mut c = Canvas::new(w, h, THEME.screen);
        let left = format!(" DYODE {} setup", env!("CARGO_PKG_VERSION"));
        c.text(0, 0, &left, THEME.bar, w);
        if self.step.1 > 0 {
            let right = format!("step {} of {} ", self.step.0, self.step.1);
            let x = w.saturating_sub(right.chars().count());
            c.text(x, 0, &right, THEME.bar, w);
        }
        let hint = format!(" {}", self.hint);
        c.text(0, h - 1, &hint, THEME.screen, w);
        c
    }

    /// Draw a centered dialog with `body` text and room for `want` rows of
    /// content and a row of buttons.
    fn frame(
        &self,
        c: &mut Canvas,
        title: &str,
        body: &[String],
        want: usize,
        buttons: &[&str],
        focus: Option<usize>,
    ) -> Frame {
        let w =
            c.w.min(78)
                .saturating_sub(4)
                .max(30)
                .min(c.w.saturating_sub(2));
        let iw = w - 4;
        let mut lines: Vec<String> = body.iter().flat_map(|p| wrap(p, iw)).collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        let fixed = 2 + if buttons.is_empty() { 0 } else { 2 }; // borders + gap + buttons
        let max_h = c.h.saturating_sub(4);
        let mut content = want;
        let mut body_rows = lines.len();
        let gap = usize::from(want > 0 && body_rows > 0);
        // Shrink the content first, then the body, to fit the screen.
        while fixed + body_rows + gap + content > max_h && content > 3.min(want) {
            content -= 1;
        }
        while fixed + body_rows + gap + content > max_h && body_rows > 1 {
            body_rows -= 1;
        }
        let h = (fixed + body_rows + gap + content).min(max_h).max(5);
        let x = (c.w - w) / 2;
        let y = 1 + (c.h.saturating_sub(2) - h) / 2;
        // shadow, box, border
        c.shade(x + 2, y + 1, w, h, THEME.shadow);
        c.fill(x, y, w, h, THEME.dialog);
        let g = &self.g;
        c.put(x, y, g.tl, THEME.border);
        c.put(x + w - 1, y, g.tr, THEME.border);
        c.put(x, y + h - 1, g.bl, THEME.border);
        c.put(x + w - 1, y + h - 1, g.br, THEME.border);
        for i in 1..w - 1 {
            c.put(x + i, y, g.h, THEME.border);
            c.put(x + i, y + h - 1, g.h, THEME.border);
        }
        for j in 1..h - 1 {
            c.put(x, y + j, g.v, THEME.border);
            c.put(x + w - 1, y + j, g.v, THEME.border);
        }
        let t = format!(" {title} ");
        let tl = t.chars().count().min(w - 4);
        let tx = x + (w - tl) / 2;
        c.put(tx - 1, y, g.lt, THEME.border);
        c.text(tx, y, &t, THEME.title, tl);
        c.put(tx + tl, y, g.rt, THEME.border);
        for (i, line) in lines.iter().take(body_rows).enumerate() {
            let shown = if i + 1 == body_rows && body_rows < lines.len() {
                format!("{line} …")
            } else {
                line.clone()
            };
            c.text(x + 2, y + 1 + i, &shown, THEME.dialog, iw);
        }
        let cy = y + 1 + body_rows + gap;
        let by = y + h - 2;
        // buttons, centered
        if !buttons.is_empty() {
            let labels: Vec<String> = buttons.iter().map(|b| format!("< {b} >")).collect();
            let total: usize =
                labels.iter().map(|l| l.chars().count()).sum::<usize>() + 3 * (labels.len() - 1);
            let mut bx = x + (w.saturating_sub(total)) / 2;
            for (i, l) in labels.iter().enumerate() {
                let st = if focus == Some(i) {
                    THEME.button_on
                } else {
                    THEME.button
                };
                bx += c.text(bx, by, l, st, l.chars().count()) + 3;
            }
        }
        Frame {
            x,
            ix: x + 2,
            iw,
            cy,
            ch: content,
            by,
        }
    }

    fn show(&mut self, c: &Canvas) {
        let _ = self.backend.present(c);
    }

    fn key(&mut self) -> Key {
        self.backend.key().unwrap_or(Key::CtrlC)
    }

    /// Ask before leaving.  True = really quit.
    fn really_quit(&mut self) -> bool {
        let saved = std::mem::replace(&mut self.hint, "Enter to confirm, Esc to stay".into());
        let mut on = 1usize;
        let answer = loop {
            let mut c = self.canvas();
            self.frame(
                &mut c,
                "Leave setup?",
                &["Nothing has been written yet. Leave anyway?".into()],
                0,
                &["Leave", "Stay"],
                Some(on),
            );
            self.show(&c);
            match self.key() {
                Key::Left | Key::Right | Key::Tab | Key::BackTab => on ^= 1,
                Key::Enter => break on == 0,
                Key::Char('y' | 'Y') => break true,
                Key::Esc | Key::Char('n' | 'N') => break false,
                Key::CtrlC => break true,
                _ => {}
            }
        };
        self.hint = saved;
        answer
    }
}

impl<B: Backend> Ui for DialogUi<B> {
    fn step(&mut self, n: usize, total: usize) {
        self.step = (n, total);
    }

    fn info(&mut self, title: &str, body: &[String]) -> Nav<()> {
        self.hint = "Enter to continue   Esc back   Ctrl-C quit".into();
        let mut on = 0usize;
        loop {
            let mut c = self.canvas();
            self.frame(&mut c, title, body, 0, &["OK", "Back"], Some(on));
            self.show(&c);
            match self.key() {
                Key::Enter => return if on == 0 { Nav::Next(()) } else { Nav::Back },
                Key::Char(' ') => return Nav::Next(()),
                Key::Left | Key::Right | Key::Tab | Key::BackTab => on ^= 1,
                Key::Esc => return Nav::Back,
                Key::CtrlC | Key::Char('q') if self.really_quit() => return Nav::Quit,
                _ => {}
            }
        }
    }

    fn notice(&mut self, title: &str, body: &[String]) -> Nav<()> {
        self.hint = "Enter to continue".into();
        loop {
            let mut c = self.canvas();
            self.frame(&mut c, title, body, 0, &["OK"], Some(0));
            self.show(&c);
            match self.key() {
                Key::Enter | Key::Char(' ') | Key::Esc => return Nav::Next(()),
                Key::CtrlC => return Nav::Quit,
                _ => {}
            }
        }
    }

    fn choose(&mut self, title: &str, body: &[String], opts: &[Opt], default: usize) -> Nav<usize> {
        self.hint = if self.g.up.is_ascii() {
            "Up/Down move   Enter select   1-9 pick   Esc back   q quit".into()
        } else {
            "↑↓ move   Enter select   1-9 pick   Esc back   q quit".into()
        };
        let mut idx = default.min(opts.len().saturating_sub(1));
        let mut top = 0usize;
        let mut on = 0usize; // 0 = Select, 1 = Back
        loop {
            let mut c = self.canvas();
            let f = self.frame(
                &mut c,
                title,
                body,
                opts.len(),
                &["Select", "Back"],
                Some(on),
            );
            let rows = f.ch.max(1);
            if idx < top {
                top = idx;
            }
            if idx >= top + rows {
                top = idx + 1 - rows;
            }
            let label_w = opts
                .iter()
                .map(|o| o.label.chars().count())
                .max()
                .unwrap_or(0)
                .min(f.iw / 2);
            for (r, i) in (top..opts.len()).take(rows).enumerate() {
                let o = &opts[i];
                let y = f.cy + r;
                let sel = i == idx;
                let base = match (sel, o.warn) {
                    (true, true) => THEME.selected_warn,
                    (true, false) => THEME.selected,
                    (false, true) => THEME.warn,
                    (false, false) => THEME.dialog,
                };
                c.fill(f.ix, y, f.iw, 1, if sel { base } else { THEME.dialog });
                let num = if i < 9 {
                    format!("{} ", i + 1)
                } else {
                    "  ".into()
                };
                let mut x = f.ix + 1;
                x += c.text(x, y, &num, if sel { base } else { THEME.dim }, 2);
                let label = format!("{:<label_w$}", o.label);
                x += c.text(x, y, &label, base, f.iw.saturating_sub(4));
                if !o.desc.is_empty() {
                    let room = (f.ix + f.iw).saturating_sub(x + 2);
                    c.text(
                        x + 2,
                        y,
                        &o.desc,
                        if sel {
                            base
                        } else if o.warn {
                            THEME.warn
                        } else {
                            THEME.dim
                        },
                        room,
                    );
                }
            }
            if top > 0 {
                c.put(f.x + f.iw + 2, f.cy, self.g.up, THEME.dim);
            }
            if top + rows < opts.len() {
                c.put(f.x + f.iw + 2, f.cy + rows - 1, self.g.down, THEME.dim);
            }
            let _ = f.by;
            self.show(&c);
            match self.key() {
                Key::Up | Key::Char('k') => idx = (idx + opts.len() - 1) % opts.len(),
                Key::Down | Key::Char('j') => idx = (idx + 1) % opts.len(),
                Key::Home => idx = 0,
                Key::End => idx = opts.len() - 1,
                Key::PageUp => idx = idx.saturating_sub(rows),
                Key::PageDown => idx = (idx + rows).min(opts.len() - 1),
                Key::Tab | Key::BackTab | Key::Left | Key::Right => on ^= 1,
                Key::Char(d @ '1'..='9') => {
                    let n = d as usize - '1' as usize;
                    if n < opts.len() {
                        return Nav::Next(n);
                    }
                }
                Key::Enter => return if on == 0 { Nav::Next(idx) } else { Nav::Back },
                Key::Esc => return Nav::Back,
                Key::CtrlC | Key::Char('q') if self.really_quit() => return Nav::Quit,
                _ => {}
            }
        }
    }

    fn text(
        &mut self,
        title: &str,
        body: &[String],
        label: &str,
        default: &str,
        check: Check,
    ) -> Nav<String> {
        self.hint = "type, then Enter   Ctrl-U clear   Esc back   Ctrl-C quit".into();
        let mut buf: Vec<char> = default.chars().collect();
        let mut pos = buf.len();
        let mut error: Option<String> = None;
        loop {
            let mut c = self.canvas();
            let f = self.frame(&mut c, title, body, 3, &["OK", "Back"], Some(0));
            c.text(f.ix, f.cy, label, THEME.dialog, f.iw);
            let fy = f.cy + 1;
            c.fill(f.ix, fy, f.iw, 1, THEME.field);
            let room = f.iw.saturating_sub(1);
            let start = pos.saturating_sub(room);
            let shown: String = buf.iter().skip(start).take(room).collect();
            c.text(f.ix, fy, &shown, THEME.field, room);
            c.cursor = Some((f.ix + pos - start, fy));
            if let Some(e) = &error {
                c.text(f.ix, f.cy + 2, &format!("! {e}"), THEME.warn, f.iw);
            }
            self.show(&c);
            match self.key() {
                Key::Enter => {
                    let v: String = buf.iter().collect::<String>().trim().to_string();
                    match check(&v) {
                        Ok(()) => return Nav::Next(v),
                        Err(e) => error = Some(e),
                    }
                }
                Key::Esc => return Nav::Back,
                Key::CtrlC if self.really_quit() => return Nav::Quit,
                Key::Backspace if pos > 0 => {
                    pos -= 1;
                    buf.remove(pos);
                    error = None;
                }
                Key::Delete if pos < buf.len() => {
                    buf.remove(pos);
                }
                Key::Left => pos = pos.saturating_sub(1),
                Key::Right => pos = (pos + 1).min(buf.len()),
                Key::Home => pos = 0,
                Key::End => pos = buf.len(),
                Key::CtrlU => {
                    buf.clear();
                    pos = 0;
                    error = None;
                }
                Key::Char(ch) => {
                    buf.insert(pos, ch);
                    pos += 1;
                    error = None;
                }
                _ => {}
            }
        }
    }

    fn confirm(
        &mut self,
        title: &str,
        body: &[String],
        yes: &str,
        no: &str,
        default_yes: bool,
    ) -> Nav<bool> {
        self.hint = "←→ choose   Enter accept   Esc back   Ctrl-C quit".into();
        if self.g.up.is_ascii() {
            self.hint = "Left/Right choose   Enter accept   Esc back   Ctrl-C quit".into();
        }
        let mut on = usize::from(!default_yes);
        loop {
            let mut c = self.canvas();
            self.frame(&mut c, title, body, 0, &[yes, no], Some(on));
            self.show(&c);
            match self.key() {
                Key::Left | Key::Right | Key::Tab | Key::BackTab => on ^= 1,
                Key::Enter => return Nav::Next(on == 0),
                Key::Char('y' | 'Y') => return Nav::Next(true),
                Key::Char('n' | 'N') => return Nav::Next(false),
                Key::Esc => return Nav::Back,
                Key::CtrlC | Key::Char('q') if self.really_quit() => return Nav::Quit,
                _ => {}
            }
        }
    }

    fn review(&mut self, title: &str, lines: &[Line], accept: &str) -> Nav<()> {
        self.hint = "↑↓ PgUp PgDn scroll   Enter write   Esc back   Ctrl-C quit".into();
        if self.g.up.is_ascii() {
            self.hint = "Up/Down/PgUp/PgDn scroll   Enter write   Esc back   Ctrl-C quit".into();
        }
        let mut top = 0usize;
        let mut on = 0usize;
        loop {
            let mut c = self.canvas();
            let ch = c.h;
            let f = self.frame(&mut c, title, &[], ch, &[accept, "Back"], Some(on));
            // Wrap now that the width is known.
            let mut rows: Vec<(String, Style)> = Vec::new();
            for l in lines {
                let (t, st, indent) = match l {
                    Line::Text(t) => (t.as_str(), THEME.dialog, "  "),
                    Line::Head(t) => (t.as_str(), THEME.head, ""),
                    Line::Warn(t) => (t.as_str(), THEME.warn, "  "),
                    Line::Code(t) => (t.as_str(), THEME.code, ""),
                };
                if matches!(l, Line::Code(_)) {
                    rows.push((t.to_string(), st));
                } else {
                    for (i, w) in wrap(t, f.iw.saturating_sub(2)).into_iter().enumerate() {
                        rows.push((if i == 0 { w } else { format!("{indent}{w}") }, st));
                    }
                }
            }
            let view = f.ch.saturating_sub(1).max(1);
            let max_top = rows.len().saturating_sub(view);
            top = top.min(max_top);
            for (r, (t, st)) in rows.iter().skip(top).take(view).enumerate() {
                c.text(f.ix, f.cy + r, t, *st, f.iw);
            }
            let pct = (top * 100).checked_div(max_top).unwrap_or(100);
            let pos = format!("{pct:>3}%  ({} lines)", rows.len());
            c.text(f.ix, f.cy + view, &pos, THEME.dim, f.iw);
            self.show(&c);
            match self.key() {
                Key::Up | Key::Char('k') => top = top.saturating_sub(1),
                Key::Down | Key::Char('j') => top = (top + 1).min(max_top),
                Key::PageUp => top = top.saturating_sub(view),
                Key::PageDown | Key::Char(' ') => top = (top + view).min(max_top),
                Key::Home => top = 0,
                Key::End => top = max_top,
                Key::Left | Key::Right | Key::Tab | Key::BackTab => on ^= 1,
                Key::Enter => return if on == 0 { Nav::Next(()) } else { Nav::Back },
                Key::Esc => return Nav::Back,
                Key::CtrlC | Key::Char('q') if self.really_quit() => return Nav::Quit,
                _ => {}
            }
        }
    }

    fn finish(&mut self) {
        self.backend.suspend();
    }

    fn full_screen(&self) -> bool {
        true
    }
}

// ==========================================================================
// Plain questions
// ==========================================================================

/// Numbered questions on stdin/stdout.  '<' goes back, 'q' quits.
pub struct PlainUi<R: BufRead, W: Write> {
    input: R,
    out: W,
}

impl<R: BufRead, W: Write> PlainUi<R, W> {
    pub fn new(input: R, out: W) -> Self {
        Self { input, out }
    }

    fn say(&mut self, s: &str) {
        let _ = writeln!(self.out, "{s}");
    }

    fn head(&mut self, title: &str, body: &[String]) {
        self.say("");
        self.say(&format!("=== {title} ==="));
        for p in body {
            for l in wrap(p, 76) {
                self.say(&l);
            }
        }
    }

    /// One line of input; None at end of input.
    fn ask(&mut self, prompt: &str) -> Option<String> {
        let _ = write!(self.out, "{prompt}");
        let _ = self.out.flush();
        let mut s = String::new();
        match self.input.read_line(&mut s) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(s.trim_end_matches(['\n', '\r']).to_string()),
        }
    }
}

impl<R: BufRead, W: Write> Ui for PlainUi<R, W> {
    fn step(&mut self, n: usize, total: usize) {
        self.say(&format!("\n[step {n} of {total}]"));
    }

    fn info(&mut self, title: &str, body: &[String]) -> Nav<()> {
        self.head(title, body);
        match self.ask("[Enter to continue, < back, q quit] ").as_deref() {
            None | Some("q") => Nav::Quit,
            Some("<") => Nav::Back,
            _ => Nav::Next(()),
        }
    }

    fn notice(&mut self, title: &str, body: &[String]) -> Nav<()> {
        self.head(title, body);
        match self.ask("[Enter] ") {
            None => Nav::Quit,
            Some(_) => Nav::Next(()),
        }
    }

    fn choose(&mut self, title: &str, body: &[String], opts: &[Opt], default: usize) -> Nav<usize> {
        loop {
            self.head(title, body);
            for (i, o) in opts.iter().enumerate() {
                let w = if o.warn { "  (!)" } else { "" };
                let d = if o.desc.is_empty() {
                    String::new()
                } else {
                    format!("   - {}", o.desc)
                };
                self.say(&format!("  {}) {}{d}{w}", i + 1, o.label));
            }
            let Some(a) = self.ask(&format!("Choice [{}] (< back, q quit): ", default + 1)) else {
                return Nav::Quit;
            };
            match a.trim() {
                "" => return Nav::Next(default),
                "<" => return Nav::Back,
                "q" => return Nav::Quit,
                n => match n.parse::<usize>() {
                    Ok(k) if (1..=opts.len()).contains(&k) => return Nav::Next(k - 1),
                    _ => self.say(&format!("  ! enter a number between 1 and {}", opts.len())),
                },
            }
        }
    }

    fn text(
        &mut self,
        title: &str,
        body: &[String],
        label: &str,
        default: &str,
        check: Check,
    ) -> Nav<String> {
        self.head(title, body);
        loop {
            let d = if default.is_empty() {
                String::new()
            } else {
                format!(" [{default}]")
            };
            let Some(a) = self.ask(&format!("{label}{d}: ")) else {
                return Nav::Quit;
            };
            match a.trim() {
                "<" => return Nav::Back,
                v => {
                    let v = if v.is_empty() {
                        default.to_string()
                    } else {
                        v.to_string()
                    };
                    match check(&v) {
                        Ok(()) => return Nav::Next(v),
                        Err(e) => self.say(&format!("  ! {e}")),
                    }
                }
            }
        }
    }

    fn confirm(
        &mut self,
        title: &str,
        body: &[String],
        yes: &str,
        no: &str,
        default_yes: bool,
    ) -> Nav<bool> {
        self.head(title, body);
        let d = if default_yes { yes } else { no };
        loop {
            let Some(a) = self.ask(&format!("1) {yes}  2) {no}  [{d}] (< back): ")) else {
                return Nav::Quit;
            };
            match a.trim().to_lowercase().as_str() {
                "" => return Nav::Next(default_yes),
                "1" | "y" | "yes" => return Nav::Next(true),
                "2" | "n" | "no" => return Nav::Next(false),
                "<" => return Nav::Back,
                "q" => return Nav::Quit,
                _ => {}
            }
        }
    }

    fn review(&mut self, title: &str, lines: &[Line], accept: &str) -> Nav<()> {
        self.head(title, &[]);
        for l in lines {
            match l {
                Line::Text(t) => self.say(t),
                Line::Head(t) => self.say(&format!("--- {t} ---")),
                Line::Warn(t) => self.say(&format!("! {t}")),
                Line::Code(t) => self.say(&format!("    {t}")),
            }
        }
        match self
            .ask(&format!(
                "[Enter to {}, < back, q quit] ",
                accept.to_lowercase()
            ))
            .as_deref()
        {
            None | Some("q") => Nav::Quit,
            Some("<") => Nav::Back,
            _ => Nav::Next(()),
        }
    }
}
