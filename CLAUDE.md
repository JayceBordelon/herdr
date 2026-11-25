# vcer (herdr fork)

vcer is Jayce's fork of herdr with a right-side agent panel and message feed.
The binary is `vcer` (Cargo `[[bin]]`, package name stays `herdr`). It runs
beside stock herdr: config in `~/.config/vcer`, launchd label `dev.vcer.*`.
Upstream remote is `herdrdev/herdr`. Upstream's AGENTS.md still applies except
where this file overrides it.

## Identity rules

Renamed to vcer: user-visible strings (CLI help, errors, TUI notices, window
title), file names inside the vcer config dir (`vcer.sock`, `vcer-client.sock`,
`vcer-server.log`, `vcer-handoff-*.sock`), and the launchd label.

Kept as herdr on purpose, do not rename: `HERDR_*` env vars (agents and shell
integrations read them), protocol and source tags (`herdr:claude` etc.),
`herdr-plugin.toml`, integration install file names (shared with stock herdr
to avoid duplicate hooks), remote install markers and the remote binary name,
serde config keys and values (`delivery = "herdr"`, `[ui.toast.herdr]`),
`~/.herdr/worktrees`, upstream URLs, and the crate name. `src/fork_identity.rs`
holds the fork constants.

Tracing targets follow the bin name, so log filters use `vcer=`, not `herdr=`.

## UI model

One master pane, subagents fan out. `pane.split` opens the new pane as its
own tab (the view is never tiled). The master is the pane in the first tab of
the first workspace. The main screen is the board (`src/client/shell/board.rs`):
master's transcript fills most of it, and a rail beside it holds one compact
card per subagent, sorted by urgency. Cards show the agent's current activity
and any pending question, which digits answer. Finished agents fold into one
line, and a ticker above the input reports status changes. The input line
sends `agent.prompt` to agents and `pane.send_text` to shells. The board is
an overlay the client entrypoint switches on (`ClientShellState::board`), so
upstream's classic layout code and its tests run without it. Keep that code
upstream-shaped. The client only streams the focused pane, so the board polls
`pane.read`.

Agents running inside vcer learn this model from `skills/herdr/SKILL.md`
(printed by `vcer --skill`, compiled into the binary). Update it whenever the
layout model or a keybind it names changes, or agents report intended
behavior as bugs.

Untrusted text (titles, pane screens) goes through `ui::sanitize_line` before
it is drawn. The board's layout test renders a matrix of sizes, counts and text
shapes. Extend it rather than adding one-off cases.

Making an API method callable from the TUI means flipping its client_shell
column in `src/api/schema.rs` and freezing its shape digest in
`advertised_client_shell_method_shapes_stay_at_the_v1_contract`. Never
rewrite the published fixture.

## macOS launchd

macOS 26.5 loads a `launchctl bootstrap` job but no longer starts it, even
with RunAtLoad. `spawn_launchd_job` therefore omits RunAtLoad and runs
`launchctl kickstart` explicitly. Do not reintroduce RunAtLoad-only startup.

## Testing

Use `cargo nextest run` (brew `cargo-nextest`), never bare `cargo test` for
the full suite: tests share process state and one panic cascades into dozens
of bogus PoisonError failures. The branch has ~30 known failures that expect
the removed spaces sidebar or the disabled remote install. Before blaming a
change, diff failures against a baseline run of the previous commit.

## Writing style

No em dashes and no semicolons in anything user facing: strings, docs,
comments, commit messages.
