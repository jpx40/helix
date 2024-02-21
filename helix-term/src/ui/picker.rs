mod handlers;
mod query;

use crate::{
    alt,
    compositor::{self, Component, Compositor, Context, Event, EventResult},
    ctrl,
    job::Callback,
    key, shift,
    ui::{
        self,
        document::{render_document, LineDecoration, LinePos, TextRenderer},
        EditorView,
    },
};
use futures_util::future::BoxFuture;
use helix_event::AsyncHook;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Nucleo, Utf32String};
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use tui::{
    buffer::Buffer as Surface,
    layout::Constraint,
    text::{Span, Spans},
    widgets::{Block, BorderType, Borders, Cell, Row, Table},
};

use tui::widgets::Widget;

use std::{
    borrow::Cow,
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{self, AtomicUsize},
        Arc,
    },
};

use crate::ui::{Prompt, PromptEvent};
use helix_core::{
    char_idx_at_visual_offset, fuzzy::MATCHER, movement::Direction,
    text_annotations::TextAnnotations, unicode::segmentation::UnicodeSegmentation, Position,
};
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Margin, Modifier, Rect},
    theme::Style,
    view::ViewPosition,
    Document, DocumentId, Editor,
};

use super::overlay::Overlay;

use self::handlers::PreviewHighlightHandler;

pub const ID: &str = "picker";

pub const MIN_AREA_WIDTH_FOR_PREVIEW: u16 = 72;
/// Biggest file size to preview in bytes
pub const MAX_FILE_SIZE_FOR_PREVIEW: u64 = 10 * 1024 * 1024;

#[derive(PartialEq, Eq, Hash)]
pub enum PathOrId {
    Id(DocumentId),
    Path(Arc<Path>),
}

impl PathOrId {
    fn get_canonicalized(self) -> Self {
        use PathOrId::*;
        match self {
            Path(path) => Path(helix_stdx::path::canonicalize(&path).into()),
            Id(id) => Id(id),
        }
    }
}

impl From<PathBuf> for PathOrId {
    fn from(v: PathBuf) -> Self {
        Self::Path(v.as_path().into())
    }
}

impl From<DocumentId> for PathOrId {
    fn from(v: DocumentId) -> Self {
        Self::Id(v)
    }
}

type FileCallback<T> = Box<dyn Fn(&Editor, &T) -> Option<FileLocation>>;

/// File path and range of lines (used to align and highlight lines)
pub type FileLocation = (PathOrId, Option<(usize, usize)>);

pub enum CachedPreview {
    Document(Box<Document>),
    Binary,
    LargeFile,
    NotFound,
}

// We don't store this enum in the cache so as to avoid lifetime constraints
// from borrowing a document already opened in the editor.
pub enum Preview<'picker, 'editor> {
    Cached(&'picker CachedPreview),
    EditorDocument(&'editor Document),
}

impl Preview<'_, '_> {
    fn document(&self) -> Option<&Document> {
        match self {
            Preview::EditorDocument(doc) => Some(doc),
            Preview::Cached(CachedPreview::Document(doc)) => Some(doc),
            _ => None,
        }
    }

    /// Alternate text to show for the preview.
    fn placeholder(&self) -> &str {
        match *self {
            Self::EditorDocument(_) => "<Invalid file location>",
            Self::Cached(preview) => match preview {
                CachedPreview::Document(_) => "<Invalid file location>",
                CachedPreview::Binary => "<Binary file>",
                CachedPreview::LargeFile => "<File too large to preview>",
                CachedPreview::NotFound => "<File not found>",
            },
        }
    }
}

fn inject_nucleo_item<T, D>(
    injector: &nucleo::Injector<T>,
    columns: &[Column<T, D>],
    item: T,
    editor_data: &D,
) {
    let column_texts: Vec<Utf32String> = columns
        .iter()
        .filter(|column| column.filter)
        .map(|column| column.format_text(&item, editor_data).into())
        .collect();
    injector.push(item, |dst| {
        for (i, text) in column_texts.into_iter().enumerate() {
            dst[i] = text;
        }
    });
}

pub struct Injector<T, D> {
    dst: nucleo::Injector<T>,
    columns: Arc<[Column<T, D>]>,
    editor_data: Arc<D>,
    version: usize,
    picker_version: Arc<AtomicUsize>,
}

impl<I, D> Clone for Injector<I, D> {
    fn clone(&self) -> Self {
        Injector {
            dst: self.dst.clone(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version,
            picker_version: self.picker_version.clone(),
        }
    }
}

#[derive(Error, Debug)]
#[error("picker has been shut down")]
pub struct InjectorShutdown;

impl<T, D> Injector<T, D> {
    pub fn push(&self, item: T) -> Result<(), InjectorShutdown> {
        if self.version != self.picker_version.load(atomic::Ordering::Relaxed) {
            return Err(InjectorShutdown);
        }

        inject_nucleo_item(&self.dst, &self.columns, item, &self.editor_data);
        Ok(())
    }
}

type ColumnFormatFn<T, D> = for<'a> fn(&'a T, &'a D) -> Cell<'a>;

pub struct Column<T, D> {
    name: String,
    format: ColumnFormatFn<T, D>,
    /// Whether the column should be passed to nucleo for matching and filtering.
    /// `DynamicPicker` uses this so that the dynamic column (for example regex in
    /// global search) is not used for filtering twice.
    filter: bool,
}

impl<T, D> Column<T, D> {
    pub fn new(name: impl Into<String>, format: ColumnFormatFn<T, D>) -> Self {
        Self {
            name: name.into(),
            format,
            filter: true,
        }
    }

    pub fn without_filtering(mut self) -> Self {
        self.filter = false;
        self
    }

    fn format<'a>(&self, item: &'a T, data: &'a D) -> Cell<'a> {
        (self.format)(item, data)
    }

    fn format_text<'a>(&self, item: &'a T, data: &'a D) -> Cow<'a, str> {
        let text: String = self.format(item, data).content.into();
        text.into()
    }
}

pub struct Picker<T: 'static + Send + Sync, D: 'static> {
    column_names: Arc<[Arc<str>]>,
    columns: Arc<[Column<T, D>]>,
    primary_column: usize,
    editor_data: Arc<D>,
    version: Arc<AtomicUsize>,
    matcher: Nucleo<T>,

    /// Current height of the completions box
    completion_height: u16,

    cursor: u32,
    prompt: Prompt,
    query: query::PickerQuery,

    /// Whether to show the preview panel (default true)
    show_preview: bool,
    /// Constraints for tabular formatting
    widths: Vec<Constraint>,

    callback_fn: PickerCallback<T>,

    pub truncate_start: bool,
    /// Caches paths to documents
    preview_cache: HashMap<Arc<Path>, CachedPreview>,
    read_buffer: Vec<u8>,
    /// Given an item in the picker, return the file path and line number to display.
    file_fn: Option<FileCallback<T>>,
    /// An event handler for syntax highlighting the currently previewed file.
    preview_highlight_handler: Sender<Arc<Path>>,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> Picker<T, D> {
    pub fn stream(columns: Vec<Column<T, D>>, editor_data: D) -> (Nucleo<T>, Injector<T, D>) {
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let streamer = Injector {
            dst: matcher.injector(),
            columns: columns.into(),
            editor_data: Arc::new(editor_data),
            version: 0,
            picker_version: Arc::new(AtomicUsize::new(0)),
        };
        (matcher, streamer)
    }

    pub fn new(
        columns: Vec<Column<T, D>>,
        primary_column: usize,
        options: Vec<T>,
        editor_data: D,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let injector = matcher.injector();
        for item in options {
            inject_nucleo_item(&injector, &columns, item, &editor_data);
        }
        Self::with(
            matcher,
            columns.into(),
            primary_column,
            Arc::new(editor_data),
            Arc::new(AtomicUsize::new(0)),
            callback_fn,
        )
    }

    pub fn with_stream(
        matcher: Nucleo<T>,
        primary_column: usize,
        injector: Injector<T, D>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        Self::with(
            matcher,
            injector.columns,
            primary_column,
            injector.editor_data,
            injector.picker_version,
            callback_fn,
        )
    }

    fn with(
        matcher: Nucleo<T>,
        columns: Arc<[Column<T, D>]>,
        default_column: usize,
        editor_data: Arc<D>,
        version: Arc<AtomicUsize>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        assert!(!columns.is_empty());

        let prompt = Prompt::new(
            "".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {},
        );

        let column_names: Arc<[_]> = columns
            .iter()
            .map(|column| column.name.as_str().into())
            .collect();
        let widths = columns
            .iter()
            .map(|column| Constraint::Length(column.name.chars().count() as u16))
            .collect();

        Self {
            column_names,
            columns,
            primary_column: default_column,
            matcher,
            editor_data,
            version,
            cursor: 0,
            prompt,
            query: query::PickerQuery::default(),
            truncate_start: true,
            show_preview: true,
            callback_fn: Box::new(callback_fn),
            completion_height: 0,
            widths,
            preview_cache: HashMap::new(),
            read_buffer: Vec::with_capacity(1024),
            file_fn: None,
            preview_highlight_handler: PreviewHighlightHandler::<T, D>::default().spawn(),
        }
    }

    pub fn injector(&self) -> Injector<T, D> {
        Injector {
            dst: self.matcher.injector(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version.load(atomic::Ordering::Relaxed),
            picker_version: self.version.clone(),
        }
    }

    pub fn truncate_start(mut self, truncate_start: bool) -> Self {
        self.truncate_start = truncate_start;
        self
    }

    pub fn with_preview(
        mut self,
        preview_fn: impl Fn(&Editor, &T) -> Option<FileLocation> + 'static,
    ) -> Self {
        self.file_fn = Some(Box::new(preview_fn));
        // assumption: if we have a preview we are matching paths... If this is ever
        // not true this could be a separate builder function
        self.matcher.update_config(Config::DEFAULT.match_paths());
        self
    }

    pub fn with_line(mut self, line: String, editor: &Editor) -> Self {
        self.prompt.set_line(line, editor);
        self.handle_prompt_change();
        self
    }

    pub fn set_options(&mut self, new_options: Vec<T>) {
        self.matcher.restart(false);
        let injector = self.matcher.injector();
        for item in new_options {
            inject_nucleo_item(&injector, &self.columns, item, &self.editor_data);
        }
    }

    /// Move the cursor by a number of lines, either down (`Forward`) or up (`Backward`)
    pub fn move_by(&mut self, amount: u32, direction: Direction) {
        let len = self.matcher.snapshot().matched_item_count();

        if len == 0 {
            // No results, can't move.
            return;
        }

        match direction {
            Direction::Forward => {
                self.cursor = self.cursor.saturating_add(amount) % len;
            }
            Direction::Backward => {
                self.cursor = self.cursor.saturating_add(len).saturating_sub(amount) % len;
            }
        }
    }

    /// Move the cursor down by exactly one page. After the last page comes the first page.
    pub fn page_up(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Backward);
    }

    /// Move the cursor up by exactly one page. After the first page comes the last page.
    pub fn page_down(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Forward);
    }

    /// Move the cursor to the first entry
    pub fn to_start(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the last entry
    pub fn to_end(&mut self) {
        self.cursor = self
            .matcher
            .snapshot()
            .matched_item_count()
            .saturating_sub(1);
    }

    pub fn selection(&self) -> Option<&T> {
        self.matcher
            .snapshot()
            .get_matched_item(self.cursor)
            .map(|item| item.data)
    }

    fn primary_query(&self) -> Arc<str> {
        self.query
            .get(&self.column_names[self.primary_column])
            .cloned()
            .unwrap_or_else(|| "".into())
    }

    fn header_height(&self) -> u16 {
        if self.columns.len() > 1 {
            1
        } else {
            0
        }
    }

    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
    }

    fn prompt_handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let EventResult::Consumed(_) = self.prompt.handle_event(event, cx) {
            self.handle_prompt_change();
        }
        EventResult::Consumed(None)
    }

    fn handle_prompt_change(&mut self) {
        // TODO: better track how the pattern has changed
        let line = self.prompt.line();
        let new_query = query::parse(&self.column_names, self.primary_column, line);
        if new_query != self.query {
            for (i, column) in self
                .columns
                .iter()
                .filter(|column| column.filter)
                .enumerate()
            {
                let pattern: &str = new_query
                    .get(column.name.as_str())
                    .map(|f| &**f)
                    .unwrap_or("");
                let append = self
                    .query
                    .get(column.name.as_str())
                    .map(|old_pattern| pattern.starts_with(&**old_pattern))
                    .unwrap_or(false);

                self.matcher.pattern.reparse(
                    i,
                    pattern,
                    CaseMatching::Smart,
                    Normalization::Smart,
                    append,
                );
            }
            self.query = new_query;
        }
    }

    fn current_file(&self, editor: &Editor) -> Option<FileLocation> {
        self.selection()
            .and_then(|current| (self.file_fn.as_ref()?)(editor, current))
            .map(|(path_or_id, line)| (path_or_id.get_canonicalized(), line))
    }

    /// Get (cached) preview for a given path. If a document corresponding
    /// to the path is already open in the editor, it is used instead.
    fn get_preview<'picker, 'editor>(
        &'picker mut self,
        path_or_id: PathOrId,
        editor: &'editor Editor,
    ) -> Preview<'picker, 'editor> {
        match path_or_id {
            PathOrId::Path(path) => {
                if let Some(doc) = editor.document_by_path(&path) {
                    return Preview::EditorDocument(doc);
                }

                if self.preview_cache.contains_key(&path) {
                    let preview = &self.preview_cache[&path];
                    match preview {
                        // If the document isn't highlighted yet, attempt to highlight it.
                        CachedPreview::Document(doc) if doc.language_config().is_none() => {
                            helix_event::send_blocking(
                                &self.preview_highlight_handler,
                                path.clone(),
                            );
                        }
                        _ => (),
                    }
                    return Preview::Cached(preview);
                }

                let data = std::fs::File::open(&path).and_then(|file| {
                    let metadata = file.metadata()?;
                    // Read up to 1kb to detect the content type
                    let n = file.take(1024).read_to_end(&mut self.read_buffer)?;
                    let content_type = content_inspector::inspect(&self.read_buffer[..n]);
                    self.read_buffer.clear();
                    Ok((metadata, content_type))
                });
                let preview = data
                    .map(
                        |(metadata, content_type)| match (metadata.len(), content_type) {
                            (_, content_inspector::ContentType::BINARY) => CachedPreview::Binary,
                            (size, _) if size > MAX_FILE_SIZE_FOR_PREVIEW => {
                                CachedPreview::LargeFile
                            }
                            _ => Document::open(&path, None, None, editor.config.clone())
                                .map(|doc| {
                                    // Asynchronously highlight the new document
                                    helix_event::send_blocking(
                                        &self.preview_highlight_handler,
                                        path.clone(),
                                    );
                                    CachedPreview::Document(Box::new(doc))
                                })
                                .unwrap_or(CachedPreview::NotFound),
                        },
                    )
                    .unwrap_or(CachedPreview::NotFound);
                self.preview_cache.insert(path.clone(), preview);
                Preview::Cached(&self.preview_cache[&path])
            }
            PathOrId::Id(id) => {
                let doc = editor.documents.get(&id).unwrap();
                Preview::EditorDocument(doc)
            }
        }
    }

    fn render_picker(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let status = self.matcher.tick(10);
        let snapshot = self.matcher.snapshot();
        if status.changed {
            self.cursor = self
                .cursor
                .min(snapshot.matched_item_count().saturating_sub(1))
        }

        let text_style = cx.editor.theme.get("ui.text");
        let selected = cx.editor.theme.get("ui.text.focus");
        let highlight_style = cx.editor.theme.get("special").add_modifier(Modifier::BOLD);

        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        // don't like this but the lifetime sucks
        let block = Block::default().borders(Borders::ALL);

        // calculate the inner area inside the box
        let inner = block.inner(area);

        block.render(area, surface);

        // -- Render the input bar:

        let area = inner.clip_left(1).with_height(1);
        // render the prompt first since it will clear its background
        self.prompt.render(area, surface, cx);

        let count = format!(
            "{}{}/{}",
            if status.running { "(running) " } else { "" },
            snapshot.matched_item_count(),
            snapshot.item_count(),
        );
        surface.set_stringn(
            (area.x + area.width).saturating_sub(count.len() as u16 + 1),
            area.y,
            &count,
            (count.len()).min(area.width as usize),
            text_style,
        );

        // -- Separator
        let sep_style = cx.editor.theme.get("ui.background.separator");
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in inner.left()..inner.right() {
            if let Some(cell) = surface.get_mut(x, inner.y + 1) {
                cell.set_symbol(borders.horizontal).set_style(sep_style);
            }
        }

        // -- Render the contents:
        // subtract area of prompt from top
        let inner = inner.clip_top(2);
        let rows = inner.height.saturating_sub(self.header_height()) as u32;
        let offset = self.cursor - (self.cursor % std::cmp::max(1, rows));
        let cursor = self.cursor.saturating_sub(offset);
        let end = offset
            .saturating_add(rows)
            .min(snapshot.matched_item_count());
        let mut indices = Vec::new();
        let mut matcher = MATCHER.lock();
        matcher.config = Config::DEFAULT;
        if self.file_fn.is_some() {
            matcher.config.set_match_paths()
        }

        let options = snapshot.matched_items(offset..end).map(|item| {
            let mut widths = self.widths.iter_mut();
            let mut matcher_index = 0;

            Row::new(self.columns.iter().map(|column| {
                let Some(Constraint::Length(max_width)) = widths.next() else {
                    unreachable!();
                };
                let mut cell = column.format(item.data, &self.editor_data);
                let width = if column.filter {
                    snapshot.pattern().column_pattern(matcher_index).indices(
                        item.matcher_columns[matcher_index].slice(..),
                        &mut matcher,
                        &mut indices,
                    );
                    indices.sort_unstable();
                    indices.dedup();
                    let mut indices = indices.drain(..);
                    let mut next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                    let mut span_list = Vec::new();
                    let mut current_span = String::new();
                    let mut current_style = Style::default();
                    let mut grapheme_idx = 0u32;
                    let mut width = 0;

                    let spans: &[Span] =
                        cell.content.lines.first().map_or(&[], |it| it.0.as_slice());
                    for span in spans {
                        // this looks like a bug on first glance, we are iterating
                        // graphemes but treating them as char indices. The reason that
                        // this is correct is that nucleo will only ever consider the first char
                        // of a grapheme (and discard the rest of the grapheme) so the indices
                        // returned by nucleo are essentially grapheme indecies
                        for grapheme in span.content.graphemes(true) {
                            let style = if grapheme_idx == next_highlight_idx {
                                next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                                span.style.patch(highlight_style)
                            } else {
                                span.style
                            };
                            if style != current_style {
                                if !current_span.is_empty() {
                                    span_list.push(Span::styled(current_span, current_style))
                                }
                                current_span = String::new();
                                current_style = style;
                            }
                            current_span.push_str(grapheme);
                            grapheme_idx += 1;
                        }
                        width += span.width();
                    }

                    span_list.push(Span::styled(current_span, current_style));
                    cell = Cell::from(Spans::from(span_list));
                    matcher_index += 1;
                    width
                } else {
                    cell.content
                        .lines
                        .first()
                        .map(|line| line.width())
                        .unwrap_or_default()
                };

                if width as u16 > *max_width {
                    *max_width = width as u16;
                }

                cell
            }))
        });

        let mut table = Table::new(options)
            .style(text_style)
            .highlight_style(selected)
            .highlight_symbol(" > ")
            .column_spacing(1)
            .widths(&self.widths);

        // -- Header
        if self.columns.len() > 1 {
            let header_style = cx.editor.theme.get("ui.picker.header");

            table =
                table.header(Row::new(self.columns.iter().map(|column| {
                    Cell::from(Span::styled(column.name.as_str(), header_style))
                })));
        }

        use tui::widgets::TableState;

        table.render_table(
            inner,
            surface,
            &mut TableState {
                offset: 0,
                selected: Some(cursor as usize),
            },
            self.truncate_start,
        );
    }

    fn render_preview(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        let text = cx.editor.theme.get("ui.text");
        surface.clear_with(area, background);

        // don't like this but the lifetime sucks
        let block = Block::default().borders(Borders::ALL);

        // calculate the inner area inside the box
        let inner = block.inner(area);
        // 1 column gap on either side
        let margin = Margin::horizontal(1);
        let inner = inner.inner(&margin);
        block.render(area, surface);

        if let Some((path, range)) = self.current_file(cx.editor) {
            let preview = self.get_preview(path, cx.editor);
            let doc = match preview.document() {
                Some(doc)
                    if range.map_or(true, |(start, end)| {
                        start <= end && end <= doc.text().len_lines()
                    }) =>
                {
                    doc
                }
                _ => {
                    let alt_text = preview.placeholder();
                    let x = inner.x + inner.width.saturating_sub(alt_text.len() as u16) / 2;
                    let y = inner.y + inner.height / 2;
                    surface.set_stringn(x, y, alt_text, inner.width as usize, text);
                    return;
                }
            };

            let mut offset = ViewPosition::default();
            if let Some((start_line, end_line)) = range {
                let height = end_line - start_line;
                let text = doc.text().slice(..);
                let start = text.line_to_char(start_line);
                let middle = text.line_to_char(start_line + height / 2);
                if height < inner.height as usize {
                    let text_fmt = doc.text_format(inner.width, None);
                    let annotations = TextAnnotations::default();
                    (offset.anchor, offset.vertical_offset) = char_idx_at_visual_offset(
                        text,
                        middle,
                        // align to middle
                        -(inner.height as isize / 2),
                        0,
                        &text_fmt,
                        &annotations,
                    );
                    if start < offset.anchor {
                        offset.anchor = start;
                        offset.vertical_offset = 0;
                    }
                } else {
                    offset.anchor = start;
                }
            }

            let syntax_highlights = EditorView::doc_syntax_highlights(
                doc,
                offset.anchor,
                area.height,
                &cx.editor.theme,
            );

            let mut overlay_highlights =
                EditorView::empty_highlight_iter(doc, offset.anchor, area.height);
            for spans in EditorView::doc_diagnostics_highlights(doc, &cx.editor.theme) {
                if spans.is_empty() {
                    continue;
                }
                overlay_highlights = Box::new(helix_core::syntax::merge(overlay_highlights, spans));
            }
            let mut decorations: Vec<Box<dyn LineDecoration>> = Vec::new();

            if let Some((start, end)) = range {
                let style = cx
                    .editor
                    .theme
                    .try_get("ui.highlight")
                    .unwrap_or_else(|| cx.editor.theme.get("ui.selection"));
                let draw_highlight = move |renderer: &mut TextRenderer, pos: LinePos| {
                    if (start..=end).contains(&pos.doc_line) {
                        let area = Rect::new(
                            renderer.viewport.x,
                            renderer.viewport.y + pos.visual_line,
                            renderer.viewport.width,
                            1,
                        );
                        renderer.surface.set_style(area, style)
                    }
                };
                decorations.push(Box::new(draw_highlight))
            }

            render_document(
                surface,
                inner,
                doc,
                offset,
                // TODO: compute text annotations asynchronously here (like inlay hints)
                &TextAnnotations::default(),
                syntax_highlights,
                overlay_highlights,
                &cx.editor.theme,
                &mut decorations,
                &mut [],
            );
        }
    }
}

impl<I: 'static + Send + Sync, D: 'static + Send + Sync> Component for Picker<I, D> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------+ +---------+
        // |prompt   | |preview  |
        // +---------+ |         |
        // |picker   | |         |
        // |         | |         |
        // +---------+ +---------+

        let render_preview =
            self.show_preview && self.file_fn.is_some() && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };

        let picker_area = area.with_width(picker_width);
        self.render_picker(picker_area, surface, cx);

        if render_preview {
            let preview_area = area.clip_left(picker_width);
            self.render_preview(preview_area, surface, cx);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        // TODO: keybinds for scrolling preview

        let key_event = match event {
            Event::Key(event) => *event,
            Event::Paste(..) => return self.prompt_handle_event(event, ctx),
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        let close_fn = |picker: &mut Self| {
            // if the picker is very large don't store it as last_picker to avoid
            // excessive memory consumption
            let callback: compositor::Callback = if picker.matcher.snapshot().item_count() > 100_000
            {
                Box::new(|compositor: &mut Compositor, _ctx| {
                    // remove the layer
                    compositor.pop();
                })
            } else {
                // stop streaming in new items in the background, really we should
                // be restarting the stream somehow once the picker gets
                // reopened instead (like for an FS crawl) that would also remove the
                // need for the special case above but that is pretty tricky
                picker.version.fetch_add(1, atomic::Ordering::Relaxed);
                Box::new(|compositor: &mut Compositor, _ctx| {
                    // remove the layer
                    compositor.last_picker = compositor.pop();
                })
            };
            EventResult::Consumed(Some(callback))
        };

        match key_event {
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_by(1, Direction::Backward);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                self.move_by(1, Direction::Forward);
            }
            key!(PageDown) | ctrl!('d') => {
                self.page_down();
            }
            key!(PageUp) | ctrl!('u') => {
                self.page_up();
            }
            key!(Home) => {
                self.to_start();
            }
            key!(End) => {
                self.to_end();
            }
            key!(Esc) | ctrl!('c') => return close_fn(self),
            alt!(Enter) => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::Load);
                }
            }
            key!(Enter) => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::Replace);
                }
                return close_fn(self);
            }
            ctrl!('s') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::HorizontalSplit);
                }
                return close_fn(self);
            }
            ctrl!('v') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::VerticalSplit);
                }
                return close_fn(self);
            }
            ctrl!('t') => {
                self.toggle_preview();
            }
            _ => {
                self.prompt_handle_event(event, ctx);
            }
        }

        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let block = Block::default().borders(Borders::ALL);
        // calculate the inner area inside the box
        let inner = block.inner(area);

        // prompt area
        let area = inner.clip_left(1).with_height(1);

        self.prompt.cursor(area, editor)
    }

    fn required_size(&mut self, (width, height): (u16, u16)) -> Option<(u16, u16)> {
        self.completion_height = height.saturating_sub(4 + self.header_height());
        Some((width, height))
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
impl<T: 'static + Send + Sync, D> Drop for Picker<T, D> {
    fn drop(&mut self) {
        // ensure we cancel any ongoing background threads streaming into the picker
        self.version.fetch_add(1, atomic::Ordering::Relaxed);
    }
}

type PickerCallback<T> = Box<dyn Fn(&mut Context, &T, Action)>;

/// Returns a new list of options to replace the contents of the picker
/// when called with the current picker query,
pub type DynQueryCallback<T> =
    Box<dyn Fn(String, &mut Editor) -> BoxFuture<'static, anyhow::Result<Vec<T>>>>;

/// A picker that updates its contents via a callback whenever the
/// query string changes. Useful for live grep, workspace symbols, etc.
pub struct DynamicPicker<T: 'static + Send + Sync, D: 'static + Send + Sync> {
    file_picker: Picker<T, D>,
    query_callback: DynQueryCallback<T>,
    query: String,
}

impl<T: Send + Sync, D: Send + Sync> DynamicPicker<T, D> {
    pub fn new(file_picker: Picker<T, D>, query_callback: DynQueryCallback<T>) -> Self {
        Self {
            file_picker,
            query_callback,
            query: String::new(),
        }
    }
}

impl<T: Send + Sync + 'static, D: Send + Sync + 'static> Component for DynamicPicker<T, D> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        self.file_picker.render(area, surface, cx);
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let event_result = self.file_picker.handle_event(event, cx);
        let Some(current_query) = self.file_picker.primary_query() else {
            return event_result;
        };

        if !matches!(event, Event::IdleTimeout) || self.query == *current_query {
            return event_result;
        }

        self.query = current_query.to_string();

        let new_options = (self.query_callback)(current_query.to_owned(), cx.editor);

        cx.jobs.callback(async move {
            let new_options = new_options.await?;
            let callback = Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                // Wrapping of pickers in overlay is done outside the picker code,
                // so this is fragile and will break if wrapped in some other widget.
                let picker = match compositor.find_id::<Overlay<Self>>(ID) {
                    Some(overlay) => &mut overlay.content.file_picker,
                    None => return,
                };
                picker.set_options(new_options);
            }));
            anyhow::Ok(callback)
        });
        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        self.file_picker.cursor(area, ctx)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.file_picker.required_size(viewport)
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
