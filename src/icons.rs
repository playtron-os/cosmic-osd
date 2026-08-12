//! The shell's glyph set, vendored under `res/icons/lucide` instead of resolved
//! through the icon theme, so every OSD and dialog draws the same Lucide marks
//! whatever icon theme happens to be installed.

use cosmic::widget;

pub const CHECK: &[u8] = include_bytes!("../res/icons/lucide/check.svg");
pub const CPU: &[u8] = include_bytes!("../res/icons/lucide/cpu.svg");
pub const EYE: &[u8] = include_bytes!("../res/icons/lucide/eye.svg");
pub const EYE_OFF: &[u8] = include_bytes!("../res/icons/lucide/eye-off.svg");
pub const HEADPHONES: &[u8] = include_bytes!("../res/icons/lucide/headphones.svg");
pub const HEADSET: &[u8] = include_bytes!("../res/icons/lucide/headset.svg");
pub const KEYBOARD: &[u8] = include_bytes!("../res/icons/lucide/keyboard.svg");
pub const LAPTOP: &[u8] = include_bytes!("../res/icons/lucide/laptop.svg");
pub const LOCK: &[u8] = include_bytes!("../res/icons/lucide/lock.svg");
pub const LOCK_KEYHOLE: &[u8] = include_bytes!("../res/icons/lucide/lock-keyhole.svg");
pub const LOG_OUT: &[u8] = include_bytes!("../res/icons/lucide/log-out.svg");
pub const MIC: &[u8] = include_bytes!("../res/icons/lucide/mic.svg");
pub const MIC_OFF: &[u8] = include_bytes!("../res/icons/lucide/mic-off.svg");
pub const MONITOR: &[u8] = include_bytes!("../res/icons/lucide/monitor.svg");
pub const PLANE: &[u8] = include_bytes!("../res/icons/lucide/plane.svg");
pub const POWER: &[u8] = include_bytes!("../res/icons/lucide/power.svg");
pub const ROTATE_CCW: &[u8] = include_bytes!("../res/icons/lucide/rotate-ccw.svg");
pub const SUN: &[u8] = include_bytes!("../res/icons/lucide/sun.svg");
pub const TOUCHPAD: &[u8] = include_bytes!("../res/icons/lucide/touchpad.svg");
pub const TOUCHPAD_OFF: &[u8] = include_bytes!("../res/icons/lucide/touchpad-off.svg");
pub const VOLUME_1: &[u8] = include_bytes!("../res/icons/lucide/volume-1.svg");
pub const VOLUME_2: &[u8] = include_bytes!("../res/icons/lucide/volume-2.svg");
pub const VOLUME_X: &[u8] = include_bytes!("../res/icons/lucide/volume-x.svg");
pub const WIFI: &[u8] = include_bytes!("../res/icons/lucide/wifi.svg");

/// Build an icon widget from one of the vendored glyphs above.
pub fn lucide_icon(bytes: &'static [u8], size: u16) -> widget::Icon {
    // `symbolic` lets the theme recolor the glyph; the vendored SVGs stroke with
    // `currentColor` for exactly that.
    let mut handle = widget::icon::from_svg_bytes(bytes);
    handle.symbolic = true;
    widget::icon::icon(handle).size(size)
}
