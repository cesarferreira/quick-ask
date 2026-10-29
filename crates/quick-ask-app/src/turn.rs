#[derive(Default)]
pub struct Turn {
    pub id: u64,
    pub prompt: Option<String>,
    pub text: String,
    pub busy: bool,
    pub network_done: bool,
    visible_len: usize,
}

impl Turn {
    pub fn visible_text(&self) -> &str {
        &self.text[..self.visible_len]
    }

    pub fn advance(&mut self) -> bool {
        if !self.busy || self.visible_len == self.text.len() {
            return false;
        }
        let remaining = &self.text[self.visible_len..];
        let count = remaining.chars().count().div_ceil(2).max(4);
        self.visible_len += remaining
            .char_indices()
            .nth(count)
            .map(|(index, _)| index)
            .unwrap_or(remaining.len());
        true
    }

    pub fn begin(&mut self, prompt: String) -> u64 {
        self.reset();
        self.prompt = Some(prompt);
        self.busy = true;
        self.id
    }
    pub fn delta(&mut self, id: u64, text: &str) {
        if id == self.id && self.busy {
            self.text.push_str(text);
        }
    }
    pub fn finish(&mut self, id: u64, success: bool) -> Option<(String, String)> {
        if id != self.id || !self.busy {
            return None;
        }
        if success && self.visible_len < self.text.len() {
            return None;
        }
        self.busy = false;
        if success {
            self.visible_len = 0;
            Some((self.prompt.take()?, std::mem::take(&mut self.text)))
        } else {
            self.visible_len = self.text.len();
            None
        }
    }
    pub fn reset(&mut self) {
        self.id = self.id.wrapping_add(1);
        self.prompt = None;
        self.text.clear();
        self.busy = false;
        self.network_done = false;
        self.visible_len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arriving_chunks_reveal_progressively_instead_of_a_single_jump() {
        let mut turn = Turn::default();
        let id = turn.begin("Question".into());
        turn.delta(id, "A smooth answer should arrive over several frames.");
        assert_eq!(turn.visible_text(), "");
        assert!(turn.advance());
        assert!(!turn.visible_text().is_empty());
        assert!(turn.visible_text().len() < turn.text.len());
    }

    #[test]
    fn successful_completion_waits_until_all_received_text_is_visible() {
        let mut turn = Turn::default();
        let id = turn.begin("Question".into());
        turn.delta(id, "An answer that still needs to be displayed.");
        assert!(turn.finish(id, true).is_none());
        assert!(turn.busy);
    }

    #[test]
    fn a_large_unicode_chunk_catches_up_within_twelve_frames_without_losing_text() {
        let mut turn = Turn::default();
        let id = turn.begin("Question".into());
        let answer = "café 👩🏽‍💻 日本語\n".repeat(200);
        turn.delta(id, &answer);
        for _ in 0..12 {
            turn.advance();
        }
        assert_eq!(turn.visible_text(), answer);
        assert_eq!(turn.finish(id, true), Some(("Question".into(), answer)));
    }

    #[test]
    fn reset_ignores_a_previous_chats_in_flight_response() {
        let mut turn = Turn::default();
        let old = turn.begin("old chat".into());
        turn.reset();
        let current = turn.begin("new chat".into());
        turn.delta(old, "wrong answer");
        assert!(turn.finish(old, true).is_none());
        assert!(turn.busy);
        turn.delta(current, "right answer");
        while turn.advance() {}
        assert_eq!(
            turn.finish(current, true),
            Some(("new chat".into(), "right answer".into()))
        );
    }

    #[test]
    fn failed_stream_keeps_partial_text_visible_without_committing_it() {
        let mut turn = Turn::default();
        let id = turn.begin("question".into());
        turn.delta(id, "partial");
        assert!(turn.finish(id, false).is_none());
        assert!(!turn.busy);
        assert_eq!(turn.text, "partial");
        assert_eq!(turn.prompt.as_deref(), Some("question"));
    }

    #[test]
    fn completed_response_is_committed_only_once() {
        let mut turn = Turn::default();
        let id = turn.begin("question".into());
        turn.delta(id, "answer");
        while turn.advance() {}
        assert!(turn.finish(id, true).is_some());
        assert!(turn.finish(id, true).is_none());
        assert!(turn.prompt.is_none());
        assert!(turn.text.is_empty());
    }
}
