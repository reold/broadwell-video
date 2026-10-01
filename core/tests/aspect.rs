//! Aspect-ratio math for non-square pixels. Needs no GPU.

use hwa_core::ffmpeg::display_aspect;

fn assert_close(got: f32, want: f32, what: &str) {
    assert!(
        (got - want).abs() < 1e-4,
        "{what}: expected {want}, got {got}"
    );
}

#[test]
fn square_pixels_use_the_frame_shape() {
    assert_close(display_aspect(1920, 1080, 1, 1), 16.0 / 9.0, "1080p");
    assert_close(display_aspect(640, 480, 1, 1), 4.0 / 3.0, "VGA");
    assert_close(display_aspect(3840, 1600, 1, 1), 2.4, "ultrawide");
}

#[test]
fn non_square_pixels_change_the_display_aspect() {
    // The classic anamorphic cases, all of which display as 16:9.
    assert_close(display_aspect(720, 480, 32, 27), 16.0 / 9.0, "NTSC wide");
    assert_close(display_aspect(720, 576, 64, 45), 16.0 / 9.0, "PAL wide");
    // NTSC 4:3, where the stored frame is already 3:2.
    assert_close(display_aspect(720, 480, 8, 9), 4.0 / 3.0, "NTSC 4:3");
}

#[test]
fn missing_sar_falls_back_to_square_pixels() {
    // 720x480 stored shape is 3:2.
    assert_close(display_aspect(720, 480, 0, 1), 1.5, "num 0");
    assert_close(display_aspect(720, 480, 1, 0), 1.5, "den 0");
    assert_close(display_aspect(720, 480, 0, 0), 1.5, "both 0");
    assert_close(display_aspect(720, 480, -1, -1), 1.5, "negative");
}

#[test]
fn degenerate_dimensions_stay_finite() {
    assert!(display_aspect(0, 0, 1, 1).is_finite());
    assert!(display_aspect(0, 1080, 32, 27).is_finite());
    assert!(display_aspect(1920, 0, 32, 27).is_finite());
}
