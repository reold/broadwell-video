//! Shared pipeline code for HWA Video.

pub mod encoder;
pub mod export;
pub mod ffmpeg;
pub mod gpu;
pub mod renderer;
pub mod state;

/// Point libva at the maintained driver before any VA-API device is opened.
///
/// This machine's `/etc/environment` pins `LIBVA_DRIVER_NAME=i965`, so every
/// process inherits the archived driver whether it asked for it or not. i965
/// drops frames on this hardware -- 840 decodable of 898 with synthetic black
/// input and nothing else running -- and once aborted ffmpeg on an assertion in
/// `i965_MapBuffer2`. The subprocess export escaped that by overriding the
/// variable for its child, which is why it measured 899 of 899 while the
/// in-process encoder, inheriting the ambient setting, kept losing a few frames
/// per thousand. This does for the whole process what that override did for the
/// child, decode included: the decode path was measured working, and faster,
/// under iHD.
///
/// `HWA_VAAPI_DRIVER` chooses instead; set it to `inherit` to leave the
/// environment alone.
pub fn select_vaapi_driver() {
    let wanted = std::env::var("HWA_VAAPI_DRIVER").unwrap_or_else(|_| "iHD".to_string());
    if wanted == "inherit" {
        return;
    }
    let current = std::env::var("LIBVA_DRIVER_NAME").ok();
    if current.as_deref() == Some(wanted.as_str()) {
        return;
    }
    // SAFETY: called from the thread that constructs the renderer, before any
    // VA-API device exists in this process, so no libva call can race it.
    unsafe { std::env::set_var("LIBVA_DRIVER_NAME", &wanted) };
    println!(
        "VA-API driver: {wanted} (overriding {})",
        current.unwrap_or_else(|| "the libva default".to_string())
    );
}

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
