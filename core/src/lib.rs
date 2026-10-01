//! Shared pipeline code for HWA Video.

pub mod export;
pub mod ffmpeg;
pub mod gpu;
pub mod renderer;
pub mod state;

/// Local wall-clock time as `HH:MM:SS`, for log lines.
///
/// Goes through libc rather than pulling in a date/time crate: it is already a
/// dependency and this is all that is needed.
pub fn wall_clock_stamp() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return "--:--:--".to_string();
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

/// SMPTE-style `HH:MM:SS:FF` for a position in milliseconds.
///
/// The frame field is derived from the sub-second part rather than from a frame
/// counter, so it labels the frame the position falls inside.
pub fn timecode(ms: i64, fps: f64) -> String {
    let fps = if fps > 0.0 { fps } else { 30.0 };
    let ms = ms.max(0);
    let seconds = ms / 1000;
    let frames = ((ms % 1000) as f64 / 1000.0 * fps).floor() as i64;
    let last_frame = fps.round().max(1.0) as i64 - 1;
    format!(
        "{:02}:{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60,
        frames.clamp(0, last_frame)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecode_counts_up_in_frames() {
        assert_eq!(timecode(0, 30.0), "00:00:00:00");
        assert_eq!(timecode(999, 30.0), "00:00:00:29");
        assert_eq!(timecode(1000, 30.0), "00:00:01:00");
        assert_eq!(timecode(60_000, 30.0), "00:01:00:00");
        assert_eq!(timecode(3_600_000, 30.0), "01:00:00:00");
        // 29.97 labels frames on the same grid.
        assert_eq!(timecode(30_030, 29.97), "00:00:30:00");
        // A nonsense position clamps instead of printing nonsense.
        assert_eq!(timecode(-5, 30.0), "00:00:00:00");
        assert_eq!(timecode(500, 0.0), "00:00:00:15");
    }

    #[test]
    fn wall_clock_stamp_looks_like_a_clock() {
        let stamp = wall_clock_stamp();
        let parts: Vec<&str> = stamp.split(':').collect();
        assert_eq!(parts.len(), 3, "{stamp}");
        assert!(parts.iter().all(|p| p.len() == 2), "{stamp}");
    }
}
