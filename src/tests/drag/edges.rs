//! Title-bar drags held at the edge of a strip, which scroll the strip one
//! column at a time.

use std::time::Duration;

use super::*;
use crate::ecs::SelectedVirtualMarker;
use crate::ecs::drag::EdgeHold;

/// Window 5's title bar, with the strip scrolled to show the last column.
/// The press is above the window's centre, so the pointer alone picks the
/// column.
const LAST_TITLE_BAR: Origin = Origin::new(824, 30);

/// Window 0's title bar, with the strip scrolled to show the first column.
const FIRST_TITLE_BAR: Origin = Origin::new(200, 30);

/// Inside the left edge band of the main display.
const LEFT_EDGE: Origin = Origin::new(2, 500);

/// Inside the right edge band of the main display.
const RIGHT_EDGE: Origin = Origin::new(TEST_DISPLAY_WIDTH - 2, 500);

/// The strip offset with the last of six columns at the right edge.
const SHOWING_LAST: i32 = TEST_DISPLAY_WIDTH - 6 * TEST_WINDOW_WIDTH;

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

/// Six windows of 400 points in their own columns on the displays of
/// `harness`, with the first or the last focused. The window server sees the
/// left button held down.
fn six_columns(harness: TestHarness, last: bool) -> TestHarness {
    let mut harness = holding(harness.with_windows(6));
    let focus = if last {
        Direction::Last
    } else {
        Direction::First
    };
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        window_command(Operation::Focus(focus)),
    ]);
    harness
}

/// [`six_columns`] on the main display alone, with the option on.
fn six_dragged_columns(last: bool) -> TestHarness {
    six_columns(TestHarness::new().with_config(drag_enabled()), last)
}

/// The strip offset of the main display's strip.
fn strip_x(world: &mut World) -> i32 {
    world
        .query_filtered::<(&LayoutStrip, &Position), With<SelectedVirtualMarker>>()
        .iter(world)
        .find(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
        .expect("a strip for the main display")
        .1
        .0
        .x
}

fn edge_holds(world: &mut World) -> usize {
    world.query::<&EdgeHold>().iter(world).count()
}

fn drag_state(world: &mut World) -> DragState {
    world.resource::<DragState>().clone()
}

/// Presses window `id`'s title bar at `from` and drags the pointer to `to`,
/// with macOS moving the window along, as it does for a title-bar drag. With
/// the option on, the drag is confirmed when this returns.
fn drag_and_hold(harness: &mut TestHarness, id: WinID, from: Origin, to: Origin) {
    let start = frame(harness.world(), id);
    harness.world().write_message(press(from, id));
    harness.advance(FRAME);
    harness.world().write_message(drag_to(to));
    harness
        .mock_state
        .os_move_window(id, start.min + (to - from));
    harness.advance(FRAME * 2);
}

fn move_pointer(harness: &mut TestHarness, to: Origin) {
    harness.world().write_message(drag_to(to));
    harness.advance(FRAME);
}

fn release_at(harness: &mut TestHarness, at: Origin) {
    harness.world().write_message(release(at));
    harness.advance(FRAME);
}

fn assert_dragging(world: &mut World) {
    assert!(
        matches!(drag_state(world), DragState::Dragging(_)),
        "{:?}",
        drag_state(world)
    );
}

/// The pointer holds the left edge with windows 0 to 3 hidden or cut off
/// there. Each scroll brings the next of them into full view at the edge:
/// window 3, which shows 224 of its 400 points, after 500 ms, then window 2
/// 350 ms later. The dragged window stays where macOS put it. After the drop
/// between windows 1 and 2, which were both off the display at the press,
/// the strip no longer scrolls.
#[test]
fn holding_the_pointer_at_an_edge_scrolls_one_column_after_500_ms_then_every_350_ms() {
    let mut harness = six_dragged_columns(true);
    assert_eq!(strip_x(harness.world()), SHOWING_LAST);
    drag_and_hold(&mut harness, 5, LAST_TITLE_BAR, LEFT_EDGE);
    assert_dragging(harness.world());
    assert_eq!(edge_holds(harness.world()), 1);
    let dragged = frame(harness.world(), 5);
    assert_eq!(dragged.min, Origin::new(-198, 490));

    harness.advance(ms(440));
    assert_eq!(strip_x(harness.world()), SHOWING_LAST, "before 500 ms");
    harness.advance(ms(80));
    assert_eq!(strip_x(harness.world()), SHOWING_LAST + 176, "at 500 ms");
    assert_eq!(frame(harness.world(), 5), dragged);

    harness.advance(ms(280));
    assert_eq!(
        strip_x(harness.world()),
        SHOWING_LAST + 176,
        "before 850 ms"
    );
    harness.advance(ms(80));
    assert_eq!(
        strip_x(harness.world()),
        SHOWING_LAST + 176 + TEST_WINDOW_WIDTH,
        "at 850 ms"
    );
    assert_eq!(frame(harness.world(), 5), dragged);

    release_at(&mut harness, LEFT_EDGE);
    harness.advance(ms(600));
    assert_eq!(
        columns(harness.world()),
        vec![vec![0], vec![1], vec![5], vec![2], vec![3], vec![4]]
    );
    assert_eq!(edge_holds(harness.world()), 0);
    let dropped = strip_x(harness.world());
    harness.advance(ms(1000));
    assert_eq!(strip_x(harness.world()), dropped);
}

/// A pointer that leaves the edge band before the delay ends the hold. Back
/// at the edge, it waits the whole delay again.
#[test]
fn leaving_the_edge_ends_the_hold_and_coming_back_waits_again() {
    let mut harness = six_dragged_columns(true);
    drag_and_hold(&mut harness, 5, LAST_TITLE_BAR, LEFT_EDGE);
    harness.advance(ms(300));
    move_pointer(&mut harness, Origin::new(40, 500));
    assert_eq!(edge_holds(harness.world()), 0);
    harness.advance(ms(400));
    assert_eq!(strip_x(harness.world()), SHOWING_LAST, "away from the edge");

    move_pointer(&mut harness, LEFT_EDGE);
    harness.advance(ms(400));
    assert_eq!(strip_x(harness.world()), SHOWING_LAST, "back for 400 ms");
    harness.advance(ms(120));
    assert_eq!(
        strip_x(harness.world()),
        SHOWING_LAST + 176,
        "back for 520 ms"
    );
}

/// Whether holding window 0 at the right edge of the main display, at
/// height `y`, scrolls the strip within 1200 ms. `ext` is another display,
/// which shows a native fullscreen space when `fullscreen` is set.
fn right_edge_scrolls(ext: Option<IRect>, fullscreen: bool, y: i32) -> bool {
    let mut harness = TestHarness::new().with_config(drag_enabled());
    if let Some(ext) = ext {
        harness = harness.with_display(EXT_DISPLAY_ID, ext, vec![EXT_WORKSPACE_ID]);
    }
    let mut harness = six_columns(harness, false);
    if fullscreen {
        harness
            .mock_state
            .activate_workspace(EXT_DISPLAY_ID, EXT_WORKSPACE_ID + 1, true);
    }
    let edge = Origin::new(RIGHT_EDGE.x, y);
    drag_and_hold(&mut harness, 0, FIRST_TITLE_BAR, edge);
    assert_dragging(harness.world());
    let before = strip_x(harness.world());
    harness.advance(ms(1200));
    strip_x(harness.world()) != before
}

/// An edge scrolls only where no other display touches it at the pointer's
/// height. A display beside the edge stops it, even one that shows a native
/// fullscreen space, which takes no drop. A display only diagonally across
/// the corner does not, and a display beside part of the edge stops it only
/// along that part.
#[test]
fn an_edge_scrolls_only_where_no_other_display_touches_it() {
    let right = |top: i32| {
        IRect::new(
            TEST_DISPLAY_WIDTH,
            top,
            TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
            top + EXT_DISPLAY_HEIGHT,
        )
    };
    for (case, ext, fullscreen, y, scrolls) in [
        ("no other display", None, false, 500, true),
        ("a display beside it", Some(right(0)), false, 500, false),
        (
            "a fullscreen display beside it",
            Some(right(0)),
            true,
            500,
            false,
        ),
        (
            "a display below and right",
            Some(right(TEST_DISPLAY_HEIGHT)),
            false,
            500,
            true,
        ),
        (
            "above a display beside its lower half",
            Some(right(384)),
            false,
            200,
            true,
        ),
        (
            "beside a display beside its lower half",
            Some(right(384)),
            false,
            600,
            false,
        ),
    ] {
        assert_eq!(right_edge_scrolls(ext, fullscreen, y), scrolls, "{case}");
    }
}

/// The pointer, not the dragged window's centre, starts an edge hold. Window
/// 0 grabbed near its right end has the pointer at the edge and its centre
/// 195 points inside. Grabbed near its left end, its centre reaches the edge
/// while the pointer stays 195 points clear of it.
#[test]
fn the_pointer_not_the_windows_centre_starts_an_edge_hold() {
    for (grab_right, scrolls) in [(true, true), (false, false)] {
        let mut harness = six_dragged_columns(false);
        let (from, to) = if grab_right {
            (Origin::new(395, 30), RIGHT_EDGE)
        } else {
            (Origin::new(5, 30), RIGHT_EDGE - IVec2::new(195, 0))
        };
        drag_and_hold(&mut harness, 0, from, to);
        assert_dragging(harness.world());
        harness.advance(ms(1200));
        assert_eq!(
            strip_x(harness.world()) != 0,
            scrolls,
            "grab_right={grab_right}"
        );
    }
}

/// The drop feedback and the drop follow the scrolled strip. Before the
/// scroll, the window's centre is over the middle half of window 3, so the
/// line runs across that column. After it, window 3 starts at the edge and
/// the centre is over its outer quarter, so the line runs down the gap
/// before it, and the drop opens a new column there.
#[test]
fn an_edge_scroll_moves_the_drop_feedback_and_the_drop() {
    let mut harness = six_dragged_columns(true);
    let log = record_overlay(&mut harness);
    drag_and_hold(&mut harness, 5, LAST_TITLE_BAR, LEFT_EDGE);
    harness.advance(FRAME);
    let Some(OverlayCall::Line(before, _)) = last_call(&log) else {
        panic!("a line before the scroll: {:?}", last_call(&log));
    };
    assert!(before.width() > before.height(), "{before:?}");

    harness.advance(ms(520));
    assert_eq!(strip_x(harness.world()), SHOWING_LAST + 176);
    let Some(OverlayCall::Line(after, _)) = last_call(&log) else {
        panic!("a line after the scroll: {:?}", last_call(&log));
    };
    assert!(after.height() > after.width(), "{after:?}");

    release_at(&mut harness, LEFT_EDGE);
    harness.advance(ms(600));
    assert_eq!(
        columns(harness.world()),
        vec![vec![0], vec![1], vec![2], vec![5], vec![3], vec![4]]
    );
}

/// A drag that ends without a release the tap reports, because the window
/// server reads the button up or the window closes, stops the scrolling.
#[test]
fn a_lost_release_or_a_closed_window_stops_the_scrolling() {
    for closed in [false, true] {
        let mut harness = six_dragged_columns(true);
        drag_and_hold(&mut harness, 5, LAST_TITLE_BAR, LEFT_EDGE);
        harness.advance(ms(520));
        assert_eq!(
            strip_x(harness.world()),
            SHOWING_LAST + 176,
            "closed={closed}"
        );
        assert_eq!(edge_holds(harness.world()), 1, "closed={closed}");

        if closed {
            harness.mock_state.os_close_window(5);
        } else {
            harness.mock_state.set_left_button_down(false);
        }
        harness.advance(ms(600));
        assert_eq!(
            drag_state(harness.world()),
            DragState::Idle,
            "closed={closed}"
        );
        assert_eq!(edge_holds(harness.world()), 0, "closed={closed}");
        let ended = strip_x(harness.world());
        harness.advance(ms(1000));
        assert_eq!(strip_x(harness.world()), ended, "closed={closed}");
    }
}

/// With the option off, which is the default, holding a dragged window at
/// the edge scrolls nothing, as before.
#[test]
fn without_the_option_an_edge_hold_scrolls_nothing() {
    let mut harness = six_columns(TestHarness::new(), true);
    drag_and_hold(&mut harness, 5, LAST_TITLE_BAR, LEFT_EDGE);
    assert_eq!(drag_state(harness.world()), DragState::Idle);
    harness.advance(ms(1200));
    assert_eq!(strip_x(harness.world()), SHOWING_LAST);
    assert_eq!(edge_holds(harness.world()), 0);
}

/// The pointer holds the right edge of an external display, which nothing
/// touches, and whose strip has three columns of 800 points, the last one
/// past that edge. When that display goes away, the pointer is on no
/// display, so the hold ends with the pointer still.
#[test]
fn a_display_removed_during_an_edge_hold_ends_the_hold() {
    let ext = IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    );
    let mut harness = TestHarness::new().with_config(drag_enabled()).with_display(
        EXT_DISPLAY_ID,
        ext,
        vec![EXT_WORKSPACE_ID],
    );
    for id in 100..103 {
        harness = harness.with_workspace_window(id, EXT_WORKSPACE_ID, move |window| {
            window.frame = IRect::new(ext.min.x, 20, ext.min.x + 800, ext.max.y);
        });
    }
    let mut harness = six_columns(harness, false);
    let edge = Origin::new(ext.max.x - 2, 500);
    drag_and_hold(&mut harness, 0, FIRST_TITLE_BAR, edge);
    assert_dragging(harness.world());
    assert_eq!(edge_holds(harness.world()), 1);

    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness.world().write_message(Event::DisplayRemoved {
        display_id: EXT_DISPLAY_ID,
    });
    harness.advance(FRAME * 2);
    assert_dragging(harness.world());
    assert_eq!(edge_holds(harness.world()), 0);
}

/// With slow animations, the strip can still be on its way to an earlier
/// target when a step comes. The step replaces that target, so the strip
/// stays where the step put it, with window 3 flush with the left edge.
#[test]
fn a_step_replaces_the_target_of_a_strip_still_animating() {
    let mut harness = six_dragged_columns(true);
    assert_eq!(strip_x(harness.world()), SHOWING_LAST);
    harness
        .world()
        .insert_resource(config("drag_to_reorder = true\nanimation_speed = 1\n"));
    drag_and_hold(&mut harness, 5, LAST_TITLE_BAR, LEFT_EDGE);

    let strip = harness
        .world()
        .query_filtered::<(Entity, &LayoutStrip), With<SelectedVirtualMarker>>()
        .iter(harness.world())
        .find_map(|(entity, strip)| (strip.id() == TEST_WORKSPACE_ID).then_some(entity))
        .expect("a strip for the main display");
    let y = harness
        .world()
        .get::<Position>(strip)
        .expect("strip position")
        .0
        .y;
    harness
        .world()
        .entity_mut(strip)
        .insert(crate::ecs::RepositionMarker(Origin::new(
            SHOWING_LAST - 100,
            y,
        )));
    harness.advance(ms(520));
    assert_eq!(strip_x(harness.world()), SHOWING_LAST + 176);
    harness.advance(ms(300));
    assert_eq!(strip_x(harness.world()), SHOWING_LAST + 176);
}

/// With the strip at its first column, no column reaches past the left
/// edge, so a pointer held there starts no hold and scrolls nothing. The
/// same goes for the right edge with the strip at its last column. The other
/// edge, with columns past it, still starts a hold.
#[test]
fn an_edge_with_no_column_past_it_holds_nothing() {
    for (last, id, title_bar, idle, other) in [
        (false, 0, FIRST_TITLE_BAR, LEFT_EDGE, RIGHT_EDGE),
        (true, 5, LAST_TITLE_BAR, RIGHT_EDGE, LEFT_EDGE),
    ] {
        let mut harness = six_dragged_columns(last);
        let x = strip_x(harness.world());
        drag_and_hold(&mut harness, id, title_bar, idle);
        assert_dragging(harness.world());
        harness.advance(ms(1200));
        assert_eq!(edge_holds(harness.world()), 0, "at {idle:?}");
        assert_eq!(strip_x(harness.world()), x, "at {idle:?}");

        move_pointer(&mut harness, other);
        assert_eq!(edge_holds(harness.world()), 1, "at {other:?}");
    }
}
