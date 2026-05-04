//! Rendering: tab bar, status bar, pane tree, and overlay widgets.

pub mod border;
pub mod core_render;
pub mod overlay;
pub mod panels;
pub mod panels_fleet;
pub mod scratch;

pub use core_render::render;
pub use overlay::{
    render_command_palette, render_confirm, render_help, render_menu, render_move_pane_target,
    render_rename, render_scroll_indicator, render_tab_list,
};
pub use panels::{render_decisions, render_tasks, task_board_columns};
pub use scratch::{render_scratch_shell, scratch_shell_rect};
