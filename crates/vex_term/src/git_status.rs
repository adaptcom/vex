//! Repository view presentation. Immutable Git snapshots stay separate from
//! navigation, fold state, and unsaved editor buffers.

use crate::{
    picker,
    screen::{Frame, Style},
    sections::{Row, Sections},
};
use std::{collections::BTreeSet, path::PathBuf};
use vex_git::status::{Entry, FileKey, Group, Snapshot};

/// Enrich read-only snapshots on the status worker. Each side is parsed in its
/// full file context; removed lines never inherit the new file's syntax tree.
pub(crate) fn highlight(
    mut result: vex_git::status::Result,
    cancellation: &vex_editor::background::Cancellation,
) -> Option<vex_git::status::Result> {
    use vex_core::{ByteOffset, Document};
    use vex_syntax::{Language, Syntax};
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    for (_, snapshot) in &mut result.views {
        let Ok(snapshot) = snapshot else {
            continue;
        };
        for (key, patch) in &mut snapshot.patches {
            if cancellation.is_cancelled() {
                return None;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            if patch.hunks.is_empty() {
                continue;
            }
            let entry = snapshot.files.iter().find(|entry| entry.key == *key)?;
            let Some((before, after)) = vex_git::status::sources(
                &snapshot.root,
                snapshot.head.as_deref(),
                entry,
                cancellation,
            ) else {
                continue;
            };
            for (old, source) in [(true, before), (false, after)] {
                let document = Document::from(source.as_str());
                let text = document.text();
                let path = if old && key.group == Group::Staged {
                    entry.old_path.as_ref().unwrap_or(&key.path)
                } else {
                    &key.path
                };
                let Some(language) = Language::detect(Some(path), text) else {
                    continue;
                };
                let mut syntax = Syntax::new(language, &document);
                for hunk in &mut patch.hunks {
                    let indices: Vec<_> = hunk
                        .lines
                        .iter()
                        .enumerate()
                        .filter(|(_, line)| {
                            if old {
                                line.text.starts_with('-')
                            } else {
                                line.text.starts_with([' ', '+'])
                            }
                        })
                        .map(|(i, line)| (i, if old { line.before_line } else { line.line }))
                        .filter(|(_, line)| *line < text.len_lines())
                        .collect();
                    let (Some((_, first)), Some((_, last))) = (indices.first(), indices.last())
                    else {
                        continue;
                    };
                    let start = text.line_to_byte(*first);
                    let end = text.line_to_byte((last + 1).min(text.len_lines()));
                    let spans = syntax
                        .highlights_current(ByteOffset(start)..ByteOffset(end), || {
                            cancellation.is_cancelled() || std::time::Instant::now() >= deadline
                        });
                    for (index, line_number) in indices {
                        let line = &mut hunk.lines[index];
                        let code = &line.text[1..];
                        let start = text.line_to_byte(line_number);
                        let end = start + code.len();
                        // Disk/index changes or Git filters can make the source
                        // differ from the displayed patch. Never miscolor it.
                        let source_line = text.line(line_number);
                        if source_line.len_bytes() > code.len() + 2
                            || source_line.to_string().trim_end_matches(['\r', '\n']) != code
                        {
                            continue;
                        }
                        line.highlights = spans
                            .iter()
                            .filter_map(|span| {
                                let a = span.range.start.0.max(start);
                                let b = span.range.end.0.min(end);
                                (a < b).then(|| vex_editor::HighlightSpan {
                                    range: ByteOffset(a - start)..ByteOffset(b - start),
                                    highlight: span.highlight,
                                })
                            })
                            .collect();
                    }
                }
            }
        }
    }
    (!cancellation.is_cancelled()).then_some(result)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Id {
    Header(u8),
    Output(usize),
    Section(Group),
    File(FileKey),
    Info(FileKey, usize),
    Hunk(FileKey, (u64, usize)),
    Line(FileKey, (u64, usize), usize),
    End,
}
impl Id {
    fn parent(&self) -> Option<Self> {
        match self {
            Self::File(file) => Some(Self::Section(file.group)),
            Self::Info(file, _) | Self::Hunk(file, _) => Some(Self::File(file.clone())),
            Self::Line(file, hunk, _) => Some(Self::Hunk(file.clone(), *hunk)),
            _ => None,
        }
    }
    pub fn file(&self) -> Option<&FileKey> {
        match self {
            Self::File(file)
            | Self::Info(file, _)
            | Self::Hunk(file, _)
            | Self::Line(file, _, _) => Some(file),
            _ => None,
        }
    }
}

pub(crate) struct View {
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
    pub expanded: BTreeSet<FileKey>,
    collapsed: BTreeSet<Group>,
    closed_hunks: BTreeSet<(FileKey, (u64, usize))>,
    pub list: Sections<Id>,
    pub unsaved: Vec<Entry>,
    pub refreshing: bool,
    pub help: bool,
    pub operation: Option<String>,
    pub output: Option<(String, bool)>,
    /// Keep the current row and scroll through an index write and its next
    /// successful refresh. Read the current position at each rebuild so moving
    /// while the worker runs never restores an old cursor position.
    pub keep_position: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            snapshot: None,
            error: None,
            expanded: BTreeSet::new(),
            collapsed: BTreeSet::new(),
            closed_hunks: BTreeSet::new(),
            list: Sections::default(),
            unsaved: Vec::new(),
            refreshing: true,
            help: false,
            operation: None,
            output: None,
            keep_position: false,
        }
    }
}

impl View {
    pub fn update(&mut self, result: Result<Snapshot, String>) {
        let refreshed = result.is_ok();
        match result {
            Ok(snapshot) => {
                if self.snapshot.as_ref() == Some(&snapshot) && self.error.is_none() {
                    self.refreshing = false;
                    if self.operation.is_none() {
                        self.keep_position = false;
                    }
                    return;
                }
                self.expanded
                    .retain(|key| snapshot.files.iter().any(|file| file.key == *key));
                self.closed_hunks.retain(|(file, id)| {
                    snapshot.files.iter().any(|entry| entry.key == *file)
                        && snapshot
                            .patches
                            .get(file)
                            .is_none_or(|patch| patch.hunks.iter().any(|hunk| hunk.id == *id))
                });
                self.snapshot = Some(snapshot);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        self.refreshing = false;
        self.rebuild();
        if refreshed && self.operation.is_none() {
            self.keep_position = false;
        }
    }

    pub fn rebuild(&mut self) {
        let mut rows = Vec::new();
        let mut clipped = false;
        let mut add = |id, text, style| {
            if rows.len() < 20_000 {
                rows.push(Row { id, text, style });
            } else {
                clipped = true;
            }
        };
        if let Some(error) = &self.error {
            add(Id::Header(0), error.clone(), Style::Error);
        }
        if let Some((output, error)) = &self.output {
            for (index, line) in output.lines().enumerate() {
                add(
                    Id::Output(index),
                    line.into(),
                    if *error { Style::Error } else { Style::Gutter },
                );
            }
        }
        if let Some(snapshot) = &self.snapshot {
            let branch = if snapshot.branch == "(detached)" {
                "detached HEAD"
            } else {
                &snapshot.branch
            };
            let upstream = snapshot
                .upstream
                .as_ref()
                .map(|name| format!(" → {name}"))
                .unwrap_or_default();
            let counts = match (snapshot.ahead, snapshot.behind) {
                (Some(a), Some(b)) => format!("   ↑{a} ↓{b} (last fetch)"),
                _ => String::new(),
            };
            add(
                Id::Header(1),
                format!("Branch  {branch}{upstream}{counts}"),
                Style::Text,
            );
            add(
                Id::Header(2),
                format!(
                    "Head    {} {}",
                    snapshot
                        .head
                        .as_ref()
                        .map(|head| &head[..head.len().min(10)])
                        .unwrap_or("no commits"),
                    snapshot.subject
                ),
                Style::Text,
            );
            if snapshot.stashes > 0 {
                add(
                    Id::Header(3),
                    format!("Stashes {}", snapshot.stashes),
                    Style::Gutter,
                );
            }
            for group in Group::ALL {
                let files: Vec<_> = snapshot
                    .files
                    .iter()
                    .chain(&self.unsaved)
                    .filter(|file| file.key.group == group)
                    .collect();
                if files.is_empty() {
                    continue;
                }
                add(
                    Id::Section(group),
                    format!(
                        "{} {} ({})",
                        if self.collapsed.contains(&group) {
                            "▸"
                        } else {
                            "▾"
                        },
                        group.title(),
                        files.len()
                    ),
                    Style::Markup(vex_syntax::markup::Attributes::STRONG),
                );
                if self.collapsed.contains(&group) {
                    continue;
                }
                for entry in files {
                    let key = &entry.key;
                    let expandable =
                        matches!(group, Group::Staged | Group::Unstaged | Group::Conflicts);
                    let expanded = self.expanded.contains(key);
                    let arrow = if !expandable {
                        " "
                    } else if expanded {
                        "▾"
                    } else {
                        "▸"
                    };
                    let name = entry
                        .old_path
                        .as_ref()
                        .map(|old| format!("{} → {}", old.display(), key.path.display()))
                        .unwrap_or_else(|| key.path.display().to_string());
                    add(
                        Id::File(key.clone()),
                        format!(
                            "  {arrow} {} {name}{}",
                            entry.status,
                            if entry.submodule { " (submodule)" } else { "" }
                        ),
                        Style::Text,
                    );
                    if !expanded || !expandable {
                        continue;
                    }
                    if let Some(patch) = snapshot.patches.get(key) {
                        for (i, info) in patch.info.iter().enumerate() {
                            add(
                                Id::Info(key.clone(), i),
                                format!("      {info}"),
                                Style::Gutter,
                            );
                        }
                        for hunk in &patch.hunks {
                            let closed = self.closed_hunks.contains(&(key.clone(), hunk.id));
                            add(
                                Id::Hunk(key.clone(), hunk.id),
                                format!("    {} {}", if closed { "▸" } else { "▾" }, hunk.heading),
                                Style::GitModified,
                            );
                            if !closed {
                                for (i, line) in hunk.lines.iter().enumerate() {
                                    let style = match line.text.as_bytes().first() {
                                        Some(b'+') => Style::GitAdded,
                                        Some(b'-') => Style::GitDeleted,
                                        _ => Style::Text,
                                    };
                                    add(
                                        Id::Line(key.clone(), hunk.id, i),
                                        format!("      {}", line.text),
                                        style,
                                    );
                                }
                            }
                        }
                    } else {
                        add(
                            Id::Info(key.clone(), 0),
                            "      Loading diff…".into(),
                            Style::Gutter,
                        );
                    }
                }
            }
            if snapshot.files.is_empty() && self.unsaved.is_empty() {
                add(Id::End, "Working tree clean".into(), Style::Gutter);
            }
            if snapshot.truncated {
                add(
                    Id::End,
                    "File list limited to 10,000 entries".into(),
                    Style::Error,
                );
            }
        } else if self.error.is_none() {
            add(Id::Header(0), "Loading repository…".into(), Style::Gutter);
        }
        if clipped {
            rows.push(Row {
                id: Id::End,
                text: "View limited to 20,000 rows; collapse sections to see more files".into(),
                style: Style::Error,
            });
        }
        let initial = self.list.rows.is_empty()
            || self
                .list
                .rows
                .iter()
                .all(|row| matches!(row.id, Id::Header(_)));
        if self.keep_position {
            self.list.selected = self.list.selected.min(rows.len().saturating_sub(1));
            self.list.top = self.list.top.min(self.list.selected);
            self.list.rows = rows;
        } else {
            self.list.replace(rows, Id::parent);
        }
        if initial && !self.keep_position {
            self.list.selected = self
                .list
                .rows
                .iter()
                .position(|row| matches!(row.id, Id::File(_)))
                .unwrap_or(0);
        }
    }

    /// Toggle the section, file, or hunk under the cursor. Returns whether new
    /// patch data must be requested from the worker.
    pub fn toggle(&mut self) -> Result<bool, &'static str> {
        let Some(row) = self.list.rows.get(self.list.selected) else {
            return Ok(false);
        };
        let mut fetch = false;
        match &row.id {
            Id::Section(group) => {
                if !self.collapsed.remove(group) {
                    self.collapsed.insert(*group);
                }
            }
            Id::File(file)
                if matches!(
                    file.group,
                    Group::Staged | Group::Unstaged | Group::Conflicts
                ) =>
            {
                if !self.expanded.remove(file) {
                    if self.expanded.len() >= vex_git::status::MAX_EXPANDED {
                        return Err("Collapse a file before expanding more (32 file limit)");
                    }
                    self.expanded.insert(file.clone());
                    fetch = true;
                }
            }
            Id::Hunk(file, id) | Id::Line(file, id, _) => {
                let key = (file.clone(), *id);
                if !self.closed_hunks.remove(&key) {
                    self.closed_hunks.insert(key);
                }
            }
            _ => {}
        }
        self.rebuild();
        Ok(fetch)
    }

    /// Select the next/previous structural section, file, or hunk.
    pub fn section(&mut self, down: bool) {
        let structural =
            |row: &Row<Id>| matches!(row.id, Id::Section(_) | Id::File(_) | Id::Hunk(_, _));
        let next = if down {
            self.list
                .rows
                .iter()
                .enumerate()
                .skip(self.list.selected + 1)
                .find(|(_, row)| structural(row))
                .map(|(i, _)| i)
        } else {
            self.list.rows[..self.list.selected]
                .iter()
                .rposition(structural)
        };
        if let Some(next) = next {
            self.list.selected = next;
        }
    }

    pub fn destination(&self) -> Option<(PathBuf, usize, Option<String>)> {
        let row = self.list.rows.get(self.list.selected)?;
        let key = row.id.file()?;
        let snapshot = self.snapshot.as_ref()?;
        let mut line = 0;
        let mut anchor = None;
        if let Id::Hunk(_, id) | Id::Line(_, id, _) = &row.id {
            let hunk = snapshot
                .patches
                .get(key)?
                .hunks
                .iter()
                .find(|h| h.id == *id)?;
            line = hunk.line;
            let selected = if let Id::Line(_, _, index) = row.id {
                hunk.lines.get(index)
            } else {
                hunk.lines
                    .iter()
                    .find(|line| line.text.starts_with('+'))
                    .or(hunk.lines.first())
            };
            if let Some(selected) = selected {
                line = selected.line;
                if selected.text.starts_with([' ', '+']) {
                    anchor = Some(selected.text[1..].into());
                }
            }
        }
        Some((snapshot.root.join(&key.path), line, anchor))
    }

    pub fn paint(&mut self, frame: &mut Frame, reserved: u16, active: bool) {
        let width = frame.width();
        let height = frame.height().saturating_sub(reserved);
        frame.cursor = None;
        if width == 0 || height == 0 {
            return;
        }
        let name = self
            .snapshot
            .as_ref()
            .and_then(|s| s.root.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repository".into());
        let status = height - 1;
        for x in 0..width {
            frame.put(x, status, "─", Style::StatusBorder);
        }
        picker::label(
            frame,
            1,
            status,
            width.saturating_sub(2),
            &format!(
                " Git · {name}{} ",
                if let Some(operation) = &self.operation {
                    format!(" · {operation}")
                } else if self.refreshing {
                    " · refreshing…".into()
                } else {
                    String::new()
                }
            ),
            if active {
                Style::StatusLine
            } else {
                Style::InactiveStatus
            },
        );
        let body = usize::from(status);
        self.list.ensure_visible(body);
        for (i, row) in self
            .list
            .rows
            .iter()
            .skip(self.list.top)
            .take(body)
            .enumerate()
        {
            let selected = self.list.top + i == self.list.selected;
            let style = if selected && active {
                Style::Selection
            } else {
                row.style
            };
            if selected && active {
                frame.fill_row(i as u16, style);
            }
            picker::label(
                frame,
                1,
                i as u16,
                width.saturating_sub(2),
                &row.text,
                style,
            );
            if let Id::Line(file, id, index) = &row.id
                && let Some(line) = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.patches.get(file))
                    .and_then(|p| p.hunks.iter().find(|h| h.id == *id))
                    .and_then(|h| h.lines.get(*index))
                && width > 9
            {
                // The prefix keeps its diff color; code uses semantic colors.
                let spans = if selected && active {
                    &[][..]
                } else {
                    &line.highlights
                };
                let fallback = if selected && active || spans.is_empty() {
                    style
                } else {
                    Style::Text
                };
                for x in 8..width - 1 {
                    frame.put(
                        x,
                        i as u16,
                        " ",
                        if selected && active {
                            Style::Selection
                        } else {
                            Style::Text
                        },
                    );
                }
                paint_code(
                    frame,
                    8,
                    i as u16,
                    width - 9,
                    &line.text[1..],
                    spans,
                    fallback,
                );
            }
        }
        if self.help && width >= 4 && status >= 3 {
            let lines = [
                "j/k, arrows        Move by row",
                "Ctrl-u/d, PgUp/Dn  Scroll",
                "n/p                Next/previous section",
                "Tab                Expand/collapse",
                "Enter              Open file at change",
                "r                  Refresh repository",
                "s/u                Stage/unstage file",
                "c c                Compose commit",
                "Ctrl-w, Space-w    Window commands",
                "q, Escape          Return to document",
                "?                  Close this help",
            ];
            let h = (lines.len() as u16 + 2).min(status);
            let w = width.min(54);
            let y = status - h;
            let x = width - w;
            picker::paint_box(frame, x, y, width, status, " Git status · keys ");
            for (i, line) in lines
                .iter()
                .take(usize::from(h.saturating_sub(2)))
                .enumerate()
            {
                picker::label(
                    frame,
                    x + 1,
                    y + 1 + i as u16,
                    w.saturating_sub(2),
                    line,
                    Style::Text,
                );
            }
        }
    }
}

fn paint_code(
    frame: &mut Frame,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    spans: &[vex_editor::HighlightSpan],
    fallback: Style,
) {
    use unicode_segmentation::UnicodeSegmentation;
    let mut column = 0usize;
    let mut highlight = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        if column >= usize::from(width) {
            break;
        }
        while highlight < spans.len() && spans[highlight].range.end.0 <= byte {
            highlight += 1;
        }
        let style = spans
            .get(highlight)
            .filter(|span| span.range.start.0 <= byte)
            .map_or(fallback, |span| Style::Syntax(span.highlight));
        let size =
            vex_core::display::width(grapheme, column, std::num::NonZeroUsize::new(4).unwrap());
        if grapheme == "\t" {
            for i in 0..size.min(usize::from(width) - column) {
                frame.put(x + (column + i) as u16, y, " ", style);
            }
        } else if column + size <= usize::from(width) {
            frame.put(x + column as u16, y, grapheme, style);
        } else {
            break;
        }
        column += size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path, process::Command};
    use vex_editor::{Highlight, background::Cancellation};
    use vex_git::status::{Batch, Request};
    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn highlighted(before: &str, after: &str) -> (tempfile::TempDir, Snapshot) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.name", "Vex Test"]);
        git(root, &["config", "user.email", "vex@example.invalid"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("main.rs"), before).unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        fs::write(root.join("main.rs"), after).unwrap();
        let cancellation = Cancellation::default();
        let result = Batch {
            request: 1,
            views: vec![Request {
                origin: root.into(),
                expanded: vec![FileKey {
                    group: Group::Unstaged,
                    path: "main.rs".into(),
                }],
            }],
            cancellation: cancellation.clone(),
        }
        .run()
        .unwrap();
        let mut result = highlight(result, &cancellation).unwrap();
        (dir, result.views.remove(0).1.unwrap())
    }
    #[test]
    fn changes_have_syntax_colors_and_diff_markers_and_grey_selection() {
        let (_dir, snapshot) = highlighted(
            "fn old() { let s = \"before\"; }\n",
            "fn new() { let s = \"界after\"; }\n",
        );
        let key = FileKey {
            group: Group::Unstaged,
            path: "main.rs".into(),
        };
        let lines = &snapshot.patches[&key].hunks[0].lines;
        for line in lines {
            assert!(
                line.highlights
                    .iter()
                    .any(|span| span.highlight == Highlight::Keyword)
            );
            assert!(
                line.highlights
                    .iter()
                    .any(|span| span.highlight == Highlight::String)
            );
        }
        let mut view = View::default();
        view.expanded.insert(key);
        view.update(Ok(snapshot));
        let mut frame = Frame::default();
        frame.reset(90, 20).unwrap();
        view.paint(&mut frame, 1, true);
        let removed = view
            .list
            .rows
            .iter()
            .position(|row| row.text.contains("-fn old"))
            .unwrap();
        let added = view
            .list
            .rows
            .iter()
            .position(|row| row.text.contains("+fn new"))
            .unwrap();
        assert_eq!(frame.style_at(7, removed as u16), Some(Style::GitDeleted));
        assert_eq!(frame.style_at(7, added as u16), Some(Style::GitAdded));
        assert_eq!(
            frame.style_at(8, added as u16),
            Some(Style::Syntax(Highlight::Keyword))
        );
        view.list.selected = added;
        frame.reset(90, 20).unwrap();
        view.paint(&mut frame, 1, true);
        assert_eq!(frame.style_at(8, added as u16), Some(Style::Selection));
    }
    #[test]
    fn highlighting_uses_comment_context_outside_the_diff_hunk() {
        let prefix = format!("/*\n{}", "unchanged\n".repeat(20));
        let (_dir, snapshot) = highlighted(
            &format!("{prefix}let before = 1;\n*/\n"),
            &format!("{prefix}let after = 2;\n*/\n"),
        );
        let patch = snapshot.patches.values().next().unwrap();
        assert!(
            !patch.hunks[0]
                .lines
                .iter()
                .any(|line| line.text.contains("/*"))
        );
        for line in patch.hunks[0]
            .lines
            .iter()
            .filter(|line| line.text.starts_with(['+', '-']))
        {
            assert!(
                line.highlights
                    .iter()
                    .any(|span| span.range.start.0 == 0 && span.highlight == Highlight::Comment)
            );
        }
    }

    #[test]
    fn unstaged_changes_after_a_rename_use_the_index_filename_for_both_languages() {
        let prefix = "// unchanged context\n".repeat(20);
        let base = format!("{prefix}fn main() {{}}\n");
        let (dir, _) = highlighted(&base, &base);
        let root = dir.path();
        git(root, &["mv", "main.rs", "renamed.ts"]);
        fs::write(
            root.join("renamed.ts"),
            format!("{prefix}interface Before {{ value: string; }}\n"),
        )
        .unwrap();
        git(root, &["add", "."]);
        fs::write(
            root.join("renamed.ts"),
            format!("{prefix}interface After {{ value: string; }}\n"),
        )
        .unwrap();
        let cancellation = Cancellation::default();
        let key = FileKey {
            group: Group::Unstaged,
            path: "renamed.ts".into(),
        };
        let result = Batch {
            request: 1,
            views: vec![Request {
                origin: root.into(),
                expanded: vec![key.clone()],
            }],
            cancellation: cancellation.clone(),
        }
        .run()
        .unwrap();
        let snapshot = result.views[0].1.as_ref().unwrap();
        assert!(
            snapshot
                .files
                .iter()
                .any(|entry| entry.old_path.as_deref() == Some(Path::new("main.rs")))
        );
        let result = highlight(result, &cancellation).unwrap();
        let patch = &result.views[0].1.as_ref().unwrap().patches[&key];
        let removed = patch
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .find(|line| line.text.starts_with("-interface"))
            .unwrap();
        assert!(
            removed
                .highlights
                .iter()
                .any(|span| span.range.start.0 == 0 && span.highlight == Highlight::Keyword)
        );
    }

    #[test]
    fn syntax_drawing_preserves_tab_stops_and_unicode_boundaries() {
        let mut frame = Frame::default();
        frame.reset(20, 3).unwrap();
        let text = "\tlet 界e\u{301}";
        let spans = vec![vex_editor::HighlightSpan {
            range: vex_core::ByteOffset(1)..vex_core::ByteOffset(4),
            highlight: Highlight::Keyword,
        }];
        paint_code(&mut frame, 2, 1, 12, text, &spans, Style::Text);
        assert!(frame.row_text(1).starts_with("      let 界e\u{301}"));
        assert_eq!(
            frame.style_at(6, 1),
            Some(Style::Syntax(Highlight::Keyword))
        );
    }
}
