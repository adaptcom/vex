//! A custom terminal viewport and cell-diff renderer, with Crossterm for I/O.

pub mod app;
mod clipboard;
mod documentation;
mod events;
pub mod files;
mod git_status;
pub mod input;
mod picker;
mod prompt;
pub mod render;
pub mod screen;
mod sections;
pub mod terminal;
mod ui;
