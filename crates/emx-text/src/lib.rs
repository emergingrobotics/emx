#![forbid(unsafe_code)]

mod block_cache;
mod display;
mod identity;
mod line_index;
mod piece_tree;
pub mod search;
mod source;
mod text;
pub mod word_syntax;

pub use block_cache::{BLOCK_SIZE, BlockCache};
pub use display::{DisplayUnit, display_units, grapheme_text, unit_width};
pub use identity::FileIdentity;
pub use line_index::{LineIndex, NthNewline, index_file};
pub use piece_tree::{Piece, PieceSource};
pub use source::{AddStore, Original, ReadError, Sources};
pub use text::{Chunk, ChunkIter, LineLookup, Text};
