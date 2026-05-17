//! Domain model: todo items, priorities, and a small aggregate with a trait
//! surface.
//!
//! `Board::headline_count` contains the stable, unique anchor
//! `self.entries.len() /* BENCH_TRAIT_ANCHOR */`. The harness rewrites
//! `.len()` to a method that does not exist, producing an `E0599`-class
//! error that rust-analyzer resolves from its **own** analysis (no Leptos
//! macro expansion needed). Do not reformat or duplicate that token.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    pub fn tag(&self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
        }
    }

    pub fn weight(&self) -> u32 {
        match self {
            Priority::Low => 1,
            Priority::Normal => 3,
            Priority::High => 9,
        }
    }
}

impl Default for Priority {
    fn default() -> Self {
        Priority::Normal
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoItem {
    pub id: u64,
    pub title: String,
    pub done: bool,
    pub priority: Priority,
}

impl TodoItem {
    pub fn new(id: u64, title: &str, priority: Priority) -> Self {
        TodoItem {
            id,
            title: title.to_string(),
            done: false,
            priority,
        }
    }

    pub fn score(&self) -> u32 {
        let base = self.priority.weight();
        if self.done {
            base / 3
        } else {
            base
        }
    }
}

/// Anything that can describe itself in one line. A small trait surface so
/// the RA-native error path has real trait resolution to do.
pub trait Summarize {
    fn summary(&self) -> String;
    fn is_noteworthy(&self) -> bool {
        self.summary().len() > 24
    }
}

impl Summarize for TodoItem {
    fn summary(&self) -> String {
        format!(
            "[{}] {} ({})",
            if self.done { "x" } else { " " },
            self.title,
            self.priority.tag()
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct Board {
    pub entries: Vec<TodoItem>,
}

impl Board {
    pub fn from_items(entries: Vec<TodoItem>) -> Self {
        Board { entries }
    }

    pub fn total_score(&self) -> u32 {
        self.entries.iter().map(TodoItem::score).sum()
    }

    pub fn open(&self) -> Vec<&TodoItem> {
        self.entries.iter().filter(|i| !i.done).collect()
    }

    /// Headline like "3 / 5 open". The `.len()` call below is the RA-native
    /// error anchor used by the benchmark harness.
    pub fn headline_count(&self) -> String {
        let total: usize = self.entries.len() /* BENCH_TRAIT_ANCHOR */;
        let open = self.open().len();
        format!("{open} / {total} open")
    }
}

impl Summarize for Board {
    fn summary(&self) -> String {
        format!("board: {}", self.headline_count())
    }

    fn is_noteworthy(&self) -> bool {
        !self.entries.is_empty()
    }
}
