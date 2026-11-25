//! Peek: every subagent laid out as a live mini terminal over a blanked
//! master. The client only streams the focused pane, so tiles poll each
//! subagent's visible screen with `pane.read` while the overlay is open.

use super::*;
use crossterm::event::KeyCode;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(350);
const MAX_TILES: usize = 9;
const MIN_TILE_WIDTH: u16 = 28;
const TILE_GAP: u16 = 2;
const MARGIN: u16 = 2;
/// Rows above the tiles: the hint line and a spacer.
const HEADER_ROWS: u16 = 2;
/// Tile rows before the screen preview: name, status, rule.
const TILE_CHROME_ROWS: u16 = 3;
/// Borders plus the name and status rows. Shorter tiles are not drawn.
const MIN_TILE_HEIGHT: u16 = 2 + 2;
/// Borders, chrome and twelve preview rows: a card, not a pane.
const MAX_TILE_HEIGHT: u16 = 2 + TILE_CHROME_ROWS + 12;

#[derive(Debug, Default)]
pub(super) struct ClientPeekOverlay {
    pub(super) pane_ids: Vec<String>,
    /// Every subagent, including those past `MAX_TILES`.
    pub(super) total: usize,
    pub(super) selected: usize,
    pub(super) previews: HashMap<String, String>,
    next_poll: Option<std::time::Instant>,
}

pub(super) struct PeekGrid {
    pub(super) columns: usize,
    tiles: Vec<Rect>,
}

/// Lays tiles out left to right, at most three across, shrinking the column
/// count until every tile is at least `MIN_TILE_WIDTH` wide. Holds only as
/// many tiles as fit at `MIN_TILE_HEIGHT`, so every tile it returns is on
/// screen and readable.
pub(super) fn peek_grid(area: Rect, count: usize) -> PeekGrid {
    let inner = Rect::new(
        area.x.saturating_add(MARGIN),
        area.y.saturating_add(HEADER_ROWS),
        area.width.saturating_sub(MARGIN * 2),
        area.height.saturating_sub(HEADER_ROWS + 1),
    );
    if count == 0 || inner.is_empty() {
        return PeekGrid {
            columns: 1,
            tiles: Vec::new(),
        };
    }
    let mut columns = count.min(3);
    while columns > 1 {
        let gaps = TILE_GAP * (columns as u16 - 1);
        if inner.width.saturating_sub(gaps) / columns as u16 >= MIN_TILE_WIDTH {
            break;
        }
        columns -= 1;
    }
    let max_rows = ((inner.height + 1) / (MIN_TILE_HEIGHT + 1)) as usize;
    let count = count.min(max_rows * columns);
    if count == 0 {
        return PeekGrid {
            columns,
            tiles: Vec::new(),
        };
    }
    let rows = count.div_ceil(columns) as u16;
    let width = inner.width.saturating_sub(TILE_GAP * (columns as u16 - 1)) / columns as u16;
    let height = (inner.height.saturating_sub(rows - 1) / rows).min(MAX_TILE_HEIGHT);
    let block_height = rows * height + (rows - 1);
    let top = inner.y + inner.height.saturating_sub(block_height) / 2;
    let tiles = (0..count)
        .map(|index| {
            let (row, column) = ((index / columns) as u16, (index % columns) as u16);
            Rect::new(
                inner.x + column * (width + TILE_GAP),
                top + row * (height + 1),
                width,
                height,
            )
        })
        .collect();
    PeekGrid { columns, tiles }
}

impl ClientShellState {
    /// Subagents in agent panel order. Master is the screen behind the tiles.
    fn peek_pane_ids(&self) -> Vec<String> {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return Vec::new();
        };
        let master = super::agent_sidebar::master_pane_id(snapshot).map(str::to_owned);
        super::agent_sidebar::ordered_agent_pane_ids(snapshot, self.config.agent_panel_sort)
            .into_iter()
            .filter(|pane_id| Some(pane_id) != master.as_ref())
            .collect()
    }

    pub(super) fn open_peek_overlay(&mut self, outcome: &mut ClientShellInput) {
        self.overlay = Some(ClientShellOverlay::Peek(ClientPeekOverlay::default()));
        self.poll_peek(std::time::Instant::now(), outcome);
        outcome.repaint = true;
    }

    pub(crate) fn tick_peek(&mut self, now: std::time::Instant, outcome: &mut ClientShellInput) {
        let due = matches!(
            &self.overlay,
            Some(ClientShellOverlay::Peek(peek)) if peek.next_poll.is_none_or(|at| at <= now)
        );
        if due {
            self.poll_peek(now, outcome);
        }
    }

    fn poll_peek(&mut self, now: std::time::Instant, outcome: &mut ClientShellInput) {
        let mut pane_ids = self.peek_pane_ids();
        let total = pane_ids.len();
        pane_ids.truncate(MAX_TILES);
        let Some(ClientShellOverlay::Peek(peek)) = self.overlay.as_mut() else {
            return;
        };
        peek.next_poll = Some(now + POLL_INTERVAL);
        if peek.total != total {
            peek.total = total;
            outcome.repaint = true;
        }
        if peek.pane_ids != pane_ids {
            peek.previews
                .retain(|pane_id, _| pane_ids.contains(pane_id));
            peek.selected = peek.selected.min(pane_ids.len().saturating_sub(1));
            peek.pane_ids = pane_ids.clone();
            outcome.repaint = true;
        }
        for pane_id in pane_ids {
            let in_flight = self.pending_requests.values().any(|pending| {
                matches!(&pending.kind, PendingEndpointKind::PeekRead { pane_id: pending } if *pending == pane_id)
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
                PendingEndpointKind::PeekRead { pane_id },
                outcome,
            );
        }
    }

    /// Previews are best effort: a failed read keeps the last good frame.
    pub(super) fn receive_peek_read(
        &mut self,
        pane_id: &str,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        let Some(ClientShellOverlay::Peek(peek)) = self.overlay.as_mut() else {
            return false;
        };
        let Ok(crate::api::schema::ResponseResult::PaneRead { read }) = result else {
            return false;
        };
        if peek.previews.get(pane_id) == Some(&read.text) {
            return false;
        }
        peek.previews.insert(pane_id.to_owned(), read.text);
        true
    }

    pub(super) fn route_peek_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::Peek(peek)) = self.overlay.as_ref() else {
            return false;
        };
        // Keys act on what is on screen: tiles that did not fit are not
        // reachable by digit or arrow.
        let (count, columns) = match self.last_composed_size {
            Some((cols, rows)) => {
                let grid = peek_grid(Rect::new(0, 0, cols, rows), peek.pane_ids.len());
                (grid.tiles.len(), grid.columns)
            }
            None => (peek.pane_ids.len(), 1),
        };
        let selected = peek.selected.min(count.saturating_sub(1));
        let step = |delta: isize| -> usize {
            (selected as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize
        };
        let next = match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(' ') => {
                self.overlay = None;
                outcome.repaint = true;
                return true;
            }
            KeyCode::Enter if count > 0 => {
                self.jump_to_peek_tile(selected, outcome);
                return true;
            }
            KeyCode::Char(digit @ '1'..='9') if (digit as usize - '1' as usize) < count => {
                self.jump_to_peek_tile(digit as usize - '1' as usize, outcome);
                return true;
            }
            KeyCode::Left | KeyCode::Char('h') => step(-1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => step(1),
            KeyCode::BackTab => step(-1),
            KeyCode::Up | KeyCode::Char('k') => step(-(columns as isize)),
            KeyCode::Down | KeyCode::Char('j') => step(columns as isize),
            _ => return true,
        };
        if let Some(ClientShellOverlay::Peek(peek)) = self.overlay.as_mut() {
            outcome.repaint |= peek.selected != next;
            peek.selected = next;
        }
        true
    }

    fn jump_to_peek_tile(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Peek(peek)) = self.overlay.as_ref() else {
            return;
        };
        let Some(pane_id) = peek.pane_ids.get(index).cloned() else {
            return;
        };
        let endpoint_id = self.active_endpoint_id.clone();
        if self.focus_or_activate(
            endpoint_id,
            ClientEndpointFocusTarget::Pane(pane_id),
            outcome,
        ) {
            self.overlay = None;
        }
        outcome.repaint = true;
    }
}

pub(super) fn render_peek_overlay(
    buffer: &mut Buffer,
    peek: &ClientPeekOverlay,
    snapshot: &ClientShellSnapshot,
    palette: &Palette,
) -> Option<super::render::OverlayRender> {
    let area = buffer.area;
    if area.is_empty() {
        return Some(super::render::OverlayRender::default());
    }
    // Blank the screen behind the tiles. Faint fragments of master and the
    // sidebar showing between tiles read as overflow, and the dim attribute
    // alone renders faintly or not at all in some terminals.
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &mut buffer[(x, y)];
            cell.set_symbol(" ").set_style(
                Style::default()
                    .fg(palette.surface1)
                    .bg(ratatui::style::Color::Reset)
                    .add_modifier(Modifier::DIM),
            );
        }
    }
    let master_icon = super::agent_sidebar::master_pane_id(snapshot)
        .and_then(|pane_id| {
            snapshot
                .agents
                .iter()
                .find(|agent| agent.pane_id == pane_id)
        })
        .and_then(super::agent_sidebar::agent_icon)
        .unwrap_or("●");
    let grid = peek_grid(area, peek.pane_ids.len());
    render_peek_header(
        buffer,
        area,
        grid.tiles.len(),
        peek.total.max(peek.pane_ids.len()),
        master_icon,
        palette,
    );
    if peek.pane_ids.is_empty() {
        render_peek_empty(buffer, area, palette);
        return Some(super::render::OverlayRender::default());
    }
    for (index, (pane_id, rect)) in peek.pane_ids.iter().zip(&grid.tiles).enumerate() {
        let Some(agent) = snapshot
            .agents
            .iter()
            .find(|agent| &agent.pane_id == pane_id)
        else {
            continue;
        };
        render_peek_tile(
            buffer,
            *rect,
            index,
            index == peek.selected,
            snapshot,
            agent,
            peek.previews.get(pane_id).map(String::as_str),
            palette,
        );
    }
    Some(super::render::OverlayRender::default())
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

/// Header segments in the order they are dropped when the row is too
/// narrow: hints go first, one whole hint at a time, then the summary.
/// The title always shows, truncated only when nothing else fits.
fn render_peek_header(
    buffer: &mut Buffer,
    area: Rect,
    shown: usize,
    total: usize,
    master_icon: &str,
    palette: &Palette,
) {
    if area.is_empty() {
        return;
    }
    let row = Rect::new(area.x, area.y, area.width, 1);
    buffer.set_style(
        row,
        Style::default()
            .bg(palette.panel_bg)
            .remove_modifier(Modifier::DIM),
    );
    for x in row.x..row.right() {
        buffer[(x, row.y)].set_symbol(" ");
    }
    let hint = Style::default().fg(palette.overlay0).bg(palette.panel_bg);
    let key = Style::default()
        .fg(palette.accent)
        .bg(palette.panel_bg)
        .add_modifier(Modifier::BOLD);
    let title = format!("{master_icon} master");
    let summary = match (shown, total) {
        (0, _) => "  ·  no subagents yet".to_owned(),
        (1, 1) => "  ·  peeking at 1 subagent".to_owned(),
        (shown, total) if shown < total => format!("  ·  peeking at {shown} of {total}"),
        (shown, _) => format!("  ·  peeking at {shown} subagents"),
    };
    let jump_label = format!("1-{}", shown.min(MAX_TILES));
    let jump = match shown {
        0 => None,
        1 => Some("1"),
        _ => Some(jump_label.as_str()),
    };
    // Display order, each with its keep priority: the way out always stays.
    let mut hints = Vec::new();
    if let Some(jump) = jump {
        hints.push((jump, " jump", 1));
        hints.push(("←→", " select", 2));
        hints.push(("⏎", " open", 3));
    }
    hints.push(("esc", " back", 0));

    let available = row.width.saturating_sub(2) as usize;
    let width = |text: &str| unicode_width::UnicodeWidthStr::width(text);
    let mut used = width(&title);
    let show_summary = used + width(&summary) <= available;
    if show_summary {
        used += width(&summary);
    }
    let cost = |kept: &[bool]| -> usize {
        let visible = hints
            .iter()
            .zip(kept)
            .filter(|(_, keep)| **keep)
            .map(|((label, text, _), _)| width(label) + width(text))
            .collect::<Vec<_>>();
        match visible.len() {
            0 => 0,
            n => width("  ·  ") + 2 * (n - 1) + visible.iter().sum::<usize>(),
        }
    };
    let mut kept = vec![false; hints.len()];
    let mut by_priority = (0..hints.len()).collect::<Vec<_>>();
    by_priority.sort_by_key(|index| hints[*index].2);
    for index in by_priority {
        kept[index] = true;
        if used + cost(&kept) > available {
            kept[index] = false;
            break;
        }
    }
    let shown_hints = hints
        .iter()
        .zip(&kept)
        .filter(|(_, keep)| **keep)
        .enumerate()
        .map(|(position, ((label, text, _), _))| {
            (if position == 0 { "  ·  " } else { "  " }, *label, *text)
        })
        .collect::<Vec<_>>();

    let right = row.right().saturating_sub(1);
    let mut x = put(
        buffer,
        row.x + 1,
        row.y,
        right.saturating_sub(row.x + 1),
        &title,
        key,
    );
    if show_summary {
        x = put(buffer, x, row.y, right.saturating_sub(x), &summary, hint);
    }
    for (lead, label, text) in shown_hints {
        x = put(buffer, x, row.y, right.saturating_sub(x), lead, hint);
        x = put(buffer, x, row.y, right.saturating_sub(x), label, key);
        x = put(buffer, x, row.y, right.saturating_sub(x), text, hint);
    }
}

/// Centered below the header. When that space is too short the header's own
/// "no subagents yet" says enough.
fn render_peek_empty(buffer: &mut Buffer, area: Rect, palette: &Palette) {
    const HEIGHT: u16 = 5;
    let body = Rect::new(
        area.x,
        area.y.saturating_add(HEADER_ROWS),
        area.width,
        area.height.saturating_sub(HEADER_ROWS),
    );
    if body.height < HEIGHT || body.width < 20 {
        return;
    }
    let width = 44.min(body.width.saturating_sub(MARGIN * 2));
    let rect = Rect::new(
        body.x + body.width.saturating_sub(width) / 2,
        body.y + body.height.saturating_sub(HEIGHT) / 2,
        width,
        HEIGHT,
    );
    let Some(inner) =
        super::render::overlay_panel(buffer, rect, palette.surface1, palette.panel_bg)
    else {
        return;
    };
    let style = Style::default().fg(palette.subtext0).bg(palette.panel_bg);
    put(
        buffer,
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        "no subagents yet",
        style,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_peek_tile(
    buffer: &mut Buffer,
    rect: Rect,
    index: usize,
    selected: bool,
    snapshot: &ClientShellSnapshot,
    agent: &crate::protocol::ClientShellAgent,
    preview: Option<&str>,
    palette: &Palette,
) {
    let tone = status_color(agent.agent_status, palette);
    let border = if selected { palette.accent } else { tone };
    let Some(inner) = super::render::overlay_panel(buffer, rect, border, palette.panel_bg) else {
        return;
    };
    let bg = palette.panel_bg;
    let label = format!(" {} ", index + 1);
    put(
        buffer,
        rect.x + 2,
        rect.y,
        rect.width.saturating_sub(4),
        &label,
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
    let icon = super::agent_sidebar::agent_icon(agent).unwrap_or("●");
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
        &super::agent_sidebar::agent_headline(snapshot, agent),
        Style::default()
            .fg(palette.text)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    );
    if inner.height < 2 {
        return;
    }
    let state = match agent.agent_status {
        crate::api::schema::AgentStatus::Blocked => "needs you",
        other => status_text(other),
    };
    put(
        buffer,
        left,
        inner.y + 1,
        width,
        state,
        Style::default().fg(tone).bg(bg),
    );
    if inner.height < TILE_CHROME_ROWS {
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

    let body_rows = inner.height.saturating_sub(TILE_CHROME_ROWS) as usize;
    let lines = preview.map(preview_tail).unwrap_or_default();
    let text_style = Style::default().fg(palette.subtext0).bg(bg);
    if lines.is_empty() {
        let waiting = if preview.is_some() {
            "(blank screen)"
        } else {
            "loading…"
        };
        put(
            buffer,
            left,
            inner.y + TILE_CHROME_ROWS,
            width,
            waiting,
            Style::default().fg(palette.overlay0).bg(bg),
        );
        return;
    }
    let start = lines.len().saturating_sub(body_rows);
    for (row, line) in lines[start..].iter().enumerate() {
        put(
            buffer,
            left,
            inner.y + TILE_CHROME_ROWS + row as u16,
            width,
            line,
            text_style,
        );
    }
}

/// The screen minus trailing blank rows, so the tile shows where the agent
/// is actually writing.
fn preview_tail(text: &str) -> Vec<&str> {
    let mut lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_caps_at_three_columns_and_wraps() {
        let grid = peek_grid(Rect::new(0, 0, 130, 34), 5);
        assert_eq!(grid.columns, 3);
        assert_eq!(grid.tiles.len(), 5);
        assert_eq!(grid.tiles[3].x, grid.tiles[0].x);
        assert!(grid.tiles[3].y > grid.tiles[0].y);
    }

    #[test]
    fn grid_drops_columns_when_tiles_would_be_too_narrow() {
        let grid = peek_grid(Rect::new(0, 0, 70, 30), 3);
        assert_eq!(grid.columns, 2);
        assert!(grid.tiles.iter().all(|tile| tile.width >= MIN_TILE_WIDTH));
    }

    #[test]
    fn preview_tail_drops_trailing_blank_rows() {
        assert_eq!(preview_tail("a\nb  \n\n   \n"), vec!["a", "b"]);
    }

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
            agent: Some(if index.is_multiple_of(2) { "claude" } else { "codex" }.into()),
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

    /// Content must never overwrite a tile border, spill into the padding
    /// column, leak control characters, or push the header to the edge.
    fn assert_layout(buffer: &Buffer, grid: &PeekGrid, context: &str) {
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
        if area.width > 1 {
            assert_eq!(
                buffer[(area.right() - 1, area.y)].symbol(),
                " ",
                "{context}: header reached the right edge"
            );
        }
        for (index, tile) in grid.tiles.iter().enumerate() {
            assert!(
                tile.right() <= area.right() && tile.bottom() <= area.bottom(),
                "{context}: tile {index} {tile:?} leaves {area:?}"
            );
            for other in &grid.tiles[index + 1..] {
                assert!(
                    tile.intersection(*other).is_empty(),
                    "{context}: tiles overlap"
                );
            }
            if tile.width < 2 || tile.height < 2 {
                continue;
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
                    "{context}: corner of tile {index}"
                );
            }
            for y in top + 1..bottom {
                assert_eq!(
                    buffer[(left, y)].symbol(),
                    "│",
                    "{context}: left edge of tile {index}"
                );
                assert_eq!(
                    buffer[(right, y)].symbol(),
                    "│",
                    "{context}: right edge of tile {index}"
                );
                let rule_row = top + 1 + 2;
                if tile.width > 3 && y != rule_row {
                    assert_eq!(
                        buffer[(right - 1, y)].symbol(),
                        " ",
                        "{context}: padding column of tile {index} row {y}"
                    );
                }
            }
        }
    }

    #[test]
    fn peek_layout_holds_for_every_size_count_and_data_shape() {
        let palette = ClientShellConfig::from_config(&crate::config::Config::default()).palette;
        let sizes = [
            (10, 4),
            (24, 8),
            (40, 12),
            (60, 20),
            (80, 24),
            (130, 34),
            (220, 60),
        ];
        let counts = [0, 1, 2, 3, 4, 8, 9, 12];
        let previews = previews();
        for (cols, rows) in sizes {
            for count in counts {
                for (variant, headline) in HEADLINES.iter().enumerate() {
                    let mut snapshot = super::super::tests::snapshot();
                    snapshot.agents = (0..count).map(|index| subagent(index, headline)).collect();
                    let shown = count.min(MAX_TILES);
                    let mut peek = ClientPeekOverlay {
                        pane_ids: (0..shown).map(|index| format!("sub_{index}")).collect(),
                        total: count,
                        selected: shown.saturating_sub(1),
                        ..ClientPeekOverlay::default()
                    };
                    for index in 0..shown {
                        if let Some(text) = &previews[(index + variant) % previews.len()] {
                            peek.previews.insert(format!("sub_{index}"), text.clone());
                        }
                    }
                    let area = Rect::new(0, 0, cols, rows);
                    let mut buffer = Buffer::empty(area);
                    render_peek_overlay(&mut buffer, &peek, &snapshot, &palette);
                    let context = format!("{cols}x{rows} agents={count} headline={variant}");
                    assert_layout(&buffer, &peek_grid(area, shown), &context);
                }
            }
        }
    }

    #[test]
    fn header_drops_whole_hints_before_truncating() {
        let palette = ClientShellConfig::from_config(&crate::config::Config::default()).palette;
        let row_text = |cols: u16, shown: usize, total: usize| {
            let area = Rect::new(0, 0, cols, 1);
            let mut buffer = Buffer::empty(area);
            render_peek_header(&mut buffer, area, shown, total, "✳", &palette);
            (0..cols)
                .map(|x| buffer[(x, 0)].symbol().to_owned())
                .collect::<String>()
        };
        let wide = row_text(120, 3, 3);
        assert!(wide.contains("peeking at 3 subagents"), "{wide:?}");
        assert!(wide.contains("esc back"), "{wide:?}");
        assert!(
            wide.contains("1-3 jump"),
            "jump range matches the tiles: {wide:?}"
        );
        let narrow = row_text(40, 3, 3);
        assert!(
            !narrow.contains('…'),
            "hints are dropped, not cut: {narrow:?}"
        );
        assert!(narrow.contains("master"), "{narrow:?}");
        let overflow = row_text(120, 9, 12);
        assert!(overflow.contains("peeking at 9 of 12"), "{overflow:?}");
        for cols in [50, 60, 70] {
            let tight = row_text(cols, 7, 7);
            assert!(
                tight.contains("esc back"),
                "{cols} cols keeps the way out: {tight:?}"
            );
            assert!(!tight.contains('…'), "{cols} cols: {tight:?}");
        }
        assert!(
            row_text(6, 3, 3).contains('…'),
            "a tiny row truncates the title"
        );
    }

    /// The sidebar shows the same headlines, so the composed TUI must stay
    /// on its grid for every headline shape and terminal width too.
    #[test]
    fn sidebar_rows_stay_on_grid_for_every_headline_shape() {
        for cols in [50u16, 80, 130, 220] {
            for headline in HEADLINES {
                let mut snapshot = super::super::tests::snapshot();
                super::super::tests::fan_out(&mut snapshot);
                snapshot.agents = (0..4).map(|index| subagent(index, headline)).collect();
                let mut state = ClientShellState::new(ClientShellConfig::from_config(
                    &crate::config::Config::default(),
                ));
                state.set_snapshot(Box::new(snapshot));
                state.set_pane_surface(super::super::tests::surface());
                let frame = state.compose(cols, 24).expect("composed frame");
                assert_eq!(frame.cells.len(), cols as usize * 24);
                for cell in &frame.cells {
                    assert!(
                        !cell.symbol.chars().any(char::is_control),
                        "{cols} cols {headline:?}: control char {:?}",
                        cell.symbol
                    );
                }
                let mobile = cols <= crate::config::DEFAULT_MOBILE_WIDTH_THRESHOLD;
                let sidebar = state.hits.agents.first().map(|(rect, _)| *rect);
                match (mobile, sidebar) {
                    (true, None) => {}
                    (true, Some(rect)) => panic!("{cols} cols is mobile, no sidebar: {rect:?}"),
                    (false, None) => panic!("{cols} cols {headline:?}: sidebar has no agent rows"),
                    (false, Some(rect)) => {
                        assert!(
                            rect.right() <= cols,
                            "{cols} cols: agent row {rect:?} off screen"
                        )
                    }
                }
            }
        }
    }

    #[test]
    fn sanitized_headlines_carry_no_control_characters() {
        let snapshot = super::super::tests::snapshot();
        let agent = subagent(2, HEADLINES[5]);
        let headline = super::super::agent_sidebar::agent_headline(&snapshot, &agent);
        assert!(!headline.chars().any(char::is_control), "{headline:?}");
        assert!(headline.contains("tab"), "{headline:?}");
    }
}
