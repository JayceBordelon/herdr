//! The board, vcer's main screen. Master's transcript takes most of it, and
//! every subagent is a compact status card in a rail beside it: what it is
//! doing, how long, and any question it is waiting on, sorted by urgency.
//! One minimal input line messages the selected agent. The client only
//! streams the focused pane, so the board polls each pane's visible screen
//! with `pane.read`. Zooming into an agent closes the board and shows its
//! raw terminal under a one line header.

use super::*;
use crossterm::event::{KeyCode, MouseButton, MouseEventKind};

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(350);
/// Borders plus the name and status rows. Shorter boxes are not drawn.
const MIN_BOX_HEIGHT: u16 = 2 + 2;
/// The rail sits beside master at this width, below it otherwise.
const SIDE_BY_SIDE_WIDTH: u16 = 90;
/// How long a status change stays in the ticker.
const TICKER_WINDOW: std::time::Duration = std::time::Duration::from_secs(90);
/// Options shown for a pending question. Digits 1 to 9 answer them.
const MAX_OPTIONS: usize = 9;
/// Screen lines shown under the selected card.
const SELECTED_PREVIEW_ROWS: usize = 6;

#[derive(Debug, Default)]
pub(super) struct ClientBoardOverlay {
    /// Master first, then every other pane in tab order. Indices into this
    /// list are stable while the rail re-sorts by urgency.
    pub(super) pane_ids: Vec<String>,
    /// The pane the input line talks to, as an index into `pane_ids`.
    pub(super) selected: usize,
    pub(super) previews: HashMap<String, String>,
    pub(super) input: TextEditor,
    prefix_pending: bool,
    next_poll: Option<std::time::Instant>,
    /// When each agent entered its current status, for the elapsed clock.
    since: HashMap<String, (crate::api::schema::AgentStatus, std::time::Instant)>,
    /// Recent status changes for the ticker, newest last.
    events: Vec<BoardEvent>,
    /// Spinner frame and render clock, advanced by the client timer.
    frame: usize,
    now: Option<std::time::Instant>,
}

#[derive(Debug, Clone)]
struct BoardEvent {
    name: String,
    status: crate::api::schema::AgentStatus,
    at: std::time::Instant,
}

pub(super) struct BoardLayout {
    header: Rect,
    ticker: Rect,
    input: Rect,
    master: Rect,
    /// Empty while master is alone.
    rail: Rect,
}

/// Header on top, input at the bottom, and the ticker just above the input
/// while something recent happened. Master fills the rest, sharing one
/// border with the rail: beside it on wide screens, below it otherwise.
pub(super) fn board_layout(area: Rect, has_rail: bool, ticker: bool) -> BoardLayout {
    let header = Rect::new(area.x, area.y, area.width, u16::from(area.height > 0));
    let input = if area.height >= 3 {
        Rect::new(area.x, area.bottom() - 1, area.width, 1)
    } else {
        Rect::default()
    };
    let ticker = if ticker && area.height >= 8 {
        Rect::new(area.x, area.bottom() - 2, area.width, 1)
    } else {
        Rect::default()
    };
    let reserved = 1 + input.height + ticker.height;
    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(reserved),
    );
    if !has_rail {
        return BoardLayout {
            header,
            ticker,
            input,
            master: body,
            rail: Rect::default(),
        };
    }
    let (master, rail) = if body.width >= SIDE_BY_SIDE_WIDTH {
        let width = (body.width * 3 / 10).clamp(34, 60);
        (
            Rect::new(body.x, body.y, body.width - width + 1, body.height),
            Rect::new(body.right() - width, body.y, width, body.height),
        )
    } else {
        let height =
            (body.height * 2 / 5).clamp(6, body.height.saturating_sub(MIN_BOX_HEIGHT).max(6));
        let height = height.min(body.height);
        (
            Rect::new(body.x, body.y, body.width, body.height - height + 1),
            Rect::new(body.x, body.bottom() - height, body.width, height),
        )
    };
    BoardLayout {
        header,
        ticker,
        input,
        master,
        rail,
    }
}

fn agent_icon(agent: &crate::protocol::ClientShellAgent) -> Option<&'static str> {
    match agent.agent.as_deref()? {
        "claude" => Some("✳"),
        "codex" => Some("⬢"),
        _ => None,
    }
}

/// The main session is the pane in the first tab of the first workspace.
fn master_pane_id(snapshot: &ClientShellSnapshot) -> Option<&str> {
    let workspace = snapshot.workspaces.iter().min_by_key(|ws| ws.number)?;
    let tab = snapshot
        .tabs
        .iter()
        .filter(|tab| tab.workspace_id == workspace.workspace_id)
        .min_by_key(|tab| tab.number)?;
    snapshot
        .panes
        .iter()
        .find(|pane| pane.tab_id == tab.tab_id)
        .map(|pane| pane.pane_id.as_str())
}

/// "master" for the main session, otherwise the task the agent reports in
/// its title, then the name it was started with. Titles that only name the
/// agent say nothing about the task.
fn agent_headline(
    snapshot: &ClientShellSnapshot,
    agent: &crate::protocol::ClientShellAgent,
) -> String {
    if master_pane_id(snapshot) == Some(agent.pane_id.as_str()) {
        return "master".into();
    }
    let names_only_the_agent = |title: &str| {
        let title = title.to_ascii_lowercase();
        title.is_empty()
            || agent.agent.as_deref().is_some_and(|name| title == name)
            || matches!(title.as_str(), "claude code" | "codex")
    };
    [
        agent.title.as_deref(),
        agent.terminal_title_stripped.as_deref(),
        agent.terminal_title.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(clean_title)
    .find(|title| !names_only_the_agent(title))
    .or_else(|| {
        agent
            .name
            .as_deref()
            .map(clean_title)
            .filter(|name| !name.is_empty())
    })
    .unwrap_or_else(|| "subagent".into())
}

/// Agents prefix their terminal title with status glyphs such as `◐`, `⠋`
/// or `✳`, which the board already shows as the agent's icon.
fn clean_title(title: &str) -> String {
    crate::ui::sanitize_line(title)
        .trim_start_matches(|ch: char| !ch.is_alphanumeric() && !"([{\"'`#@~/.".contains(ch))
        .trim()
        .to_owned()
}

/// What the rail knows about one agent.
#[derive(Debug)]
struct Card {
    index: usize,
    pane_id: String,
    icon: &'static str,
    /// The name the agent was started with, else its task.
    name: String,
    /// The task it reports, when that says more than the name.
    task: Option<String>,
    status: Option<crate::api::schema::AgentStatus>,
    activity: Option<String>,
    question: Option<Question>,
}

#[derive(Debug, PartialEq)]
struct Question {
    prompt: Option<String>,
    options: Vec<String>,
}

/// Lower sorts first: agents that need the user, then busy ones, then the
/// rest, which the rail collapses.
fn urgency(status: Option<crate::api::schema::AgentStatus>) -> u8 {
    use crate::api::schema::AgentStatus;
    match status {
        Some(AgentStatus::Blocked) => 0,
        Some(AgentStatus::Working) => 1,
        Some(AgentStatus::Unknown) | None => 2,
        Some(AgentStatus::Idle | AgentStatus::Done) => 3,
    }
}

fn finished(status: Option<crate::api::schema::AgentStatus>) -> bool {
    urgency(status) == 3
}

/// The name the user knows an agent by: the name master started it with,
/// else its task, else what runs there.
fn display_name(snapshot: &ClientShellSnapshot, pane_id: &str) -> String {
    if master_pane_id(snapshot) == Some(pane_id) {
        return "master".into();
    }
    match snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
    {
        Some(agent) => agent
            .name
            .as_deref()
            .map(|name| crate::ui::sanitize_line(name).trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| agent_headline(snapshot, agent)),
        None => "shell".into(),
    }
}

fn card_for(board: &ClientBoardOverlay, snapshot: &ClientShellSnapshot, index: usize) -> Card {
    let pane_id = board.pane_ids[index].clone();
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id);
    let name = display_name(snapshot, &pane_id);
    let task = agent
        .map(|agent| agent_headline(snapshot, agent))
        .filter(|task| *task != name && task != "subagent");
    let status = agent.map(|agent| agent.agent_status);
    let lines = board
        .previews
        .get(&pane_id)
        .map(|text| preview_lines(text))
        .unwrap_or_default();
    let plain = lines
        .iter()
        .map(|line| line.plain.as_str())
        .collect::<Vec<_>>();
    let question = (status == Some(crate::api::schema::AgentStatus::Blocked))
        .then(|| detect_question(&plain))
        .flatten();
    Card {
        index,
        icon: agent
            .and_then(agent_icon)
            .unwrap_or(if agent.is_some() { "●" } else { "▸" }),
        activity: activity_line(&plain, status),
        pane_id,
        name,
        task,
        status,
        question,
    }
}

/// Subagent indices in rail order: most urgent first, then tab order.
fn rail_order(board: &ClientBoardOverlay, snapshot: &ClientShellSnapshot) -> Vec<usize> {
    let mut order = (1..board.pane_ids.len()).collect::<Vec<_>>();
    let status = |index: usize| {
        snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == board.pane_ids[index])
            .map(|agent| agent.agent_status)
    };
    order.sort_by_key(|index| (urgency(status(*index)), *index));
    order
}

const ACTION_GLYPHS: [char; 3] = ['⏺', '●', '•'];
const SPINNER_GLYPHS: [char; 8] = ['✻', '✳', '✶', '✢', '✽', '✺', '·', '*'];

/// One line saying what the agent is doing. While it works, Claude Code's
/// live status line (`✻ Refactoring… (48s · ↓ 2k tokens)`) says it best.
/// Otherwise the newest action or answer line, marked `⏺`, `●` or `•`.
/// Falls back to the last non-empty line.
fn activity_line(
    lines: &[&str],
    status: Option<crate::api::schema::AgentStatus>,
) -> Option<String> {
    let strip =
        |line: &str, glyphs: &[char]| line.trim().trim_start_matches(glyphs).trim().to_owned();
    if status == Some(crate::api::schema::AgentStatus::Working) {
        let live = lines.iter().rev().find(|line| {
            let line = line.trim();
            line.starts_with(SPINNER_GLYPHS) && line.contains('…')
        });
        if let Some(line) = live {
            let text = strip(line, &SPINNER_GLYPHS);
            return Some(text.split(" (").next().unwrap_or(&text).to_owned());
        }
    }
    lines
        .iter()
        .rev()
        .find(|line| line.trim().starts_with(ACTION_GLYPHS))
        .map(|line| strip(line, &ACTION_GLYPHS))
        .filter(|text| !text.is_empty())
        .or_else(|| {
            lines
                .iter()
                .rev()
                .map(|line| line.trim())
                .find(|line| !line.is_empty() && !line.starts_with(SPINNER_GLYPHS))
                .map(str::to_owned)
        })
}

/// Finds a numbered menu (`❯ 1. board.rs`, `2. Peek history`, …) near the
/// bottom of the screen and the question above it. The options must count
/// up from 1. Lines between options, such as descriptions, are skipped.
fn detect_question(lines: &[&str]) -> Option<Question> {
    let option = |line: &str| -> Option<(usize, String)> {
        let line = line
            .trim_start()
            .trim_start_matches(['❯', '>', '›'])
            .trim_start();
        let (number, rest) = line.split_once(". ")?;
        let number = number.parse::<usize>().ok()?;
        (1..=MAX_OPTIONS)
            .contains(&number)
            .then(|| (number, rest.trim().to_owned()))
    };
    let found = lines
        .iter()
        .enumerate()
        .filter_map(|(row, line)| option(line).map(|(number, text)| (row, number, text)))
        .collect::<Vec<_>>();
    let mut run = Vec::new();
    for (row, number, text) in found.iter().rev() {
        let expected = run
            .last()
            .map(|(_, last, _): &(usize, usize, String)| last - 1);
        match expected {
            None => run.push((*row, *number, text.clone())),
            Some(expected) if *number == expected => run.push((*row, *number, text.clone())),
            Some(_) => break,
        }
        if *number == 1 {
            break;
        }
    }
    run.reverse();
    if run.len() < 2 || run.first().map(|(_, number, _)| *number) != Some(1) {
        return None;
    }
    let first_row = run[0].0;
    let prompt = lines[..first_row]
        .iter()
        .rev()
        .map(|line| line.trim())
        .find(|line| !line.is_empty() && !line.chars().all(|ch| ch == '─' || ch == '━'))
        .map(|line| {
            line.trim_start_matches(['☐', '☑', '□', '■', '?'])
                .trim()
                .to_owned()
        })
        .filter(|line| !line.is_empty());
    Some(Question {
        prompt,
        options: run.into_iter().map(|(_, _, text)| text).collect(),
    })
}

impl ClientShellState {
    /// Master first, then every other pane by workspace and tab order.
    fn board_pane_ids(&self) -> Vec<String> {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return Vec::new();
        };
        let master = master_pane_id(snapshot).map(str::to_owned);
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
        let pane_ids = self.board_pane_ids();
        let statuses = self.snapshot.as_deref().map_or_else(Vec::new, |snapshot| {
            snapshot
                .agents
                .iter()
                .map(|agent| {
                    (
                        agent.pane_id.clone(),
                        agent.agent_status,
                        display_name(snapshot, &agent.pane_id),
                    )
                })
                .collect::<Vec<_>>()
        });
        let Some(ClientShellOverlay::Board(board)) = self.overlay.as_mut() else {
            return;
        };
        board.next_poll = Some(now + POLL_INTERVAL);
        board.now = Some(now);
        board
            .since
            .retain(|pane_id, _| statuses.iter().any(|(id, _, _)| id == pane_id));
        for (pane_id, status, name) in statuses {
            let previous = board.since.get(&pane_id).map(|(seen, _)| *seen);
            if previous == Some(status) {
                continue;
            }
            board.since.insert(pane_id, (status, now));
            // First sight is not news. Only real transitions reach the ticker.
            // The client reports a seen completion as idle, so working to idle
            // is a finish too.
            use crate::api::schema::AgentStatus;
            let event = match (previous, status) {
                (None, _) => None,
                (_, AgentStatus::Blocked) => Some(AgentStatus::Blocked),
                (_, AgentStatus::Done) | (Some(AgentStatus::Working), AgentStatus::Idle) => {
                    Some(AgentStatus::Done)
                }
                (Some(AgentStatus::Idle | AgentStatus::Done), AgentStatus::Working) => {
                    Some(AgentStatus::Working)
                }
                _ => None,
            };
            if let Some(status) = event {
                board.events.push(BoardEvent {
                    name,
                    status,
                    at: now,
                });
            }
        }
        board
            .events
            .retain(|event| now.saturating_duration_since(event.at) < TICKER_WINDOW);
        if board.pane_ids != pane_ids {
            let selected = board.pane_ids.get(board.selected).cloned();
            board
                .previews
                .retain(|pane_id, _| pane_ids.contains(pane_id));
            board.selected = selected
                .and_then(|selected| pane_ids.iter().position(|pane_id| *pane_id == selected))
                .unwrap_or(0);
            board.pane_ids = pane_ids.clone();
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
        let input_empty = board.input.as_str().is_empty();
        // Tab walks master, then the rail in its urgency order, so the agent
        // that needs the user is always one Tab away.
        let cycle =
            |board: &ClientBoardOverlay, snapshot: Option<&ClientShellSnapshot>, step: isize| {
                let mut order = vec![0];
                if let Some(snapshot) = snapshot {
                    order.extend(rail_order(board, snapshot));
                }
                let at = order
                    .iter()
                    .position(|index| *index == board.selected)
                    .unwrap_or(0);
                order[(at as isize + step).rem_euclid(order.len() as isize) as usize]
            };
        let snapshot = self.snapshot.as_deref();
        if let (KeyCode::Char(digit @ '1'..='9'), true) = (key.code, input_empty) {
            let answer = snapshot
                .filter(|_| board.selected > 0)
                .map(|snapshot| card_for(board, snapshot, board.selected))
                .and_then(|card| card.question.map(|question| (card.pane_id, question)))
                .filter(|(_, question)| (digit as usize - '0' as usize) <= question.options.len());
            if let Some((pane_id, _)) = answer {
                self.push_endpoint_method_with_kind(
                    crate::api::schema::Method::PaneSendText(
                        crate::api::schema::PaneSendTextParams {
                            pane_id,
                            text: digit.to_string(),
                        },
                    ),
                    PendingEndpointKind::Generic,
                    outcome,
                );
                return true;
            }
        }
        match key.code {
            KeyCode::Tab => board.selected = cycle(board, snapshot, 1),
            KeyCode::Down if input_empty => board.selected = cycle(board, snapshot, 1),
            KeyCode::BackTab => board.selected = cycle(board, snapshot, -1),
            KeyCode::Up if input_empty => board.selected = cycle(board, snapshot, -1),
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
    let now = board.now.unwrap_or_else(std::time::Instant::now);
    let recent = board
        .events
        .iter()
        .filter(|event| now.saturating_duration_since(event.at) < TICKER_WINDOW)
        .collect::<Vec<_>>();
    let layout = board_layout(area, board.pane_ids.len() > 1, !recent.is_empty());
    render_board_header(buffer, layout.header, snapshot, board, palette);

    // Frames first, the selected one last so its accent outline stays whole
    // where the two share a border. Titles and content go on top afterwards.
    let master_selected = board.selected == 0;
    let blocked = snapshot
        .agents
        .iter()
        .any(|agent| agent.agent_status == crate::api::schema::AgentStatus::Blocked);
    let master_color = if master_selected {
        palette.accent
    } else {
        palette.surface1
    };
    let rail_color = if !master_selected {
        palette.accent
    } else if blocked {
        palette.red
    } else {
        palette.surface1
    };
    let mut frames = [(layout.master, master_color), (layout.rail, rail_color)];
    if master_selected {
        frames.reverse();
    }
    for (rect, color) in frames {
        if !rect.is_empty() {
            rounded_box(
                buffer,
                rect,
                Style::default().fg(color),
                ratatui::style::Color::Reset,
            );
        }
    }
    if let Some(master) = board.pane_ids.first() {
        render_box(buffer, layout.master, 0, board, snapshot, master, palette);
        rendered.board_boxes.push((layout.master, 0));
    }
    if !layout.rail.is_empty() {
        render_rail(
            buffer,
            layout.rail,
            board,
            snapshot,
            palette,
            &mut rendered.board_boxes,
        );
    }
    render_ticker(buffer, layout.ticker, &recent, board.frame, palette);
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
    let master = master_pane_id(snapshot) == Some(pane_id);
    match snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
    {
        Some(agent) => (
            agent_icon(agent).unwrap_or("●"),
            agent_headline(snapshot, agent),
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

/// `⠹ working 2m` becomes `⠹ 2m`, `● needs you 7s` becomes `● 7s`, and a
/// pill without a clock keeps its first word.
fn compact_pill(pill: &str) -> String {
    let words = pill.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        [glyph, .., clock] if words.len() > 2 && clock.chars().any(|ch| ch.is_ascii_digit()) => {
            format!("{glyph} {clock}")
        }
        [first, ..] => (*first).to_owned(),
        [] => String::new(),
    }
}

/// The top bar: wordmark, how many agents, live counts, and the zoom hint.
/// The hint drops first, then counts from the right.
fn render_board_header(
    buffer: &mut Buffer,
    row: Rect,
    snapshot: &ClientShellSnapshot,
    board: &ClientBoardOverlay,
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
    let subagents = board.pane_ids.len().saturating_sub(1);
    let summary = match subagents {
        0 => "master".to_owned(),
        1 => "master + 1 subagent".to_owned(),
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

/// The agents rail: one card per subagent, most urgent first. Busy and
/// blocked agents get a card with their activity, a blocked one also its
/// question and options. Finished agents fold into a single line unless
/// selected. The selected card also shows the tail of its screen.
fn render_rail(
    buffer: &mut Buffer,
    rect: Rect,
    board: &ClientBoardOverlay,
    snapshot: &ClientShellSnapshot,
    palette: &Palette,
    hits: &mut Vec<(Rect, usize)>,
) {
    let edge = ratatui::style::Color::Reset;
    put(
        buffer,
        rect.x + 2,
        rect.y,
        rect.width.saturating_sub(4),
        " agents ",
        Style::default()
            .fg(palette.overlay1)
            .bg(edge)
            .add_modifier(Modifier::BOLD),
    );
    let inner = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    );
    if inner.width < 6 || inner.height == 0 {
        return;
    }
    let left = inner.x + 1;
    let width = inner.width.saturating_sub(2);
    let bottom = inner.bottom();
    let cards = rail_order(board, snapshot)
        .into_iter()
        .map(|index| card_for(board, snapshot, index))
        .collect::<Vec<_>>();
    let (folded, shown): (Vec<&Card>, Vec<&Card>) = cards
        .iter()
        .partition(|card| finished(card.status) && card.index != board.selected);
    let mut y = inner.y;
    let mut hidden = 0usize;
    for (position, card) in shown.iter().enumerate() {
        let selected = card.index == board.selected;
        // A question already shows what matters, so only other selected
        // cards add the tail of their screen.
        let preview = if selected && card.question.is_none() {
            board
                .previews
                .get(&card.pane_id)
                .map(|text| preview_lines(text))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let question_rows = card
            .question
            .as_ref()
            .map_or(0, |question| question.options.len() + 2);
        let preview_rows = preview.len().min(SELECTED_PREVIEW_ROWS);
        let rows = 2 + question_rows + preview_rows;
        // Keep one row for the fold line or the overflow note.
        let reserve = u16::from(!folded.is_empty() || position + 1 < shown.len());
        if y + rows as u16 > bottom.saturating_sub(reserve) && position > 0 {
            hidden = shown.len() - position;
            break;
        }
        let top = y;
        if selected {
            for row in top..(top + rows as u16).min(bottom) {
                put(
                    buffer,
                    inner.x,
                    row,
                    1,
                    "▌",
                    Style::default().fg(palette.accent),
                );
            }
        }
        // Title: number, icon, name, task, and the status pill on the right.
        let (pill, pill_style) = status_pill(board, &card.pane_id, card.status, palette);
        // In a narrow rail the name matters more than the pill's words, so
        // the pill falls back to its glyph and clock (`● 7s`).
        let name_room = |pill: &str| width.saturating_sub(width_of(pill) as u16 + 5);
        let pill = if name_room(&pill) < 16 {
            compact_pill(&pill)
        } else {
            pill
        };
        let pill_width = width_of(&pill) as u16;
        let right = left + width;
        let show_pill = width > pill_width + 12;
        let limit = if show_pill {
            right.saturating_sub(pill_width + 1)
        } else {
            right
        };
        let icon_color = card
            .status
            .map_or(palette.overlay0, |status| status_color(status, palette));
        let mut x = put(
            buffer,
            left,
            y,
            limit.saturating_sub(left),
            &format!("{} ", card.index + 1),
            Style::default().fg(palette.overlay0),
        );
        x = put(
            buffer,
            x,
            y,
            limit.saturating_sub(x),
            card.icon,
            Style::default().fg(icon_color),
        );
        x = put(buffer, x, y, limit.saturating_sub(x), " ", Style::default());
        x = put(
            buffer,
            x,
            y,
            limit.saturating_sub(x),
            &card.name,
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        );
        if let Some(task) = &card.task {
            put(
                buffer,
                x,
                y,
                limit.saturating_sub(x),
                &format!(" · {task}"),
                Style::default().fg(palette.overlay0),
            );
        }
        if show_pill {
            put(buffer, right - pill_width, y, pill_width, &pill, pill_style);
        }
        y += 1;
        // Second row: the question, or what the agent is doing.
        if let Some(question) = &card.question {
            let prompt = question
                .prompt
                .as_deref()
                .unwrap_or("Waiting for your answer");
            put(
                buffer,
                left + 2,
                y,
                width.saturating_sub(2),
                prompt,
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            );
            y += 1;
            for (number, option) in question.options.iter().enumerate() {
                if y >= bottom {
                    break;
                }
                let x = put(
                    buffer,
                    left + 4,
                    y,
                    width.saturating_sub(4),
                    &format!("{} ", number + 1),
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                );
                put(
                    buffer,
                    x,
                    y,
                    right.saturating_sub(x),
                    option,
                    Style::default().fg(palette.subtext0),
                );
                y += 1;
            }
            if y < bottom {
                let hint = if selected {
                    format!("press 1-{} to answer", question.options.len())
                } else {
                    "⇥ select to answer".to_owned()
                };
                put(
                    buffer,
                    left + 4,
                    y,
                    width.saturating_sub(4),
                    &hint,
                    Style::default().fg(palette.overlay0),
                );
                y += 1;
            }
        } else if y < bottom {
            let activity = card.activity.as_deref().unwrap_or("starting…");
            put(
                buffer,
                left + 2,
                y,
                width.saturating_sub(2),
                activity,
                Style::default().fg(palette.subtext0),
            );
            y += 1;
        }
        // The selected card also shows the tail of its screen.
        let start = preview.len().saturating_sub(preview_rows);
        for line in &preview[start..] {
            if y >= bottom {
                break;
            }
            draw_styled_line(
                buffer,
                left + 2,
                y,
                width.saturating_sub(2),
                line,
                Style::default().fg(palette.text),
            );
            y += 1;
        }
        hits.push((Rect::new(inner.x, top, inner.width, y - top), card.index));
        if y < bottom {
            y += 1;
        }
    }
    if hidden > 0 && y < bottom {
        put(
            buffer,
            left,
            y,
            width,
            &format!("+{hidden} more · ⇥ to cycle"),
            Style::default().fg(palette.overlay0),
        );
        y += 1;
    }
    if !folded.is_empty() && y < bottom {
        let names = folded
            .iter()
            .map(|card| card.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let line_y = bottom - 1;
        let x = put(
            buffer,
            left,
            line_y,
            width,
            &format!("✓ {} finished ", folded.len()),
            Style::default().fg(palette.teal),
        );
        put(
            buffer,
            x,
            line_y,
            (left + width).saturating_sub(x),
            &format!("· {names}"),
            Style::default().fg(palette.overlay0),
        );
        hits.push((Rect::new(inner.x, line_y, inner.width, 1), folded[0].index));
    }
}

/// Recent status changes, newest first: `● asker needs you   ✓ quick-count finished`.
fn render_ticker(
    buffer: &mut Buffer,
    row: Rect,
    events: &[&BoardEvent],
    frame: usize,
    palette: &Palette,
) {
    use crate::api::schema::AgentStatus;
    if row.is_empty() {
        return;
    }
    let mut x = row.x + 2;
    for event in events.iter().rev() {
        let (glyph, verb, color) = match event.status {
            AgentStatus::Blocked => ("●", "needs you", palette.red),
            AgentStatus::Done => ("✓", "finished", palette.teal),
            AgentStatus::Working => (SPINNER[frame % SPINNER.len()], "started", palette.yellow),
            _ => continue,
        };
        let text = format!("{glyph} {} {verb}", event.name);
        if (x + width_of(&text) as u16) > row.right().saturating_sub(1) {
            break;
        }
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            glyph,
            Style::default().fg(color),
        );
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            &format!(" {} ", event.name),
            Style::default().fg(palette.text),
        );
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            verb,
            Style::default().fg(color),
        );
        x = put(
            buffer,
            x,
            row.y,
            row.right().saturating_sub(x),
            "   ",
            Style::default(),
        );
    }
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
    let (icon, _, status) = box_identity(snapshot, pane_id);
    let icon_color = status.map_or(palette.overlay0, |status| status_color(status, palette));
    let target = crate::ui::truncate_end(
        &display_name(snapshot, pane_id),
        (row.width / 3).max(6) as usize,
    );
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
        for (index, rect) in boxes.iter().enumerate() {
            assert!(
                rect.right() <= area.right() && rect.bottom() <= area.bottom(),
                "{context}: box {index} {rect:?} leaves {area:?}"
            );
            for other in &boxes[index + 1..] {
                let shared = rect.intersection(*other);
                assert!(
                    shared.is_empty() || shared.width == 1 || shared.height == 1,
                    "{context}: boxes overlap past a shared border: {shared:?}"
                );
            }
            let (left, right, top, bottom) = (rect.x, rect.right() - 1, rect.y, rect.bottom() - 1);
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
        use crate::api::schema::AgentStatus;
        let palette = ClientShellConfig::from_config(&crate::config::Config::default()).palette;
        let sizes = [(10, 4), (30, 10), (60, 20), (80, 24), (130, 34), (220, 60)];
        let previews = previews();
        let now = std::time::Instant::now();
        for (cols, rows) in sizes {
            for count in [0usize, 1, 2, 4, 8, 12] {
                for (variant, headline) in HEADLINES.iter().enumerate() {
                    let snapshot = snapshot_with(count, headline);
                    let mut pane_ids = vec!["pane_1".to_owned()];
                    pane_ids.extend((0..count).map(|index| format!("sub_{index}")));
                    let mut board = ClientBoardOverlay {
                        selected: variant % pane_ids.len(),
                        now: Some(now),
                        ..ClientBoardOverlay::default()
                    };
                    if variant % 2 == 1 {
                        board.events.push(BoardEvent {
                            name: (*headline).to_owned(),
                            status: AgentStatus::Blocked,
                            at: now,
                        });
                    }
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
                    let layout = board_layout(area, count > 0, !board.events.is_empty());
                    let boxes = [layout.master, layout.rail]
                        .into_iter()
                        .filter(|rect| rect.width >= 2 && rect.height >= 2)
                        .collect::<Vec<_>>();
                    assert_layout(&buffer, &boxes, &context);
                }
            }
        }
    }

    #[test]
    fn master_takes_the_screen_and_the_rail_shares_its_border() {
        let area = Rect::new(0, 0, 130, 34);
        let alone = board_layout(area, false, false);
        assert_eq!(alone.master.width, 130, "flush to both edges");
        assert!(alone.rail.is_empty());
        assert_eq!(alone.input.y, 33, "input line sits on the last row");
        let wide = board_layout(area, true, false);
        assert_eq!(wide.rail.x + 1, wide.master.right(), "rail beside master");
        assert!(
            wide.master.width > wide.rail.width * 2,
            "master gets most of it"
        );
        let narrow = board_layout(Rect::new(0, 0, 80, 40), true, false);
        assert_eq!(
            narrow.rail.y + 1,
            narrow.master.bottom(),
            "rail below master"
        );
        let ticker = board_layout(area, true, true);
        assert_eq!(ticker.ticker.y, 32, "ticker sits just above the input");
        assert_eq!(ticker.master.bottom(), 32);
    }

    /// The screens from a real run: Claude Code working, done, and waiting
    /// on a question, plus a Codex permission prompt.
    #[test]
    fn activity_and_questions_come_from_the_agent_screen() {
        use crate::api::schema::AgentStatus;
        let working = [
            "⏺ Update(src/auth/session_guard.rs)",
            "  ⎿  +42 -17 across 3 files",
            "",
            "✻ Refactoring… (48s · ↓ 2.1k tokens)",
        ];
        assert_eq!(
            activity_line(&working, Some(AgentStatus::Working)).as_deref(),
            Some("Refactoring…")
        );
        let done = [
            "❯ Read-only task in ~/Personal/herdr: count lines",
            "  Read 1 file",
            "● src/client/shell/board.rs has 1,894 lines.",
            "",
            "✻ Cogitated for 4s · done 6:49 PM",
        ];
        assert_eq!(
            activity_line(&done, Some(AgentStatus::Done)).as_deref(),
            Some("src/client/shell/board.rs has 1,894 lines.")
        );
        let asker = [
            "☐ File",
            "",
            "Which file should I summarize?",
            "",
            "❯ 1. Board.rs",
            "     src/client/shell/board.rs, the board main screen",
            "  2. Peek history",
            "     The peek history file or feature",
            "  3. Agent skill",
            "     skills/herdr/SKILL.md, the skill agents read via vcer --skill",
            "  4. Type something.",
            "────────────────────────",
            "  5. Chat about this",
            "",
            "Enter to select · ↑/↓ to navigate · Esc to cancel",
        ];
        let question = detect_question(&asker).expect("question");
        assert_eq!(
            question.prompt.as_deref(),
            Some("Which file should I summarize?")
        );
        assert_eq!(
            question.options,
            vec![
                "Board.rs",
                "Peek history",
                "Agent skill",
                "Type something.",
                "Chat about this"
            ]
        );
        let codex = [
            "• Running cleanup step",
            "",
            "  Allow command?",
            "  rm -rf build/",
            "  1. Yes",
            "  2. No",
        ];
        let question = detect_question(&codex).expect("codex question");
        assert_eq!(question.prompt.as_deref(), Some("rm -rf build/"));
        assert_eq!(question.options, vec!["Yes", "No"]);
        assert_eq!(detect_question(&["1. only one option"]), None);
        assert_eq!(
            detect_question(&["2. Peek", "3. Skill"]),
            None,
            "must count from 1"
        );
    }

    #[test]
    fn rail_puts_blocked_agents_first_and_folds_finished_ones() {
        use crate::api::schema::AgentStatus;
        let mut snapshot = snapshot_with(4, "task");
        let statuses = [
            AgentStatus::Done,
            AgentStatus::Working,
            AgentStatus::Blocked,
            AgentStatus::Idle,
        ];
        for (agent, status) in snapshot.agents.iter_mut().zip(statuses) {
            agent.agent_status = status;
        }
        let board = ClientBoardOverlay {
            pane_ids: ["pane_1", "sub_0", "sub_1", "sub_2", "sub_3"]
                .map(String::from)
                .to_vec(),
            ..ClientBoardOverlay::default()
        };
        assert_eq!(rail_order(&board, &snapshot), vec![3, 2, 1, 4]);

        let palette = ClientShellConfig::from_config(&crate::config::Config::default()).palette;
        let mut buffer = Buffer::empty(Rect::new(0, 0, 130, 34));
        render_board(&mut buffer, &board, &snapshot, &palette);
        let text = (0..34)
            .map(|y| {
                (0..130)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("✓ 2 finished"), "{text}");
    }

    #[test]
    fn numbers_answer_the_selected_agents_question() {
        use crate::api::schema::AgentStatus;
        let mut state = board_state(1);
        if let Some(snapshot) = state.snapshot.as_deref() {
            let mut snapshot = snapshot.clone();
            snapshot.agents[0].agent_status = AgentStatus::Blocked;
            state.set_snapshot(Box::new(snapshot));
        }
        if let Some(ClientShellOverlay::Board(board)) = state.overlay.as_mut() {
            board.previews.insert(
                "sub_0".into(),
                "Which file?\n❯ 1. Board.rs\n  2. Peek history\n  3. Agent skill\n".into(),
            );
        }
        let outcome = press(&mut state, KeyCode::Char('2'));
        assert!(
            sent_methods(&outcome).is_empty(),
            "master selected: digits type"
        );
        assert_eq!(board(&state).input.as_str(), "2");
        press(&mut state, KeyCode::Esc);
        press(&mut state, KeyCode::Tab);
        assert_eq!(
            board(&state).selected,
            1,
            "the blocked agent is one tab away"
        );
        let outcome = press(&mut state, KeyCode::Char('2'));
        assert!(matches!(
            sent_methods(&outcome)[..],
            [crate::api::schema::Method::PaneSendText(params)]
                if params.pane_id == "sub_0" && params.text == "2"
        ));
        let outcome = press(&mut state, KeyCode::Char('7'));
        assert!(sent_methods(&outcome).is_empty(), "no option 7");
    }

    #[test]
    fn ticker_reports_transitions_but_not_first_sightings() {
        use crate::api::schema::AgentStatus;
        let mut state = board_state(1);
        assert!(board(&state).events.is_empty(), "first sight is not news");
        let mut snapshot = state.snapshot.as_deref().expect("snapshot").clone();
        // The client reports a seen completion as idle.
        snapshot.agents[0].agent_status = AgentStatus::Idle;
        snapshot.agents[0].name = Some("quick-count".into());
        state.set_snapshot(Box::new(snapshot));
        let mut outcome = ClientShellInput::default();
        let later = std::time::Instant::now() + POLL_INTERVAL * 2;
        state.tick_board(later, &mut outcome);
        let events = &board(&state).events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "quick-count");
        assert_eq!(events[0].status, AgentStatus::Done);
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
                row: rect.y,
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
        assert_eq!(compact_pill("⠹ working 2m"), "⠹ 2m");
        assert_eq!(compact_pill("● needs you 7s"), "● 7s");
        assert_eq!(compact_pill("✓ done"), "✓");
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
            agent_headline(&snapshot, &agent)
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
