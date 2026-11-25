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
/// A tile tall enough to show a few lines of its screen. Below this, a
/// second column opens instead of squeezing tiles further.
const MIN_TILE_HEIGHT: u16 = 8;
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
    /// When each agent entered its current status, for the elapsed clock.
    since: HashMap<String, (crate::api::schema::AgentStatus, std::time::Instant)>,
    /// Spinner frame and render clock, advanced by the client timer.
    frame: usize,
    now: Option<std::time::Instant>,
}

pub(super) struct BoardLayout {
    header: Rect,
    input: Rect,
    /// Index aligned with the overlay's pane ids, holding only boxes that fit.
    boxes: Vec<Rect>,
}

/// Header on top, input line at the bottom, boxes between. Master takes the
/// left three fifths on wide screens and the top half otherwise.
pub(super) fn board_layout(area: Rect, count: usize) -> BoardLayout {
    let header = Rect::new(area.x, area.y, area.width, u16::from(area.height > 0));
    let input = if area.height >= 3 {
        Rect::new(area.x, area.bottom() - 1, area.width, 1)
    } else {
        Rect::default()
    };
    // Flush: no margins and no gaps, like any native terminal layout.
    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(2),
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
                        body.x + width,
                        body.y,
                        body.width.saturating_sub(width),
                        body.height,
                    ),
                )
            } else {
                // Stacked, the tiles need more room than beside master.
                let height = (body.height / 2).max(MIN_BOX_HEIGHT);
                (
                    Rect::new(body.x, body.y, body.width, height),
                    Rect::new(
                        body.x,
                        body.y + height,
                        body.width,
                        body.height.saturating_sub(height),
                    ),
                )
            };
            if fits(master) {
                boxes.push(master);
                // Neighbors share one border line, like native terminal panes.
                boxes.extend(tile_grid(tiles, count - 1).into_iter().map(|tile| {
                    let left = u16::from(tile.x > body.x);
                    let up = u16::from(tile.y > body.y);
                    Rect::new(
                        tile.x - left,
                        tile.y - up,
                        tile.width + left,
                        tile.height + up,
                    )
                }));
            }
        }
    }
    BoardLayout {
        header,
        input,
        boxes,
    }
}

/// Tiles fill their area top to bottom in one column, touching like panes.
/// A second column only opens once one column cannot show every tile at a
/// readable height. Leftover rows and columns go to the last tile so the
/// grid ends exactly at the edge, and a partly filled last row stretches.
fn tile_grid(area: Rect, count: usize) -> Vec<Rect> {
    if count == 0 || area.width < MIN_BOX_WIDTH || area.height < MIN_BOX_HEIGHT {
        return Vec::new();
    }
    let one_column_rows = (area.height / MIN_TILE_HEIGHT).max(1) as usize;
    let mut columns = if count > one_column_rows { 2 } else { 1 };
    while columns > 1 && area.width / (columns as u16) < MIN_BOX_WIDTH {
        columns -= 1;
    }
    let max_rows = (area.height / MIN_BOX_HEIGHT) as usize;
    let count = count.min(max_rows * columns);
    let rows = count.div_ceil(columns);
    let split = |total: u16, parts: usize, index: usize| -> (u16, u16) {
        let size = total / parts as u16;
        let start = size * index as u16;
        let end = if index + 1 == parts {
            total
        } else {
            start + size
        };
        (start, end - start)
    };
    (0..count)
        .map(|index| {
            let (row, column) = (index / columns, index % columns);
            let in_row = if row + 1 == rows {
                count - row * columns
            } else {
                columns
            };
            let (x, width) = split(area.width, in_row, column);
            let (y, height) = split(area.height, rows, row);
            Rect::new(area.x + x, area.y + y, width, height)
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
        let working = self.snapshot.as_deref().is_some_and(|snapshot| {
            snapshot
                .agents
                .iter()
                .any(|agent| agent.agent_status == crate::api::schema::AgentStatus::Working)
        });
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_mut() else {
            return;
        };
        board.now = Some(now);
        if working {
            board.frame = board.frame.wrapping_add(1);
            outcome.repaint = true;
        }
        if board.next_poll.is_none_or(|at| at <= now) {
            self.poll_board(now, outcome);
        }
    }

    fn poll_board(&mut self, now: std::time::Instant, outcome: &mut ClientShellInput) {
        let mut pane_ids = self.board_pane_ids();
        let total = pane_ids.len();
        pane_ids.truncate(MAX_BOXES);
        let statuses = self.snapshot.as_deref().map_or_else(Vec::new, |snapshot| {
            snapshot
                .agents
                .iter()
                .map(|agent| (agent.pane_id.clone(), agent.agent_status))
                .collect::<Vec<_>>()
        });
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_mut() else {
            return;
        };
        board.next_poll = Some(now + POLL_INTERVAL);
        board.now = Some(now);
        board
            .since
            .retain(|pane_id, _| statuses.iter().any(|(id, _)| id == pane_id));
        for (pane_id, status) in statuses {
            let changed = board
                .since
                .get(&pane_id)
                .is_none_or(|(seen, _)| *seen != status);
            if changed {
                board.since.insert(pane_id, (status, now));
            }
        }
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
                    format: crate::api::schema::ReadFormat::Ansi,
                    strip_ansi: false,
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
            // Reset, not patch: a patched style keeps attributes such as dim
            // from whatever was drawn underneath.
            let cell = &mut buffer[(x, y)];
            cell.reset();
            cell.set_style(Style::default().fg(palette.text));
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
    // Frames first, the selected one last so its accent outline stays whole
    // where borders are shared. Titles and content go on top afterwards, so
    // no frame drawn later can cut through a neighbor's title.
    let boxes = board
        .pane_ids
        .iter()
        .zip(&layout.boxes)
        .enumerate()
        .collect::<Vec<_>>();
    let frame_order = boxes
        .iter()
        .filter(|(index, _)| *index != board.selected)
        .chain(boxes.iter().filter(|(index, _)| *index == board.selected));
    for (index, (pane_id, rect)) in frame_order {
        render_box_frame(buffer, **rect, *index, board, snapshot, pane_id, palette);
    }
    for (index, (pane_id, rect)) in &boxes {
        render_box(buffer, **rect, *index, board, snapshot, pane_id, palette);
        rendered.board_boxes.push((**rect, *index));
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

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn elapsed_label(since: std::time::Duration) -> String {
    let seconds = since.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// Status pill text and color: a spinner and clock while working, a loud
/// marker when the agent needs the user, quiet words otherwise.
fn status_pill(
    board: &ClientBoardOverlay,
    pane_id: &str,
    status: Option<crate::api::schema::AgentStatus>,
    palette: &Palette,
) -> (String, Style) {
    use crate::api::schema::AgentStatus;
    let elapsed = board
        .since
        .get(pane_id)
        .zip(board.now)
        .map(|((_, since), now)| elapsed_label(now.saturating_duration_since(*since)));
    let with_clock = |text: String| match &elapsed {
        Some(elapsed) => format!("{text} {elapsed}"),
        None => text,
    };
    match status {
        Some(AgentStatus::Working) => (
            with_clock(format!("{} working", SPINNER[board.frame % SPINNER.len()])),
            Style::default().fg(palette.yellow),
        ),
        Some(AgentStatus::Blocked) => (
            with_clock("● needs you".into()),
            Style::default()
                .fg(palette.red)
                .add_modifier(Modifier::BOLD),
        ),
        Some(AgentStatus::Done) => ("✓ done".into(), Style::default().fg(palette.teal)),
        Some(AgentStatus::Idle) => ("idle".into(), Style::default().fg(palette.overlay0)),
        Some(AgentStatus::Unknown) => ("…".into(), Style::default().fg(palette.overlay0)),
        None => ("shell".into(), Style::default().fg(palette.overlay0)),
    }
}

/// The top bar: wordmark, how many agents, live counts, and the zoom hint.
/// The hint drops first, then counts from the right.
fn render_board_header(
    buffer: &mut Buffer,
    row: Rect,
    snapshot: &ClientShellSnapshot,
    board: &ClientBoardOverlay,
    shown: usize,
    palette: &Palette,
) {
    use crate::api::schema::AgentStatus;
    if row.is_empty() {
        return;
    }
    // The terminal's own background, so vcer reads as part of it.
    let bg = ratatui::style::Color::Reset;
    for x in row.x..row.right() {
        buffer[(x, row.y)]
            .set_symbol(" ")
            .set_style(Style::default().bg(bg));
    }
    let subagents = board.total.saturating_sub(1);
    let summary = match subagents {
        0 => "master".to_owned(),
        1 => "master + 1 subagent".to_owned(),
        n if shown < board.total.min(MAX_BOXES) || board.total > MAX_BOXES => {
            format!("master + {n} subagents ({} shown)", shown.saturating_sub(1))
        }
        n => format!("master + {n} subagents"),
    };
    let count = |wanted: AgentStatus| {
        snapshot
            .agents
            .iter()
            .filter(|agent| agent.agent_status == wanted)
            .count()
    };
    let mut right: Vec<(String, Style)> = Vec::new();
    for (status, label, color) in [
        (AgentStatus::Working, "working", palette.yellow),
        (AgentStatus::Blocked, "need you", palette.red),
        (AgentStatus::Done, "done", palette.teal),
    ] {
        let n = count(status);
        if n > 0 {
            right.push((format!("● {n} {label}"), Style::default().fg(color).bg(bg)));
        }
    }
    let hint = (
        "⌃b space zoom".to_owned(),
        Style::default().fg(palette.overlay0).bg(bg),
    );

    let brand = " ◆ vcer ";
    let brand_style = Style::default()
        .fg(palette.accent)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let mut x = put(buffer, row.x, row.y, row.width, brand, brand_style);
    x = put(
        buffer,
        x,
        row.y,
        row.right().saturating_sub(x),
        "│ ",
        Style::default().fg(palette.surface1).bg(bg),
    );
    let left_end = put(
        buffer,
        x,
        row.y,
        row.right().saturating_sub(x),
        &summary,
        Style::default().fg(palette.text).bg(bg),
    );

    // Right side, built from the edge inward with two spaces between parts.
    let mut parts = right;
    parts.push(hint);
    loop {
        let total: usize = parts
            .iter()
            .map(|(text, _)| width_of(text) + 3)
            .sum::<usize>()
            + 1;
        if parts.is_empty() || (left_end as usize) + 2 + total <= row.right() as usize {
            break;
        }
        parts.pop();
    }
    let total: usize = parts
        .iter()
        .map(|(text, _)| width_of(text) + 3)
        .sum::<usize>();
    let mut x = row.right().saturating_sub(total as u16 + 1);
    for (text, style) in parts {
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            "   ",
            Style::default().bg(bg),
        );
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            &text,
            style,
        );
    }
}

/// A rounded box filled with `bg`. Returns the area inside the border.
fn rounded_box(
    buffer: &mut Buffer,
    rect: Rect,
    border: Style,
    bg: ratatui::style::Color,
) -> Option<Rect> {
    let rect = rect.intersection(buffer.area);
    if rect.width < 2 || rect.height < 2 {
        return None;
    }
    for y in rect.y + 1..rect.bottom() - 1 {
        for x in rect.x + 1..rect.right() - 1 {
            buffer[(x, y)]
                .set_symbol(" ")
                .set_style(Style::default().bg(bg));
        }
    }
    let border = border.bg(ratatui::style::Color::Reset);
    let (right, bottom) = (rect.right() - 1, rect.bottom() - 1);
    for x in rect.x..=right {
        let ends = if x == rect.x {
            RIGHT
        } else if x == right {
            LEFT
        } else {
            LEFT | RIGHT
        };
        let top_arms = ends | if x == rect.x || x == right { DOWN } else { 0 };
        let bottom_arms = ends | if x == rect.x || x == right { UP } else { 0 };
        draw_edge(buffer, x, rect.y, top_arms, border);
        draw_edge(buffer, x, bottom, bottom_arms, border);
    }
    for y in rect.y + 1..bottom {
        draw_edge(buffer, rect.x, y, UP | DOWN, border);
        draw_edge(buffer, right, y, UP | DOWN, border);
    }
    Some(Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width - 2,
        rect.height - 2,
    ))
}

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

/// The directions a box drawing character reaches, as `UP | DOWN | ...`.
fn edge_arms(symbol: &str) -> u8 {
    match symbol {
        "─" => LEFT | RIGHT,
        "│" => UP | DOWN,
        "╭" | "┌" => DOWN | RIGHT,
        "╮" | "┐" => DOWN | LEFT,
        "╰" | "└" => UP | RIGHT,
        "╯" | "┘" => UP | LEFT,
        "├" => UP | DOWN | RIGHT,
        "┤" => UP | DOWN | LEFT,
        "┬" => DOWN | LEFT | RIGHT,
        "┴" => UP | LEFT | RIGHT,
        "┼" => UP | DOWN | LEFT | RIGHT,
        _ => 0,
    }
}

fn edge_symbol(arms: u8) -> &'static str {
    match arms {
        a if a == LEFT | RIGHT || a == LEFT || a == RIGHT => "─",
        a if a == UP | DOWN || a == UP || a == DOWN => "│",
        a if a == DOWN | RIGHT => "╭",
        a if a == DOWN | LEFT => "╮",
        a if a == UP | RIGHT => "╰",
        a if a == UP | LEFT => "╯",
        a if a == UP | DOWN | RIGHT => "├",
        a if a == UP | DOWN | LEFT => "┤",
        a if a == DOWN | LEFT | RIGHT => "┬",
        a if a == UP | LEFT | RIGHT => "┴",
        _ => "┼",
    }
}

/// Draws a border cell, joining any line already there into a junction so
/// boxes that share an edge show one line with proper tees and crosses.
fn draw_edge(buffer: &mut Buffer, x: u16, y: u16, arms: u8, style: Style) {
    let cell = &mut buffer[(x, y)];
    let joined = edge_arms(cell.symbol()) | arms;
    cell.set_symbol(edge_symbol(joined)).set_style(style);
}

/// The box outline: accent when selected, red when the agent needs the
/// user, quiet otherwise. Shared edges join into junctions.
fn render_box_frame(
    buffer: &mut Buffer,
    rect: Rect,
    index: usize,
    board: &ClientBoardOverlay,
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
    palette: &Palette,
) {
    let (_, _, status) = box_identity(snapshot, pane_id);
    let color = if index == board.selected {
        palette.accent
    } else if status == Some(crate::api::schema::AgentStatus::Blocked) {
        palette.red
    } else {
        palette.surface1
    };
    rounded_box(
        buffer,
        rect,
        Style::default().fg(color),
        ratatui::style::Color::Reset,
    );
}

/// `╭─ 1  ✳ headline ───────── ⠹ working 2m ─╮` over the agent's own screen,
/// in its own colors, newest line at the bottom.
fn render_box(
    buffer: &mut Buffer,
    rect: Rect,
    index: usize,
    board: &ClientBoardOverlay,
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
    palette: &Palette,
) {
    let selected = index == board.selected;
    let (icon, headline, status) = box_identity(snapshot, pane_id);
    // The terminal's own background, so vcer reads as part of it.
    let bg = ratatui::style::Color::Reset;
    let rect = rect.intersection(buffer.area);
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);

    // Title row inside the top border.
    let (pill, pill_style) = status_pill(board, pane_id, status, palette);
    let pill_width = width_of(&pill) as u16 + 2;
    let title_y = rect.y;
    let edge = ratatui::style::Color::Reset;
    let title_right = rect.right().saturating_sub(2);
    let badge = format!(" {} ", index + 1);
    let badge_style = if selected {
        Style::default()
            .fg(palette.accent_foreground())
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.overlay1).bg(edge)
    };
    let after_badge = rect.x + 2 + width_of(&badge) as u16;
    let show_pill = title_right.saturating_sub(after_badge) > pill_width + 12;
    let limit = if show_pill {
        title_right.saturating_sub(pill_width + 1)
    } else {
        title_right
    };
    let icon_color = status.map_or(palette.overlay0, |status| status_color(status, palette));
    let name_style = Style::default()
        .fg(if selected {
            palette.text
        } else {
            palette.subtext0
        })
        .bg(edge)
        .add_modifier(Modifier::BOLD);
    let gap = Style::default().bg(edge);
    let mut x = put(
        buffer,
        rect.x + 2,
        title_y,
        limit.saturating_sub(rect.x + 2),
        &badge,
        badge_style,
    );
    x = put(buffer, x, title_y, limit.saturating_sub(x), " ", gap);
    x = put(
        buffer,
        x,
        title_y,
        limit.saturating_sub(x),
        icon,
        Style::default().fg(icon_color).bg(edge),
    );
    x = put(buffer, x, title_y, limit.saturating_sub(x), " ", gap);
    x = put(
        buffer,
        x,
        title_y,
        limit.saturating_sub(x + 1),
        &headline,
        name_style,
    );
    put(buffer, x, title_y, limit.saturating_sub(x), " ", gap);
    if show_pill {
        let x = put(
            buffer,
            title_right.saturating_sub(pill_width),
            title_y,
            1,
            " ",
            gap,
        );
        let x = put(
            buffer,
            x,
            title_y,
            pill_width - 2,
            &pill,
            pill_style.bg(edge),
        );
        put(buffer, x, title_y, 1, " ", gap);
    }

    // Content: the pane's own screen, padded one column each side.
    let content = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    if content.is_empty() {
        return;
    }
    let base = Style::default().fg(palette.text).bg(bg);
    let lines = board
        .previews
        .get(pane_id)
        .map(|text| preview_lines(text))
        .unwrap_or_default();
    if lines.is_empty() {
        let waiting = if board.previews.contains_key(pane_id) {
            "blank screen"
        } else {
            "connecting…"
        };
        let x = content.x + content.width.saturating_sub(width_of(waiting) as u16) / 2;
        let y = content.y + content.height / 2;
        put(
            buffer,
            x,
            y,
            content.right().saturating_sub(x),
            waiting,
            Style::default().fg(palette.overlay0).bg(bg),
        );
        return;
    }
    let rows = content.height as usize;
    let start = lines.len().saturating_sub(rows);
    let shown = &lines[start..];
    let first_row = content.y + (rows - shown.len()) as u16;
    for (row, line) in shown.iter().enumerate() {
        draw_styled_line(
            buffer,
            content.x,
            first_row + row as u16,
            content.width,
            line,
            base,
        );
    }
}

/// One screen row: its plain text, for detection, and its styled runs.
#[derive(Debug, Default)]
struct StyledLine {
    plain: String,
    spans: Vec<(String, Style)>,
}

/// Parses an ANSI screen dump into styled rows. Only SGR styling is kept.
/// Other escape sequences and control characters are dropped, tabs become
/// spaces, and backgrounds are kept so message bubbles still read as such.
fn parse_ansi(text: &str) -> Vec<StyledLine> {
    let mut lines = Vec::new();
    let mut line = StyledLine::default();
    let mut style = Style::default();
    let mut run = String::new();
    let mut width = 0usize;
    let mut chars = text.chars().peekable();
    let flush = |line: &mut StyledLine, run: &mut String, style: Style| {
        if !run.is_empty() {
            line.spans.push((std::mem::take(run), style));
        }
    };
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => {
                flush(&mut line, &mut run, style);
                lines.push(std::mem::take(&mut line));
                width = 0;
            }
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut final_byte = None;
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            final_byte = Some(next);
                            break;
                        }
                        params.push(next);
                    }
                    if final_byte == Some('m') {
                        flush(&mut line, &mut run, style);
                        style = apply_sgr(style, &params);
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\t' => {
                let pad = 4 - width % 4;
                run.extend(std::iter::repeat_n(' ', pad));
                line.plain.extend(std::iter::repeat_n(' ', pad));
                width += pad;
            }
            ch if ch.is_control() => {}
            ch => {
                run.push(ch);
                line.plain.push(ch);
                width += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            }
        }
    }
    flush(&mut line, &mut run, style);
    if !line.plain.is_empty() || !line.spans.is_empty() {
        lines.push(line);
    }
    lines
}

fn apply_sgr(style: Style, params: &str) -> Style {
    use ratatui::style::Color;
    let codes = params
        .split([';', ':'])
        .map(|code| code.parse::<u16>().ok())
        .collect::<Vec<_>>();
    let mut style = style;
    let mut index = 0;
    if codes.iter().all(Option::is_none) {
        return Style::default();
    }
    let color_at = |index: &mut usize| -> Option<Color> {
        match codes.get(*index + 1).copied().flatten() {
            Some(5) => {
                let color = codes
                    .get(*index + 2)
                    .copied()
                    .flatten()
                    .map(|n| Color::Indexed(n as u8));
                *index += 2;
                color
            }
            Some(2) => {
                // Accept both 38;2;r;g;b and the colon form 38:2::r:g:b.
                let rest = &codes[*index + 2..];
                let values = rest.iter().copied().flatten().take(3).collect::<Vec<_>>();
                let skip = if rest.first().is_some_and(Option::is_none) {
                    4
                } else {
                    3
                };
                *index += 1 + skip;
                (values.len() == 3)
                    .then(|| Color::Rgb(values[0] as u8, values[1] as u8, values[2] as u8))
            }
            _ => None,
        }
    };
    while index < codes.len() {
        match codes[index] {
            Some(0) | None => style = Style::default(),
            Some(1) => style = style.add_modifier(Modifier::BOLD),
            Some(2) => style = style.add_modifier(Modifier::DIM),
            Some(3) => style = style.add_modifier(Modifier::ITALIC),
            Some(4) => style = style.add_modifier(Modifier::UNDERLINED),
            Some(7) => style = style.add_modifier(Modifier::REVERSED),
            Some(9) => style = style.add_modifier(Modifier::CROSSED_OUT),
            Some(22) => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            Some(23) => style = style.remove_modifier(Modifier::ITALIC),
            Some(24) => style = style.remove_modifier(Modifier::UNDERLINED),
            Some(27) => style = style.remove_modifier(Modifier::REVERSED),
            Some(29) => style = style.remove_modifier(Modifier::CROSSED_OUT),
            Some(code @ 30..=37) => style.fg = Some(Color::Indexed((code - 30) as u8)),
            Some(code @ 90..=97) => style.fg = Some(Color::Indexed((code - 90 + 8) as u8)),
            Some(code @ 40..=47) => style.bg = Some(Color::Indexed((code - 40) as u8)),
            Some(code @ 100..=107) => style.bg = Some(Color::Indexed((code - 100 + 8) as u8)),
            Some(38) => style.fg = color_at(&mut index).or(style.fg),
            Some(48) => style.bg = color_at(&mut index).or(style.bg),
            Some(39) => style.fg = None,
            Some(49) => style.bg = None,
            Some(_) => {}
        }
        index += 1;
    }
    style
}

/// The screen as styled rows, minus trailing blank rows and the agent's own
/// input box and footer, so the box shows the work.
fn preview_lines(text: &str) -> Vec<StyledLine> {
    let mut lines = parse_ansi(text);
    let plain = lines
        .iter()
        .map(|line| line.plain.trim_end())
        .collect::<Vec<_>>();
    if let Some(cut) = input_box_start(&plain) {
        lines.truncate(cut);
    }
    while lines
        .last()
        .is_some_and(|line| line.plain.trim().is_empty())
    {
        lines.pop();
    }
    lines
}

/// Draws styled runs over `base`, cut to `max_width` with a trailing `…`.
fn draw_styled_line(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    max_width: u16,
    line: &StyledLine,
    base: Style,
) {
    let max_width = max_width as usize;
    let total = width_of(&line.plain);
    let budget = if total > max_width {
        max_width.saturating_sub(1)
    } else {
        max_width
    };
    let mut used = 0usize;
    let mut cursor = x;
    'runs: for (text, style) in &line.spans {
        let style = base.patch(*style);
        for ch in text.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + ch_width > budget {
                break 'runs;
            }
            let mut encoded = [0u8; 4];
            let (end, _) = buffer.set_stringn(
                cursor,
                y,
                ch.encode_utf8(&mut encoded),
                ch_width.max(1),
                style,
            );
            cursor = end;
            used += ch_width;
        }
    }
    if total > max_width && max_width > 0 {
        buffer.set_stringn(cursor, y, "…", 1, base.fg(base.fg.unwrap_or_default()));
    }
}

/// One minimal row: `✳ master ❯ message…`, with key hints on the right.
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
    let icon_color = status.map_or(palette.overlay0, |status| status_color(status, palette));
    let target = crate::ui::truncate_end(&headline, (row.width / 3).max(6) as usize);
    let mut x = put(
        buffer,
        row.x + 2,
        row.y,
        row.width.saturating_sub(2),
        icon,
        Style::default().fg(icon_color),
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
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    );

    let hint = if board.prefix_pending {
        "1-9 zoom  ·  space zoom here"
    } else if board.input.as_str().is_empty() {
        "⇥ switch  ·  ⏎ zoom in"
    } else {
        "⏎ send"
    };
    let hint_width = width_of(hint) as u16;
    let show_hint = row.right().saturating_sub(x) > hint_width + 20;
    let field_right = if show_hint {
        row.right().saturating_sub(hint_width + 3)
    } else {
        row.right().saturating_sub(1)
    };
    if show_hint {
        put(
            buffer,
            field_right + 1,
            row.y,
            hint_width + 1,
            hint,
            Style::default().fg(palette.overlay0),
        );
    }
    let field = Rect::new(x, row.y, field_right.saturating_sub(x), 1);
    if field.is_empty() {
        return None;
    }
    if board.input.as_str().is_empty() {
        put(
            buffer,
            field.x,
            field.y,
            field.width,
            &format!("Message {target}…"),
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
    // The terminal's own background, so vcer reads as part of it.
    let bg = ratatui::style::Color::Reset;
    for x in row.x..row.right() {
        buffer[(x, row.y)]
            .set_symbol(" ")
            .set_style(Style::default().bg(bg));
    }
    let key = Style::default()
        .fg(palette.accent)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let end = put(buffer, row.x, row.y, row.width, " ‹ board ", key);
    hits.board_return = Rect::new(row.x, row.y, end - row.x, 1);
    let mut x = put(
        buffer,
        end,
        row.y,
        row.right().saturating_sub(end),
        "│ ",
        Style::default().fg(palette.surface1).bg(bg),
    );
    if let Some(pane_id) = snapshot.focused_pane_id.as_deref() {
        let (icon, headline, status) = box_identity(snapshot, pane_id);
        let icon_color = status.map_or(palette.overlay0, |status| status_color(status, palette));
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            icon,
            Style::default().fg(icon_color).bg(bg),
        );
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            " ",
            Style::default().bg(bg),
        );
        x = put(
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
    let hint = "⌃b space board ";
    if (x as usize) + width_of(hint) + 3 <= row.right() as usize {
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
        const CORNERS: &[&str] = &["╭", "╮", "╰", "╯", "├", "┤", "┬", "┴", "┼"];
        const VERTICAL: &[&str] = &["│", "├", "┤", "┼"];
        for (index, tile) in boxes.iter().enumerate() {
            assert!(
                tile.right() <= area.right() && tile.bottom() <= area.bottom(),
                "{context}: box {index} {tile:?} leaves {area:?}"
            );
            for other in &boxes[index + 1..] {
                let shared = tile.intersection(*other);
                assert!(
                    shared.is_empty() || shared.width == 1 || shared.height == 1,
                    "{context}: boxes overlap past a shared border: {shared:?}"
                );
            }
            let (left, right, top, bottom) = (tile.x, tile.right() - 1, tile.y, tile.bottom() - 1);
            for (x, y) in [(left, top), (right, top), (left, bottom), (right, bottom)] {
                let symbol = buffer[(x, y)].symbol();
                assert!(
                    CORNERS.contains(&symbol),
                    "{context}: corner of box {index} at ({x},{y}) is {symbol:?}"
                );
            }
            for y in top + 1..bottom {
                for x in [left, right] {
                    let symbol = buffer[(x, y)].symbol();
                    assert!(
                        VERTICAL.contains(&symbol),
                        "{context}: edge of box {index} at ({x},{y}) is {symbol:?}"
                    );
                }
                assert_eq!(
                    buffer[(right - 1, y)].symbol(),
                    " ",
                    "{context}: padding column of box {index} row {y}"
                );
            }
            assert!(
                matches!(buffer[(right - 1, top)].symbol(), "─" | " "),
                "{context}: title of box {index} ran into its corner"
            );
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
        assert_eq!(alone.boxes[0].width, 130, "flush to both edges");
        let wide = board_layout(area, 4);
        assert_eq!(wide.boxes.len(), 4);
        assert!(
            wide.boxes[1].x + 1 == wide.boxes[0].right(),
            "tiles beside master when wide"
        );
        let narrow = board_layout(Rect::new(0, 0, 80, 40), 3);
        assert!(
            narrow.boxes[1].y + 1 == narrow.boxes[0].bottom(),
            "tiles below master when narrow"
        );
        assert_eq!(alone.input.y, 33, "input line sits on the last row");
    }

    /// Overlays dim the screen behind them, which once grayed out the whole
    /// board. Nothing the board draws itself may carry that dim.
    #[test]
    fn the_composed_board_is_never_dimmed() {
        let mut state = board_state(3);
        let frame = state.compose(120, 30).expect("board frame");
        let buffer = frame.to_ratatui_buffer().expect("board buffer");
        for y in 0..30 {
            for x in 0..120 {
                let cell = &buffer[(x, y)];
                assert!(
                    !cell.modifier.contains(Modifier::DIM),
                    "cell ({x},{y}) {:?} is dimmed",
                    cell.symbol()
                );
            }
        }
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

    #[test]
    fn ansi_screens_keep_colors_and_drop_everything_else() {
        use ratatui::style::Color;
        let lines = parse_ansi(
            "\u{1b}[0m\u{1b}[38;2;215;119;87m⏺\u{1b}[0m Done \u{1b}[1;31mfail\u{1b}[22;39m ok\r\n\u{1b}]0;title\u{7}a\tb\u{7}c\n\u{1b}[38:2::1:2:3mx\u{1b}[38;5;208my",
        );
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].plain, "⏺ Done fail ok");
        assert_eq!(
            lines[0].spans[0],
            (
                "⏺".to_owned(),
                Style::default().fg(Color::Rgb(215, 119, 87))
            )
        );
        let fail = lines[0]
            .spans
            .iter()
            .find(|(text, _)| text == "fail")
            .expect("fail run");
        assert_eq!(fail.1.fg, Some(Color::Indexed(1)));
        assert!(fail.1.add_modifier.contains(Modifier::BOLD));
        let ok = lines[0]
            .spans
            .iter()
            .find(|(text, _)| text == " ok")
            .expect("ok run");
        assert_eq!(ok.1.fg, None);
        assert_eq!(
            lines[1].plain, "a   bc",
            "osc dropped, tab expanded, bell dropped"
        );
        assert_eq!(lines[2].spans[0].1.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(lines[2].spans[1].1.fg, Some(Color::Indexed(208)));
    }

    #[test]
    fn status_pills_spin_and_count_while_working() {
        use crate::api::schema::AgentStatus;
        let palette = ClientShellConfig::from_config(&crate::config::Config::default()).palette;
        let now = std::time::Instant::now();
        let mut board = ClientBoardOverlay {
            now: Some(now),
            ..ClientBoardOverlay::default()
        };
        board.since.insert(
            "a".into(),
            (
                AgentStatus::Working,
                now - std::time::Duration::from_secs(135),
            ),
        );
        let (first, _) = status_pill(&board, "a", Some(AgentStatus::Working), &palette);
        board.frame += 1;
        let (second, _) = status_pill(&board, "a", Some(AgentStatus::Working), &palette);
        assert!(first.ends_with("working 2m"), "{first:?}");
        assert_ne!(first, second, "the spinner advances");
        assert_eq!(status_pill(&board, "b", None, &palette).0, "shell");
    }

    fn preview_tail(text: &str) -> Vec<String> {
        preview_lines(text)
            .into_iter()
            .map(|line| line.plain.trim_end().to_owned())
            .collect()
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
