//! The board model. Plain Rust types held inside a signal, with private fields, so the registry
//! has to reach them through the signal and through code emitted next to the types.

#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "scry", derive(scry::Reflect))]
pub enum Priority {
    Low,
    Normal,
    High,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "scry", derive(scry::Reflect))]
pub struct Task {
    pub id: u32,
    pub title: String,
    pub done: bool,
    pub priority: Priority,
}

#[cfg_attr(feature = "scry", derive(scry::Reflect))]
pub struct Board {
    tasks: Vec<Task>,
    next_id: u32,
    completed_total: u32,
}

impl Board {
    pub fn seeded() -> Self {
        let mut board = Self { tasks: Vec::new(), next_id: 1, completed_total: 0 };
        board.add("Install Rust", Priority::High);
        board.add("Build the demos", Priority::Normal);
        board.add("Design the registry", Priority::Low);
        board.toggle(1);
        board
    }

    pub fn add(&mut self, title: &str, priority: Priority) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.tasks.push(Task { id, title: title.to_owned(), done: false, priority });
        id
    }

    pub fn toggle(&mut self, id: u32) {
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
            task.done = !task.done;
            if task.done {
                self.completed_total += 1;
            }
        }
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn summary(&self) -> String {
        let done = count_where(&self.tasks, |task| task.done);
        let open = self.tasks.len() - done;
        let percent = ratio(done as u32, self.tasks.len() as u32) * 100.0;
        format!("{open} open · {done} done ({percent:.0}%) · {} completed so far", self.completed_total)
    }
}

/// Generic over a closure type: an instantiation a registry can never name, kept here on purpose.
pub fn count_where<T>(items: &[T], predicate: impl Fn(&T) -> bool) -> usize {
    items.iter().filter(|item| predicate(item)).count()
}

/// Generic over a nameable type: used as `ratio::<u32>`.
pub fn ratio<T: Into<f64>>(part: T, whole: T) -> f64 {
    let whole = whole.into();
    if whole == 0.0 { 0.0 } else { part.into() / whole }
}
