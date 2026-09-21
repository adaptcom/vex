//! A custom terminal viewport and cell-diff renderer, with Crossterm for I/O.

pub mod app;
mod clipboard;
mod documentation;
mod events;
pub mod files;
pub mod input;
mod picker;
mod prompt;
pub mod render;
pub mod screen;
pub mod terminal;
pub mod theme;
mod ui;

pub(crate) use vex_editor::paths;
