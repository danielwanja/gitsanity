//! Commit graph for the Lens view: a topological order plus lane layout,
//! one text row per commit.
//!
//! The layout is computed lazily. Lane state is checkpointed every `STEP`
//! rows, so any page of a million-commit history can be drawn by replaying at
//! most `STEP` rows, without keeping every row in memory.

use crate::store::{NO_PARENT, Store};

const STEP: usize = 128;
const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Lane {
    /// The commit this lane is waiting for, or `NONE` when free.
    want: u32,
    color: u8,
}

#[derive(Clone, Default)]
struct State {
    lanes: Vec<Lane>,
    next_color: u8,
}

pub struct Row {
    pub commit: u32,
    /// Two cells per lane: (glyph, colour index).
    pub cells: Vec<(char, u8)>,
}

pub struct Graph {
    /// Commits newest first, every child before its parents.
    pub order: Vec<u32>,
    /// Row of each commit in `order`.
    pub pos: Vec<u32>,
    checkpoints: Vec<State>,
    state: State,
    computed: usize,
}

impl Graph {
    pub fn new(store: &Store) -> Graph {
        let n = store.len();
        // Topological order the way `git log --topo-order` does it: a stack of
        // commits whose children are all out, so a merged branch is listed in
        // one run right under its merge instead of interleaved by date. That
        // keeps the graph a few lanes wide even for the Linux kernel.
        let mut pending = vec![0u32; n];
        for p in &store.parents {
            if *p != NO_PARENT {
                pending[*p as usize] += 1;
            }
        }
        // Tips, oldest first, so the newest is popped first.
        let mut stack: Vec<u32> = (0..n as u32)
            .filter(|i| pending[*i as usize] == 0)
            .collect();
        let mut order = Vec::with_capacity(n);
        let mut pos = vec![0u32; n];
        while let Some(c) = stack.pop() {
            pos[c as usize] = order.len() as u32;
            order.push(c);
            // First parent goes on first, so side branches come off the stack before the mainline.
            for p in store.parents_of(c as usize) {
                if *p != NO_PARENT {
                    pending[*p as usize] -= 1;
                    if pending[*p as usize] == 0 {
                        stack.push(*p);
                    }
                }
            }
        }
        Graph {
            order,
            pos,
            checkpoints: Vec::new(),
            state: State::default(),
            computed: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Rows `start..start + count`, laid out.
    pub fn rows(&mut self, store: &Store, start: usize, count: usize) -> Vec<Row> {
        let end = (start + count).min(self.order.len());
        // Advance the running state (saving checkpoints) far enough to cover the page.
        while self.computed < end {
            if self.computed.is_multiple_of(STEP) {
                self.checkpoints.push(self.state.clone());
            }
            step(&mut self.state, store, self.order[self.computed], None);
            self.computed += 1;
        }
        if start >= end {
            return Vec::new();
        }
        let mut state = self.checkpoints[start / STEP].clone();
        for commit in &self.order[(start / STEP) * STEP..start] {
            step(&mut state, store, *commit, None);
        }
        self.order[start..end]
            .iter()
            .map(|commit| {
                let mut cells = Vec::new();
                step(&mut state, store, *commit, Some(&mut cells));
                Row {
                    commit: *commit,
                    cells,
                }
            })
            .collect()
    }
}

fn free_lane(state: &mut State, from: usize) -> usize {
    let color = state.next_color;
    state.next_color = (state.next_color + 1) % 6;
    let lane = Lane { want: NONE, color };
    match state.lanes.iter().skip(from).position(|l| l.want == NONE) {
        Some(i) => {
            state.lanes[from + i] = lane;
            from + i
        }
        None => {
            state.lanes.push(lane);
            state.lanes.len() - 1
        }
    }
}

/// Draw a horizontal link between two lanes on the commit's row.
fn link(cells: &mut [(char, u8)], a: usize, b: usize, color: u8) {
    let (lo, hi) = (a.min(b), a.max(b));
    for cell in &mut cells[2 * lo + 1..2 * hi] {
        *cell = match cell.0 {
            ' ' | '─' => ('─', color),
            // A lane passing through keeps its line; the link visibly crosses it.
            _ => ('┼', cell.1),
        };
    }
}

fn step(state: &mut State, store: &Store, commit: u32, mut out: Option<&mut Vec<(char, u8)>>) {
    // The first lane waiting for this commit is its column; others merge into it.
    let mut col = None;
    let mut joining = Vec::new();
    for (i, lane) in state.lanes.iter().enumerate() {
        if lane.want == commit {
            if col.is_none() {
                col = Some(i);
            } else {
                joining.push(i);
            }
        }
    }
    let col = col.unwrap_or_else(|| free_lane(state, 0));
    let color = state.lanes[col].color;

    if let Some(cells) = out.as_deref_mut() {
        cells.clear();
        cells.resize(state.lanes.len() * 2, (' ', 0));
        for (i, lane) in state.lanes.iter().enumerate() {
            if lane.want != NONE && i != col && !joining.contains(&i) {
                cells[2 * i] = ('│', lane.color);
            }
        }
    }
    for i in &joining {
        if let Some(cells) = out.as_deref_mut() {
            link(cells, col, *i, state.lanes[*i].color);
            cells[2 * i] = (if *i > col { '╯' } else { '╰' }, state.lanes[*i].color);
        }
        state.lanes[*i].want = NONE;
    }

    let mut parents = store
        .parents_of(commit as usize)
        .iter()
        .copied()
        .filter(|p| *p != NO_PARENT);
    // The lane continues with the first parent (or ends at a root).
    state.lanes[col].want = parents.next().unwrap_or(NONE);
    // Each further parent of a merge gets a lane: an existing one already
    // waiting for it, or a new one opening to the side.
    for p in parents {
        let lane = match state.lanes.iter().position(|l| l.want == p) {
            Some(i) if i != col => i,
            _ => {
                let i = free_lane(state, col + 1);
                state.lanes[i].want = p;
                if let Some(cells) = out.as_deref_mut()
                    && cells.len() < state.lanes.len() * 2
                {
                    cells.resize(state.lanes.len() * 2, (' ', 0));
                }
                i
            }
        };
        if let Some(cells) = out.as_deref_mut() {
            link(cells, col, lane, state.lanes[lane].color);
            cells[2 * lane] = (if lane > col { '╮' } else { '╭' }, state.lanes[lane].color);
        }
    }
    if let Some(cells) = out {
        let merge = store.parents_of(commit as usize).len() > 1;
        cells[2 * col] = (if merge { '◆' } else { '●' }, color);
        while cells.last().is_some_and(|c| c.0 == ' ') {
            cells.pop();
        }
    }
    while state.lanes.last().is_some_and(|l| l.want == NONE) {
        state.lanes.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with just the columns the graph reads.
    fn store(parents: &[&[u32]]) -> Store {
        let n = parents.len();
        let mut s = crate::store::empty();
        s.oid = vec![[0; 20]; n];
        s.time = (0..n as i64).collect();
        s.par_start = vec![0];
        for p in parents {
            s.parents.extend_from_slice(p);
            s.par_start.push(s.parents.len() as u32);
        }
        s
    }

    fn draw(g: &mut Graph, s: &Store) -> Vec<String> {
        g.rows(s, 0, g.len())
            .iter()
            .map(|r| {
                format!(
                    "{} {}",
                    r.commit,
                    r.cells.iter().map(|c| c.0).collect::<String>()
                )
            })
            .collect()
    }

    #[test]
    fn branch_and_merge() {
        // 0 <- 1 <- 3 (merge of 1 and 2) <- 4,  0 <- 2
        let s = store(&[&[], &[0], &[0], &[1, 2], &[3]]);
        let mut g = Graph::new(&s);
        assert_eq!(g.order, [4, 3, 2, 1, 0]);
        assert_eq!(
            draw(&mut g, &s),
            ["4 ●", "3 ◆─╮", "2 │ ●", "1 ● │", "0 ●─╯"]
        );
        // Any page can be drawn on its own and matches the full drawing.
        let mut fresh = Graph::new(&s);
        let page = fresh.rows(&s, 3, 2);
        assert_eq!(page.iter().map(|r| r.commit).collect::<Vec<_>>(), [1, 0]);
        assert_eq!(page[1].cells.iter().map(|c| c.0).collect::<String>(), "●─╯");
    }

    #[test]
    fn pages_match_across_checkpoints() {
        // A long chain with a side branch merged every tenth commit.
        let mut parents: Vec<Vec<u32>> = vec![vec![]];
        for i in 1..1000u32 {
            parents.push(if i % 10 == 0 && i > 20 {
                vec![i - 1, i - 7]
            } else {
                vec![i - 1]
            });
        }
        let refs: Vec<&[u32]> = parents.iter().map(Vec::as_slice).collect();
        let s = store(&refs);
        let mut all = Graph::new(&s);
        let full = draw(&mut all, &s);
        let mut paged = Graph::new(&s);
        for start in [990, 500, 127, 128, 129, 0, 700] {
            let page: Vec<String> = paged
                .rows(&s, start, 20)
                .iter()
                .map(|r| {
                    format!(
                        "{} {}",
                        r.commit,
                        r.cells.iter().map(|c| c.0).collect::<String>()
                    )
                })
                .collect();
            assert_eq!(page, full[start..(start + 20).min(1000)], "page at {start}");
        }
    }
}
