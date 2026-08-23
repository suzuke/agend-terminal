use super::*;

/// #3314: a candidate must be STABLE before it is answered. One sighting is
/// not enough — deterministic, because the clock is injected.
#[test]
fn single_sighting_is_not_stable_enough_to_answer_3314() {
    let mut gen = Generation3314::new("3314-unstable", true);
    assert!(!gen.frame(FRAME_LIVE_MODAL_3314));
    assert!(gen.bytes().is_empty(), "one sighting must write nothing");
}

/// #3333: production cannot wait for a second PTY read: the modal is a
/// static frame. The first complete sighting must therefore schedule the
/// existing delayed writer, whose late barrier check proves the frame stayed
/// untouched for the stability window.
#[test]
fn static_modal_first_sighting_schedules_delayed_cr_3333() {
    let mut gen = Generation3314::new("3333-static", true);
    set_inline_dismiss_write_for_test(false);
    assert!(try_prepared_dismiss_dialog(
        &gen.tag,
        FRAME_LIVE_MODAL_2_1_241_3314,
        &gen.writer,
        &gen.prepared,
        DismissScanScope::RearmPreIdle,
        &mut gen.gate,
        LogicalMs(0),
    ));
    assert!(gen.bytes().is_empty(), "the CR must remain delayed");
    for _ in 0..20 {
        if gen.bytes().as_slice() == b"\r" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(gen.bytes().as_slice(), b"\r");
}

#[test]
fn unchanged_complete_modal_rearms_pre_idle_scan_3333() {
    assert!(dev_modal_rescan_armed(false, FRAME_LIVE_MODAL_2_1_241_3314));
    assert!(!dev_modal_rescan_armed(true, FRAME_LIVE_MODAL_2_1_241_3314));
    assert!(!dev_modal_rescan_armed(false, "ordinary transcript"));
}

#[test]
fn newer_child_output_cancels_and_reschedules_static_modal_cr_3333() {
    let mut gen = Generation3314::new("3333-output", true);
    set_inline_dismiss_write_for_test(false);
    assert!(try_prepared_dismiss_dialog(
        &gen.tag,
        FRAME_LIVE_MODAL_2_1_241_3314,
        &gen.writer,
        &gen.prepared,
        DismissScanScope::RearmPreIdle,
        &mut gen.gate,
        LogicalMs(0),
    ));
    gen.note_activity(); // production: note_pty_output before the next scan
    std::thread::sleep(std::time::Duration::from_millis(350));
    assert!(gen.bytes().is_empty());

    assert!(try_prepared_dismiss_dialog(
        &gen.tag,
        FRAME_LIVE_MODAL_2_1_241_3314,
        &gen.writer,
        &gen.prepared,
        DismissScanScope::RearmPreIdle,
        &mut gen.gate,
        LogicalMs(1),
    ));
    for _ in 0..20 {
        if gen.bytes().as_slice() == b"\r" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(gen.bytes().as_slice(), b"\r");
}
