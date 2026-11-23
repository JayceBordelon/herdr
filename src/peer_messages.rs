//! In-memory log of text one pane sent to another through the API.
//!
//! The log is a runtime fact owned by the server. It is never persisted, since
//! messages between agents can contain sensitive text.

use std::collections::VecDeque;

use crate::protocol::ClientShellPeerMessage;

const MAX_MESSAGES: usize = 50;
const MAX_PREVIEW_CHARS: usize = 160;

/// JSON request field carrying the sending pane. It sits beside `method` so
/// the frozen parameter shapes of the send methods stay unchanged.
pub(crate) const SENDER_PANE_FIELD: &str = "sender_pane_id";

#[derive(Debug, Default)]
pub(crate) struct PeerMessageLog {
    next_seq: u64,
    messages: VecDeque<ClientShellPeerMessage>,
}

pub(crate) struct PeerEndpoint {
    pub pane_id: String,
    pub label: String,
}

impl PeerMessageLog {
    /// Records a message and returns whether the log changed.
    pub(crate) fn record(&mut self, from: PeerEndpoint, to: PeerEndpoint, text: &str) -> bool {
        if from.pane_id == to.pane_id {
            return false;
        }
        let preview = message_preview(text);
        if preview.is_empty() {
            return false;
        }
        self.next_seq += 1;
        self.messages.push_back(ClientShellPeerMessage {
            seq: self.next_seq,
            from_pane_id: from.pane_id,
            from_label: from.label,
            to_pane_id: to.pane_id,
            to_label: to.label,
            preview,
        });
        while self.messages.len() > MAX_MESSAGES {
            self.messages.pop_front();
        }
        true
    }

    pub(crate) fn newest_first(&self) -> Vec<ClientShellPeerMessage> {
        self.messages.iter().rev().cloned().collect()
    }
}

/// Reads the sending pane from a raw request line.
pub(crate) fn sender_pane_from_request_line(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Sender {
        sender_pane_id: Option<String>,
    }
    serde_json::from_str::<Sender>(line)
        .ok()?
        .sender_pane_id
        .filter(|pane_id| !pane_id.is_empty())
}

/// Collapses text to one printable line so it cannot inject terminal controls.
fn message_preview(text: &str) -> String {
    let printable = text
        .chars()
        .map(|character| {
            if character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .filter(|character| !character.is_control())
        .collect::<String>();
    printable
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_PREVIEW_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(pane_id: &str) -> PeerEndpoint {
        PeerEndpoint {
            pane_id: pane_id.into(),
            label: format!("label-{pane_id}"),
        }
    }

    #[test]
    fn records_newest_first() {
        let mut log = PeerMessageLog::default();
        assert!(log.record(endpoint("a"), endpoint("b"), "first"));
        assert!(log.record(endpoint("b"), endpoint("a"), "second"));
        let messages = log.newest_first();
        assert_eq!(messages[0].preview, "second");
        assert_eq!(messages[1].preview, "first");
        assert!(messages[0].seq > messages[1].seq);
    }

    #[test]
    fn ignores_self_sends_and_blank_text() {
        let mut log = PeerMessageLog::default();
        assert!(!log.record(endpoint("a"), endpoint("a"), "hello"));
        assert!(!log.record(endpoint("a"), endpoint("b"), " \n\t "));
        assert!(log.newest_first().is_empty());
    }

    #[test]
    fn retains_only_the_newest_messages() {
        let mut log = PeerMessageLog::default();
        for index in 0..(MAX_MESSAGES + 5) {
            log.record(endpoint("a"), endpoint("b"), &format!("message {index}"));
        }
        let messages = log.newest_first();
        assert_eq!(messages.len(), MAX_MESSAGES);
        assert_eq!(messages[0].preview, format!("message {}", MAX_MESSAGES + 4));
    }

    #[test]
    fn preview_strips_controls_and_collapses_lines() {
        assert_eq!(
            message_preview("hello\n\x1b[31mworld\x07\t again"),
            "hello [31mworld again"
        );
        assert_eq!(
            message_preview(&"x".repeat(MAX_PREVIEW_CHARS + 10))
                .chars()
                .count(),
            MAX_PREVIEW_CHARS
        );
    }

    #[test]
    fn sender_is_read_from_the_request_envelope() {
        assert_eq!(
            sender_pane_from_request_line(
                r#"{"id":"1","method":"pane.send_text","params":{},"sender_pane_id":"w1:p2"}"#
            )
            .as_deref(),
            Some("w1:p2")
        );
        assert_eq!(
            sender_pane_from_request_line(r#"{"id":"1","method":"ping","params":{}}"#),
            None
        );
    }
}
