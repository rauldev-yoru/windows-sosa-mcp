//! Hands-free mode, synthetic input and waits (docs/design.md, D25-D37).
//!
//! * [`session`] — the detached `__handsfree-session` process: state
//!   machine + (on Windows) LL hooks and overlay in `session_win`;
//! * [`state`] — the shared `state.json` / `stop.json` files;
//! * [`keys`] — key names, chords and the release hotkey.

pub mod input;
#[cfg(windows)]
pub mod input_win;
pub mod keys;
pub mod paint;
pub mod session;
#[cfg(windows)]
pub mod session_win;
pub mod state;
pub mod tools;
