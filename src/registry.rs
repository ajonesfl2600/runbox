//! Registry of open runbox request windows.
//!
//! Pure logic, no GTK. Tracks which request windows are open, their state,
//! and the window offset used to keep windows from stacking.

pub const MAX_OPEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Waiting,
    Running,
    Done,
}

#[derive(Debug)]
pub struct Entry {
    pub id: String,
    pub command: String,
    pub state: State,
    pub opened_at: u64, // unix seconds
}

#[derive(Debug)]
pub struct Registry {
    entries: Vec<Entry>,
    counter: u32,
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            entries: Vec::new(),
            counter: 1,
        }
    }

    fn is_open(&self, id: &str) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    /// Add a request. `id` None -> generate "r1", "r2", ... (skip any already in use).
    /// Err("busy: N requests already open") when MAX_OPEN entries are open.
    /// Err("id in use: X") when the given id is already open.
    pub fn open(&mut self, id: Option<String>, command: &str, now: u64) -> Result<String, String> {
        if self.entries.len() >= MAX_OPEN {
            return Err(format!(
                "busy: {} requests already open",
                self.entries.len()
            ));
        }
        let id = match id {
            Some(id) => {
                if self.is_open(&id) {
                    return Err(format!("id in use: {}", id));
                }
                id
            }
            None => {
                let mut candidate = self.counter.max(1);
                while self.is_open(&format!("r{}", candidate)) {
                    candidate += 1;
                }
                self.counter = candidate + 1;
                format!("r{}", candidate)
            }
        };
        self.entries.push(Entry {
            id: id.clone(),
            command: command.to_string(),
            state: State::Waiting,
            opened_at: now,
        });
        Ok(id)
    }

    /// Set the state of an open request. false if unknown id.
    pub fn set_state(&mut self, id: &str, state: State) -> bool {
        match self.entries.iter_mut().find(|e| e.id == id) {
            Some(e) => {
                e.state = state;
                true
            }
            None => false,
        }
    }

    /// The state of an open request. None if unknown id.
    pub fn state(&self, id: &str) -> Option<State> {
        self.entries.iter().find(|e| e.id == id).map(|e| e.state)
    }

    /// The id of the request window that finished longest ago (opened first), if any.
    /// Its window sits idle, so a new request can reuse it.
    pub fn oldest_done(&self) -> Option<String> {
        self.entries
            .iter()
            .filter(|e| e.state == State::Done)
            .min_by_key(|e| e.opened_at)
            .map(|e| e.id.clone())
    }

    /// Remove an open request. false if unknown id.
    pub fn close(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        self.entries.len() != before
    }

    /// Number of open requests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no requests are open.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Window offset for the Nth open slot so windows don't stack:
    /// (x, y) = (slot*32, slot*32), slot = position in the list.
    pub fn offset(&self, id: &str) -> Option<(i32, i32)> {
        let slot = self.entries.iter().position(|e| e.id == id)? as i32;
        Some((slot * 32, slot * 32))
    }

    /// One line per entry, oldest first:
    /// "<id>\t<state>\t<seconds open>s\t<command first line, max 60 chars>"
    /// State words: waiting, running, done. Empty registry -> "no open requests".
    pub fn list_text(&self, now: u64) -> String {
        if self.is_empty() {
            return "no open requests".to_string();
        }
        let mut order: Vec<usize> = (0..self.entries.len()).collect();
        order.sort_by_key(|&i| self.entries[i].opened_at);
        order
            .iter()
            .map(|&i| {
                let e = &self.entries[i];
                let state = match e.state {
                    State::Waiting => "waiting",
                    State::Running => "running",
                    State::Done => "done",
                };
                let first_line = e.command.split('\n').next().unwrap_or("");
                let first_line: String = first_line.chars().take(60).collect();
                format!(
                    "{}\t{}\t{}s\t{}",
                    e.id,
                    state,
                    now.saturating_sub(e.opened_at),
                    first_line
                )
            })
            .collect::<Vec<String>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_open_list_and_set_state() {
        let mut reg = Registry::new();
        assert_eq!(
            reg.open(Some("mac-wipe".to_string()), "wipe the mac", 1000),
            Ok("mac-wipe".to_string())
        );
        assert_eq!(reg.open(None, "run the job", 1010), Ok("r1".to_string()));
        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.list_text(1030),
            "mac-wipe\twaiting\t30s\twipe the mac\nr1\twaiting\t20s\trun the job"
        );
        assert!(reg.set_state("r1", State::Running));
        assert_eq!(
            reg.list_text(1030),
            "mac-wipe\twaiting\t30s\twipe the mac\nr1\trunning\t20s\trun the job"
        );
    }

    #[test]
    fn generated_id_skips_in_use() {
        let mut reg = Registry::new();
        assert_eq!(
            reg.open(Some("r1".to_string()), "a", 0),
            Ok("r1".to_string())
        );
        assert_eq!(reg.open(None, "b", 1), Ok("r2".to_string()));
    }

    #[test]
    fn command_first_line_and_truncate() {
        let mut reg = Registry::new();
        assert_eq!(
            reg.open(None, "line one\nline two", 0),
            Ok("r1".to_string())
        );
        assert_eq!(reg.list_text(0), "r1\twaiting\t0s\tline one");

        let long = "x".repeat(100);
        let mut reg2 = Registry::new();
        assert_eq!(reg2.open(None, &long, 0), Ok("r1".to_string()));
        assert_eq!(
            reg2.list_text(0),
            format!("r1\twaiting\t0s\t{}", "x".repeat(60))
        );
    }

    #[test]
    fn close_frees_a_slot() {
        let mut reg = Registry::new();
        for i in 0..MAX_OPEN {
            assert!(reg.open(None, "cmd", i as u64).is_ok());
        }
        assert_eq!(reg.len(), MAX_OPEN);
        assert!(reg.open(None, "cmd", 10).is_err());
        assert!(reg.close("r1"));
        assert_eq!(reg.open(None, "cmd", 11), Ok("r5".to_string()));
        assert_eq!(reg.len(), MAX_OPEN);
    }

    #[test]
    fn is_empty_tracks_open_and_close() {
        let mut reg = Registry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.open(None, "cmd", 0).is_ok());
        assert!(!reg.is_empty());
        // Closing an unknown id leaves it non-empty.
        assert!(!reg.close("nope"));
        assert!(!reg.is_empty());
        assert!(reg.close("r1"));
        assert!(reg.is_empty());
        // A full registry is not empty, and a refused open changes nothing.
        let mut full = Registry::new();
        for i in 0..MAX_OPEN {
            assert!(full.open(None, "cmd", i as u64).is_ok());
        }
        assert!(full.open(None, "cmd", 10).is_err());
        assert!(!full.is_empty());
        assert!(Registry::default().is_empty());
    }

    #[test]
    fn negative_paths() {
        let mut reg = Registry::new();
        for i in 0..MAX_OPEN {
            assert!(reg.open(None, "cmd", i as u64).is_ok());
        }
        let busy = reg.open(None, "cmd", 10).unwrap_err();
        assert!(busy.starts_with("busy"));
        // A duplicate id is refused while there's still room (when full, busy wins).
        assert!(reg.close("r4"));
        let dup = reg.open(Some("r1".to_string()), "cmd", 10).unwrap_err();
        assert!(dup.starts_with("id in use"));
        assert!(!reg.set_state("nope", State::Running));
        assert!(!reg.close("nope"));
        assert_eq!(reg.offset("nope"), None);
    }

    #[test]
    fn state_lookup() {
        let mut reg = Registry::new();
        assert_eq!(reg.open(None, "a", 0), Ok("r1".to_string()));
        assert_eq!(reg.state("r1"), Some(State::Waiting));
        assert!(reg.set_state("r1", State::Done));
        assert_eq!(reg.state("r1"), Some(State::Done));
        assert_eq!(reg.state("nope"), None);
    }

    #[test]
    fn oldest_done_picks_the_first_finished() {
        let mut reg = Registry::new();
        // Nothing open, then nothing finished: no window to reuse.
        assert_eq!(reg.oldest_done(), None);
        assert_eq!(reg.open(None, "a", 5), Ok("r1".to_string()));
        assert_eq!(reg.open(None, "b", 1), Ok("r2".to_string()));
        assert_eq!(reg.open(None, "c", 3), Ok("r3".to_string()));
        assert_eq!(reg.oldest_done(), None);
        // A running request is never handed out.
        assert!(reg.set_state("r2", State::Running));
        assert_eq!(reg.oldest_done(), None);
        assert!(reg.set_state("r1", State::Done));
        assert!(reg.set_state("r3", State::Done));
        // r3 opened before r1 (t=3 vs t=5), so it goes first.
        assert_eq!(reg.oldest_done(), Some("r3".to_string()));
        assert!(reg.close("r3"));
        assert_eq!(reg.oldest_done(), Some("r1".to_string()));
    }

    #[test]
    fn offset_slots() {
        let mut reg = Registry::new();
        assert_eq!(reg.open(None, "a", 0), Ok("r1".to_string()));
        assert_eq!(reg.open(None, "b", 1), Ok("r2".to_string()));
        assert_eq!(reg.offset("r1"), Some((0, 0)));
        assert_eq!(reg.offset("r2"), Some((32, 32)));
    }
}
