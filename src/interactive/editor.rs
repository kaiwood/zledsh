mod completion_menu;

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::OpenOptionsExt;

use crate::completion;
use crate::history::History;
use crate::runtime::Invocation;
use crate::syntax;
use crate::terminal_modes::{
    TTY, attributes, editor_attributes, restore_attributes, set_attributes,
};
use crate::transcript;

pub(super) struct EditorTerminal {
    original: libc::termios,
    editor: libc::termios,
    kills: KillRing,
}

impl EditorTerminal {
    pub(super) fn new() -> io::Result<Self> {
        if unsafe { libc::tcgetpgrp(TTY) != libc::getpgrp() } {
            return Err(io::Error::other("shell must be in the foreground"));
        }
        let original = attributes()?;
        Ok(Self {
            original,
            editor: editor_attributes(original),
            kills: KillRing::default(),
        })
    }

    pub(super) fn with_editor_mode<T>(&self, task: impl FnOnce() -> T) -> io::Result<T> {
        set_attributes(&self.editor)?;
        let result = task();
        set_attributes(&self.original)?;
        Ok(result)
    }

    pub(super) fn clear_kill_ring(&mut self) {
        self.kills.clear();
    }

    pub(super) fn read_line(
        &mut self,
        input: &mut impl BufRead,
        prompt: &str,
        history: &mut History,
        primary: bool,
        shell: &mut Invocation<'_>,
        draft: String,
    ) -> io::Result<(Line, bool)> {
        set_attributes(&self.editor)?;
        let mut stderr = io::stderr().lock();
        let kills = (!history.paused).then_some(&mut self.kills);
        let line = stderr
            .write_all(b"\x1b[?2004h")
            .and_then(|()| write_prompt(&mut stderr, prompt))
            .and_then(|()| {
                read_keys(
                    input,
                    &mut stderr,
                    prompt,
                    primary.then_some(history),
                    kills,
                    shell,
                    (draft, &self.original, &self.editor),
                )
            });
        stderr.write_all(b"\x1b[?2004l")?;
        match &line {
            Ok(Line::Submitted(_)) => stderr.write_all(b"\n")?,
            Ok(Line::Cancelled) => stderr.write_all(b"^C\n")?,
            _ => {}
        }
        stderr.flush()?;
        let pending = transcript::input_pending();
        // Builtins such as `read` run with the user's cooked settings.
        set_attributes(&self.original)?;
        Ok((line?, pending))
    }
}

impl Drop for EditorTerminal {
    fn drop(&mut self) {
        restore_attributes(&self.original);
    }
}

pub(super) enum Line {
    Submitted(String),
    Cancelled,
    End,
}

pub(super) fn write_prompt(output: &mut impl Write, prompt: &str) -> io::Result<()> {
    output.write_all(prompt.as_bytes())?;
    output.flush()
}

#[derive(Default)]
struct KillRing {
    entries: VecDeque<Vec<u8>>,
}

impl KillRing {
    fn remember(&mut self, text: &[u8], consecutive: bool, backward: bool) -> bool {
        const ENTRIES: usize = 8;
        const ENTRY_BYTES: usize = 64 * 1024;
        if text.len() > ENTRY_BYTES || std::str::from_utf8(text).is_err() {
            return false;
        }
        if consecutive {
            if let Some(last) = self
                .entries
                .front_mut()
                .filter(|last| last.len() + text.len() <= ENTRY_BYTES)
            {
                if backward {
                    last.splice(..0, text.iter().copied());
                } else {
                    last.extend_from_slice(text);
                }
                return true;
            }
        }
        self.entries.push_front(text.to_vec());
        self.entries.truncate(ENTRIES);
        true
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn entry(&self, index: usize) -> Option<&[u8]> {
        self.entries.get(index).map(Vec::as_slice)
    }
}

fn cut(
    line: &mut Vec<u8>,
    range: std::ops::Range<usize>,
    kills: Option<&mut KillRing>,
    consecutive: bool,
    backward: bool,
) -> bool {
    if range.is_empty() {
        return consecutive;
    }
    let remembered =
        kills.is_some_and(|kills| kills.remember(&line[range.clone()], consecutive, backward));
    line.drain(range);
    remembered
}

struct CompletionCycle {
    matches: completion::Matches,
    anchor: usize,
    selected: Option<usize>,
    menu_offset: Option<usize>,
}

impl CompletionCycle {
    fn advance(&mut self, line: &mut Vec<u8>, cursor: usize, backwards: bool) -> usize {
        let len = self.matches.len();
        let index = match (self.selected, backwards) {
            (None, false) => 0,
            (None | Some(0), true) => len - 1,
            (Some(index), false) => (index + 1) % len,
            (Some(index), true) => index - 1,
        };
        self.selected = Some(index);
        let suffix = self.matches.extension(self.selected);
        line.splice(self.anchor..cursor, suffix.bytes());
        self.anchor + suffix.len()
    }
}

fn read_keys(
    input: &mut impl BufRead,
    echo: &mut impl Write,
    prompt: &str,
    mut history: Option<&mut History>,
    mut kills: Option<&mut KillRing>,
    shell: &mut Invocation<'_>,
    (draft, cooked, raw): (String, &libc::termios, &libc::termios),
) -> io::Result<Line> {
    let primary = history.is_some();
    // Only the last prompt row belongs to the editable line. Reprint the
    // header only when drawing a fresh prompt, not for each edit.
    let (header, input_prompt) = prompt.split_at(prompt.rfind('\n').map_or(0, |end| end + 1));
    let mut consecutive_cut = false;
    let mut yank = None;
    let mut undo = VecDeque::new();
    let mut before_edit: Option<(Vec<u8>, usize)> = None;
    let mut history_index = None;
    let mut saved_draft = (Vec::new(), 0);
    let mut completion: Option<CompletionCycle> = None;
    echo.write_all(display_draft(draft.as_bytes()).as_bytes())?;
    echo.flush()?;
    let mut line = draft.into_bytes();
    let mut cursor = line.len();
    while let Some(mut key) = read_key(input, shell)? {
        if let Some(before) = before_edit.take().filter(|(text, _)| *text != line) {
            undo.push_back(before);
            if undo.len() > 32 {
                undo.pop_front();
            }
        }
        if key.edits_draft() {
            if line.len() > 64 * 1024 {
                undo.clear();
            } else if std::str::from_utf8(&line).is_ok() {
                before_edit = Some((line.clone(), cursor));
            }
        }
        let previous_yank = yank.take();
        let previous_cut = std::mem::take(&mut consecutive_cut);
        if !matches!(key, Key::Byte(b'\t') | Key::BackTab) {
            if let Some(down) = completion.take().and_then(|cycle| cycle.menu_offset) {
                completion_menu::dismiss(echo, down)?;
            }
        }
        if matches!(key, Key::Byte(18 | 19)) {
            let Some(history) = history.as_deref_mut().filter(|history| !history.paused) else {
                continue;
            };
            history.refresh();
            completion_menu::Screen::new(input_prompt, &display_draft(&line[..cursor]))
                .clear(echo)?;
            let Some(result) = search_history(
                input,
                echo,
                history,
                shell,
                key == Key::Byte(18),
                history_index,
                header,
            )?
            else {
                return Ok(Line::End);
            };
            if let Some((text, position)) = result.selected {
                line = text.as_bytes().to_vec();
                cursor = position;
                history_index = None;
            }
            completion_menu::Screen::new(input_prompt, "")
                .at_start()
                .draw(echo, &line, cursor, None)?;
            echo.flush()?;
            key = result.next;
        }
        if let Key::Byte(byte @ (b' '..=b'~' | 128..=255)) = key {
            if cursor == line.len() {
                line.push(byte);
                cursor += 1;
                echo.write_all(&[byte])?;
                echo.flush()?;
                continue;
            }
        }
        let mut screen =
            completion_menu::Screen::new(input_prompt, &display_draft(&line[..cursor]));
        match key {
            Key::Byte(b'\n' | b'\r') => {
                if cursor != line.len() {
                    screen.draw(echo, &line, line.len(), None)?;
                }
                line.push(b'\n');
                return String::from_utf8(line)
                    .map(Line::Submitted)
                    .map_err(io::Error::other);
            }
            Key::Byte(3) => {
                if cursor != line.len() {
                    screen.draw(echo, &line, line.len(), None)?;
                }
                return Ok(Line::Cancelled);
            }
            Key::Byte(4) if line.is_empty() => return Ok(Line::End),
            Key::Byte(4) | Key::Delete if cursor < line.len() => {
                line.drain(cursor..next_char(&line, cursor));
            }
            Key::Byte(8 | 127) if cursor > 0 => {
                let start = previous_char(&line, cursor);
                line.drain(start..cursor);
                cursor = start;
            }
            Key::Byte(11) if cursor < line.len() => {
                // At an embedded newline, delete it to join the two lines.
                let end = line[cursor..]
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .map_or(line.len(), |offset| cursor + offset.max(1));
                consecutive_cut = cut(
                    &mut line,
                    cursor..end,
                    kills.as_deref_mut(),
                    previous_cut,
                    false,
                );
            }
            Key::Byte(31) => {
                let Some(previous) = undo.pop_back() else {
                    continue;
                };
                (line, cursor) = previous;
                history_index = None;
            }
            Key::Byte(12) => {
                echo.write_all(b"\x1b[H\x1b[2J")?;
                echo.write_all(header.as_bytes())?;
                screen = screen.at_start();
            }
            Key::Byte(21) => {
                consecutive_cut = cut(
                    &mut line,
                    0..cursor,
                    kills.as_deref_mut(),
                    previous_cut,
                    true,
                );
                cursor = 0;
            }
            Key::Byte(23) => {
                let end = cursor;
                while cursor > 0 && line[cursor - 1].is_ascii_whitespace() {
                    cursor -= 1;
                }
                while cursor > 0 && !line[cursor - 1].is_ascii_whitespace() {
                    cursor -= 1;
                }
                consecutive_cut = cut(
                    &mut line,
                    cursor..end,
                    kills.as_deref_mut(),
                    previous_cut,
                    true,
                );
            }
            Key::KillWord | Key::BackwardKillWord => {
                let backward = key == Key::BackwardKillWord;
                let range = if backward {
                    let start = previous_word(&line, cursor);
                    let range = start..cursor;
                    cursor = start;
                    range
                } else {
                    cursor..next_word(&line, cursor)
                };
                consecutive_cut = cut(
                    &mut line,
                    range,
                    kills.as_deref_mut(),
                    previous_cut,
                    backward,
                );
            }
            Key::Byte(25) | Key::YankPop => {
                let Some(kills) = kills.as_deref() else {
                    continue;
                };
                let (start, end, index) = if key == Key::YankPop {
                    let Some((start, index)) = previous_yank else {
                        continue;
                    };
                    (start, cursor, (index + 1) % kills.entries.len())
                } else {
                    (cursor, cursor, 0)
                };
                let Some(text) = kills.entry(index) else {
                    continue;
                };
                line.splice(start..end, text.iter().copied());
                cursor = start + text.len();
                yank = Some((start, index));
            }
            Key::Byte(1) | Key::Home => cursor = 0,
            Key::Byte(5) | Key::End => cursor = line.len(),
            Key::Byte(2) | Key::Left => cursor = previous_char(&line, cursor),
            Key::Byte(6) | Key::Right => cursor = next_char(&line, cursor),
            Key::PreviousWord => cursor = previous_word(&line, cursor),
            Key::NextWord => cursor = next_word(&line, cursor),
            Key::Paste => match read_paste(input)? {
                Paste::Text(bytes) => {
                    let len = bytes.len();
                    line.splice(cursor..cursor, bytes);
                    cursor += len;
                }
                Paste::Rejected => echo.write_all(b"\x07")?,
                Paste::End => return Ok(Line::End),
            },
            Key::Byte(b'\t') | Key::BackTab
                if primary && (cursor == line.len() || line[cursor].is_ascii_whitespace()) =>
            {
                if let Some(cycle) = &mut completion {
                    cursor = cycle.advance(&mut line, cursor, key == Key::BackTab);
                    cycle.menu_offset =
                        screen.draw(echo, &line, cursor, Some((&cycle.matches, cycle.selected)))?;
                    echo.flush()?;
                    continue;
                } else {
                    let Some(matches) =
                        std::str::from_utf8(&line[..cursor])
                            .ok()
                            .and_then(|prefix| {
                                completion::matches(prefix, cursor == line.len(), shell)
                            })
                    else {
                        continue;
                    };
                    let mut suffix = matches.extension(None);
                    if cursor < line.len() && suffix.ends_with(' ') {
                        suffix.pop();
                    }
                    let anchor = cursor;
                    line.splice(cursor..cursor, suffix.bytes());
                    cursor += suffix.len();
                    if matches.len() > 1 {
                        let menu_offset =
                            screen.draw(echo, &line, cursor, Some((&matches, None)))?;
                        completion = Some(CompletionCycle {
                            matches,
                            anchor,
                            selected: None,
                            menu_offset,
                        });
                        echo.flush()?;
                        continue;
                    }
                }
            }
            Key::ExternalEdit => {
                if history.as_deref().is_none_or(|history| history.paused) {
                    continue;
                }
                if cursor != line.len() {
                    screen.draw(echo, &line, line.len(), None)?;
                }
                echo.write_all(b"\x1b[?2004l\n")?;
                echo.flush()?;
                set_attributes(cooked)?;
                let edited = edit_draft(shell, &line);
                set_attributes(raw)?;
                match edited {
                    Ok(Some(text)) => {
                        line = text;
                        cursor = line.len();
                        history_index = None;
                    }
                    Ok(None) => {}
                    Err(error) => writeln!(echo, "zledsh: edit draft: {error}")?,
                }
                echo.write_all(b"\x1b[?2004h")?;
                echo.write_all(header.as_bytes())?;
                screen = screen.at_start();
            }
            Key::LastArgument => {
                let Some(history) = history.as_deref_mut().filter(|history| !history.paused) else {
                    continue;
                };
                history.refresh();
                let Some(word) = history
                    .entries()
                    .next_back()
                    .and_then(syntax::last_argument)
                else {
                    continue;
                };
                line.splice(cursor..cursor, word.bytes());
                cursor += word.len();
            }
            Key::Up | Key::Down | Key::Byte(14 | 16) => {
                let backward = matches!(key, Key::Up | Key::Byte(16));
                let prefix_only = matches!(key, Key::Up | Key::Down);
                let Some(history) = history.as_deref_mut().filter(|history| !history.paused) else {
                    continue;
                };
                if history_index.is_none() {
                    if !backward {
                        continue;
                    }
                    history.refresh();
                    saved_draft = (line.clone(), cursor);
                }
                let matches_prefix = |text: &str| {
                    !prefix_only || text.as_bytes().starts_with(&saved_draft.0[..saved_draft.1])
                };
                if backward {
                    let bound = history_index.unwrap_or_else(|| history.entries().len());
                    history_index = history
                        .entries()
                        .take(bound)
                        .rposition(matches_prefix)
                        .or(history_index);
                    if history_index.is_none() {
                        continue;
                    }
                } else if let Some(index) = history_index {
                    history_index = history
                        .entries()
                        .enumerate()
                        .skip(index + 1)
                        .find_map(|(index, text)| matches_prefix(text).then_some(index));
                }
                undo.clear();
                (line, cursor) = history_index
                    .and_then(|index| history.entries().nth(index))
                    .map_or_else(
                        || saved_draft.clone(),
                        |text| (text.as_bytes().to_vec(), text.len()),
                    );
            }
            Key::Byte(byte @ (b' '..=b'~' | 128..=255 | b'\t')) => {
                line.insert(cursor, byte);
                cursor += 1;
            }
            _ => continue,
        }
        screen.draw(echo, &line, cursor, None)?;
        echo.flush()?;
    }
    if let Some(down) = completion.and_then(|cycle| cycle.menu_offset) {
        completion_menu::dismiss(echo, down)?;
    }
    Ok(Line::End)
}

#[derive(PartialEq, Eq)]
enum Key {
    Byte(u8),
    Up,
    Down,
    Left,
    Right,
    PreviousWord,
    NextWord,
    Home,
    End,
    Paste,
    BackTab,
    Delete,
    KillWord,
    BackwardKillWord,
    YankPop,
    LastArgument,
    ExternalEdit,
    Ignored,
}

impl Key {
    fn edits_draft(&self) -> bool {
        matches!(self,
            Self::Byte(b' '..=b'~' | 128..=255 | b'\t' | 4 | 8 | 11 | 18 | 19 | 21 | 23 | 25 | 127)
            | Self::Delete | Self::Paste | Self::KillWord | Self::BackwardKillWord | Self::YankPop | Self::BackTab
            | Self::LastArgument | Self::ExternalEdit
        )
    }
}

fn read_key(input: &mut impl BufRead, shell: &mut Invocation<'_>) -> io::Result<Option<Key>> {
    match read_editor_byte(input, shell)? {
        Some(27) => read_escape(input).map(Some),
        Some(24) => Ok(Some(if read_editor_byte(input, shell)? == Some(5) {
            Key::ExternalEdit
        } else {
            Key::Ignored
        })),
        byte => Ok(byte.map(Key::Byte)),
    }
}

fn edit_draft(shell: &mut Invocation<'_>, draft: &[u8]) -> io::Result<Option<Vec<u8>>> {
    const LIMIT: u64 = 1024 * 1024;
    if draft.len() as u64 > LIMIT {
        return Err(io::Error::other("draft is too large"));
    }
    let directory = tempfile::tempdir()?;
    let mut file = tempfile::NamedTempFile::new_in(directory.path())?;
    file.write_all(draft)?;
    file.flush()?;
    let editor = shell
        .variable("VISUAL")
        .filter(|editor| !editor.is_empty())
        .or_else(|| shell.variable("EDITOR").filter(|editor| !editor.is_empty()))
        .unwrap_or_else(|| "vi".into());
    let path = file
        .path()
        .to_str()
        .ok_or_else(|| io::Error::other("draft path is not UTF-8"))?;
    if !shell.run_editor(&[editor, path.into()]) {
        return Ok(None);
    }
    // Some editors replace the file on save. Reopen the path in the private directory.
    let mut edited = Vec::new();
    let edited_file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file.path())?;
    if !edited_file.metadata()?.is_file() {
        return Err(io::Error::other("edited draft is not a regular file"));
    }
    edited_file.take(LIMIT + 1).read_to_end(&mut edited)?;
    if edited.len() as u64 > LIMIT
        || !std::str::from_utf8(&edited).is_ok_and(|text| {
            text.chars()
                .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        })
    {
        return Err(io::Error::other("edited draft contains unsupported text"));
    }
    if edited.last() == Some(&b'\n') {
        edited.pop();
    }
    Ok(Some(edited))
}

struct SearchExit<'a> {
    selected: Option<(&'a str, usize)>,
    next: Key,
}

fn search_history<'a>(
    input: &mut impl BufRead,
    echo: &mut impl Write,
    history: &'a History,
    shell: &mut Invocation<'_>,
    mut backward: bool,
    anchor: Option<usize>,
    header: &str,
) -> io::Result<Option<SearchExit<'a>>> {
    let mut query = Vec::new();
    let mut found = None;
    let count = history.entries().len();
    loop {
        let direction = if backward { "reverse" } else { "forward" };
        let prompt = format!("({direction}-i-search)`{}': ", display(&query));
        let text = found.map_or("", |(_, text, _)| text);
        let visible = display(text.as_bytes());
        write!(echo, "{prompt}{visible}")?;
        echo.flush()?;
        let Some(key) = read_key(input, shell)? else {
            return Ok(None);
        };
        completion_menu::Screen::new(&prompt, &visible).clear(echo)?;
        let repeat = match key {
            Key::Byte(18 | 19) => {
                backward = key == Key::Byte(18);
                true
            }
            Key::Byte(8 | 127) => {
                query.truncate(previous_char(&query, query.len()));
                false
            }
            Key::Byte(byte @ (b' '..=b'~' | 128..=255)) => {
                if query.len() >= 4096 {
                    continue;
                }
                query.push(byte);
                false
            }
            Key::Byte(12) => {
                echo.write_all(b"\x1b[H\x1b[2J")?;
                echo.write_all(header.as_bytes())?;
                continue;
            }
            Key::Byte(7) => {
                return Ok(Some(SearchExit {
                    selected: None,
                    next: Key::Ignored,
                }));
            }
            Key::Byte(b'\n' | b'\r') if found.is_none() => {
                return Ok(Some(SearchExit {
                    selected: None,
                    next: Key::Ignored,
                }));
            }
            key => {
                return Ok(Some(SearchExit {
                    selected: found.map(|(_, text, position)| (text, position)),
                    next: key,
                }));
            }
        };
        let pattern = std::str::from_utf8(&query)
            .ok()
            .filter(|text| !text.is_empty());
        if let Some(pattern) = pattern {
            let current = repeat.then_some(found).flatten().map(|(index, _, _)| index);
            let mut entries = history.entries().enumerate();
            let mut matching = |(index, text): (usize, &'a str)| {
                let within = if let Some(current) = current {
                    if backward {
                        index < current
                    } else {
                        index > current
                    }
                } else {
                    anchor.is_none_or(|anchor| {
                        if backward {
                            index <= anchor
                        } else {
                            index >= anchor
                        }
                    })
                };
                within
                    .then(|| text.find(pattern))
                    .flatten()
                    .map(|position| (index, text, position))
            };
            let matched = if backward {
                entries.rev().find_map(matching)
            } else {
                entries.find_map(&mut matching)
            };
            if !repeat || matched.is_some() || count == 0 {
                found = matched;
            }
        } else {
            found = None;
        }
    }
}

fn read_escape(input: &mut impl BufRead) -> io::Result<Key> {
    match read_key_byte(input)? {
        Some(b'[' | b'O') => {}
        Some(b'b' | b'B') => return Ok(Key::PreviousWord),
        Some(b'f' | b'F') => return Ok(Key::NextWord),
        Some(b'd' | b'D') => return Ok(Key::KillWord),
        Some(8 | 127) => return Ok(Key::BackwardKillWord),
        Some(b'y' | b'Y') => return Ok(Key::YankPop),
        Some(b'.') => return Ok(Key::LastArgument),
        _ => return Ok(Key::Ignored),
    }
    let mut sequence = [0; 16];
    for index in 0..sequence.len() {
        let Some(byte) = read_key_byte(input)? else {
            return Ok(Key::Ignored);
        };
        sequence[index] = byte;
        if (0x40..=0x7e).contains(&byte) {
            return Ok(match &sequence[..=index] {
                b"A" => Key::Up,
                b"B" => Key::Down,
                b"C" => Key::Right,
                b"D" => Key::Left,
                b"H" | b"1~" => Key::Home,
                b"F" | b"4~" => Key::End,
                b"200~" => Key::Paste,
                b"3~" => Key::Delete,
                b"Z" => Key::BackTab,
                _ => Key::Ignored,
            });
        }
    }
    Ok(Key::Ignored)
}

enum Paste {
    Text(Vec<u8>),
    Rejected,
    End,
}

fn read_paste(input: &mut impl BufRead) -> io::Result<Paste> {
    const END: &[u8] = b"\x1b[201~";
    const LIMIT: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let mut matched = 0;
    let mut rejected = false;
    while let Some(byte) = read_key_byte(input)? {
        if !rejected {
            if bytes.len() == LIMIT + END.len() {
                rejected = true;
            } else {
                bytes.push(byte);
            }
        }
        if byte == END[matched] {
            matched += 1;
        } else {
            matched = usize::from(byte == END[0]);
        }
        if matched == END.len() {
            if rejected {
                return Ok(Paste::Rejected);
            }
            bytes.truncate(bytes.len() - END.len());
            let Ok(text) = String::from_utf8(bytes) else {
                return Ok(Paste::Rejected);
            };
            if text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\r' | '\n' | '\t'))
            {
                return Ok(Paste::Rejected);
            }
            return Ok(Paste::Text(
                text.replace("\r\n", "\n").replace('\r', "\n").into_bytes(),
            ));
        }
    }
    Ok(Paste::End)
}

fn previous_char(line: &[u8], cursor: usize) -> usize {
    let mut index = cursor.saturating_sub(1);
    while index > 0 && line[index] & 0xc0 == 0x80 {
        index -= 1;
    }
    index
}

fn next_char(line: &[u8], cursor: usize) -> usize {
    let mut index = (cursor + 1).min(line.len());
    while index < line.len() && line[index] & 0xc0 == 0x80 {
        index += 1;
    }
    index
}

fn is_word_char(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.chars().next())
        .is_some_and(char::is_alphanumeric)
}

fn previous_word(line: &[u8], mut cursor: usize) -> usize {
    for word in [false, true] {
        while cursor > 0 {
            let start = previous_char(line, cursor);
            if is_word_char(&line[start..cursor]) != word {
                break;
            }
            cursor = start;
        }
    }
    cursor
}

fn next_word(line: &[u8], mut cursor: usize) -> usize {
    for word in [false, true] {
        while cursor < line.len() {
            let end = next_char(line, cursor);
            if is_word_char(&line[cursor..end]) != word {
                break;
            }
            cursor = end;
        }
    }
    cursor
}

fn display(bytes: &[u8]) -> String {
    visible_text(bytes, false)
}

fn display_draft(bytes: &[u8]) -> String {
    visible_text(bytes, true)
}

fn visible_text(bytes: &[u8], keep_newlines: bool) -> String {
    let mut visible = String::new();
    for ch in String::from_utf8_lossy(bytes).chars() {
        if ch.is_control() && !(keep_newlines && ch == '\n') {
            visible.extend(ch.escape_default());
        } else {
            visible.push(ch);
        }
    }
    visible
}

fn read_editor_byte(
    input: &mut impl BufRead,
    shell: &mut Invocation<'_>,
) -> io::Result<Option<u8>> {
    if let Some(jobs) = &mut shell.foreground {
        while jobs.has_live_jobs() {
            let mut fd = libc::pollfd {
                fd: TTY,
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut fd, 1, 100) } {
                0 => jobs.refresh()?,
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                _ => break,
            }
        }
    }
    read_key_byte(input)
}

pub(super) fn read_key_byte(input: &mut impl BufRead) -> io::Result<Option<u8>> {
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(byte[0])),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}
