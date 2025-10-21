use super::card_style::{
    browser_card_style,
    fill_card_background,
    primary_text_style,
    rows_to_lines,
    secondary_text_style,
    truncate_with_ellipsis,
    CardRow,
    CardSegment,
    CardStyle,
    CARD_ACCENT_WIDTH,
};
use super::{HistoryCell, HistoryCellType, ToolCellStatus};
use crate::colors;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Style;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui_image::{Image, Resize};
use ratatui_image::picker::Picker;
use ratatui_image::FilterType;
use image::ImageReader;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use url::Url;
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

const BORDER_TOP: &str = "╭─";
const BORDER_BODY: &str = "│";
const BORDER_BOTTOM: &str = "╰─";

const MAX_ACTIONS: usize = 24;
const MAX_CONSOLE: usize = 12;
const ACTION_DISPLAY_HEAD: usize = 4;
const ACTION_DISPLAY_TAIL: usize = 4;
const MIN_SCREENSHOT_ROWS: usize = 6;
const MAX_SCREENSHOT_ROWS: usize = 60;
const DEFAULT_TEXT_INDENT: usize = 2;
const TEXT_RIGHT_PADDING: usize = 2;
const SCREENSHOT_GAP: usize = 2;
const SCREENSHOT_MIN_WIDTH: usize = 18;
const SCREENSHOT_MAX_WIDTH: usize = 64;
const SCREENSHOT_LEFT_PAD: usize = 1;
const MIN_TEXT_WIDTH: usize = 28;
const ACTION_LABEL_GAP: usize = 2;
pub(crate) struct BrowserSessionCell {
    url: Option<String>,
    title: Option<String>,
    actions: Vec<BrowserAction>,
    console_messages: Vec<String>,
    screenshot_path: Option<String>,
    total_duration: Duration,
    completed: bool,
    cell_key: Option<String>,
    headless: Option<bool>,
    status_code: Option<String>,
    cached_picker: Rc<RefCell<Option<ratatui_image::picker::Picker>>>,
    cached_image_protocol: Rc<RefCell<Option<(PathBuf, ratatui::layout::Rect, ratatui_image::protocol::Protocol)>>>,
}

impl Clone for BrowserSessionCell {
    fn clone(&self) -> Self {
        Self {
            url: self.url.clone(),
            title: self.title.clone(),
            actions: self.actions.clone(),
            console_messages: self.console_messages.clone(),
            screenshot_path: self.screenshot_path.clone(),
            total_duration: self.total_duration,
            completed: self.completed,
            cell_key: self.cell_key.clone(),
            headless: self.headless,
            status_code: self.status_code.clone(),
            cached_picker: Rc::clone(&self.cached_picker),
            cached_image_protocol: Rc::clone(&self.cached_image_protocol),
        }
    }
}

impl Default for BrowserSessionCell {
    fn default() -> Self {
        Self {
            url: None,
            title: None,
            actions: Vec::new(),
            console_messages: Vec::new(),
            screenshot_path: None,
            total_duration: Duration::ZERO,
            completed: false,
            cell_key: None,
            headless: None,
            status_code: None,
            cached_picker: Rc::new(RefCell::new(None)),
            cached_image_protocol: Rc::new(RefCell::new(None)),
        }
    }
}

struct ScreenshotLayout {
    start_row: usize,
    height_rows: usize,
    width_cols: usize,
    indent_cols: usize,
}

#[derive(Clone)]
struct ActionEntry {
    label: String,
    detail: String,
}

enum ActionDisplayLine {
    Entry(ActionEntry),
    Ellipsis,
}

#[derive(Clone)]
struct BrowserAction {
    action: String,
    target: Option<String>,
    value: Option<String>,
    outcome: Option<String>,
}

impl BrowserSessionCell {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn ensure_picker_initialized(
        &self,
        picker: Option<Picker>,
        font_size: (u16, u16),
    ) {
        let mut slot = self.cached_picker.borrow_mut();
        if slot.is_some() {
            return;
        }
        if let Some(p) = picker {
            *slot = Some(p);
        } else {
            *slot = Some(Picker::from_fontsize(font_size));
        }
    }

    pub(crate) fn set_url(&mut self, url: impl Into<String>) {
        self.url = Some(url.into());
    }

    pub(crate) fn summary_label(&self) -> String {
        self.display_label()
    }

    pub(crate) fn current_url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    pub(crate) fn record_action(
        &mut self,
        timestamp: Duration,
        duration: Duration,
        action: String,
        target: Option<String>,
        value: Option<String>,
        outcome: Option<String>,
    ) {
        if self.actions.last().map_or(false, |last| {
            last.action == action
                && last.target == target
                && last.value == value
                && last.outcome == outcome
        }) {
            return;
        }
        let action_entry = BrowserAction {
            action,
            target,
            value,
            outcome: outcome.clone(),
        };
        self.actions.push(action_entry);
        if self.actions.len() > MAX_ACTIONS {
            let overflow = self.actions.len() - MAX_ACTIONS;
            self.actions.drain(0..overflow);
        }
        let finish = timestamp.saturating_add(duration);
        if finish > self.total_duration {
            self.total_duration = finish;
        }
        if let Some(outcome) = outcome {
            if let Some(code) = extract_status_code(&outcome) {
                self.status_code = Some(code);
            }
        }
    }

    pub(crate) fn add_console_message(&mut self, message: String) {
        self.console_messages.push(message);
        if self.console_messages.len() > MAX_CONSOLE {
            let overflow = self.console_messages.len() - MAX_CONSOLE;
            self.console_messages.drain(0..overflow);
        }
    }

    pub(crate) fn set_screenshot(&mut self, path: PathBuf) {
        self.screenshot_path = Some(path.display().to_string());
        self.cached_image_protocol.borrow_mut().take();
    }

    pub(crate) fn set_headless(&mut self, headless: Option<bool>) {
        self.headless = headless;
    }

    pub(crate) fn set_status_code(&mut self, code: Option<String>) {
        self.status_code = code;
    }

    pub(crate) fn set_cell_key(&mut self, key: Option<String>) {
        self.cell_key = key;
    }

    pub(crate) fn cell_key(&self) -> Option<&str> {
        self.cell_key.as_deref()
    }

    fn accent_style(style: &CardStyle) -> Style {
        let dim = colors::mix_toward(style.accent_fg, colors::text_dim(), 0.85);
        Style::default().fg(dim)
    }

    fn normalized_title(&self) -> Option<String> {
        self.title
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("(pending)"))
            .map(|value| value.to_string())
    }

    fn header_summary_text(&self) -> String {
        let label = if self.headless.unwrap_or(true) {
            "Browser (headless)"
        } else {
            "Browser"
        };

        let mut title = format!("{}: {}", label, self.display_label());
        if let Some(code) = &self.status_code {
            title.push_str(&format!(" [{}]", code));
        }
        title
    }

    fn top_border_row(&self, body_width: usize, style: &CardStyle) -> CardRow {
        let mut segments = Vec::new();
        if body_width == 0 {
            return CardRow::new(
                BORDER_TOP.to_string(),
                Self::accent_style(style),
                segments,
                None,
            );
        }

        segments.push(CardSegment::new(" ".to_string(), primary_text_style(style)));
        let remaining = body_width.saturating_sub(1);
        let text = truncate_with_ellipsis(self.header_summary_text().as_str(), remaining);
        if !text.is_empty() {
            segments.push(CardSegment::new(text, primary_text_style(style)));
        }
        CardRow::new(BORDER_TOP.to_string(), Self::accent_style(style), segments, None)
    }

    fn blank_border_row(&self, body_width: usize, style: &CardStyle) -> CardRow {
        CardRow::new(
            BORDER_BODY.to_string(),
            Self::accent_style(style),
            vec![CardSegment::new(" ".repeat(body_width), Style::default())],
            None,
        )
    }

    fn body_text_row(
        &self,
        text: impl Into<String>,
        body_width: usize,
        style: &CardStyle,
        text_style: Style,
        indent_cols: usize,
        right_padding_cols: usize,
    ) -> CardRow {
        if body_width == 0 {
            return CardRow::new(BORDER_BODY.to_string(), Self::accent_style(style), Vec::new(), None);
        }
        let indent = indent_cols.min(body_width.saturating_sub(1));
        let available = body_width.saturating_sub(indent);
        let mut segments = Vec::new();
        if indent > 0 {
            segments.push(CardSegment::new(" ".repeat(indent), Style::default()));
        }
        let text: String = text.into();
        if available == 0 {
            return CardRow::new(BORDER_BODY.to_string(), Self::accent_style(style), segments, None);
        }
        let usable_width = available.saturating_sub(right_padding_cols);
        let display = if usable_width == 0 {
            String::new()
        } else {
            truncate_with_ellipsis(text.as_str(), usable_width)
        };
        segments.push(CardSegment::new(display, text_style));
        if right_padding_cols > 0 && available > 0 {
            let pad = right_padding_cols.min(available);
            segments.push(CardSegment::new(" ".repeat(pad), Style::default()));
        }
        CardRow::new(BORDER_BODY.to_string(), Self::accent_style(style), segments, None)
    }

    fn bottom_border_row(&self, body_width: usize, style: &CardStyle) -> CardRow {
        let text_value = format!(" [Ctrl+B] View · [Esc] Stop");
        let text = truncate_with_ellipsis(text_value.as_str(), body_width);
        let segment = CardSegment::new(text, secondary_text_style(style));
        CardRow::new(BORDER_BOTTOM.to_string(), Self::accent_style(style), vec![segment], None)
    }

    fn display_host(&self) -> Option<String> {
        self
            .url
            .as_ref()
            .and_then(|url| Url::parse(url).ok())
            .and_then(|parsed| parsed.host_str().map(|host| host.to_string()))
    }

    fn display_label(&self) -> String {
        if let Some(title) = self.normalized_title() {
            return title;
        }
        if let Some(host) = self.display_host() {
            return host;
        }
        self
            .url
            .as_ref()
            .map(|url| url.clone())
            .unwrap_or_else(|| "Browser Session".to_string())
    }

    fn build_card_rows(&self, width: u16, style: &CardStyle) -> (Vec<CardRow>, Option<ScreenshotLayout>) {
        if width == 0 {
            return (Vec::new(), None);
        }

        let accent_width = CARD_ACCENT_WIDTH.min(width as usize);
        let body_width = width.saturating_sub(accent_width as u16) as usize;
        if body_width == 0 {
            return (Vec::new(), None);
        }

        let mut rows: Vec<CardRow> = Vec::new();
        rows.push(self.top_border_row(body_width, style));
        rows.push(self.blank_border_row(body_width, style));

        let mut screenshot_layout = self.compute_screenshot_layout(body_width);
        let indent_cols = screenshot_layout
            .as_ref()
            .map(|layout| layout.indent_cols)
            .unwrap_or(DEFAULT_TEXT_INDENT);
        let indent_cols = indent_cols.min(body_width.saturating_sub(1));
        let right_padding = TEXT_RIGHT_PADDING.min(body_width);

        let content_start = rows.len();

        let action_display = self.formatted_action_display();
        let label_width = action_display
            .iter()
            .filter_map(|line| match line {
                ActionDisplayLine::Entry(entry) => Some(string_display_width(entry.label.as_str())),
                ActionDisplayLine::Ellipsis => None,
            })
            .max()
            .unwrap_or(0);

        if action_display.is_empty() {
            for wrapped in wrap_card_lines(
                "No browser actions yet",
                body_width,
                indent_cols,
                right_padding,
            ) {
                rows.push(self.body_text_row(
                    wrapped,
                    body_width,
                    style,
                    secondary_text_style(style),
                    indent_cols,
                    right_padding,
                ));
            }
        } else {
            for line in action_display {
                match line {
                    ActionDisplayLine::Entry(entry) => {
                        let entry_rows = self.render_action_entry_rows(
                            &entry,
                            body_width,
                            style,
                            indent_cols,
                            right_padding,
                            label_width,
                        );
                        rows.extend(entry_rows);
                    }
                    ActionDisplayLine::Ellipsis => {
                        rows.push(self.body_text_row(
                            "⋮",
                            body_width,
                            style,
                            primary_text_style(style),
                            indent_cols,
                            right_padding,
                        ));
                    }
                }
            }
        }

        let console_rows = self.console_rows(body_width, style, indent_cols, right_padding);
        if !console_rows.is_empty() {
            rows.push(self.blank_border_row(body_width, style));
            rows.extend(console_rows);
        }

        if let Some(layout) = screenshot_layout.as_mut() {
            layout.start_row = content_start;
            let existing = rows.len().saturating_sub(content_start);
            if existing < layout.height_rows {
                let missing = layout.height_rows - existing;
                for _ in 0..missing {
                    rows.push(self.body_text_row(
                        "",
                        body_width,
                        style,
                        Style::default(),
                        indent_cols,
                        right_padding,
                    ));
                }
            }
        }

        rows.push(self.blank_border_row(body_width, style));
        rows.push(self.bottom_border_row(body_width, style));

        (rows, screenshot_layout)
    }

    fn console_rows(
        &self,
        body_width: usize,
        style: &CardStyle,
        indent_cols: usize,
        right_padding: usize,
    ) -> Vec<CardRow> {
        let last = match self.console_messages.last() {
            Some(value) => value.clone(),
            None => return Vec::new(),
        };
        let style_color = if last.contains('⚠') {
            Style::default().fg(colors::warning())
        } else {
            secondary_text_style(style)
        };
        let text = format!("Console: {}", last);
        wrap_card_lines(text.as_str(), body_width, indent_cols, right_padding)
            .into_iter()
            .map(|wrapped| {
                self.body_text_row(
                    wrapped,
                    body_width,
                    style,
                    style_color,
                    indent_cols,
                    right_padding,
                )
            })
            .collect()
    }

    fn render_action_entry_rows(
        &self,
        entry: &ActionEntry,
        body_width: usize,
        style: &CardStyle,
        indent_cols: usize,
        right_padding: usize,
        label_width: usize,
    ) -> Vec<CardRow> {
        if body_width == 0 {
            return Vec::new();
        }
        let indent = indent_cols.min(body_width.saturating_sub(1));
        let available = body_width.saturating_sub(indent);
        if available == 0 {
            return Vec::new();
        }

        let base_available = available.saturating_sub(right_padding);
        if base_available == 0 {
            return Vec::new();
        }

        let max_label_width = base_available.saturating_sub(ACTION_LABEL_GAP + 1);
        if max_label_width == 0 {
            return self.render_fallback_entry(entry, body_width, style, indent_cols, right_padding);
        }

        let effective_label_width = label_width.min(max_label_width);
        let detail_width = base_available
            .saturating_sub(effective_label_width + ACTION_LABEL_GAP);
        if detail_width == 0 {
            return self.render_fallback_entry(entry, body_width, style, indent_cols, right_padding);
        }

        let label_full_width = string_display_width(entry.label.as_str());
        if effective_label_width < label_full_width {
            return self.render_fallback_entry(entry, body_width, style, indent_cols, right_padding);
        }

        let label_display = entry.label.clone();
        let label_padding = effective_label_width.saturating_sub(label_full_width);
        let gap = " ".repeat(ACTION_LABEL_GAP);

        let mut lines = wrap_line_to_width(entry.detail.as_str(), detail_width);
        if lines.is_empty() {
            lines.push(String::new());
        }

        let label_column = format!("{}{}", label_display, " ".repeat(label_padding));
        let mut rows = Vec::new();
        let label_style = secondary_text_style(style);
        let detail_style = primary_text_style(style);

        let indent_string = if indent > 0 {
            Some(" ".repeat(indent))
        } else {
            None
        };

        if let Some(first) = lines.first() {
            rows.push(self.build_action_row(
                body_width,
                style,
                indent_string.as_deref().unwrap_or(""),
                &label_column,
                &gap,
                first,
                indent,
                effective_label_width,
                right_padding,
                label_style,
                detail_style,
            ));
        }

        let continuation_label = " ".repeat(effective_label_width);
        for detail_line in lines.iter().skip(1) {
            rows.push(self.build_action_row(
                body_width,
                style,
                indent_string.as_deref().unwrap_or(""),
                &continuation_label,
                &gap,
                detail_line,
                indent,
                effective_label_width,
                right_padding,
                label_style,
                detail_style,
            ));
        }

        rows
    }

    fn build_action_row(
        &self,
        body_width: usize,
        style: &CardStyle,
        indent: &str,
        label: &str,
        gap: &str,
        detail: &str,
        indent_cols: usize,
        label_width: usize,
        right_padding: usize,
        label_style: Style,
        detail_style: Style,
    ) -> CardRow {
        let mut segments = Vec::new();
        let mut consumed = 0usize;
        if !indent.is_empty() {
            segments.push(CardSegment::new(indent.to_string(), Style::default()));
            consumed += indent_cols;
        }

        if !label.is_empty() {
            segments.push(CardSegment::new(label.to_string(), label_style));
            consumed += label_width;
        }

        if !gap.is_empty() {
            segments.push(CardSegment::new(gap.to_string(), Style::default()));
            consumed += ACTION_LABEL_GAP;
        }

        segments.push(CardSegment::new(detail.to_string(), detail_style));
        consumed += string_display_width(detail);

        let available = body_width.saturating_sub(consumed);
        if available > 0 {
            let pad = available.min(right_padding);
            if pad > 0 {
                segments.push(CardSegment::new(" ".repeat(pad), Style::default()));
            }
        }

        CardRow::new(BORDER_BODY.to_string(), Self::accent_style(style), segments, None)
    }

    fn render_fallback_entry(
        &self,
        entry: &ActionEntry,
        body_width: usize,
        style: &CardStyle,
        indent_cols: usize,
        right_padding: usize,
    ) -> Vec<CardRow> {
        let combined = if entry.detail.is_empty() {
            entry.label.clone()
        } else {
            format!("{} {}", entry.label.trim(), entry.detail.trim())
        };
        wrap_card_lines(combined.trim(), body_width, indent_cols, right_padding)
            .into_iter()
            .map(|wrapped| {
                self.body_text_row(
                    wrapped,
                    body_width,
                    style,
                    primary_text_style(style),
                    indent_cols,
                    right_padding,
                )
            })
            .collect()
    }

    fn formatted_action_display(&self) -> Vec<ActionDisplayLine> {
        let mut entries: Vec<ActionEntry> = Vec::new();
        let has_actions = !self.actions.is_empty();
        if !has_actions {
            if let Some(url) = self.url.as_ref() {
                entries.push(ActionEntry {
                    label: "Opened".to_string(),
                    detail: url.clone(),
                });
            }
        }

        entries.extend(self.actions.iter().map(format_action_entry));

        if entries.is_empty() {
            return Vec::new();
        }

        if entries.len() > ACTION_DISPLAY_HEAD + ACTION_DISPLAY_TAIL {
            let mut display: Vec<ActionDisplayLine> = Vec::new();
            for entry in entries.iter().take(ACTION_DISPLAY_HEAD) {
                display.push(ActionDisplayLine::Entry(entry.clone()));
            }
            display.push(ActionDisplayLine::Ellipsis);
            let tail = entries
                .iter()
                .rev()
                .take(ACTION_DISPLAY_TAIL)
                .cloned()
                .collect::<Vec<_>>();
            for entry in tail.into_iter().rev() {
                display.push(ActionDisplayLine::Entry(entry));
            }
            display
        } else {
            entries
                .into_iter()
                .map(ActionDisplayLine::Entry)
                .collect()
        }
    }

    fn compute_screenshot_layout(&self, body_width: usize) -> Option<ScreenshotLayout> {
        if self.screenshot_path.is_none() {
            return None;
        }

        if body_width
            < SCREENSHOT_LEFT_PAD + SCREENSHOT_MIN_WIDTH + SCREENSHOT_GAP + MIN_TEXT_WIDTH + TEXT_RIGHT_PADDING
        {
            return None;
        }

        let max_screenshot = body_width
            .saturating_sub(SCREENSHOT_LEFT_PAD + MIN_TEXT_WIDTH + SCREENSHOT_GAP + TEXT_RIGHT_PADDING);
        if max_screenshot < SCREENSHOT_MIN_WIDTH {
            return None;
        }

        let mut screenshot_cols = max_screenshot;
        if screenshot_cols > SCREENSHOT_MAX_WIDTH {
            screenshot_cols = SCREENSHOT_MAX_WIDTH;
        }
        if screenshot_cols < SCREENSHOT_MIN_WIDTH {
            screenshot_cols = SCREENSHOT_MIN_WIDTH;
        }

        let rows = self.compute_screenshot_rows(screenshot_cols)?;
        Some(ScreenshotLayout {
            start_row: 0,
            height_rows: rows,
            width_cols: screenshot_cols,
            indent_cols: SCREENSHOT_LEFT_PAD + screenshot_cols + SCREENSHOT_GAP,
        })
    }

    fn ensure_picker(&self) -> Picker {
        let mut picker_ref = self.cached_picker.borrow_mut();
        if picker_ref.is_none() {
            *picker_ref = Some(Picker::from_fontsize((8, 16)));
        }
        picker_ref.as_ref().unwrap().clone()
    }

fn compute_screenshot_rows(&self, screenshot_cols: usize) -> Option<usize> {
        if screenshot_cols == 0 {
            return None;
        }
        let path = Path::new(self.screenshot_path.as_ref()?);

        let picker = self.ensure_picker();
        let (cell_w, cell_h) = picker.font_size();
        if cell_w == 0 || cell_h == 0 {
            return Some(MIN_SCREENSHOT_ROWS);
        }

        let (img_w, img_h) = match image::image_dimensions(path) {
            Ok(dim) if dim.0 > 0 && dim.1 > 0 => dim,
            _ => return Some(MIN_SCREENSHOT_ROWS),
        };

        let cols = screenshot_cols as u32;
        if cols == 0 {
            return None;
        }

        let cw = cell_w as u32;
        let ch = cell_h as u32;
        let img_w = img_w as u32;
        let img_h = img_h as u32;

        let rows_by_w = (cols * cw * img_h) as f64 / (img_w * ch) as f64;
        let rows = rows_by_w.ceil().max(1.0) as usize;
        Some(rows.clamp(MIN_SCREENSHOT_ROWS, MAX_SCREENSHOT_ROWS))
    }

    fn build_plain_summary(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let status = if self.completed { "done" } else { "running" };
        lines.push(format!("Browser Session: {} [{}]", self.display_label(), status));
        if let Some(url) = &self.url {
            lines.push(format!("Opened: {}", url));
        }
        if let Some(code) = &self.status_code {
            lines.push(format!("Status: {}", code));
        }
        for action in self
            .actions
            .iter()
            .rev()
            .take(3)
            .rev()
        {
            lines.push(format!("Action: {}", format_action_line(action)));
        }
        if let Some(path) = &self.screenshot_path {
            lines.push(format!("Screenshot: {}", path));
        }
        lines
    }
}

impl HistoryCell for BrowserSessionCell {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn gutter_symbol(&self) -> Option<&'static str> {
        if self.completed {
            Some("✔")
        } else {
            None
        }
    }

    fn kind(&self) -> HistoryCellType {
        let status = if self.completed {
            ToolCellStatus::Success
        } else {
            ToolCellStatus::Running
        };
        HistoryCellType::Tool { status }
    }

    fn display_lines(&self) -> Vec<Line<'static>> {
        self.build_plain_summary().into_iter().map(Line::from).collect()
    }

    fn desired_height(&self, width: u16) -> u16 {
        let style = browser_card_style();
        let (rows, _) = self.build_card_rows(width, &style);
        rows.len().max(1) as u16
    }

    fn has_custom_render(&self) -> bool {
        true
    }

    fn custom_render_with_skip(&self, area: Rect, buf: &mut Buffer, skip_rows: u16) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let style = browser_card_style();
        fill_card_background(buf, area, &style);
        let (rows, screenshot_meta) = self.build_card_rows(area.width, &style);
        let lines = rows_to_lines(&rows, &style, area.width);
        let text = Text::from(lines);

        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((skip_rows, 0))
            .render(area, buf);

        if let Some(layout) = screenshot_meta.as_ref() {
            if let Some(path) = self.screenshot_path.as_ref() {
                self.render_screenshot_preview(area, buf, skip_rows, layout, path);
            }
        }
    }
}

impl BrowserSessionCell {
    fn render_screenshot_preview(
        &self,
        area: Rect,
        buf: &mut Buffer,
        skip_rows: u16,
        layout: &ScreenshotLayout,
        path_str: &str,
    ) {
        let accent_width = CARD_ACCENT_WIDTH.min(area.width as usize) as u16;
        if accent_width >= area.width {
            return;
        }

        let viewport_top = skip_rows as usize;
        let viewport_bottom = viewport_top + area.height as usize;
        let shot_top = layout.start_row;
        let shot_bottom = layout.start_row + layout.height_rows;

        if shot_bottom <= viewport_top || shot_top >= viewport_bottom {
            return;
        }

        let visible_top = shot_top.max(viewport_top);
        let visible_bottom = shot_bottom.min(viewport_bottom);
        if visible_bottom <= visible_top {
            return;
        }

        let body_width = area.width.saturating_sub(accent_width);
        if body_width == 0 {
            return;
        }

        let left_pad = SCREENSHOT_LEFT_PAD.min(body_width as usize) as u16;
        if body_width <= left_pad {
            return;
        }

        let usable_width = body_width.saturating_sub(left_pad);
        let screenshot_width = layout.width_cols.min(usable_width as usize) as u16;
        if screenshot_width == 0 {
            return;
        }

        let path = Path::new(path_str);
        if !path.exists() {
            let placeholder_area = Rect {
                x: area.x + accent_width + left_pad,
                y: area.y,
                width: screenshot_width,
                height: layout.height_rows.min(area.height as usize) as u16,
            };
            self.render_screenshot_placeholder(path, placeholder_area, buf);
            return;
        }

        let full_height = layout.height_rows as u16;
        if full_height == 0 {
            return;
        }

        let offscreen = match self.render_screenshot_buffer(path, screenshot_width, full_height) {
            Ok(buffer) => buffer,
            Err(_) => {
                let placeholder_area = Rect {
                    x: area.x + accent_width + left_pad,
                    y: area.y,
                    width: screenshot_width,
                    height: layout.height_rows.min(area.height as usize) as u16,
                };
                self.render_screenshot_placeholder(path, placeholder_area, buf);
                return;
            }
        };

        let src_start_row = (visible_top - shot_top) as u16;
        let rows_to_copy = (visible_bottom - visible_top) as u16;
        if rows_to_copy == 0 {
            return;
        }

        let dest_x = area.x + accent_width + left_pad;
        let dest_y = area.y + (visible_top - viewport_top) as u16;
        let area_bottom = area.y + area.height;
        let area_right = area.x + area.width;

        for row in 0..rows_to_copy {
            let dest_row = dest_y + row;
            if dest_row >= area_bottom {
                break;
            }
            let src_row = src_start_row + row;
            for col in 0..screenshot_width {
                let dest_col = dest_x + col;
                if dest_col >= area_right {
                    break;
                }
                let Some(src_cell) = offscreen.cell((col, src_row)) else { continue; };
                if let Some(dest_cell) = buf.cell_mut((dest_col, dest_row)) {
                    *dest_cell = src_cell.clone();
                }
            }
        }
    }

    fn render_screenshot_placeholder(&self, path: &Path, area: Rect, buf: &mut Buffer) {
        use ratatui::style::{Modifier, Style};
        use ratatui::widgets::{Block, Borders};

        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("screenshot");
        let placeholder_text = format!("Screenshot:\n{}", filename);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(colors::info()))
            .title("Browser");
        let inner = block.inner(area);
        block.render(area, buf);
        Paragraph::new(placeholder_text)
            .style(
                Style::default()
                    .fg(colors::text_dim())
                    .add_modifier(Modifier::ITALIC),
            )
            .wrap(Wrap { trim: true })
            .render(inner, buf);
    }

    fn render_screenshot_buffer(&self, path: &Path, width: u16, height: u16) -> Result<Buffer, ()> {
        if width == 0 || height == 0 {
            return Err(());
        }

        let picker = self.ensure_picker();
        let target = Rect::new(0, 0, width, height);
        self.ensure_protocol(path, target, &picker)?;

        let mut buffer = Buffer::empty(target);
        if let Some((_, _, protocol)) = self.cached_image_protocol.borrow_mut().as_mut() {
            let image = Image::new(protocol);
            image.render(target, &mut buffer);
            Ok(buffer)
        } else {
            Err(())
        }
    }

    fn ensure_protocol(&self, path: &Path, target: Rect, picker: &Picker) -> Result<(), ()> {
        let mut cache = self.cached_image_protocol.borrow_mut();
        let needs_recreate = match cache.as_ref() {
            Some((cached_path, cached_rect, _)) => cached_path != path || *cached_rect != target,
            None => true,
        };

        if needs_recreate {
            let dyn_img = match ImageReader::open(path) {
                Ok(reader) => reader.decode().map_err(|_| ())?,
                Err(_) => return Err(()),
            };
            let protocol = picker
                .new_protocol(dyn_img, target, Resize::Fit(Some(FilterType::Lanczos3)))
                .map_err(|_| ())?;
            *cache = Some((path.to_path_buf(), target, protocol));
        }

        Ok(())
    }

}

fn wrap_card_lines(text: &str, body_width: usize, indent_cols: usize, right_padding: usize) -> Vec<String> {
    let available = body_width
        .saturating_sub(indent_cols)
        .saturating_sub(right_padding);
    if available == 0 {
        return vec![String::new()];
    }
    wrap_line_to_width(text, available)
}

fn wrap_line_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    if text.trim().is_empty() {
        return vec![String::new()];
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for word in text.split_whitespace() {
        let mut word_parts = if string_display_width(word) > width {
            split_long_card_word(word, width)
        } else {
            vec![word.to_string()]
        };

        for part in word_parts.drain(..) {
            let part_width = string_display_width(part.as_str());
            if current.is_empty() {
                current.push_str(part.as_str());
                current_width = part_width;
            } else if current_width + 1 + part_width > width {
                lines.push(current);
                current = part.clone();
                current_width = part_width;
            } else {
                current.push(' ');
                current.push_str(part.as_str());
                current_width += 1 + part_width;
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn split_long_card_word(word: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;

    for ch in word.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1);
        if current_width + ch_width > width && !current.is_empty() {
            parts.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }

    if !current.is_empty() {
        parts.push(current);
    }

    if parts.is_empty() {
        parts.push(String::new());
    }
    parts
}

fn string_display_width(text: &str) -> usize {
    text
        .chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

fn format_action_summary(action: &BrowserAction) -> String {
    match (&action.target, &action.value, &action.outcome) {
        (Some(target), Some(value), Some(outcome)) => {
            format!("{} {} → {}", action.action, target, outcome_for_display(outcome, value))
        }
        (Some(target), Some(value), None) => {
            format!("{} {} = {}", action.action, target, value)
        }
        (Some(target), None, Some(outcome)) => {
            format!("{} {} → {}", action.action, target, outcome)
        }
        (Some(target), None, None) => format!("{} {}", action.action, target),
        (None, Some(value), Some(outcome)) => {
            format!("{} {} → {}", action.action, value, outcome)
        }
        (None, Some(value), None) => format!("{} {}", action.action, value),
        (None, None, Some(outcome)) => format!("{} → {}", action.action, outcome),
        _ => action.action.clone(),
    }
}

fn format_action_entry(action: &BrowserAction) -> ActionEntry {
    let action_lower = action.action.to_ascii_lowercase();
    match action_lower.as_str() {
        "click" | "mouse_click" => {
            let target = action.target.as_deref().unwrap_or("").trim();
            let detail = if target.starts_with('(') && target.ends_with(')') {
                format!("at {}", target)
            } else if !target.is_empty() {
                target.to_string()
            } else if let Some(value) = action.value.as_deref() {
                value.trim().to_string()
            } else if let Some(outcome) = action.outcome.as_deref() {
                outcome.trim().to_string()
            } else {
                String::new()
            };
            ActionEntry {
                label: "Clicked".to_string(),
                detail,
            }
        }
        "press_key" | "key" | "press" => {
            let key_raw = action
                .value
                .as_deref()
                .or(action.outcome.as_deref())
                .or(action.target.as_deref())
                .unwrap_or("?")
                .trim();
            let key = sanitize_pressed_detail(key_raw);
            ActionEntry {
                label: "Pressed".to_string(),
                detail: key,
            }
        }
        "type" | "input" | "enter_text" | "fill" | "insert_text" => {
            let typed = action
                .value
                .as_deref()
                .or(action.outcome.as_deref())
                .unwrap_or("?")
                .trim()
                .to_string();
            ActionEntry {
                label: "Typed".to_string(),
                detail: typed,
            }
        }
        "navigate" | "open" | "nav" => {
            let dest = action
                .target
                .as_deref()
                .map(|value| sanitize_nav_text(value))
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    action
                        .value
                        .as_deref()
                        .map(|value| sanitize_nav_text(value))
                        .filter(|s| !s.is_empty())
                })
                .or_else(|| {
                    action
                        .outcome
                        .as_deref()
                        .map(|value| sanitize_nav_text(value))
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or_else(|| "".to_string());
            ActionEntry {
                label: "Opened".to_string(),
                detail: dest,
            }
        }
        other if other.starts_with("scroll") => {
            let detail = action
                .value
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .map(|v| v.trim().to_string())
                .or_else(|| {
                    action
                        .outcome
                        .as_deref()
                        .filter(|o| !o.trim().is_empty())
                        .map(|o| o.trim().to_string())
                })
                .or_else(|| {
                    action
                        .target
                        .as_deref()
                        .filter(|t| !t.trim().is_empty())
                        .map(|t| t.trim().to_string())
                })
                .unwrap_or_else(|| {
                    format_action_summary(action)
                        .strip_prefix(other)
                        .map(|suffix| suffix.trim_start_matches(|c| c == ' ' || c == ':' || c == '-'))
                        .filter(|suffix| !suffix.is_empty())
                        .map(|suffix| suffix.to_string())
                        .unwrap_or_else(|| format_action_summary(action))
                });
            ActionEntry {
                label: "Scrolled".to_string(),
                detail,
            }
        }
        _ => {
            let summary = format_action_summary(action);
            let label = titleize_action(action.action.as_str());
            let trimmed = summary
                .strip_prefix(action.action.as_str())
                .map(|suffix| suffix.trim_start_matches(|c| c == ' ' || c == ':' || c == '-'))
                .filter(|suffix| !suffix.is_empty())
                .map(|suffix| suffix.to_string())
                .unwrap_or_else(|| summary.clone());
            ActionEntry {
                label,
                detail: trimmed,
            }
        }
    }
}

fn titleize_action(raw: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for segment in raw.split(['_', '-']).filter(|part| !part.is_empty()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            let first_upper = first.to_uppercase().collect::<String>();
            let rest = chars.as_str().to_ascii_lowercase();
            words.push(format!("{}{}", first_upper, rest));
        }
    }
    if words.is_empty() {
        raw.to_string()
    } else {
        words.join(" ")
    }
}

fn sanitize_pressed_detail(raw: &str) -> String {
    let mut candidate = raw;
    const PREFIXES: &[&str] = &[
        "pressed key:",
        "press key:",
        "key pressed:",
        "key:",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = strip_prefix_ignore_case(candidate, prefix) {
            candidate = rest;
            break;
        }
    }
    let cleaned = candidate.trim();
    if cleaned.is_empty() {
        raw.trim().to_string()
    } else {
        cleaned.to_string()
    }
}

fn sanitize_nav_text(raw: &str) -> String {
    let mut candidate = raw;
    const PREFIXES: &[&str] = &[
        "browser opened to:",
        "opened to:",
        "navigated to",
        "nav to:",
        "opened:",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = strip_prefix_ignore_case(candidate, prefix) {
            candidate = rest;
            break;
        }
    }
    let cleaned = candidate.trim().trim_start_matches(':').trim();
    cleaned.to_string()
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let text_bytes = text.as_bytes();
    let prefix_bytes = prefix.as_bytes();
    if text_bytes.len() < prefix_bytes.len() {
        return None;
    }
    for (idx, prefix_byte) in prefix_bytes.iter().enumerate() {
        if text_bytes[idx].to_ascii_lowercase() != prefix_byte.to_ascii_lowercase() {
            return None;
        }
    }
    Some(text.get(prefix.len()..)?.trim_start())
}

fn format_action_line(action: &BrowserAction) -> String {
    let action_lower = action.action.to_ascii_lowercase();
    let target = action.target.as_deref().unwrap_or("?");
    let value = action.value.as_deref();
    let outcome = action.outcome.as_deref();

    match action_lower.as_str() {
        "click" | "mouse_click" => {
            let display = target.trim();
            if display.starts_with('(') {
                format!("Clicked at {}", display)
            } else if !display.is_empty() {
                format!("Clicked {}", display)
            } else {
                "Clicked".to_string()
            }
        }
        "press_key" | "key" | "press" => {
            let key = value.or(outcome).unwrap_or("?");
            format!("Pressed key: {}", key)
        }
        "type" | "input" | "enter_text" | "fill" | "insert_text" => {
            let typed = value.or(outcome).unwrap_or("?");
            format!("Typed: {}", typed)
        }
        "navigate" | "open" => {
            let dest = value
                .or(action.target.as_deref())
                .or(outcome)
                .unwrap_or("?");
            format!("Navigated to {}", dest)
        }
        other => {
            let summary = format_action_summary(action);
            if summary.is_empty() {
                other.to_string()
            } else {
                summary
            }
        }
    }
}

fn outcome_for_display(outcome: &str, value: &str) -> String {
    if outcome == "value set" {
        value.to_string()
    } else {
        outcome.to_string()
    }
}

fn extract_status_code(outcome: &str) -> Option<String> {
    let trimmed = outcome.trim();
    if trimmed.len() < 3 {
        return None;
    }
    let code: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if code.len() == 3 {
        Some(code)
    } else {
        None
    }
}


impl crate::chatwidget::tool_cards::ToolCardCell for BrowserSessionCell {
    fn tool_card_key(&self) -> Option<&str> {
        self.cell_key()
    }

    fn set_tool_card_key(&mut self, key: Option<String>) {
        self.set_cell_key(key);
    }
}
