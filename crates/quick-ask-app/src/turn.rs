#[derive(Default)]
pub struct Turn {
    pub id: u64,
    pub prompt: Option<String>,
    pub text: String,
    pub busy: bool,
}

impl Turn {
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
        self.busy = false;
        if success {
            Some((self.prompt.take()?, std::mem::take(&mut self.text)))
        } else {
            None
        }
    }
    pub fn reset(&mut self) {
        self.id = self.id.wrapping_add(1);
        self.prompt = None;
        self.text.clear();
        self.busy = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(turn.finish(id, true).is_some());
        assert!(turn.finish(id, true).is_none());
        assert!(turn.prompt.is_none());
        assert!(turn.text.is_empty());
    }
}
