//! Neovim owns reader interaction; its TUI is rendered into a Ratatui rectangle.
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};
use rmpv::Value;
use serde_json::{Value as Json, json};
use tui_term::{vt100, widget::PseudoTerminal};

use crate::{
    annotations::{self, Position},
    git::{FileDiff, RowKind},
    theme::Palette,
};

fn error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

pub struct Reader {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
    updates: mpsc::Receiver<()>,
    rpc: UnixStream,
    messages: mpsc::Receiver<Value>,
    actions: Vec<Json>,
    serial: u64,
    size: (u16, u16),
    pub generation: Option<u64>,
    pub seek: bool,
    notes: Vec<Json>,
    _directory: tempfile::TempDir,
}

impl Reader {
    pub fn start(root: &Path) -> io::Result<Self> {
        // /tmp keeps Unix socket paths below macOS's 104-byte limit.
        let directory = tempfile::Builder::new()
            .prefix("rexdiff-")
            .tempdir_in("/tmp")?;
        let socket = directory.path().join("nvim.sock");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(error)?;
        let mut command = CommandBuilder::new("nvim");
        command.args(["--clean", "-n", "-i", "NONE", "-M", "--listen"]);
        command.arg(&socket);
        command.cwd(root);
        command.env("TERM", "xterm-256color");
        // The host supplies colors; the nested terminal has no OSC query responder.
        command.env("NVIM_NOTTYFAST", "1");
        command.env("NVIM_LOG_FILE", directory.path().join("nvim.log"));
        let mut output = pair.master.try_clone_reader().map_err(error)?;
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| error(format!("Cannot start Neovim (install nvim 0.10+): {e}")))?;
        drop(pair.slave);
        let screen = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let (tx, updates) = mpsc::sync_channel(1);
        let parser = screen.clone();
        std::thread::spawn(move || {
            let mut bytes = [0; 16384];
            while let Ok(n) = output.read(&mut bytes) {
                if n == 0 {
                    break;
                }
                parser.lock().unwrap().process(&bytes[..n]);
                let _ = tx.try_send(());
            }
        });
        let until = Instant::now() + Duration::from_secs(3);
        let rpc = loop {
            match UnixStream::connect(&socket) {
                Ok(stream) => break stream,
                Err(e) if Instant::now() >= until => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error(format!("Neovim RPC startup failed: {e}")));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        let mut incoming = match rpc
            .set_write_timeout(Some(Duration::from_secs(2)))
            .and_then(|_| rpc.try_clone())
        {
            Ok(incoming) => incoming,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        let (tx, messages) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(value) = rmpv::decode::read_value(&mut incoming) {
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut reader = Self {
            child,
            master: pair.master,
            screen,
            updates,
            rpc,
            messages,
            actions: Vec::new(),
            serial: 0,
            size: (24, 80),
            generation: None,
            seek: false,
            notes: Vec::new(),
            _directory: directory,
        };
        let info = reader.call("nvim_get_api_info", json!([]))?;
        let channel = info[0]
            .as_u64()
            .ok_or_else(|| error("Invalid Neovim API response"))?;
        let version = reader.call("nvim_eval", json!(["has('nvim-0.10')"]))?;
        if version.as_u64() != Some(1) {
            return Err(error("rexdiff requires Neovim 0.10 or newer"));
        }
        reader.call(
            "nvim_exec_lua",
            json!([include_str!("reader.lua"), [channel]]),
        )?;
        Ok(reader)
    }

    fn send(&mut self, value: Json) -> io::Result<()> {
        let value = rmpv::ext::to_value(value).map_err(error)?;
        rmpv::encode::write_value(&mut self.rpc, &value).map_err(error)
    }

    pub fn notify(&mut self, method: &str, args: Json) -> io::Result<()> {
        self.send(json!([2, method, args]))
    }

    pub fn call(&mut self, method: &str, args: Json) -> io::Result<Value> {
        self.serial += 1;
        self.send(json!([0, self.serial, method, args]))?;
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let value = self
                .messages
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .map_err(error)?;
            if value[0].as_u64() == Some(1) && value[1].as_u64() == Some(self.serial) {
                if !value[2].is_nil() {
                    return Err(error(format!("Neovim: {}", value[2])));
                }
                return Ok(value[3].clone());
            }
            self.receive(value);
        }
    }

    fn receive(&mut self, value: Value) {
        if value[0].as_u64() == Some(2) && value[1].as_str() == Some("rexdiff") {
            if let Ok(action) = rmpv::ext::from_value(value[2][0].clone()) {
                self.actions.push(action);
            }
        }
    }

    pub fn poll(&mut self) -> io::Result<(bool, Vec<Json>)> {
        if let Some(status) = self.child.try_wait().map_err(error)? {
            return Err(error(format!(
                "Neovim exited ({status}); restart rexdiff to reopen the reader"
            )));
        }
        while let Ok(message) = self.messages.try_recv() {
            self.receive(message);
        }
        Ok((
            self.updates.try_iter().count() > 0,
            std::mem::take(&mut self.actions),
        ))
    }

    pub fn load(
        &mut self,
        diff: &FileDiff,
        generation: u64,
        cursor: Position,
        palette: &Palette,
    ) -> io::Result<()> {
        let mut document = document(diff, generation, cursor, palette);
        document["seek"] = json!(self.seek);
        self.call("nvim_exec_lua", json!(["RexDiffLoad(...)", [document]]))?;
        self.generation = Some(generation);
        self.seek = false;
        self.notes.clear();
        Ok(())
    }

    pub fn notes(&mut self, notes: Vec<Json>) -> io::Result<()> {
        if notes != self.notes {
            self.notify("nvim_exec_lua", json!(["RexDiffNotes(...)", [notes]]))?;
            self.notes = notes;
        }
        Ok(())
    }

    pub fn goto(&mut self, cursor: Position, diff: &FileDiff) -> io::Result<()> {
        let byte = diff
            .rows
            .get(cursor.row)
            .map(|r| {
                annotations::text(r)
                    .chars()
                    .take(cursor.col)
                    .map(char::len_utf8)
                    .sum::<usize>()
            })
            .unwrap_or(0);
        self.notify("nvim_win_set_cursor", json!([0, [cursor.row + 1, byte]]))
    }

    pub fn reveal(&mut self, cursor: Position, diff: &FileDiff) -> io::Result<()> {
        self.goto(cursor, diff)?;
        self.notify("nvim_exec_lua", json!(["vim.cmd('normal! zz')", []]))
    }

    pub fn key(&mut self, key: KeyEvent) -> io::Result<()> {
        if let Some(key) = key_notation(key) {
            self.notify("nvim_input", json!([key]))?;
        }
        Ok(())
    }

    pub fn mouse(&mut self, event: MouseEvent, area: Rect) -> io::Result<()> {
        let button = |b| match b {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        };
        let (button, action) = match event.kind {
            MouseEventKind::Down(b) => (button(b), "press"),
            MouseEventKind::Up(b) => (button(b), "release"),
            MouseEventKind::Drag(b) => (button(b), "drag"),
            MouseEventKind::ScrollDown => ("wheel", "down"),
            MouseEventKind::ScrollUp => ("wheel", "up"),
            _ => return Ok(()),
        };
        self.notify(
            "nvim_input_mouse",
            json!([
                button,
                action,
                "",
                0,
                event
                    .row
                    .saturating_sub(area.y)
                    .min(area.height.saturating_sub(1)),
                event
                    .column
                    .saturating_sub(area.x)
                    .min(area.width.saturating_sub(1))
            ]),
        )
    }

    pub fn resize(&mut self, area: Rect) -> io::Result<()> {
        let size = (area.height.max(2), area.width.max(12));
        if size != self.size {
            self.master
                .resize(PtySize {
                    rows: size.0,
                    cols: size.1,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(error)?;
            self.screen
                .lock()
                .unwrap()
                .screen_mut()
                .set_size(size.0, size.1);
            self.size = size;
        }
        Ok(())
    }

    pub fn draw(&self, area: Rect, buffer: &mut Buffer, focused: bool) {
        let parser = self.screen.lock().unwrap();
        PseudoTerminal::new(parser.screen())
            .cursor(tui_term::widget::Cursor::default().visibility(focused))
            .render(area, buffer);
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.rpc.shutdown(std::net::Shutdown::Both);
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn key_notation(key: KeyEvent) -> Option<String> {
    let name = match key.code {
        KeyCode::Char('<') => "lt".into(),
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            return Some(c.to_string());
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "CR".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Backspace => "BS".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => return Some("<S-Tab>".into()),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Delete => "Del".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => return None,
    };
    let mut modifiers = String::new();
    for (flag, name) in [
        (KeyModifiers::CONTROL, "C-"),
        (KeyModifiers::ALT, "M-"),
        (KeyModifiers::SHIFT, "S-"),
        (KeyModifiers::SUPER, "D-"),
    ] {
        if key.modifiers.contains(flag) {
            modifiers.push_str(name);
        }
    }
    Some(format!("<{modifiers}{name}>"))
}

fn color(color: Color) -> Json {
    match color {
        Color::Rgb(r, g, b) => json!(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Indexed(n) => json!(n),
        _ => json!("NONE"),
    }
}

fn document(diff: &FileDiff, generation: u64, cursor: Position, p: &Palette) -> Json {
    let mut highlights = serde_json::Map::new();
    for (kind, bg, gutter, fg) in [
        ("Add", p.add_bg, p.add_gutter, p.green),
        ("Del", p.del_bg, p.del_gutter, p.red),
        ("Hunk", p.hunk_bg, p.hunk_bg, p.faint),
        ("Ctx", Color::Reset, Color::Reset, p.faint),
        ("Meta", Color::Reset, Color::Reset, p.faint),
    ] {
        highlights.insert(format!("Rx{kind}"), json!({"bg":color(bg)}));
        highlights.insert(
            format!("Rx{kind}Gutter"),
            json!({"fg":color(fg),"bg":color(gutter)}),
        );
    }
    for (name, bg, fg) in [
        ("Visual", p.blue, p.selection_fg),
        ("Search", p.yellow, p.search_fg),
        ("IncSearch", p.yellow, p.search_fg),
    ] {
        highlights.insert(
            name.into(),
            json!({"bg":color(bg),"fg":color(fg),"bold":true}),
        );
    }
    highlights.insert("Normal".into(), json!({"fg":"NONE","bg":"NONE"}));
    highlights.insert(
        "RxAnnotation".into(),
        json!({"fg":color(p.annotation),"bg":"NONE","bold":true}),
    );
    let rows:Vec<_>=diff.rows.iter().map(|row| {
        let (kind,sign,number)=match row.kind {
            RowKind::Add=>("Add","+",row.new.map(|n|n.to_string()).unwrap_or_default()),
            RowKind::Del=>("Del","−",row.old.map(|n|n.to_string()).unwrap_or_default()),
            RowKind::Hunk=>("Hunk"," ","⋯".into()), RowKind::Meta=>("Meta"," ",String::new()),
            RowKind::Ctx=>("Ctx"," ",row.new.map(|n|n.to_string()).unwrap_or_default()),
        };
        let mut byte=0;
        let mut spans=Vec::new();
        for seg in &row.segs {
            let start=byte; byte+=seg.text.len();
            if byte==start { continue; }
            let group=format!("Rx{kind}S{}B{}E{}",seg.fg.unwrap_or(255),seg.bold as u8,seg.emph as u8);
            let mut style=json!({"bold":seg.bold});
            if let Some(fg)=seg.fg {
                let (r,g,b)=p.ansi[usize::from(fg).min(15)];
                style["fg"]=color(Color::Rgb(r,g,b));
            }
            if seg.emph && matches!(row.kind,RowKind::Add|RowKind::Del) { style["bg"]=color(if row.kind==RowKind::Add {p.add_emph} else {p.del_emph}); }
            highlights.insert(group.clone(),style);
            spans.push(json!({"start":start,"finish":byte,"group":group}));
        }
        json!({"text":annotations::text(row),"kind":kind,"sign":sign,"number":number,"spans":spans,"old":row.old,"new":row.new})
    }).collect();
    let byte = diff
        .rows
        .get(cursor.row)
        .map(|r| {
            annotations::text(r)
                .chars()
                .take(cursor.col)
                .map(char::len_utf8)
                .sum::<usize>()
        })
        .unwrap_or(0);
    json!({"path":diff.path,"max_lineno":diff.max_lineno,"rows":rows,"generation":generation,"cursor":[cursor.row,byte],"highlights":highlights})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{DRow, Seg};

    fn fixture() -> FileDiff {
        FileDiff {
            revision: 1,
            path: "x.rs".into(),
            context: 3,
            max_lineno: 42,
            rows: vec![
                DRow {
                    kind: RowKind::Hunk,
                    old: None,
                    new: None,
                    segs: vec![Seg {
                        text: "@@ -40,2 +40,2 @@".into(),
                        fg: None,
                        bold: false,
                        emph: false,
                    }],
                },
                DRow {
                    kind: RowKind::Del,
                    old: Some(40),
                    new: None,
                    segs: vec![Seg {
                        text: "é old value".into(),
                        fg: Some(5),
                        bold: false,
                        emph: true,
                    }],
                },
                DRow {
                    kind: RowKind::Add,
                    old: None,
                    new: Some(40),
                    segs: vec![Seg {
                        text: "é new value".into(),
                        fg: Some(2),
                        bold: false,
                        emph: true,
                    }],
                },
                DRow {
                    kind: RowKind::Ctx,
                    old: Some(41),
                    new: Some(41),
                    segs: vec![Seg {
                        text: "last line".into(),
                        fg: None,
                        bold: false,
                        emph: false,
                    }],
                },
            ],
        }
    }

    #[test]
    fn input_notation_preserves_native_keys() {
        let key = |c, m| key_notation(KeyEvent::new(c, m)).unwrap();
        assert_eq!(key(KeyCode::Char('<'), KeyModifiers::NONE), "<lt>");
        assert_eq!(key(KeyCode::Char('é'), KeyModifiers::NONE), "é");
        assert_eq!(key(KeyCode::Char('V'), KeyModifiers::SHIFT), "V");
        assert_eq!(key(KeyCode::Char('f'), KeyModifiers::CONTROL), "<C-f>");
        assert_eq!(key(KeyCode::BackTab, KeyModifiers::SHIFT), "<S-Tab>");
    }

    #[test]
    fn document_uses_source_gutters_and_byte_spans() {
        let d = document(
            &fixture(),
            7,
            Position { row: 1, col: 2 },
            &Palette::from(None),
        );
        assert_eq!(d["rows"][1]["number"], "40");
        assert_eq!(d["rows"][1]["text"], "é old value");
        assert_eq!(d["rows"][1]["spans"][0]["finish"], 12);
        assert_eq!(d["cursor"], json!([1, 3]));
        assert!(
            d["highlights"]["RxDelS5B0E1"]["fg"]
                .as_str()
                .unwrap()
                .starts_with('#')
        );
    }

    #[test]
    #[ignore = "requires installed Neovim 0.10+ and permission for PTYs/local sockets"]
    fn annotation_callouts_update_without_changing_source_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = Reader::start(dir.path()).unwrap();
        reader
            .load(&fixture(), 1, Position::default(), &Palette::from(None))
            .unwrap();
        let rendered = |reader: &mut Reader| {
            assert_eq!(
                reader
                    .call("nvim_buf_line_count", json!([0]))
                    .unwrap()
                    .as_u64(),
                Some(4)
            );
            std::thread::sleep(Duration::from_millis(150));
            let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 24));
            reader.draw(buffer.area, &mut buffer, false);
            let text = reader.screen.lock().unwrap().screen().contents();
            (buffer, text)
        };
        reader
            .notes(vec![json!({"rows":[1,2],"comment":"hi\nsecond line"})])
            .unwrap();
        let (buffer, text) = rendered(&mut reader);
        assert!(text.contains("╭─ 💬 hi"), "{text}");
        assert!(text.contains("│  second line"));
        let rails: Vec<_> = buffer
            .content
            .iter()
            .filter(|c| c.symbol() == "▌")
            .collect();
        assert_eq!(rails.len(), 2);
        assert!(rails.iter().all(|c| c.fg == Palette::from(None).annotation));
        assert_eq!(
            reader
                .call("nvim_eval", json!(["search('second line', 'nw')"]))
                .unwrap()
                .as_u64(),
            Some(0)
        );
        reader
            .notes(vec![json!({"rows":[1,2],"comment":"edited"})])
            .unwrap();
        let (_, text) = rendered(&mut reader);
        assert!(text.contains("💬 edited"));
        assert!(!text.contains("second line"));
        reader.notes(Vec::new()).unwrap();
        let (_, text) = rendered(&mut reader);
        assert!(!text.contains('▌') && !text.contains("edited"));
        reader
            .notes(vec![json!({"rows":[1],"comment":"old file"})])
            .unwrap();
        let mut next = fixture();
        next.path = "next.rs".into();
        reader
            .load(&next, 2, Position::default(), &Palette::from(None))
            .unwrap();
        let (_, text) = rendered(&mut reader);
        assert!(!text.contains('▌') && !text.contains("old file"));
    }

    #[test]
    #[ignore = "requires installed Neovim 0.10+ and permission for PTYs/local sockets"]
    fn neovim_reader_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = Reader::start(dir.path()).unwrap();
        reader
            .load(&fixture(), 7, Position::default(), &Palette::from(None))
            .unwrap();
        let input = |r: &mut Reader, keys: &str| {
            r.call("nvim_input", json!([keys])).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        };
        input(&mut reader, "2G0vl a");
        let (_, actions) = reader.poll().unwrap();
        let selection = actions
            .iter()
            .find(|a| a["action"] == "annotate")
            .expect("annotation callback");
        assert_eq!(selection["generation"], 7);
        assert_eq!(selection["selection"][0]["row"], 1);
        assert_eq!(selection["selection"][0]["text"], "é ");
        input(&mut reader, "/new<CR>");
        assert_eq!(
            reader
                .call("nvim_eval", json!(["line('.')"]))
                .unwrap()
                .as_u64(),
            Some(3)
        );
        input(&mut reader, "ggdd");
        assert_eq!(
            reader
                .call("nvim_buf_line_count", json!([0]))
                .unwrap()
                .as_u64(),
            Some(4)
        );
        input(&mut reader, "<Esc>2G0<C-v>jl a");
        let (_, actions) = reader.poll().unwrap();
        let selection = actions
            .iter()
            .find(|a| a["action"] == "annotate")
            .expect("block selection callback");
        assert_eq!(selection["selection"].as_array().unwrap().len(), 2);
        assert_eq!(selection["selection"][1]["text"], "é ");
        input(&mut reader, "2G0vaw");
        assert_eq!(
            reader
                .call("nvim_eval", json!(["mode()"]))
                .unwrap()
                .as_str(),
            Some("v")
        );
        reader
            .load(&fixture(), 8, Position::default(), &Palette::from(None))
            .unwrap();
        assert_eq!(
            reader
                .call("nvim_eval", json!(["mode()"]))
                .unwrap()
                .as_str(),
            Some("v")
        );
        assert_eq!(
            reader
                .call("nvim_eval", json!(["line('.')"]))
                .unwrap()
                .as_u64(),
            Some(2)
        );
        input(&mut reader, "<Esc>");
        let mut updated = fixture();
        updated.rows[1].segs[0].text = "changed old line".into();
        reader
            .load(&updated, 9, Position::default(), &Palette::from(None))
            .unwrap();
        assert_eq!(
            reader
                .call("nvim_eval", json!(["line('.')"]))
                .unwrap()
                .as_u64(),
            Some(2)
        );
        reader.resize(Rect::new(0, 0, 60, 18)).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            reader
                .call("nvim_eval", json!(["&columns"]))
                .unwrap()
                .as_u64(),
            Some(60)
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 18));
        reader.draw(buffer.area, &mut buffer, true);
        let cell = buffer
            .content
            .iter()
            .find(|c| c.symbol() == "é")
            .expect("rendered Unicode text");
        assert_eq!(cell.bg, Palette::from(None).add_emph);
        let hit = buffer
            .content
            .windows(3)
            .find(|cells| {
                cells[0].symbol() == "n" && cells[1].symbol() == "e" && cells[2].symbol() == "w"
            })
            .unwrap();
        assert_eq!(hit[0].bg, Palette::from(None).yellow);
        let mut tall = fixture();
        tall.path = "long.rs".into();
        tall.max_lineno = 120;
        tall.rows = (1..=120)
            .map(|line| {
                let mut row = fixture().rows[3].clone();
                row.old = Some(line);
                row.new = Some(line);
                row.segs[0].text = format!("source row {line}");
                row
            })
            .collect();
        reader.resize(Rect::new(0, 0, 60, 8)).unwrap();
        reader
            .load(&tall, 10, Position::default(), &Palette::from(None))
            .unwrap();
        reader.reveal(Position { row: 89, col: 0 }, &tall).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            reader
                .screen
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("source row 90")
        );
        let screen_row = reader
            .call("nvim_eval", json!(["winline()"]))
            .unwrap()
            .as_u64()
            .unwrap();
        assert!(
            screen_row > 1 && screen_row < 7,
            "selected code should be centered in the resized reader"
        );
        input(&mut reader, ":e ../outside<CR>");
        let (_, actions) = reader.poll().unwrap();
        assert!(
            actions
                .iter()
                .any(|a| a["action"] == "open" && a["argument"] == "../outside")
        );
        reader.child.kill().unwrap();
        reader.child.wait().unwrap();
        assert!(reader.poll().is_err());
    }
}
