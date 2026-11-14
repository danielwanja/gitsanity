//! The Gitsanity engine: read a repository's history straight from its object
//! database, keep it as time-sorted columns, and answer dashboard questions
//! from those. Shared by the terminal app (this crate's binary) and, through
//! `ffi/`, by the Mac app.

pub mod git;
pub mod graph;
pub mod insight;
pub mod query;
pub mod store;
pub mod time;
