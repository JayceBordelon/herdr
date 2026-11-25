//! The board, vcer's main screen. Every pane is a live box, master the
//! biggest, and one minimal input line messages the selected box. The client
//! only streams the focused pane, so boxes poll each pane's visible screen
//! with `pane.read`. Zooming into a box closes the board and shows that
//! pane's raw terminal under a one line header.

use super::*;
use crossterm::event::{KeyCode, MouseButton, MouseEventKind};

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(350);
/// Digits 1 to 9 address the boxes.
const MAX_BOXES: usize = 9;
const MIN_BOX_WIDTH: u16 = 28;
/// Borders plus the name and status rows. Shorter boxes are not drawn.
const MIN_BOX_HEIGHT: u16 = 2 + 2;
/// Borders, chrome and twelve preview rows for a subagent tile.
const MAX_TILE_HEIGHT: u16 = 2 + BOX_CHROME_ROWS + 12;
/// Box rows before the screen preview: name, status, rule.
const BOX_CHROME_ROWS: u16 = 3;
const GAP: u16 = 2;
const MARGIN: u16 = 2;
/// Master sits beside the tiles at this width, above them below it.
const SIDE_BY_SIDE_WIDTH: u16 = 90;

#[derive(Debug, Default)]
pub(super) struct ClientBoardOverlay {
    /// Master first, then every other pane in tab order.
    pub(super) pane_ids: Vec<String>,
    /// Every pane, including those past `MAX_BOXES`.
    pub(super) total: usize,
    /// The box the input line talks to.
    pub(super) selected: usize,
    pub(super) previews: HashMap<String, String>,
    pub(super) input: TextEditor,
    prefix_pending: bool,
    next_poll: Option<std::time::Instant>,
}

pub(super) struct BoardLayout {
    header: Rect,
    input: Rect,
    /// Index aligned with the overlay's pane ids, holding only boxes that fit.
    boxes: Vec<Rect>,
}

/// Header on top, input line at the bottom, boxes between. Master takes the
/// left three fifths on wide screens and the top three fifths otherwise.
pub(super) fn board_layout(area: Rect, count: usize) -> BoardLayout {
    let header = Rect::new(area.x, area.y, area.width, u16::from(area.height > 0));
    let input = if area.height >= 3 {
        Rect::new(area.x, area.bottom() - 1, area.width, 1)
    } else {
        Rect::default()
    };
    let body = Rect::new(
        area.x.saturating_add(MARGIN),
        area.y.saturating_add(2),
        area.width.saturating_sub(MARGIN * 2),
        area.height.saturating_sub(4),
    );
    let fits = |rect: Rect| rect.width >= MIN_BOX_WIDTH && rect.height >= MIN_BOX_HEIGHT;
    let mut boxes = Vec::new();
    if count > 0 && fits(body) {
        if count == 1 {
            boxes.push(body);
        } else {
            let (master, tiles) = if body.width >= SIDE_BY_SIDE_WIDTH {
                let width = body.width * 3 / 5;
                (
                    Rect::new(body.x, body.y, width, body.height),
                    Rect::new(
                        body.x + width + GAP,
                        body.y,
                        body.width.saturating_sub(width + GAP),
                        body.height,
                    ),
                )
            } else {
                let height = (body.height * 3 / 5).max(MIN_BOX_HEIGHT);
                (
                    Rect::new(body.x, body.y, body.width, height),
                    Rect::new(
                        body.x,
                        body.y + height + 1,
                        body.width,
                        body.height.saturating_sub(height + 1),
                    ),
                )
            };
            if fits(master) {
                boxes.push(master);
                boxes.extend(tile_grid(tiles, count - 1));
            }
        }
    }
    BoardLayout {
        header,
        input,
        boxes,
    }
}

/// Top aligned tiles, at most two across, as many as fit at the minimum size.
fn tile_grid(area: Rect, count: usize) -> Vec<Rect> {
    if count == 0 || area.width < MIN_BOX_WIDTH || area.height < MIN_BOX_HEIGHT {
        return Vec::new();
    }
    let mut columns = count.min(2);
    while columns > 1 && area.width.saturating_sub(GAP) / (columns as u16) < MIN_BOX_WIDTH {
        columns -= 1;
    }
    let max_rows = ((area.height + 1) / (MIN_BOX_HEIGHT + 1)) as usize;
    let count = count.min(max_rows * columns);
    let rows = count.div_ceil(columns) as u16;
    let width = area.width.saturating_sub(GAP * (columns as u16 - 1)) / columns as u16;
    let height = (area.height.saturating_sub(rows - 1) / rows).min(MAX_TILE_HEIGHT);
    (0..count)
        .map(|index| {
            let (row, column) = ((index / columns) as u16, (index % columns) as u16);
            Rect::new(
                area.x + column * (width + GAP),
                area.y + row * (height + 1),
                width,
                height,
            )
        })
        .collect()
}

impl ClientShellState {
    /// Master first, then every other pane by workspace and tab order.
    fn board_pane_ids(&self) -> Vec<String> {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return Vec::new();
        };
        let master = super::agent_sidebar::master_pane_id(snapshot).map(str::to_owned);
        let order = |pane: &crate::protocol::ClientShellPane| {
            let workspace = snapshot
                .workspaces
                .iter()
                .position(|workspace| workspace.workspace_id == pane.workspace_id)
                .unwrap_or(usize::MAX);
            let tab = snapshot
                .tabs
                .iter()
                .find(|tab| tab.tab_id == pane.tab_id)
                .map_or(usize::MAX, |tab| tab.number);
            (Some(&pane.pane_id) != master.as_ref(), workspace, tab)
        };
        let mut panes = snapshot.panes.iter().collect::<Vec<_>>();
        panes.sort_by_key(|pane| order(pane));
        panes.into_iter().map(|pane| pane.pane_id.clone()).collect()
    }

    pub(super) fn open_board(&mut self, outcome: &mut ClientShellInput) {
        self.board_zoomed = false;
        if !matches!(self.overlay, Some(ClientShellOverlay::Board(_))) {
            self.overlay = Some(ClientShellOverlay::Board(ClientBoardOverlay::default()));
        }
        self.poll_board(std::time::Instant::now(), outcome);
        outcome.repaint = true;
    }

    /// Opens the board whenever nothing else is on screen and the user has
    /// not zoomed into a pane, then keeps its previews fresh.
    pub(crate) fn tick_board(&mut self, now: std::time::Instant, outcome: &mut ClientShellInput) {
        if self.board && self.overlay.is_none() && !self.board_zoomed && self.snapshot.is_some() {
            self.open_board(outcome);
            return;
        }
        let due = matches!(
            &self.overlay,
            Some(ClientShellOverlay::Board(board)) if board.next_poll.is_none_or(|at| at <= now)
        );
        if due {
            self.poll_board(now, outcome);
        }
    }

    fn poll_board(&mut self, now: std::time::Instant, outcome: &mut ClientShellInput) {
        let mut pane_ids = self.board_pane_ids();
        let total = pane_ids.len();
        pane_ids.truncate(MAX_BOXES);
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_mut() else {
            return;
        };
        board.next_poll = Some(now + POLL_INTERVAL);
        if board.total != total || board.pane_ids != pane_ids {
            let selected = board.pane_ids.get(board.selected).cloned();
            board
                .previews
                .retain(|pane_id, _| pane_ids.contains(pane_id));
            board.selected = selected
                .and_then(|selected| pane_ids.iter().position(|pane_id| *pane_id == selected))
                .unwrap_or(0);
            board.pane_ids = pane_ids.clone();
            board.total = total;
            outcome.repaint = true;
        }
        for pane_id in pane_ids {
            let in_flight = self.pending_requests.values().any(|pending| {
                matches!(&pending.kind, PendingEndpointKind::BoardRead { pane_id: pending } if *pending == pane_id)
            });
            if in_flight {
                continue;
            }
            self.push_endpoint_method_with_kind(
                crate::api::schema::Method::PaneRead(crate::api::schema::PaneReadParams {
                    pane_id: pane_id.clone(),
                    source: crate::api::schema::ReadSource::Visible,
                    lines: None,
                    format: crate::api::schema::ReadFormat::Text,
                    strip_ansi: true,
                    intent: crate::api::schema::ReadIntent::Passive,
                }),
                PendingEndpointKind::BoardRead { pane_id },
                outcome,
            );
        }
    }

    /// Previews are best effort: a failed read keeps the last good frame.
    pub(super) fn receive_board_read(
        &mut self,
        pane_id: &str,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_mut() else {
            return false;
        };
        let Ok(crate::api::schema::ResponseResult::PaneRead { read }) = result else {
            return false;
        };
        if board.previews.get(pane_id) == Some(&read.text) {
            return false;
        }
        board.previews.insert(pane_id.to_owned(), read.text);
        true
    }

    pub(super) fn insert_board_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_mut() else {
            return false;
        };
        board.prefix_pending = false;
        board.input.insert(text);
        true
    }

    /// Typing goes to the input line. The prefix still reaches every prefix
    /// binding, with digits and space zooming into boxes.
    pub(super) fn route_board_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_mut() else {
            return false;
        };
        outcome.repaint = true;
        if board.prefix_pending {
            board.prefix_pending = false;
            let selected = board.selected;
            match key.code {
                KeyCode::Char(digit @ '1'..='9') => {
                    self.zoom_board(digit as usize - '1' as usize, outcome);
                }
                KeyCode::Char(' ') => self.zoom_board(selected, outcome),
                KeyCode::Esc => {}
                _ => {
                    if let Some(binding) =
                        crate::input::resolve_prefix_binding(&self.config.keybinds.keybinds, key)
                    {
                        self.record_binding(binding, outcome);
                    }
                }
            }
            return true;
        }
        if self.config.keybinds.matches_prefix(key) {
            board.prefix_pending = true;
            return true;
        }
        let count = board.pane_ids.len();
        let input_empty = board.input.as_str().is_empty();
        match key.code {
            KeyCode::Tab | KeyCode::Down
                if count > 0 && (key.code == KeyCode::Tab || input_empty) =>
            {
                board.selected = (board.selected + 1) % count;
            }
            KeyCode::BackTab | KeyCode::Up
                if count > 0 && (key.code == KeyCode::BackTab || input_empty) =>
            {
                board.selected = (board.selected + count - 1) % count;
            }
            KeyCode::Esc => board.input.clear(),
            KeyCode::Enter => self.submit_board_input(outcome),
            _ => {
                board.input.handle_key(key);
            }
        }
        true
    }

    /// Sends the line to the selected box: as an agent prompt when an agent
    /// runs there, otherwise as typed shell input. An empty line zooms in.
    fn submit_board_input(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_ref() else {
            return;
        };
        let selected = board.selected;
        let text = board.input.as_str().trim().to_owned();
        let Some(pane_id) = board.pane_ids.get(selected).cloned() else {
            return;
        };
        if text.is_empty() {
            self.zoom_board(selected, outcome);
            return;
        }
        let has_agent = self
            .snapshot
            .as_deref()
            .is_some_and(|snapshot| snapshot.agents.iter().any(|agent| agent.pane_id == pane_id));
        let method = if has_agent {
            crate::api::schema::Method::AgentPrompt(crate::api::schema::AgentPromptParams {
                target: pane_id,
                text,
                wait: None,
            })
        } else {
            crate::api::schema::Method::PaneSendText(crate::api::schema::PaneSendTextParams {
                pane_id,
                text: format!("{text}\r"),
            })
        };
        if self.push_endpoint_method_with_kind(method, PendingEndpointKind::Generic, outcome) {
            if let Some(ClientShellOverlay::Board(board)) = self.overlay.as_mut() {
                board.input.clear();
            }
        }
    }

    pub(super) fn zoom_board(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_ref() else {
            return;
        };
        let Some(pane_id) = board.pane_ids.get(index).cloned() else {
            return;
        };
        let endpoint_id = self.active_endpoint_id.clone();
        if self.focus_or_activate(
            endpoint_id,
            ClientEndpointFocusTarget::Pane(pane_id),
            outcome,
        ) {
            self.board_zoomed = true;
            self.overlay = None;
        }
        outcome.repaint = true;
    }

    /// A click selects a box, and a click on the selected box zooms into it.
    pub(super) fn handle_board_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let point = (mouse.column, mouse.row);
        let Some(index) = self
            .hits
            .board_boxes
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, index)| *index)
        else {
            return;
        };
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_mut() else {
            return;
        };
        if board.selected == index {
            self.zoom_board(index, outcome);
        } else {
            board.selected = index;
            outcome.repaint = true;
        }
    }
}

pub(super) fn render_board(
    buffer: &mut Buffer,
    board: &ClientBoardOverlay,
    snapshot: &ClientShellSnapshot,
    palette: &Palette,
) -> Option<super::render::OverlayRender> {
    let area = buffer.area;
    let mut rendered = super::render::OverlayRender {
        area,
        ..super::render::OverlayRender::default()
    };
    if area.is_empty() {
        return Some(rendered);
    }
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buffer[(x, y)].set_symbol(" ").set_style(
                Style::default()
                    .fg(palette.text)
                    .bg(ratatui::style::Color::Reset),
            );
        }
    }
    let layout = board_layout(area, board.pane_ids.len());
    render_board_header(
        buffer,
        layout.header,
        snapshot,
        board,
        layout.boxes.len(),
        palette,
    );
    for (index, (pane_id, rect)) in board.pane_ids.iter().zip(&layout.boxes).enumerate() {
        render_box(
            buffer,
            *rect,
            index,
            index == board.selected,
            snapshot,
            pane_id,
            board.previews.get(pane_id).map(String::as_str),
            palette,
        );
        rendered.board_boxes.push((*rect, index));
    }
    rendered.cursor = render_input(buffer, layout.input, board, snapshot, palette);
    Some(rendered)
}

/// Draws one row of untrusted text, never past `max_width` columns.
fn put(buffer: &mut Buffer, x: u16, y: u16, max_width: u16, text: &str, style: Style) -> u16 {
    let area = buffer.area;
    if max_width == 0 || y < area.y || y >= area.bottom() || x >= area.right() {
        return x;
    }
    let max_width = max_width.min(area.right() - x) as usize;
    let text = crate::ui::truncate_end(&crate::ui::sanitize_line(text), max_width);
    let (end, _) = buffer.set_stringn(x, y, &text, max_width, style);
    end
}

fn width_of(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Icon, headline and status for a box, whether or not an agent runs there.
fn box_identity(
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
) -> (
    &'static str,
    String,
    Option<crate::api::schema::AgentStatus>,
) {
    let master = super::agent_sidebar::master_pane_id(snapshot) == Some(pane_id);
    match snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
    {
        Some(agent) => (
            super::agent_sidebar::agent_icon(agent).unwrap_or("●"),
            super::agent_sidebar::agent_headline(snapshot, agent),
            Some(agent.agent_status),
        ),
        None => (
            "▸",
            if master {
                "master".into()
            } else {
                "shell".into()
            },
            None,
        ),
    }
}

/// Header segments in the order they are dropped when the row is too
/// narrow: the hint first, then counts from the right. The title stays.
fn render_board_header(
    buffer: &mut Buffer,
    row: Rect,
    snapshot: &ClientShellSnapshot,
    board: &ClientBoardOverlay,
    shown: usize,
    palette: &Palette,
) {
    if row.is_empty() {
        return;
    }
    let bg = palette.panel_bg;
    for x in row.x..row.right() {
        buffer[(x, row.y)]
            .set_symbol(" ")
            .set_style(Style::default().bg(bg));
    }
    let count_status = |status: crate::api::schema::AgentStatus| {
        board
            .pane_ids
            .iter()
            .filter(|pane_id| {
                snapshot
                    .agents
                    .iter()
                    .any(|agent| &agent.pane_id == *pane_id && agent.agent_status == status)
            })
            .count()
    };
    use crate::api::schema::AgentStatus;
    let agents = board.total.saturating_sub(1);
    let mut segments: Vec<(String, Style)> = vec![(
        " vcer".into(),
        Style::default()
            .fg(palette.accent)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    )];
    let summary = match (agents, shown < board.pane_ids.len().min(board.total)) {
        (0, _) => "  ·  master only".to_owned(),
        (1, _) => "  ·  1 subagent".to_owned(),
        (n, false) => format!("  ·  {n} subagents"),
        (n, true) => format!("  ·  {n} subagents, {} shown", shown.saturating_sub(1)),
    };
    segments.push((summary, Style::default().fg(palette.subtext0).bg(bg)));
    for (status, label, color) in [
        (AgentStatus::Working, "working", palette.yellow),
        (AgentStatus::Blocked, "need you", palette.red),
        (AgentStatus::Done, "done", palette.teal),
    ] {
        let count = count_status(status);
        if count > 0 {
            segments.push((
                format!("  {count} {label}"),
                Style::default().fg(color).bg(bg),
            ));
        }
    }
    let hint = "⌃b 1-9 zoom ";
    let available = row.width as usize;
    let mut used = 0usize;
    let mut kept = 0usize;
    for (text, _) in &segments {
        if used + width_of(text) > available {
            break;
        }
        used += width_of(text);
        kept += 1;
    }
    let mut x = row.x;
    for (text, style) in segments.iter().take(kept.max(1)) {
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            text,
            *style,
        );
    }
    if used + width_of(hint) + 2 <= available {
        let hint_x = row.right() - width_of(hint) as u16;
        put(
            buffer,
            hint_x,
            row.y,
            width_of(hint) as u16,
            hint,
            Style::default().fg(palette.overlay0).bg(bg),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_box(
    buffer: &mut Buffer,
    rect: Rect,
    index: usize,
    selected: bool,
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
    preview: Option<&str>,
    palette: &Palette,
) {
    let (icon, headline, status) = box_identity(snapshot, pane_id);
    let tone = status.map_or(palette.overlay0, |status| status_color(status, palette));
    let border = if selected { palette.accent } else { tone };
    let Some(inner) = super::render::overlay_panel(buffer, rect, border, palette.panel_bg) else {
        return;
    };
    let bg = palette.panel_bg;
    put(
        buffer,
        rect.x + 2,
        rect.y,
        rect.width.saturating_sub(4),
        &format!(" {} ", index + 1),
        Style::default()
            .fg(if selected {
                palette.accent_foreground()
            } else {
                palette.accent
            })
            .bg(if selected { palette.accent } else { bg })
            .add_modifier(Modifier::BOLD),
    );
    let width = inner.width.saturating_sub(2);
    let left = inner.x + 1;
    let mut x = put(
        buffer,
        left,
        inner.y,
        width,
        icon,
        Style::default().fg(tone).bg(bg),
    );
    x = put(
        buffer,
        x,
        inner.y,
        width.saturating_sub(x - left),
        " ",
        Style::default().bg(bg),
    );
    put(
        buffer,
        x,
        inner.y,
        width.saturating_sub(x - left),
        &headline,
        Style::default()
            .fg(palette.text)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    );
    if inner.height < 2 {
        return;
    }
    let state = match status {
        Some(crate::api::schema::AgentStatus::Blocked) => "needs you",
        Some(other) => status_text(other),
        None => "shell",
    };
    put(
        buffer,
        left,
        inner.y + 1,
        width,
        state,
        Style::default().fg(tone).bg(bg),
    );
    if inner.height < BOX_CHROME_ROWS {
        return;
    }
    let rule = "┄".repeat(inner.width as usize);
    put(
        buffer,
        inner.x,
        inner.y + 2,
        inner.width,
        &rule,
        Style::default().fg(palette.surface_dim).bg(bg),
    );
    let body_rows = inner.height.saturating_sub(BOX_CHROME_ROWS) as usize;
    let lines = preview.map(preview_tail).unwrap_or_default();
    if lines.is_empty() {
        let waiting = if preview.is_some() {
            "(blank screen)"
        } else {
            "loading…"
        };
        put(
            buffer,
            left,
            inner.y + BOX_CHROME_ROWS,
            width,
            waiting,
            Style::default().fg(palette.overlay0).bg(bg),
        );
        return;
    }
    // Anchored to the bottom like a chat, so the newest line sits last.
    let start = lines.len().saturating_sub(body_rows);
    let shown = &lines[start..];
    let first_row = inner.y + BOX_CHROME_ROWS + (body_rows - shown.len()) as u16;
    for (row, line) in shown.iter().enumerate() {
        put(
            buffer,
            left,
            first_row + row as u16,
            width,
            line,
            Style::default().fg(palette.subtext0).bg(bg),
        );
    }
}

/// `{icon} {target} ❯ message`, or a dim placeholder naming the keys.
fn render_input(
    buffer: &mut Buffer,
    row: Rect,
    board: &ClientBoardOverlay,
    snapshot: &ClientShellSnapshot,
    palette: &Palette,
) -> Option<crate::protocol::CursorState> {
    if row.is_empty() {
        return None;
    }
    let pane_id = board.pane_ids.get(board.selected)?;
    let (icon, headline, status) = box_identity(snapshot, pane_id);
    let tone = status.map_or(palette.overlay0, |status| status_color(status, palette));
    let target = crate::ui::truncate_end(&headline, (row.width / 3).max(6) as usize);
    let mut x = put(
        buffer,
        row.x + 1,
        row.y,
        row.width.saturating_sub(1),
        icon,
        Style::default().fg(tone),
    );
    x = put(
        buffer,
        x,
        row.y,
        row.right().saturating_sub(x),
        " ",
        Style::default(),
    );
    x = put(
        buffer,
        x,
        row.y,
        row.right().saturating_sub(x),
        &target,
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    );
    x = put(
        buffer,
        x,
        row.y,
        row.right().saturating_sub(x),
        " ❯ ",
        Style::default().fg(palette.accent),
    );
    let field = Rect::new(x, row.y, row.right().saturating_sub(x + 1), 1);
    if field.is_empty() {
        return None;
    }
    if board.input.as_str().is_empty() {
        let hint = if board.prefix_pending {
            "1-9 zoom into a box · space zoom into this one".to_owned()
        } else {
            format!("message {target} · tab switch · ⏎ on empty zooms in")
        };
        put(
            buffer,
            field.x,
            field.y,
            field.width,
            &hint,
            Style::default().fg(palette.overlay0),
        );
        return Some(crate::protocol::CursorState {
            x: field.x,
            y: field.y,
            visible: true,
            shape: 0,
        });
    }
    super::text_editor::render(
        buffer,
        field,
        &board.input,
        Style::default().fg(palette.text),
    )
}

/// Header shown while zoomed into a pane: the way back, then who this is.
pub(super) fn render_zoom_header(
    buffer: &mut Buffer,
    row: Rect,
    snapshot: &ClientShellSnapshot,
    palette: &Palette,
    hits: &mut ShellHitMap,
) {
    if row.is_empty() {
        return;
    }
    let bg = palette.panel_bg;
    for x in row.x..row.right() {
        buffer[(x, row.y)]
            .set_symbol(" ")
            .set_style(Style::default().bg(bg));
    }
    let back = " ‹ board ";
    let key = Style::default()
        .fg(palette.accent)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let end = put(buffer, row.x, row.y, row.width, back, key);
    hits.board_return = Rect::new(row.x, row.y, end - row.x, 1);
    let mut x = put(
        buffer,
        end,
        row.y,
        row.right().saturating_sub(end),
        "⌃b space",
        Style::default().fg(palette.overlay0).bg(bg),
    );
    if let Some(pane_id) = snapshot.focused_pane_id.as_deref() {
        let (icon, headline, status) = box_identity(snapshot, pane_id);
        let tone = status.map_or(palette.overlay0, |status| status_color(status, palette));
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            "   ·   ",
            Style::default().fg(palette.overlay0).bg(bg),
        );
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            icon,
            Style::default().fg(tone).bg(bg),
        );
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            " ",
            Style::default().bg(bg),
        );
        put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            &headline,
            Style::default()
                .fg(palette.text)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );
    }
}

/// The screen minus trailing blank rows and the agent's own input box, so
/// the box shows the work rather than an empty prompt and status footer.
fn preview_tail(text: &str) -> Vec<&str> {
    let mut lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    if let Some(cut) = input_box_start(&lines) {
        lines.truncate(cut);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// Claude Code draws its prompt as a `❯` line under a full width rule, or in
/// older versions as `│ >` inside a `╭─` box. Everything from that rule down
/// is input box and footer. Returns None when no such box is found, or when
/// cutting would leave nothing to show.
fn input_box_start(lines: &[&str]) -> Option<usize> {
    let is_rule = |line: &str| {
        let line = line.trim();
        line.chars().count() >= 10 && line.chars().all(|ch| ch == '─')
    };
    (1..lines.len())
        .rev()
        .find(|&index| {
            let line = lines[index].trim_start();
            let above = lines[index - 1];
            (line.starts_with('❯') && is_rule(above))
                || ((line.starts_with("│ >") || line.starts_with("│ ❯"))
                    && above.trim_start().starts_with("╭─"))
        })
        .map(|index| index - 1)
        .filter(|&cut| lines[..cut].iter().any(|line| !line.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::TerminalKey;
    use crossterm::event::KeyModifiers;

    const HEADLINES: &[&str] = &[
        "",
        "x",
        "Refactor auth middleware onto the new session API and migrate every caller in one pass so nothing drifts",
        "修复认证中间件的会话令牌轮换问题修复认证中间件的会话令牌轮换问题",
        "🚀 deploy the 🚀 thing",
        "tab\there\u{1b}[31m escape\u{7} bell\nnewline\rreturn",
    ];

    fn previews() -> Vec<Option<String>> {
        vec![
            None,
            Some(String::new()),
            Some("short".into()),
            Some("w".repeat(500)),
            Some("\tindented\twith\ttabs\n\u{1b}[1mbold\u{1b}[0m\r\ncarriage".into()),
            Some("界".repeat(90) + "\n" + &"é".repeat(90)),
            Some((0..200).map(|n| format!("line {n}\n")).collect()),
        ]
    }

    fn subagent(index: usize, headline: &str) -> crate::protocol::ClientShellAgent {
        crate::protocol::ClientShellAgent {
            pane_id: format!("sub_{index}"),
            workspace_id: "ws_1".into(),
            tab_id: format!("tab_sub_{index}"),
            name: None,
            display_agent: None,
            agent: Some(
                if index.is_multiple_of(2) {
                    "claude"
                } else {
                    "codex"
                }
                .into(),
            ),
            title: Some(headline.into()),
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: [
                crate::api::schema::AgentStatus::Working,
                crate::api::schema::AgentStatus::Blocked,
                crate::api::schema::AgentStatus::Done,
                crate::api::schema::AgentStatus::Idle,
            ][index % 4],
            state_change_seq: index as u64,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
        }
    }

    /// Master plus `count` subagents, each in its own tab, as vcer lays out.
    fn snapshot_with(count: usize, headline: &str) -> ClientShellSnapshot {
        let mut snapshot = super::super::tests::snapshot();
        for index in 0..count {
            snapshot.tabs.push(crate::protocol::ClientShellTab {
                tab_id: format!("tab_sub_{index}"),
                workspace_id: "ws_1".into(),
                number: index + 2,
                label: format!("{}", index + 2),
                custom_label: false,
                zoomed: false,
                focused: false,
                agent_status: crate::api::schema::AgentStatus::Idle,
            });
            snapshot.panes.push(crate::protocol::ClientShellPane {
                pane_id: format!("sub_{index}"),
                workspace_id: "ws_1".into(),
                tab_id: format!("tab_sub_{index}"),
                label: None,
                cwd: None,
                foreground_cwd: None,
                focused: false,
                right_click_passthrough: false,
            });
        }
        snapshot.agents = (0..count).map(|index| subagent(index, headline)).collect();
        snapshot
    }

    fn board_state(count: usize) -> ClientShellState {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        state.board = true;
        state.set_snapshot(Box::new(snapshot_with(count, "task")));
        state.set_pane_surface(super::super::tests::surface());
        state.compose(120, 30).expect("frame");
        let mut outcome = ClientShellInput::default();
        state.tick_board(std::time::Instant::now(), &mut outcome);
        state.compose(120, 30).expect("board frame");
        state
    }

    fn press(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
        state.handle_raw_events(vec![RawInputEvent::Key(TerminalKey::new(
            code,
            KeyModifiers::empty(),
        ))])
    }

    fn type_text(state: &mut ClientShellState, text: &str) {
        for ch in text.chars() {
            press(state, KeyCode::Char(ch));
        }
    }

    fn sent_methods(outcome: &ClientShellInput) -> Vec<&crate::api::schema::Method> {
        outcome
            .actions
            .iter()
            .filter_map(|action| match action {
                ClientShellAction::Endpoint { request, .. } => Some(&request.method),
                _ => None,
            })
            .collect()
    }

    fn board(state: &ClientShellState) -> &ClientBoardOverlay {
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::Board(board)) => board,
            _ => panic!("board should be open"),
        }
    }

    /// Content must never overwrite a box border, spill into the padding
    /// column, leak control characters, or leave the screen.
    fn assert_layout(buffer: &Buffer, boxes: &[Rect], context: &str) {
        let area = buffer.area;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let symbol = buffer[(x, y)].symbol();
                assert!(
                    !symbol.chars().any(char::is_control),
                    "{context}: control char {symbol:?} at ({x},{y})"
                );
            }
        }
        for (index, tile) in boxes.iter().enumerate() {
            assert!(
                tile.right() <= area.right() && tile.bottom() <= area.bottom(),
                "{context}: box {index} {tile:?} leaves {area:?}"
            );
            for other in &boxes[index + 1..] {
                assert!(
                    tile.intersection(*other).is_empty(),
                    "{context}: boxes overlap"
                );
            }
            let (left, right, top, bottom) = (tile.x, tile.right() - 1, tile.y, tile.bottom() - 1);
            for (x, y, expected) in [
                (left, top, "┌"),
                (right, top, "┐"),
                (left, bottom, "└"),
                (right, bottom, "┘"),
            ] {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    expected,
                    "{context}: corner of box {index}"
                );
            }
            for y in top + 1..bottom {
                assert_eq!(
                    buffer[(left, y)].symbol(),
                    "│",
                    "{context}: left edge of box {index}"
                );
                assert_eq!(
                    buffer[(right, y)].symbol(),
                    "│",
                    "{context}: right edge of box {index}"
                );
                if y != top + 3 {
                    assert_eq!(
                        buffer[(right - 1, y)].symbol(),
                        " ",
                        "{context}: padding column of box {index} row {y}"
                    );
                }
            }
        }
    }

    #[test]
    fn board_layout_holds_for_every_size_count_and_data_shape() {
        let palette = ClientShellConfig::from_config(&crate::config::Config::default()).palette;
        let sizes = [(10, 4), (30, 10), (60, 20), (80, 24), (130, 34), (220, 60)];
        let previews = previews();
        for (cols, rows) in sizes {
            for count in [0usize, 1, 2, 4, 8, 12] {
                for (variant, headline) in HEADLINES.iter().enumerate() {
                    let snapshot = snapshot_with(count, headline);
                    let mut pane_ids = vec!["pane_1".to_owned()];
                    pane_ids.extend((0..count).map(|index| format!("sub_{index}")));
                    let total = pane_ids.len();
                    pane_ids.truncate(MAX_BOXES);
                    let mut board = ClientBoardOverlay {
                        selected: pane_ids.len() - 1,
                        total,
                        ..ClientBoardOverlay::default()
                    };
                    for (index, pane_id) in pane_ids.iter().enumerate() {
                        if let Some(text) = &previews[(index + variant) % previews.len()] {
                            board.previews.insert(pane_id.clone(), text.clone());
                        }
                    }
                    board.input.insert(headline);
                    board.pane_ids = pane_ids;
                    let area = Rect::new(0, 0, cols, rows);
                    let mut buffer = Buffer::empty(area);
                    render_board(&mut buffer, &board, &snapshot, &palette);
                    let context = format!("{cols}x{rows} subagents={count} headline={variant}");
                    let layout = board_layout(area, board.pane_ids.len());
                    assert_layout(&buffer, &layout.boxes, &context);
                }
            }
        }
    }

    #[test]
    fn master_fills_the_body_alone_and_shares_it_with_subagents() {
        let area = Rect::new(0, 0, 130, 34);
        let alone = board_layout(area, 1);
        assert_eq!(alone.boxes.len(), 1);
        assert_eq!(alone.boxes[0].width, 126);
        let wide = board_layout(area, 4);
        assert_eq!(wide.boxes.len(), 4);
        assert!(
            wide.boxes[1].x > wide.boxes[0].right(),
            "tiles beside master when wide"
        );
        let narrow = board_layout(Rect::new(0, 0, 80, 40), 3);
        assert!(
            narrow.boxes[1].y > narrow.boxes[0].bottom(),
            "tiles below master when narrow"
        );
        assert_eq!(alone.input.y, 33, "input line sits on the last row");
    }

    #[test]
    fn board_opens_on_its_own_and_stays_closed_after_zooming() {
        let mut state = board_state(2);
        assert_eq!(board(&state).pane_ids, vec!["pane_1", "sub_0", "sub_1"]);
        press(&mut state, KeyCode::Enter);
        assert!(state.overlay.is_none(), "enter on an empty line zooms in");
        assert!(state.board_zoomed);
        let mut outcome = ClientShellInput::default();
        state.tick_board(std::time::Instant::now(), &mut outcome);
        assert!(
            state.overlay.is_none(),
            "a zoomed pane keeps the board closed"
        );
        state.compose(120, 30).expect("zoomed frame");
        assert!(
            !state.hits.board_return.is_empty(),
            "zoom header offers the way back"
        );
        assert!(state.hits.agents.is_empty(), "no sidebar on the board");
    }

    #[test]
    fn input_line_prompts_agents_and_types_into_shells() {
        let mut state = board_state(1);
        type_text(&mut state, "hello master");
        let outcome = press(&mut state, KeyCode::Enter);
        assert!(matches!(
            sent_methods(&outcome)[..],
            [crate::api::schema::Method::PaneSendText(params)]
                if params.pane_id == "pane_1" && params.text == "hello master\r"
        ));
        assert!(
            board(&state).input.as_str().is_empty(),
            "sent text clears the line"
        );

        press(&mut state, KeyCode::Tab);
        assert_eq!(board(&state).selected, 1);
        type_text(&mut state, "review the diff");
        let outcome = press(&mut state, KeyCode::Enter);
        assert!(matches!(
            sent_methods(&outcome)[..],
            [crate::api::schema::Method::AgentPrompt(params)]
                if params.target == "sub_0" && params.text == "review the diff" && params.wait.is_none()
        ));

        type_text(&mut state, "draft");
        press(&mut state, KeyCode::Esc);
        assert!(
            board(&state).input.as_str().is_empty(),
            "esc clears the line"
        );
    }

    #[test]
    fn prefix_digits_zoom_and_other_prefix_bindings_still_work() {
        let mut state = board_state(2);
        state.handle_raw_events(crate::raw_input::parse_framed_input(b"\x022"));
        assert!(
            state.overlay.is_none() && state.board_zoomed,
            "prefix 2 zooms into box 2"
        );

        let mut state = board_state(2);
        state.handle_raw_events(crate::raw_input::parse_framed_input(b"\x02?"));
        assert!(
            matches!(state.overlay, Some(ClientShellOverlay::Help(_))),
            "prefix ? still opens help"
        );
    }

    /// Ghostty and other kitty keyboard terminals encode ctrl+b and space
    /// differently from legacy terminals, and also report key releases.
    #[test]
    fn prefix_space_toggles_the_board_in_legacy_and_kitty_keyboard_modes() {
        let encodings: [(&str, &[u8]); 3] = [
            ("legacy", b"\x02 "),
            ("kitty", b"\x1b[98;5u\x1b[32u"),
            (
                "kitty with releases",
                b"\x1b[98;5u\x1b[98;5:3u\x1b[32u\x1b[32;1:3u",
            ),
        ];
        for (label, bytes) in encodings {
            let mut state = board_state(1);
            press(&mut state, KeyCode::Enter);
            assert!(state.overlay.is_none(), "{label}: zoomed in first");
            state.handle_raw_events(crate::raw_input::parse_framed_input(bytes));
            assert!(
                matches!(state.overlay, Some(ClientShellOverlay::Board(_))),
                "{label}: prefix then space returns to the board"
            );
        }
    }

    #[test]
    fn clicking_selects_a_box_and_clicking_it_again_zooms_in() {
        let mut state = board_state(2);
        let (rect, index) = state.hits.board_boxes[1];
        let click = |state: &mut ClientShellState| {
            state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 2,
                row: rect.y + 2,
                modifiers: KeyModifiers::empty(),
            })]);
        };
        click(&mut state);
        assert_eq!(board(&state).selected, index);
        click(&mut state);
        assert!(state.overlay.is_none() && state.board_zoomed);
    }

    #[test]
    fn preview_tail_cuts_claude_code_input_box_and_footer() {
        assert_eq!(preview_tail("a\nb  \n\n   \n"), vec!["a", "b"]);
        let rule = "─".repeat(60);
        let screen = format!(
            "⏺ I've started four research tracks.\n\n✻ Waiting for 4 background agents\n{rule}\n❯ \n{rule}\n  ⏵⏵ auto mode on\n  ◯ general-purpose  Searching\n"
        );
        assert_eq!(
            preview_tail(&screen),
            vec![
                "⏺ I've started four research tracks.",
                "",
                "✻ Waiting for 4 background agents"
            ]
        );
        let boxed = "⏺ Done.\n╭────────────╮\n│ >         │\n╰────────────╯\n  ? for shortcuts\n";
        assert_eq!(preview_tail(boxed), vec!["⏺ Done."]);
        let only_box = format!("{rule}\n❯ \n{rule}\n");
        assert!(
            !preview_tail(&only_box).is_empty(),
            "nothing above the box keeps the box"
        );
    }

    /// Claude Code titles itself "◐ Claude Code" while working.
    #[test]
    fn headlines_skip_status_glyphs_and_fall_back_to_the_agent_name() {
        let snapshot = super::super::tests::snapshot();
        let headline = |title: Option<&str>, terminal: Option<&str>, name: Option<&str>| {
            let mut agent = subagent(2, "");
            agent.title = title.map(str::to_owned);
            agent.terminal_title = terminal.map(str::to_owned);
            agent.name = name.map(str::to_owned);
            super::super::agent_sidebar::agent_headline(&snapshot, &agent)
        };
        assert_eq!(
            headline(None, Some("◐ Claude Code"), Some("test-counter")),
            "test-counter"
        );
        assert_eq!(
            headline(None, Some("⠋ Refactor auth"), Some("x")),
            "Refactor auth"
        );
        assert_eq!(headline(None, Some("◐ Claude Code"), None), "subagent");
        assert_eq!(headline(Some("(draft) notes"), None, None), "(draft) notes");
        let messy = headline(Some(HEADLINES[5]), None, None);
        assert!(!messy.chars().any(char::is_control), "{messy:?}");
    }
}
