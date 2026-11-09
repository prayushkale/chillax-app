//! A braille spinner for slow work. The run loop bumps `App::tick` every 100 ms, but
//! only while something is loading, so an idle TUI never redraws on a timer.

pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The frame to draw at `tick`; wraps around.
pub fn frame(tick: u64) -> &'static str {
    FRAMES[(tick % FRAMES.len() as u64) as usize]
}
