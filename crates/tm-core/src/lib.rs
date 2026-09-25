//! TurboMerger core: everything between "a folder" and "LLM-ready files",
//! with no GUI dependency. The command line (`apps/tm-cli`), the MCP server
//! (`apps/tm-mcp`) and the desktop app (`src-tauri`) are thin shells over it.

pub mod applyback;
pub mod cache;
pub mod cancel;
pub mod compress;
pub mod config;
pub mod job;
pub mod merger;
pub mod progress;
pub mod remote;
pub mod repomap;
pub mod scanner;
pub mod security;
pub mod tokens;

pub use cancel::{CancelToken, Cancelled};
pub use job::MergeOptions;
