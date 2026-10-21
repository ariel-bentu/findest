//! Fast filename search, in the spirit of FSearch / Everything: an in-memory
//! index of every file under the indexed roots, searched in parallel with a
//! small query language. Original implementation (no FSearch code is used).

pub mod index;
pub mod live;
pub mod query;
pub mod run;
pub mod saved;
pub mod source;

pub use index::{Excludes, Index};
pub use live::{LiveProgress, live_search};
pub use query::{Query, parse};
pub use run::{SearchOptions, SearchResult, search};
