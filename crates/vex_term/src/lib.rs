//! A custom terminal viewport and cell-diff renderer, with Crossterm for I/O.

pub mod app;
mod events;
pub mod files;
pub mod input;
mod picker;
pub mod render;
pub mod screen;
pub mod terminal;
