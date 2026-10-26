//! Dragging a tiled window by its title bar into a column or to a new column
//! (samanyudas/miri#43).

mod part_c;

use std::time::Duration;

use bevy::math::{IRect, IVec2};
use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::commands::{Command, Direction, Operation, Side};
use crate::ecs::drag::{
    Band, ColumnKind, ColumnSnap, DisplaySnap, Drag, DragEffect, DragFeedback, DragInput,
    DragOverlayApi, DragOverlayHandle, DragState, DropTarget, LayoutSnapshot, OverlayShape,
    PressedWindow, RELEASE_GRACE, Refusal, hit_test, overlay_shape, step,
};
use crate::ecs::layout::{FALLBACK_TOP_BAR_HEIGHT, LayoutStrip};
use crate::ecs::{ActiveWorkspaceMarker, MouseHeldMarker, SpawnWindowTrigger};
use crate::events::{Event, MouseButton};
use crate::manager::{Origin, Size, Window};
use crate::platform::{Modifiers, WinID};

use super::*;

// --- The drag state machine ---

const DISPLAY: u32 = 1;

fn entities(count: usize) -> Vec<Entity> {
    let mut world = World::new();
    (0..count).map(|_| world.spawn_empty().id()).collect()
}

/// A hit test that names where the pointer is, so a test can see which
/// pointer position a target came from.
fn hit_at(_window: Entity, pointer: IVec2, _centre_dx: i32) -> DropTarget {
    DropTarget::NewColumn {
        display: DISPLAY,
        index: usize::try_from(pointer.x).unwrap_or(0),
    }
}

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

const FRAME_RECT: IRect = IRect {
    min: IVec2::new(0, 20),
    max: IVec2::new(400, 768),
};
const PRESS: IVec2 = IVec2::new(100, 30);

fn down(window: Entity, point: IVec2) -> DragInput {
    DragInput::Down {
        point,
        button: MouseButton::Left,
        window: Some(PressedWindow {
            entity: window,
            frame: FRAME_RECT,
            top_bar_height: FALLBACK_TOP_BAR_HEIGHT,
            tiled: true,
            screen_edges: bevy::math::BVec2::FALSE,
        }),
    }
}

fn moved_by(window: Entity, delta: IVec2) -> DragInput {
    DragInput::WindowMoved {
        window,
        frame: IRect::from_corners(FRAME_RECT.min + delta, FRAME_RECT.max + delta),
    }
}

/// Runs `inputs` through the state machine, each 20ms after the last.
fn run(inputs: &[DragInput]) -> (DragState, Vec<DragEffect>) {
    run_from(DragState::Idle, inputs, Duration::ZERO)
}

fn run_from(
    mut state: DragState,
    inputs: &[DragInput],
    start: Duration,
) -> (DragState, Vec<DragEffect>) {
    let mut effects = Vec::new();
    let mut now = start;
    for input in inputs {
        let (next, more) = step(state, *input, now, &hit_at);
        state = next;
        effects.extend(more);
        now += ms(20);
    }
    (state, effects)
}

fn is_pressed(state: &DragState) -> bool {
    matches!(state, DragState::Pressed(_))
}

/// A press that did not become a drag, still holding its window until the
/// button is released.
fn is_unconfirmed(state: &DragState) -> bool {
    matches!(state, DragState::Unconfirmed(_))
}

#[test]
fn a_press_in_the_top_bar_of_a_tiled_window_may_start_a_drag() {
    let w = entities(1)[0];
    let (state, effects) = run(&[down(w, PRESS)]);
    assert!(is_pressed(&state), "{state:?}");
    assert_eq!(effects, Vec::<DragEffect>::new());

    let last_row = IVec2::new(100, FRAME_RECT.min.y + FALLBACK_TOP_BAR_HEIGHT - 1);
    assert!(is_pressed(&run(&[down(w, last_row)]).0));
}

#[test]
fn other_presses_never_start_a_drag() {
    let w = entities(1)[0];
    let below_the_bar = IVec2::new(100, FRAME_RECT.min.y + FALLBACK_TOP_BAR_HEIGHT);
    let floating = DragInput::Down {
        point: PRESS,
        button: MouseButton::Left,
        window: Some(PressedWindow {
            entity: w,
            frame: FRAME_RECT,
            top_bar_height: FALLBACK_TOP_BAR_HEIGHT,
            tiled: false,
            screen_edges: bevy::math::BVec2::FALSE,
        }),
    };
    let right_button = DragInput::Down {
        point: PRESS,
        button: MouseButton::Right,
        window: Some(PressedWindow {
            entity: w,
            frame: FRAME_RECT,
            top_bar_height: FALLBACK_TOP_BAR_HEIGHT,
            tiled: true,
            screen_edges: bevy::math::BVec2::FALSE,
        }),
    };
    let no_window = DragInput::Down {
        point: PRESS,
        button: MouseButton::Left,
        window: None,
    };
    for input in [down(w, below_the_bar), floating, right_button, no_window] {
        assert_eq!(run(&[input]).0, DragState::Idle, "{input:?}");
    }
}

#[test]
fn a_window_following_the_pointer_starts_a_drag() {
    let w = entities(1)[0];
    let (state, effects) = run(&[
        down(w, PRESS),
        DragInput::Dragged {
            point: PRESS + IVec2::new(50, 5),
        },
        moved_by(w, IVec2::new(50, 5)),
    ]);
    let DragState::Dragging(drag) = state else {
        panic!("{state:?}");
    };
    assert_eq!(drag.window, w);
    assert_eq!(drag.target, hit_at(w, PRESS + IVec2::new(50, 5), 0));
    assert_eq!(effects, Vec::<DragEffect>::new());
}

#[test]
fn a_window_move_lagging_behind_the_pointer_still_starts_a_drag() {
    let w = entities(1)[0];
    let (state, _) = run(&[
        down(w, PRESS),
        DragInput::Dragged {
            point: PRESS + IVec2::new(20, 0),
        },
        DragInput::Dragged {
            point: PRESS + IVec2::new(40, 0),
        },
        DragInput::Dragged {
            point: PRESS + IVec2::new(60, 0),
        },
        moved_by(w, IVec2::new(20, 0)),
    ]);
    assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
}

#[test]
fn a_small_jitter_is_not_a_drag() {
    let w = entities(1)[0];
    let jitter = IVec2::new(3, 1);
    let (state, effects) = run(&[
        down(w, PRESS),
        DragInput::Dragged {
            point: PRESS + jitter,
        },
        moved_by(w, jitter),
    ]);
    assert!(is_pressed(&state), "{state:?}");
    assert_eq!(effects, Vec::<DragEffect>::new());

    let (state, effects) = run_from(
        state,
        &[DragInput::Up {
            point: PRESS + jitter,
        }],
        ms(60),
    );
    assert_eq!(state, DragState::Idle);
    assert_eq!(effects, Vec::<DragEffect>::new());
}

#[test]
fn a_resize_is_not_a_drag() {
    let w = entities(1)[0];
    let dragged = DragInput::Dragged {
        point: PRESS + IVec2::new(0, 30),
    };

    let (state, _) = run(&[
        down(w, PRESS),
        dragged,
        DragInput::WindowResized { window: w },
    ]);
    assert!(is_unconfirmed(&state), "a resize notification: {state:?}");

    // Dragging the top edge moves the origin and changes the size together.
    let top_edge = DragInput::WindowMoved {
        window: w,
        frame: IRect::from_corners(FRAME_RECT.min + IVec2::new(0, 30), FRAME_RECT.max),
    };
    let (state, _) = run(&[down(w, PRESS), dragged, top_edge]);
    assert!(
        is_unconfirmed(&state),
        "a move that changes the size: {state:?}"
    );
}

#[test]
fn a_pointer_moving_without_its_window_is_not_a_drag() {
    let w = entities(1)[0];
    let (state, _) = run_from(
        DragState::Idle,
        &[
            down(w, PRESS),
            DragInput::Dragged {
                point: PRESS + IVec2::new(50, 0),
            },
        ],
        Duration::ZERO,
    );
    assert!(is_pressed(&state), "still waiting for the window");

    // Text selection, or reordering tabs inside an app: the pointer keeps
    // moving and the window never does.
    let (state, _) = run_from(
        state,
        &[DragInput::Dragged {
            point: PRESS + IVec2::new(90, 0),
        }],
        ms(300),
    );
    assert!(is_unconfirmed(&state), "{state:?}");
}

#[test]
fn a_window_move_ahead_of_the_pointer_starts_a_drag_once_it_catches_up() {
    let w = entities(1)[0];
    let (state, _) = run(&[
        down(w, PRESS),
        DragInput::Dragged {
            point: PRESS + IVec2::new(20, 0),
        },
        moved_by(w, IVec2::new(40, 0)),
    ]);
    assert!(is_pressed(&state), "{state:?}");

    let (state, _) = run_from(
        state,
        &[DragInput::Dragged {
            point: PRESS + IVec2::new(40, 0),
        }],
        ms(60),
    );
    assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
}

#[test]
fn a_window_moving_away_from_the_pointer_is_not_a_drag() {
    let w = entities(1)[0];
    let (state, _) = run(&[
        down(w, PRESS),
        DragInput::Dragged {
            point: PRESS + IVec2::new(50, 0),
        },
        moved_by(w, IVec2::new(-200, 0)),
    ]);
    assert!(is_pressed(&state), "{state:?}");

    let (state, _) = run_from(
        state,
        &[DragInput::Dragged {
            point: PRESS + IVec2::new(60, 0),
        }],
        ms(300),
    );
    assert!(is_unconfirmed(&state), "{state:?}");
}

#[test]
fn another_window_moving_changes_nothing() {
    let e = entities(2);
    let (state, _) = run(&[
        down(e[0], PRESS),
        DragInput::Dragged {
            point: PRESS + IVec2::new(50, 0),
        },
        moved_by(e[1], IVec2::new(50, 0)),
    ]);
    assert!(is_pressed(&state), "{state:?}");
}

#[test]
fn a_right_press_leaves_a_held_left_gesture_unchanged() {
    let w = entities(1)[0];
    let (pressed, _) = run(&[down(w, PRESS)]);
    let right = DragInput::Down {
        button: MouseButton::Right,
        point: PRESS,
        window: None,
    };
    for state in [pressed, dragging(w)] {
        let (next, effects) = step(state.clone(), right, ms(100), &hit_at);
        assert_eq!(
            next, state,
            "a right press must not replace the left gesture"
        );
        assert!(effects.is_empty(), "a right press must not drop the window");
    }
}

#[test]
fn a_window_move_with_the_pointer_below_the_threshold_is_not_a_drag() {
    let w = entities(1)[0];
    let nudge = PRESS + IVec2::new(1, 0);
    let (state, _) = run(&[
        down(w, PRESS),
        DragInput::Dragged { point: nudge },
        moved_by(w, IVec2::new(5, 0)),
    ]);
    assert!(is_pressed(&state), "{state:?}");

    let (state, effects) = run_from(state, &[DragInput::Up { point: nudge }], ms(60));
    assert_eq!(state, DragState::Idle);
    assert!(effects.is_empty(), "no drop: {effects:?}");
}

#[test]
fn a_delayed_move_uses_the_release_point_despite_later_pointer_input() {
    let w = entities(1)[0];
    let sample = PRESS + IVec2::new(50, 0);
    let release = sample + IVec2::new(5, 0);
    let (state, effects) = run(&[
        down(w, PRESS),
        DragInput::Dragged { point: sample },
        DragInput::Up { point: release },
        DragInput::Down {
            button: MouseButton::Right,
            point: PRESS,
            window: None,
        },
        DragInput::Moved { point: PRESS },
        moved_by(w, sample - PRESS),
    ]);
    assert!(matches!(state, DragState::Settling(_)), "{state:?}");
    assert_eq!(
        effects,
        vec![DragEffect::Drop {
            window: w,
            target: hit_at(w, release, 0)
        }]
    );
}

#[test]
fn a_released_unconfirmed_drag_can_expire_or_be_cancelled() {
    let w = entities(1)[0];
    let sample = PRESS + IVec2::new(50, 0);
    let (released, _) = run(&[
        down(w, PRESS),
        DragInput::Dragged { point: sample },
        DragInput::Up { point: sample },
    ]);
    for input in [
        DragInput::WindowResized { window: w },
        DragInput::WindowGone { window: w },
    ] {
        let (state, effects) = step(released.clone(), input, ms(60), &hit_at);
        assert_eq!(state, DragState::Idle);
        assert_eq!(effects, Vec::<DragEffect>::new());
    }
    let (state, effects) = step(released.clone(), down(w, PRESS), ms(60), &hit_at);
    assert!(is_pressed(&state));
    assert_eq!(effects, Vec::<DragEffect>::new());
    let (state, effects) = step(
        released,
        DragInput::Tick { button_down: false },
        ms(271),
        &hit_at,
    );
    assert_eq!(state, DragState::Idle);
    assert_eq!(effects, Vec::<DragEffect>::new());
}

#[test]
fn a_window_that_follows_only_after_the_confirm_window_is_not_a_drag() {
    let w = entities(1)[0];
    let (state, _) = run(&[
        down(w, PRESS),
        DragInput::Dragged {
            point: PRESS + IVec2::new(50, 0),
        },
    ]);
    assert!(is_pressed(&state), "{state:?}");

    // Text selection: the pointer moves and pauses, and much later the app
    // moves its window by the same amount.
    let (state, _) = run_from(state, &[moved_by(w, IVec2::new(50, 0))], ms(400));
    assert!(is_unconfirmed(&state), "{state:?}");
}

#[test]
fn a_button_read_up_for_the_grace_ends_a_held_press_or_drag() {
    let w = entities(1)[0];
    let up = DragInput::Tick { button_down: false };
    let (pressed, _) = run(&[down(w, PRESS)]);

    let (state, _) = run_from(pressed.clone(), &[up], ms(100));
    assert!(is_pressed(&state), "the button just read up: {state:?}");
    let (state, effects) = run_from(state, &[up], ms(100) + RELEASE_GRACE + ms(1));
    assert_eq!(state, DragState::Idle);
    assert_eq!(effects, Vec::<DragEffect>::new());

    let (state, _) = run_from(dragging(w), &[up], ms(100));
    let (state, effects) = run_from(state, &[up], ms(100) + RELEASE_GRACE + ms(1));
    assert_eq!(state, DragState::Idle);
    assert_eq!(
        effects,
        vec![DragEffect::Drop {
            window: w,
            target: DropTarget::Home,
        }]
    );
}

#[test]
fn the_release_grace_runs_from_when_the_button_first_reads_up() {
    let w = entities(1)[0];
    let up = DragInput::Tick { button_down: false };
    let held = DragInput::Tick { button_down: true };

    // The pointer pauses over a target well past the grace, then the button
    // reads up before the tap delivers the release.
    let (state, effects) = run_from(dragging(w), &[up], ms(600));
    assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
    assert_eq!(effects, Vec::<DragEffect>::new());
    let release = IVec2::new(300, 200);
    let (state, effects) = run_from(state, &[DragInput::Up { point: release }], ms(700));
    assert_eq!(state.settling_window(), Some(w));
    assert_eq!(
        effects,
        vec![DragEffect::Drop {
            window: w,
            target: hit_at(w, release, 0),
        }]
    );

    // A read of the button down starts the grace over.
    let (state, _) = run_from(dragging(w), &[up], ms(600));
    let (state, _) = run_from(state, &[held], ms(700));
    let (state, _) = run_from(state, &[up], ms(800));
    let (state, _) = run_from(state, &[up], ms(799) + RELEASE_GRACE);
    assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
}

#[test]
fn a_still_hold_with_the_button_down_ends_a_press_only_at_its_limit() {
    let w = entities(1)[0];
    let held = DragInput::Tick { button_down: true };
    let (pressed, _) = run(&[down(w, PRESS)]);
    let much_later = Duration::from_secs(60);

    let (state, _) = run_from(pressed.clone(), &[held], Duration::from_secs(9));
    assert!(is_pressed(&state), "{state:?}");
    // A button state stuck down cannot hold an unconfirmed window forever.
    let (state, _) = run_from(pressed, &[held], much_later);
    assert_eq!(state, DragState::Idle);
    // A confirmed drag held still keeps going.
    let (state, effects) = run_from(dragging(w), &[held], much_later);
    assert!(matches!(state, DragState::Dragging(_)), "{state:?}");
    assert_eq!(effects, Vec::<DragEffect>::new());
}

#[test]
fn a_held_window_that_goes_away_ends_the_drag() {
    let e = entities(2);
    let (pressed, _) = run(&[down(e[0], PRESS)]);
    let (settling, _) = run_from(
        dragging(e[0]),
        &[DragInput::Up {
            point: IVec2::new(300, 200),
        }],
        ms(100),
    );
    for state in [pressed, dragging(e[0]), settling] {
        let (next, _) = run_from(
            state.clone(),
            &[DragInput::WindowGone { window: e[1] }],
            ms(120),
        );
        assert_eq!(next, state, "another window");
        let (next, effects) = run_from(
            state.clone(),
            &[DragInput::WindowGone { window: e[0] }],
            ms(120),
        );
        assert_eq!(next, DragState::Idle, "{state:?}");
        assert!(effects.is_empty(), "{effects:?}");
    }
}

fn dragging(w: Entity) -> DragState {
    run(&[
        down(w, PRESS),
        DragInput::Dragged {
            point: PRESS + IVec2::new(50, 0),
        },
        moved_by(w, IVec2::new(50, 0)),
    ])
    .0
}

#[test]
fn a_drag_follows_the_pointer_and_drops_on_release() {
    let w = entities(1)[0];
    let pointer = IVec2::new(300, 200);
    let (state, effects) = run_from(
        dragging(w),
        &[DragInput::Dragged { point: pointer }],
        ms(100),
    );
    let DragState::Dragging(drag) = &state else {
        panic!("{state:?}");
    };
    assert_eq!(drag.target, hit_at(w, pointer, 0));
    assert_eq!(effects, Vec::<DragEffect>::new());

    let release = IVec2::new(320, 200);
    let (state, effects) = run_from(state, &[DragInput::Up { point: release }], ms(200));
    assert_eq!(
        effects,
        vec![DragEffect::Drop {
            window: w,
            target: hit_at(w, release, 0),
        }]
    );
    assert_eq!(state.settling_window(), Some(w));
}

#[test]
fn a_lost_release_returns_the_window() {
    let w = entities(1)[0];
    let (state, effects) = run_from(
        dragging(w),
        &[DragInput::Moved {
            point: IVec2::new(300, 200),
        }],
        ms(100),
    );
    assert_eq!(state, DragState::Idle);
    assert_eq!(
        effects,
        vec![DragEffect::Drop {
            window: w,
            target: DropTarget::Home,
        }]
    );
}

#[test]
fn settling_ends_at_its_deadline_or_at_the_next_press() {
    let w = entities(1)[0];
    let (settling, _) = run_from(
        dragging(w),
        &[DragInput::Up {
            point: IVec2::new(300, 200),
        }],
        ms(100),
    );
    let still = DragInput::Moved {
        point: IVec2::new(300, 200),
    };

    let (state, _) = run_from(settling.clone(), &[still], ms(200));
    assert_eq!(state.settling_window(), Some(w), "before the deadline");
    let (state, _) = run_from(settling.clone(), &[still], ms(800));
    assert_eq!(state, DragState::Idle, "after the deadline");
    let (state, _) = run_from(settling, &[down(w, PRESS)], ms(200));
    assert!(is_pressed(&state), "a new press");
}

// --- The drop target ---

const BOUNDS: IRect = IRect {
    min: IVec2::new(0, 0),
    max: IVec2::new(1024, 768),
};
const VIEWPORT: IRect = IRect {
    min: IVec2::new(0, 20),
    max: IVec2::new(1024, 768),
};

fn band(windows: &[Entity], y: std::ops::Range<i32>) -> Band {
    Band {
        windows: windows.to_vec(),
        y,
    }
}

fn column(index: usize, x: std::ops::Range<i32>, bands: Vec<Band>) -> ColumnSnap {
    ColumnSnap {
        index,
        x,
        bands,
        kind: ColumnKind::Tiled,
    }
}

fn snapshot(columns: Vec<ColumnSnap>) -> LayoutSnapshot {
    LayoutSnapshot {
        managed_strip_displays: vec![DisplaySnap {
            id: DISPLAY,
            bounds: BOUNDS,
            viewport: VIEWPORT,
            columns,
        }],
    }
}

/// `A | [B, C] | D | E`, 400 points per column, with E past the right edge.
fn four_columns(e: &[Entity]) -> LayoutSnapshot {
    snapshot(vec![
        column(0, 0..400, vec![band(&[e[0]], 20..768)]),
        column(
            1,
            400..800,
            vec![band(&[e[1]], 20..394), band(&[e[2]], 394..768)],
        ),
        column(2, 800..1200, vec![band(&[e[3]], 20..768)]),
        column(3, 1200..1600, vec![band(&[e[4]], 20..768)]),
    ])
}

fn into(column: usize, slot: usize) -> DropTarget {
    DropTarget::IntoColumn {
        display: DISPLAY,
        column,
        slot,
    }
}

fn new_column(index: usize) -> DropTarget {
    DropTarget::NewColumn {
        display: DISPLAY,
        index,
    }
}

fn hit(snapshot: &LayoutSnapshot, dragged: Entity, x: i32, y: i32, limit: usize) -> DropTarget {
    hit_test(snapshot, dragged, IVec2::new(x, y), 0, |_| limit)
}

#[test]
fn the_middle_half_of_a_column_drops_into_it_between_its_windows() {
    let e = entities(5);
    let snap = four_columns(&e);

    assert_eq!(
        hit(&snap, e[4], 200, 100, 5),
        into(0, 0),
        "above the middle"
    );
    assert_eq!(
        hit(&snap, e[4], 200, 700, 5),
        into(0, 1),
        "below the middle"
    );
    assert_eq!(hit(&snap, e[4], 600, 30, 5), into(1, 0), "above the top");
    assert_eq!(hit(&snap, e[4], 600, 300, 5), into(1, 1), "between");
    assert_eq!(hit(&snap, e[4], 600, 767, 5), into(1, 2), "the bottom");
}

#[test]
fn the_outer_quarters_of_a_column_open_a_new_column_beside_it() {
    let e = entities(5);
    let snap = four_columns(&e);

    assert_eq!(hit(&snap, e[4], 410, 300, 5), new_column(1));
    assert_eq!(hit(&snap, e[4], 499, 300, 5), new_column(1));
    assert_eq!(hit(&snap, e[4], 500, 300, 5), into(1, 1));
    assert_eq!(hit(&snap, e[4], 699, 300, 5), into(1, 1));
    assert_eq!(hit(&snap, e[4], 700, 300, 5), new_column(2));
}

/// A window grabbed near one edge of its top bar puts the pointer in a
/// column's outer quarter while the window's centre is over the middle half,
/// so the centre, not the pointer, picks the zone.
#[test]
fn the_dragged_windows_centre_picks_the_zone_not_the_pointer() {
    let e = entities(5);
    let snap = four_columns(&e);
    let at = |x, centre_dx| hit_test(&snap, e[4], IVec2::new(x, 300), centre_dx, |_| 5);

    assert_eq!(at(790, -190), into(1, 1), "grabbed near the right edge");
    assert_eq!(at(410, 190), into(1, 1), "grabbed near the left edge");
    assert_eq!(at(600, 150), new_column(2), "centre over the right quarter");
    assert_eq!(at(600, -150), new_column(1), "centre over the left quarter");
}

/// The pointer picks the display. A centre still on the display the drag
/// left counts as the nearest point of the pointer's display, so the drop
/// never lands in a column scrolled away past that display's edge.
#[test]
fn a_centre_outside_the_pointers_display_counts_as_its_nearest_edge() {
    const RIGHT: u32 = 2;
    let e = entities(3);
    let right = IRect::new(1024, 0, 2048, 768);
    let snap = LayoutSnapshot {
        managed_strip_displays: vec![
            DisplaySnap {
                id: DISPLAY,
                bounds: BOUNDS,
                viewport: VIEWPORT,
                columns: vec![column(0, 0..1024, vec![band(&[e[0]], 20..768)])],
            },
            DisplaySnap {
                id: RIGHT,
                bounds: right,
                viewport: IRect::new(1024, 20, 2048, 768),
                columns: vec![
                    column(0, 624..1024, vec![band(&[e[1]], 20..768)]),
                    column(1, 1024..1424, vec![band(&[e[2]], 20..768)]),
                ],
            },
        ],
    };

    assert_eq!(
        hit_test(&snap, e[0], IVec2::new(1300, 300), -400, |_| 5),
        DropTarget::NewColumn {
            display: RIGHT,
            index: 1,
        }
    );
}

#[test]
fn empty_space_opens_a_new_column_at_the_nearest_end() {
    let e = entities(3);
    let snap = snapshot(vec![
        column(0, 200..500, vec![band(&[e[0]], 20..768)]),
        column(
            1,
            500..800,
            vec![band(&[e[1]], 20..394), band(&[e[2]], 394..768)],
        ),
    ]);

    assert_eq!(hit(&snap, e[1], 50, 300, 5), new_column(0));
    assert_eq!(hit(&snap, e[1], 900, 300, 5), new_column(2));
}

#[test]
fn a_drop_on_its_own_lone_column_goes_home() {
    let e = entities(5);
    let snap = four_columns(&e);

    for x in [10, 200, 390, 410] {
        assert_eq!(hit(&snap, e[0], x, 300, 5), DropTarget::Home, "x {x}");
    }
    assert_eq!(hit(&snap, e[3], 790, 300, 5), DropTarget::Home, "its gap");
}

#[test]
fn a_drop_in_its_own_stack_reorders_it_and_is_never_refused() {
    let e = entities(5);
    let snap = four_columns(&e);

    assert_eq!(hit(&snap, e[1], 600, 700, 2), into(1, 1));
    assert_eq!(hit(&snap, e[2], 600, 100, 2), into(1, 0));
    assert_eq!(
        hit(&snap, e[1], 410, 300, 2),
        new_column(1),
        "out of its stack"
    );
}

#[test]
fn a_full_column_refuses_a_drop() {
    let e = entities(5);
    let snap = four_columns(&e);

    assert_eq!(
        hit(&snap, e[0], 600, 300, 2),
        DropTarget::Refused {
            reason: Refusal::StackFull,
            display: DISPLAY,
        }
    );
    assert_eq!(hit(&snap, e[0], 600, 300, 3), into(1, 1));
}

#[test]
fn a_fullscreen_column_refuses_a_drop() {
    let e = entities(2);
    let mut snap = snapshot(vec![
        column(0, 0..400, vec![band(&[e[0]], 20..768)]),
        column(1, 400..800, vec![band(&[e[1]], 20..768)]),
    ]);
    snap.managed_strip_displays[0].columns[1].kind = ColumnKind::Fullscreen;

    assert_eq!(
        hit(&snap, e[0], 600, 300, 5),
        DropTarget::Refused {
            reason: Refusal::Fullscreen,
            display: DISPLAY,
        }
    );
}

#[test]
fn a_native_tab_group_counts_as_one_window() {
    let e = entities(4);
    let snap = snapshot(vec![
        column(0, 0..400, vec![band(&[e[0], e[1]], 20..768)]),
        column(
            1,
            400..800,
            vec![band(&[e[2]], 20..394), band(&[e[3]], 394..768)],
        ),
    ]);

    assert_eq!(hit(&snap, e[1], 200, 300, 5), DropTarget::Home);
    assert_eq!(hit(&snap, e[1], 600, 300, 3), into(1, 1));
    assert_eq!(hit(&snap, e[2], 200, 300, 2), into(0, 0));
}

#[test]
fn a_pointer_off_every_display_goes_home() {
    let e = entities(5);
    let snap = four_columns(&e);

    assert_eq!(hit(&snap, e[4], -50, 300, 5), DropTarget::Home);
    assert_eq!(hit(&snap, e[4], 200, 900, 5), DropTarget::Home);
}

// --- The drag overlay ---

/// A drag of `window` with the pointer at `pointer`, whose release would
/// land at `target`.
fn dragging_to(window: Entity, pointer: IVec2, target: DropTarget) -> DragState {
    DragState::Dragging(Drag {
        window,
        pointer,
        centre_dx: 0,
        target,
        button_up_since: None,
    })
}

fn shape(e: &[Entity], dragged: usize, target: DropTarget) -> Option<OverlayShape> {
    let state = dragging_to(e[dragged], IVec2::new(600, 300), target);
    overlay_shape(&state, &four_columns(e))
}

fn line(x: std::ops::Range<i32>, y: std::ops::Range<i32>) -> OverlayShape {
    OverlayShape::Line(IRect::new(x.start, y.start, x.end, y.end))
}

#[test]
fn a_drop_into_a_column_shows_a_line_across_it_at_the_gap() {
    let e = entities(5);

    assert_eq!(
        shape(&e, 4, into(1, 1)),
        Some(line(400..800, 393..396)),
        "between B and C"
    );
}

#[test]
fn a_drop_at_the_top_or_bottom_of_a_column_shows_a_line_inside_the_screen() {
    let e = entities(5);

    assert_eq!(
        shape(&e, 4, into(1, 0)),
        Some(line(400..800, 20..23)),
        "top"
    );
    assert_eq!(
        shape(&e, 4, into(1, 2)),
        Some(line(400..800, 765..768)),
        "bottom"
    );
    assert_eq!(
        shape(&e, 4, into(0, 1)),
        Some(line(0..400, 765..768)),
        "below a lone window"
    );
}

#[test]
fn a_reorder_shows_the_line_between_the_other_windows_of_its_stack() {
    let e = entities(5);

    assert_eq!(
        shape(&e, 1, into(1, 1)),
        Some(line(400..800, 765..768)),
        "B under C"
    );
    assert_eq!(
        shape(&e, 2, into(1, 0)),
        Some(line(400..800, 20..23)),
        "C over B"
    );
}

#[test]
fn a_new_column_shows_a_line_down_the_full_height_of_its_gap() {
    let e = entities(5);

    assert_eq!(
        shape(&e, 4, new_column(1)),
        Some(line(399..402, 20..768)),
        "between A and B"
    );
    assert_eq!(
        shape(&e, 4, new_column(0)),
        Some(line(0..3, 20..768)),
        "before the first column"
    );
}

#[test]
fn a_new_column_on_an_empty_display_shows_a_full_line_inside_its_viewport() {
    let e = entities(5);
    let mut snap = four_columns(&e);
    snap.managed_strip_displays.push(DisplaySnap {
        id: DISPLAY + 1,
        bounds: IRect::new(182, 1486, 2230, 2772),
        viewport: IRect::new(182, 1486, 2230, 2772),
        columns: vec![],
    });
    let pointer = IVec2::new(1206, 1560);
    let target = hit_test(&snap, e[4], pointer, 0, |_| 5);
    assert_eq!(
        target,
        DropTarget::NewColumn {
            display: DISPLAY + 1,
            index: 0,
        }
    );

    assert_eq!(
        overlay_shape(&dragging_to(e[4], pointer, target), &snap),
        Some(line(182..185, 1486..2772))
    );
}

#[test]
fn a_new_column_without_neighbors_on_a_nonempty_display_shows_nothing() {
    let e = entities(5);

    assert_eq!(shape(&e, 4, new_column(5)), None);
}

#[test]
fn a_new_column_in_a_padded_gap_shows_its_line_in_the_middle() {
    let e = entities(3);
    let snap = snapshot(vec![
        column(0, 200..500, vec![band(&[e[0]], 20..768)]),
        column(1, 510..800, vec![band(&[e[1]], 20..768)]),
    ]);
    let at = |index| {
        overlay_shape(
            &dragging_to(e[2], IVec2::new(900, 300), new_column(index)),
            &snap,
        )
    };

    assert_eq!(at(1), Some(line(504..507, 20..768)), "between the columns");
    assert_eq!(
        at(2),
        Some(line(799..802, 20..768)),
        "after the last column"
    );
}

#[test]
fn a_refused_drop_shows_the_refusal_symbol_beside_the_pointer_and_no_line() {
    let e = entities(5);
    let pointer = IVec2::new(600, 300);
    for reason in [Refusal::StackFull, Refusal::Fullscreen] {
        let state = dragging_to(
            e[0],
            pointer,
            DropTarget::Refused {
                reason,
                display: DISPLAY,
            },
        );

        assert_eq!(
            overlay_shape(&state, &four_columns(&e)),
            Some(OverlayShape::Refusal(IRect::new(614, 314, 636, 336))),
            "{reason:?}"
        );
    }
}

#[test]
fn the_overlay_hides_unless_a_drag_would_move_the_window() {
    let e = entities(5);
    let snap = four_columns(&e);
    let w = e[4];
    let settling = run_from(
        dragging(w),
        &[DragInput::Up {
            point: IVec2::new(300, 200),
        }],
        ms(100),
    )
    .0;

    for state in [
        DragState::Idle,
        run(&[down(w, PRESS)]).0,
        dragging_to(w, IVec2::new(600, 300), DropTarget::Home),
        settling,
    ] {
        assert_eq!(overlay_shape(&state, &snap), None, "{state:?}");
    }
}

#[test]
fn a_viewport_too_small_for_a_line_shows_nothing() {
    let e = entities(3);
    for span in 0..3 {
        let mut snap = snapshot(vec![
            column(0, 0..400, vec![band(&[e[0]], 20..20 + span)]),
            column(1, 400..800, vec![band(&[e[1]], 20..20 + span)]),
        ]);
        snap.managed_strip_displays[0].viewport = IRect::new(0, 20, 1024, 20 + span);
        let into_column = dragging_to(e[2], IVec2::new(600, 20), into(1, 0));
        assert_eq!(overlay_shape(&into_column, &snap), None, "{span} pt high");

        snap.managed_strip_displays[0].viewport = IRect::new(400, 20, 400 + span, 768);
        let new_column = dragging_to(e[2], IVec2::new(400, 300), new_column(1));
        assert_eq!(overlay_shape(&new_column, &snap), None, "{span} pt wide");
    }
}

#[test]
fn a_target_on_a_display_the_snapshot_lacks_shows_nothing() {
    let e = entities(5);
    let elsewhere = DropTarget::NewColumn {
        display: DISPLAY + 1,
        index: 1,
    };

    assert_eq!(shape(&e, 4, elsewhere), None);
}

// --- Drops in the running engine ---

/// One harness frame, as `TestHarness::advance` paces them.
const FRAME: Duration = Duration::from_millis(20);
const SETTLE: Duration = Duration::from_secs(2);

fn cg(point: Origin) -> CGPoint {
    CGPoint::new(point.x.into(), point.y.into())
}

fn mouse_down(point: Origin, window_id: Option<WinID>) -> Event {
    Event::MouseDown {
        point: cg(point),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
        window_id,
        clicks: 1,
    }
}

fn mouse_dragged(point: Origin) -> Event {
    Event::MouseDragged {
        point: cg(point),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
    }
}

fn mouse_up(point: Origin) -> Event {
    Event::MouseUp {
        point: cg(point),
        modifiers: Modifiers::empty(),
        button: MouseButton::Left,
    }
}

fn frame(world: &mut World, id: WinID) -> IRect {
    world
        .query::<&Window>()
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window exists")
        .frame()
}

/// The window IDs of each column of the active strip, left to right, each
/// column top to bottom.
fn layout(world: &mut World) -> Vec<Vec<WinID>> {
    let ids = world
        .query::<(Entity, &Window)>()
        .iter(world)
        .map(|(entity, window)| (entity, window.id()))
        .collect::<std::collections::HashMap<_, _>>();
    world
        .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
        .single(world)
        .expect("one active strip")
        .columns()
        .map(|column| column.window_iter().map(|entity| ids[&entity]).collect())
        .collect()
}

fn drag_state(world: &mut World) -> DragState {
    world.resource::<DragState>().clone()
}

/// The point in the top bar of a window at `frame` where a test grabs it:
/// across from its centre, so the pointer and the window's centre pick the
/// same column.
fn grab(frame: IRect) -> Origin {
    Origin::new(frame.center().x, frame.min.y + 10)
}

/// Presses window `id`'s title bar and drags it to `to` in six steps, the way
/// macOS reports it: each pointer move, then the window following one frame
/// later with a `WindowMoved`. Returns the window's frame before the drag.
fn press_and_drag(harness: &mut TestHarness, id: WinID, to: Origin) -> IRect {
    let start = frame(harness.world(), id);
    let press = grab(start);
    press_at(harness, press);
    harness.advance(FRAME);
    let steps = 6;
    for at in 1..=steps {
        let point = press + (to - press) * at / steps;
        harness.world().write_message(mouse_dragged(point));
        harness
            .mock_state
            .os_move_window(id, start.min + (point - press));
        harness.advance(FRAME);
    }
    start
}

/// Presses the left button at `point`: the window server sees it down, and
/// the tap reports the press.
fn press_at(harness: &mut TestHarness, point: Origin) {
    harness.mock_state.set_left_button_down(true);
    let window_id = harness.mock_state.window_at(point);
    harness.world().write_message(mouse_down(point, window_id));
}

fn release(harness: &mut TestHarness, at: Origin) {
    harness.mock_state.set_left_button_down(false);
    harness.world().write_message(mouse_up(at));
    harness.advance(FRAME);
}

/// Drags window `id` by its title bar, drops it at `to`, and lets the layout
/// settle. Returns the window's frame before the drag.
fn title_drag(harness: &mut TestHarness, id: WinID, to: Origin) -> IRect {
    let start = press_and_drag(harness, id, to);
    release(harness, to);
    harness.advance(SETTLE);
    start
}

/// A point in the middle half of window `id`'s column, `y` points down.
fn middle_of(world: &mut World, id: WinID, y: i32) -> Origin {
    let frame = frame(world, id);
    Origin::new(frame.center().x, y)
}

/// Where the pointer goes, `y` points down, so a window at `frame` grabbed at
/// `grab` has its centre over the middle of window `id`'s column.
fn centred_over(world: &mut World, id: WinID, y: i32, frame: IRect, grab: Origin) -> Origin {
    middle_of(world, id, y) - IVec2::new(frame.center().x - grab.x, 0)
}

fn harness_with(config: &str, display_height: i32, windows: i32) -> TestHarness {
    let config = crate::config::Config::try_from(config).expect("config parses");
    let harness = TestHarness::new().with_config(config);
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, display_height),
    );
    let mut harness = harness.with_windows(windows);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);
    harness
}

fn three_columns() -> TestHarness {
    harness_with("", TEST_DISPLAY_HEIGHT, 3)
}

#[test]
fn a_title_bar_press_uses_its_cached_height() {
    for (height, offset, expected) in [(96, 60, true), (32, 40, false)] {
        let mut harness = harness_with("", TEST_DISPLAY_HEIGHT, 1);
        let entity = harness
            .world()
            .query::<(Entity, &Window)>()
            .iter(harness.world())
            .find_map(|(entity, window)| (window.id() == 0).then_some(entity))
            .expect("window exists");
        harness
            .world()
            .entity_mut(entity)
            .insert(crate::ecs::TopBarHeight(height));
        harness
            .mock_state
            .update_window(0, |window| window.top_bar_height = Some(height));
        let start = frame(harness.world(), 0).min + IVec2::new(100, offset);

        press_at(&mut harness, start);
        harness.advance(FRAME);

        let state = drag_state(harness.world());
        assert_eq!(
            is_pressed(&state),
            expected,
            "height {height}, press offset {offset}: {state:?}"
        );
    }
}

fn command(operation: Operation) -> Event {
    Event::Command {
        command: Command::Window(operation),
    }
}

/// Each window's frame top to bottom, checking they stay in one column and
/// neither overlap nor leave a gap.
fn assert_one_column(world: &mut World, ids: &[WinID]) {
    let frames = ids.iter().map(|id| frame(world, *id)).collect::<Vec<_>>();
    for pair in frames.windows(2) {
        assert_eq!(pair[0].min.x, pair[1].min.x, "one column: {frames:?}");
        assert_eq!(pair[0].width(), pair[1].width(), "one width: {frames:?}");
        assert_eq!(
            pair[0].max.y, pair[1].min.y,
            "no overlap or gap: {frames:?}"
        );
    }
}

/// Checks window `id` is back in its slot just right of window `left`, at
/// the size it had before. The strip may have scrolled to show it.
fn assert_back_right_of(world: &mut World, left: WinID, id: WinID, start: IRect) {
    let left = frame(world, left);
    let now = frame(world, id);
    assert_eq!(now.min, IVec2::new(left.max.x, start.min.y), "{now:?}");
    assert_eq!(now.size(), start.size(), "{now:?}");
}

#[test]
fn a_window_dropped_on_a_column_joins_it_where_it_lands() {
    let mut harness = three_columns();
    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);

    let below_the_middle = middle_of(harness.world(), 0, 600);
    title_drag(&mut harness, 2, below_the_middle);

    assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);
    assert_one_column(harness.world(), &[0, 2]);
    crate::assert_focused!(harness.world(), 2);
}

#[test]
fn a_quick_drop_waits_for_its_delayed_move_notification() {
    let mut harness = three_columns();
    let start = frame(harness.world(), 2);
    let press = grab(start);
    let to = middle_of(harness.world(), 0, 600);
    press_at(&mut harness, press);
    harness.advance(FRAME);
    harness.world().write_message(mouse_dragged(to));
    harness.mock_state.update_window(2, |window| {
        window.frame = IRect::from_corners(start.min + to - press, start.max + to - press);
    });
    release(&mut harness, to);
    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);

    harness
        .world()
        .write_message(Event::WindowMoved { window_id: 2 });
    harness.advance(FRAME);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);
    crate::assert_focused!(harness.world(), 2);
}

#[test]
fn a_quick_drop_waits_for_a_move_after_release_within_the_original_deadline() {
    for delay in [FRAME, Duration::from_millis(300)] {
        let mut harness = three_columns();
        let start = frame(harness.world(), 2);
        let press = grab(start);
        let to = middle_of(harness.world(), 0, 600);
        press_at(&mut harness, press);
        harness.advance(FRAME);
        harness.world().write_message(mouse_dragged(to));
        release(&mut harness, to);
        harness.advance(delay);
        harness.mock_state.os_move_window(2, start.min + to - press);
        harness.advance(FRAME);
        harness.advance(FRAME);

        let expected = if delay == FRAME {
            vec![vec![0, 2], vec![1]]
        } else {
            vec![vec![0], vec![1], vec![2]]
        };
        assert_eq!(layout(harness.world()), expected, "move delay {delay:?}");
        harness.advance(SETTLE);
        assert_eq!(layout(harness.world()), expected);
        assert_eq!(drag_state(harness.world()), DragState::Idle);
    }
}

#[test]
fn a_drop_restoring_focus_scrolls_only_as_far_as_needed_with_auto_center() {
    let mut harness = harness_with("[options]\nauto_center = true\n", TEST_DISPLAY_HEIGHT, 3);
    crate::assert_focused!(harness.world(), 0);
    let parked = frame(harness.world(), 2);
    assert!(parked.min.x >= TEST_DISPLAY_WIDTH - 10, "{parked:?}");
    harness.mock_state.raise_window(2);
    let start = parked.min + IVec2::new(2, 10);
    let to = centred_over(harness.world(), 1, 600, parked, start);

    press_at(&mut harness, start);
    harness.advance(FRAME);
    for at in 1..=6 {
        let point = start + (to - start) * at / 6;
        harness.world().write_message(mouse_dragged(point));
        harness
            .mock_state
            .os_move_window(2, parked.min + (point - start));
        harness.advance(FRAME);
    }
    release(&mut harness, to);
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world()), vec![vec![0], vec![1, 2]]);
    crate::assert_focused!(harness.world(), 2);
    assert_eq!(
        frame(harness.world(), 2).max.x,
        TEST_DISPLAY_WIDTH,
        "the drop only scrolls enough to expose the destination column"
    );
}

#[test]
fn a_window_dropped_above_a_column_becomes_its_top_window_at_its_width() {
    let mut harness = three_columns();
    harness
        .mock_state
        .os_resize_window(2, Size::new(600, TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT));
    harness.advance(SETTLE);
    let column_width = frame(harness.world(), 0).width();
    assert_ne!(frame(harness.world(), 2).width(), column_width);

    let above_the_top = middle_of(harness.world(), 0, 30);
    title_drag(&mut harness, 2, above_the_top);

    assert_eq!(layout(harness.world()), vec![vec![2, 0], vec![1]]);
    assert_one_column(harness.world(), &[2, 0]);
    assert_eq!(frame(harness.world(), 2).width(), column_width);
}

#[test]
fn a_window_dropped_on_a_columns_outer_quarter_opens_a_new_column() {
    let mut harness = three_columns();

    let column = frame(harness.world(), 1);
    let east_quarter = Origin::new(column.max.x - 10, 300);
    title_drag(&mut harness, 0, east_quarter);

    assert_eq!(layout(harness.world()), vec![vec![1], vec![0], vec![2]]);
}

#[test]
fn a_stacked_window_dropped_beside_its_stack_leaves_it() {
    let mut harness = three_columns();
    let below_the_middle = middle_of(harness.world(), 0, 600);
    title_drag(&mut harness, 2, below_the_middle);
    assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);

    let column = frame(harness.world(), 0);
    let west_quarter = Origin::new(column.min.x + 10, 300);
    title_drag(&mut harness, 2, west_quarter);

    assert_eq!(layout(harness.world()), vec![vec![2], vec![0], vec![1]]);
}

#[test]
fn a_drop_within_its_own_stack_reorders_it() {
    let mut harness = three_columns();
    let below_the_middle = middle_of(harness.world(), 0, 600);
    title_drag(&mut harness, 2, below_the_middle);
    assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);

    let above_the_top = middle_of(harness.world(), 0, 30);
    title_drag(&mut harness, 2, above_the_top);

    assert_eq!(layout(harness.world()), vec![vec![2, 0], vec![1]]);
    assert_one_column(harness.world(), &[2, 0]);
}

/// Sam's case from #72: the top window of a stack grabbed beside the
/// right end of its top bar and dragged straight down. The pointer stays in
/// the column's right quarter, but the window's centre stays over its
/// middle, so the drop reorders the stack instead of opening a new column.
#[test]
fn a_stack_member_grabbed_near_its_right_edge_and_dragged_down_reorders_it() {
    let mut harness = three_columns();
    let below_the_middle = middle_of(harness.world(), 0, 600);
    title_drag(&mut harness, 2, below_the_middle);
    assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);

    let start = frame(harness.world(), 0);
    let press = Origin::new(start.max.x - 5, start.min.y + 10);
    press_at(&mut harness, press);
    harness.advance(FRAME);
    let to = Origin::new(press.x, TEST_DISPLAY_HEIGHT - 1);
    for at in 1..=6 {
        let point = press + (to - press) * at / 6;
        harness.world().write_message(mouse_dragged(point));
        harness
            .mock_state
            .os_move_window(0, start.min + (point - press));
        harness.advance(FRAME);
    }
    assert!(
        matches!(
            feedback(harness.world()),
            Some(OverlayShape::Line(line)) if line.width() > line.height()
        ),
        "a horizontal insertion line: {:?}",
        feedback(harness.world())
    );
    release(&mut harness, to);
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world()), vec![vec![2, 0], vec![1]]);
    assert_one_column(harness.world(), &[2, 0]);
}

#[test]
fn a_full_column_refuses_a_drop_and_the_window_returns() {
    let mut harness = harness_with("[options]\nstack_limit = 2\n", TEST_DISPLAY_HEIGHT, 3);
    harness.run(vec![
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0, 1], vec![2]]);

    let into_the_stack = middle_of(harness.world(), 0, 600);
    let start = title_drag(&mut harness, 2, into_the_stack);

    assert_eq!(layout(harness.world()), vec![vec![0, 1], vec![2]]);
    assert_back_right_of(harness.world(), 0, 2, start);
}

#[test]
fn a_drop_on_its_own_column_returns_the_window() {
    let mut harness = three_columns();

    let start = frame(harness.world(), 1);
    let nearby = start.min + IVec2::new(140, 200);
    title_drag(&mut harness, 1, nearby);

    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
    assert_eq!(frame(harness.world(), 1), start, "back in its place");
}

#[test]
fn a_lost_release_in_the_engine_returns_the_window() {
    let mut harness = three_columns();

    let into_the_column = middle_of(harness.world(), 0, 600);
    let start = press_and_drag(&mut harness, 2, into_the_column);
    assert!(matches!(
        drag_state(harness.world()),
        DragState::Dragging(_)
    ));
    harness.world().write_message(Event::MouseMoved {
        point: cg(into_the_column),
        modifiers: Modifiers::empty(),
    });
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
    assert_back_right_of(harness.world(), 1, 2, start);
}

#[test]
fn a_small_jitter_is_not_a_drag_in_the_engine() {
    let mut harness = three_columns();

    let start = frame(harness.world(), 2);
    let press = grab(start);
    let jitter = press + IVec2::new(3, 1);
    press_at(&mut harness, press);
    harness.advance(FRAME);
    harness.world().write_message(mouse_dragged(jitter));
    harness
        .mock_state
        .os_move_window(2, start.min + IVec2::new(3, 1));
    harness.advance(FRAME);
    harness.advance(FRAME);
    assert!(matches!(drag_state(harness.world()), DragState::Pressed(_)));
    release(&mut harness, jitter);

    assert_eq!(drag_state(harness.world()), DragState::Idle, "no drop");
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
}

#[test]
fn a_resize_from_the_top_edge_is_not_a_drag() {
    let mut harness = three_columns();

    let start = frame(harness.world(), 2);
    let press = start.min + IVec2::new(100, 2);
    press_at(&mut harness, press);
    harness.advance(FRAME);
    let to = middle_of(harness.world(), 0, 300);
    harness.world().write_message(mouse_dragged(to));
    harness.mock_state.os_move_window(2, start.min.with_y(300));
    harness
        .mock_state
        .os_resize_window(2, start.size().with_y(start.max.y - 300));
    harness.advance(FRAME);
    harness.advance(FRAME);
    assert!(is_unconfirmed(&drag_state(harness.world())));
    release(&mut harness, to);
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
}

#[test]
fn a_pointer_drag_that_leaves_the_window_still_is_not_a_drag() {
    let mut harness = three_columns();

    let start = frame(harness.world(), 2);
    let press = grab(start);
    press_at(&mut harness, press);
    harness.advance(FRAME);
    let to = middle_of(harness.world(), 0, 600);
    for at in 1..=20 {
        let point = press + (to - press) * at / 20;
        harness.world().write_message(mouse_dragged(point));
        harness.advance(FRAME);
    }
    assert!(is_unconfirmed(&drag_state(harness.world())));
    release(&mut harness, to);
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
}

/// Gives window `leader` a native tab, window `tab`, sharing its frame.
fn add_native_tab(harness: &mut TestHarness, leader: WinID, tab: WinID) {
    let frame = frame(harness.world(), leader);
    let window = harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, tab, frame);
    harness.mock_state.window_visible(leader, false);
    harness.world().trigger(SpawnWindowTrigger(vec![window]));
    harness.advance(SETTLE);
}

#[test]
fn a_native_tab_group_moves_as_one() {
    let mut harness = three_columns();
    add_native_tab(&mut harness, 2, 3);
    let tabs = layout(harness.world())[2].clone();
    assert_eq!(tabs.len(), 2, "one tab group: {tabs:?}");

    let below_the_middle = middle_of(harness.world(), 0, 600);
    title_drag(&mut harness, 3, below_the_middle);

    let columns = layout(harness.world());
    assert_eq!(columns.len(), 2, "{columns:?}");
    assert_eq!(columns[0][0], 0, "{columns:?}");
    assert_eq!(&columns[0][1..], &tabs[..], "{columns:?}");
    assert_eq!(columns[1], vec![1]);
}

/// The tap can hand over a whole gesture in one update, after the window has
/// already moved to where it was released.
#[test]
fn a_gesture_batched_into_one_update_still_drops() {
    let mut harness = three_columns();
    let start = frame(harness.world(), 2);
    let press = grab(start);
    let to = middle_of(harness.world(), 0, 600);

    press_at(&mut harness, press);
    for at in 1..=6 {
        let point = press + (to - press) * at / 6;
        harness.world().write_message(mouse_dragged(point));
        harness
            .mock_state
            .os_move_window(2, start.min + (point - press));
        for event in harness.mock_state.drain_events() {
            harness.world().write_message(event);
        }
    }
    harness.mock_state.set_left_button_down(false);
    harness.world().write_message(mouse_up(to));
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);
}

#[test]
fn a_release_the_tap_never_reports_returns_the_window() {
    let mut harness = three_columns();

    let into_the_column = middle_of(harness.world(), 0, 600);
    let start = press_and_drag(&mut harness, 2, into_the_column);
    assert!(matches!(
        drag_state(harness.world()),
        DragState::Dragging(_)
    ));
    harness.mock_state.set_left_button_down(false);
    harness.advance(SETTLE);

    assert_eq!(drag_state(harness.world()), DragState::Idle);
    assert_eq!(layout(harness.world()), vec![vec![0], vec![1], vec![2]]);
    assert_back_right_of(harness.world(), 1, 2, start);
}

#[test]
fn a_held_window_closing_ends_the_drag() {
    let mut harness = three_columns();

    let into_the_column = middle_of(harness.world(), 0, 600);
    press_and_drag(&mut harness, 2, into_the_column);
    assert!(matches!(
        drag_state(harness.world()),
        DragState::Dragging(_)
    ));
    // The button still reads as down, so only the closed window can end it.
    harness.mock_state.os_close_window(2);
    harness.advance(SETTLE);

    assert_eq!(drag_state(harness.world()), DragState::Idle);
    assert_eq!(layout(harness.world()), vec![vec![0], vec![1]]);
}

/// A window scrolled past the edge sits as a sliver, inset from its slot.
/// Its visible title bar still starts a drag.
#[test]
fn a_parked_window_is_dragged_by_its_visible_title_bar() {
    let mut harness = harness_with("[options]\nsliver_height = 0.5\n", TEST_DISPLAY_HEIGHT, 4);
    let parked = frame(harness.world(), 3);
    assert!(
        parked.min.x >= TEST_DISPLAY_WIDTH - 10 && parked.min.y > TEST_MENUBAR_HEIGHT,
        "window 3 is a sliver at the right edge: {parked:?}"
    );

    // The sliver sits over the right edge of window 2, and is on top.
    harness.mock_state.raise_window(3);
    let start = parked.min + IVec2::new(2, 10);
    let to = centred_over(harness.world(), 0, 600, parked, start);
    press_at(&mut harness, start);
    harness.advance(FRAME);
    for at in 1..=6 {
        let point = start + (to - start) * at / 6;
        harness.world().write_message(mouse_dragged(point));
        harness
            .mock_state
            .os_move_window(3, parked.min + (point - start));
        harness.advance(FRAME);
    }
    assert!(
        matches!(drag_state(harness.world()), DragState::Dragging(_)),
        "{:?}",
        drag_state(harness.world())
    );
    release(&mut harness, to);
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world())[0], vec![0, 3]);
}

/// The tap hands over a parked window's whole gesture in one update. By
/// then the sliver has moved off the press, and the window under the press
/// is its neighbor, so the press must keep the window the tap saw.
#[test]
fn a_batched_drag_of_a_parked_window_moves_that_window() {
    let mut harness = harness_with("[options]\nsliver_height = 0.5\n", TEST_DISPLAY_HEIGHT, 4);
    let parked = frame(harness.world(), 3);
    harness.mock_state.raise_window(3);
    let start = parked.min + IVec2::new(2, 10);
    let to = centred_over(harness.world(), 0, 600, parked, start);

    press_at(&mut harness, start);
    for at in 1..=6 {
        let point = start + (to - start) * at / 6;
        harness.world().write_message(mouse_dragged(point));
        harness
            .mock_state
            .os_move_window(3, parked.min + (point - start));
        for event in harness.mock_state.drain_events() {
            harness.world().write_message(event);
        }
    }
    harness.mock_state.set_left_button_down(false);
    harness.world().write_message(mouse_up(to));
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world())[0], vec![0, 3]);
}

/// The same batched press, with the release in a later update. The click
/// bookkeeping must hold the sliver too. If it held the neighbor the sliver
/// sat over, the release would scroll the strip to show that neighbor.
#[test]
fn a_batched_press_of_a_parked_window_holds_that_window() {
    let mut harness = harness_with("[options]\nsliver_height = 0.5\n", TEST_DISPLAY_HEIGHT, 5);
    let parked = frame(harness.world(), 4);
    harness.mock_state.raise_window(4);
    let start = parked.min + IVec2::new(2, 10);
    let to = centred_over(harness.world(), 0, 600, parked, start);

    press_at(&mut harness, start);
    for at in 1..=6 {
        let point = start + (to - start) * at / 6;
        harness.world().write_message(mouse_dragged(point));
        harness
            .mock_state
            .os_move_window(4, parked.min + (point - start));
        for event in harness.mock_state.drain_events() {
            harness.world().write_message(event);
        }
    }
    harness.advance(FRAME);
    let world = harness.world();
    let held = world
        .query::<&MouseHeldMarker>()
        .iter(world)
        .map(|marker| marker.0)
        .collect::<Vec<_>>();
    let held = held
        .into_iter()
        .filter_map(|entity| world.get::<Window>(entity).map(|window| window.id()))
        .collect::<Vec<_>>();
    assert_eq!(held, vec![4]);

    release(&mut harness, to);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world())[0], vec![0, 4]);
}

#[test]
fn a_release_after_a_pause_still_drops() {
    let mut harness = three_columns();
    let into_the_column = middle_of(harness.world(), 0, 600);
    press_and_drag(&mut harness, 2, into_the_column);
    harness.advance(Duration::from_millis(500));

    // The window server sees the button up a frame before the tap delivers
    // the release.
    harness.mock_state.set_left_button_down(false);
    harness.advance(FRAME);
    release(&mut harness, into_the_column);
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);
}

#[test]
fn a_drag_held_still_with_the_button_down_still_drops() {
    let mut harness = three_columns();
    let into_the_column = middle_of(harness.world(), 0, 600);
    press_and_drag(&mut harness, 2, into_the_column);
    harness.advance(Duration::from_secs(12));
    assert!(matches!(
        drag_state(harness.world()),
        DragState::Dragging(_)
    ));

    release(&mut harness, into_the_column);
    harness.advance(SETTLE);

    assert_eq!(layout(harness.world()), vec![vec![0, 2], vec![1]]);
}

// --- The drag overlay in the running engine ---

fn feedback(world: &mut World) -> Option<OverlayShape> {
    world.resource::<DragFeedback>().shape
}

#[test]
fn a_drag_over_a_column_shows_a_line_until_the_drop() {
    let mut harness = three_columns();
    let target = frame(harness.world(), 0);

    let above_the_top = middle_of(harness.world(), 0, 30);
    press_and_drag(&mut harness, 2, above_the_top);

    assert_eq!(
        feedback(harness.world()),
        Some(OverlayShape::Line(IRect::new(
            target.min.x,
            target.min.y,
            target.max.x,
            target.min.y + 3,
        )))
    );

    release(&mut harness, above_the_top);
    assert_eq!(feedback(harness.world()), None, "hidden on drop");
}

#[test]
fn a_drag_over_an_outer_quarter_shows_a_vertical_line() {
    let mut harness = three_columns();
    let column = frame(harness.world(), 1);

    let east_quarter = Origin::new(column.max.x - 10, 300);
    press_and_drag(&mut harness, 0, east_quarter);

    let Some(OverlayShape::Line(line)) = feedback(harness.world()) else {
        panic!("{:?}", feedback(harness.world()));
    };
    assert_eq!(line.width(), 3, "{line:?}");
    assert_eq!(line.min.x, column.max.x - 1, "{line:?}");
    assert_eq!(line.min.y, column.min.y, "full height: {line:?}");

    release(&mut harness, east_quarter);
    assert_eq!(feedback(harness.world()), None);
}

#[test]
fn a_drag_over_a_full_column_shows_the_refusal_symbol_until_the_return() {
    let mut harness = harness_with("[options]\nstack_limit = 2\n", TEST_DISPLAY_HEIGHT, 3);
    harness.run(vec![
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);

    let into_the_stack = middle_of(harness.world(), 0, 600);
    press_and_drag(&mut harness, 2, into_the_stack);

    let corner = into_the_stack + IVec2::new(14, 14);
    assert_eq!(
        feedback(harness.world()),
        Some(OverlayShape::Refusal(IRect::from_corners(
            corner,
            corner + IVec2::splat(22),
        )))
    );

    release(&mut harness, into_the_stack);
    assert_eq!(feedback(harness.world()), None, "hidden on the return");
}

#[test]
fn a_lost_release_hides_the_overlay() {
    let mut harness = three_columns();
    let into_the_column = middle_of(harness.world(), 0, 600);
    press_and_drag(&mut harness, 2, into_the_column);
    assert!(feedback(harness.world()).is_some());

    harness.world().write_message(Event::MouseMoved {
        point: cg(into_the_column),
        modifiers: Modifiers::empty(),
    });
    harness.advance(FRAME);

    assert_eq!(feedback(harness.world()), None);
}

#[test]
fn a_release_the_tap_never_reports_hides_the_overlay() {
    let mut harness = three_columns();
    let into_the_column = middle_of(harness.world(), 0, 600);
    press_and_drag(&mut harness, 2, into_the_column);
    assert!(feedback(harness.world()).is_some());

    harness.mock_state.set_left_button_down(false);
    harness.advance(SETTLE);

    assert_eq!(drag_state(harness.world()), DragState::Idle);
    assert_eq!(feedback(harness.world()), None);
}

#[test]
fn a_press_that_never_becomes_a_drag_shows_nothing() {
    let mut harness = three_columns();
    let start = frame(harness.world(), 1);
    press_at(&mut harness, grab(start));
    harness.advance(FRAME);

    assert!(matches!(drag_state(harness.world()), DragState::Pressed(_)));
    assert_eq!(feedback(harness.world()), None);
}

/// One call the engine made to the drag overlay.
#[derive(Clone, Debug, PartialEq)]
enum OverlayCall {
    Line(IRect, (f64, f64, f64)),
    Refusal(IRect),
    Hide,
    Remove,
}

/// The line color with no border color configured.
const WHITE: (f64, f64, f64) = (1.0, 1.0, 1.0);

type OverlayLog = std::rc::Rc<std::cell::RefCell<Vec<OverlayCall>>>;

struct RecordingOverlay(OverlayLog);

impl DragOverlayApi for RecordingOverlay {
    fn show_line(&mut self, line: IRect, color: (f64, f64, f64)) {
        self.0.borrow_mut().push(OverlayCall::Line(line, color));
    }

    fn show_refusal(&mut self, square: IRect) {
        self.0.borrow_mut().push(OverlayCall::Refusal(square));
    }

    fn hide(&mut self) {
        self.0.borrow_mut().push(OverlayCall::Hide);
    }

    fn remove(&mut self) {
        self.0.borrow_mut().push(OverlayCall::Remove);
    }
}

/// Gives the engine a drag overlay that records each call, as the app's
/// `DragOverlay` would receive them.
fn record_overlay(harness: &mut TestHarness) -> OverlayLog {
    let log = OverlayLog::default();
    harness
        .world()
        .insert_non_send(DragOverlayHandle(Box::new(RecordingOverlay(log.clone()))));
    log
}

#[test]
fn a_screen_change_rebuilds_the_overlay_even_for_an_unchanged_shape() {
    let mut harness = three_columns();
    let log = record_overlay(&mut harness);
    let above_the_top = middle_of(harness.world(), 0, 30);
    press_and_drag(&mut harness, 2, above_the_top);
    let Some(OverlayShape::Line(line)) = feedback(harness.world()) else {
        panic!("{:?}", feedback(harness.world()));
    };
    assert_eq!(log.borrow().last(), Some(&OverlayCall::Line(line, WHITE)));
    log.borrow_mut().clear();

    harness.world().write_message(Event::DisplayResized {
        display_id: TEST_DISPLAY_ID,
    });
    harness.advance(FRAME);

    assert_eq!(
        feedback(harness.world()),
        Some(OverlayShape::Line(line)),
        "the shape is unchanged"
    );
    assert_eq!(
        *log.borrow(),
        vec![OverlayCall::Remove, OverlayCall::Line(line, WHITE)],
        "the windows are built again for the new screens"
    );
}

#[test]
fn exiting_during_a_drag_removes_the_overlay() {
    use bevy::app::AppExit;
    use bevy::ecs::system::RunSystemOnce as _;

    let mut harness = three_columns();
    let log = record_overlay(&mut harness);
    let into_the_column = middle_of(harness.world(), 0, 600);
    press_and_drag(&mut harness, 2, into_the_column);
    assert!(matches!(log.borrow().last(), Some(OverlayCall::Line(..))));
    log.borrow_mut().clear();

    // Only the cleanup runs. The full exit schedule also saves state to the
    // real user state file.
    let world = harness.world();
    world.write_message(AppExit::Success);
    world
        .run_system_once(crate::ecs::systems::cleanup_on_exit)
        .expect("running cleanup_on_exit");

    assert_eq!(*log.borrow(), vec![OverlayCall::Remove]);
}

#[test]
fn a_still_drag_leaves_the_drag_state_unchanged() {
    let mut harness = three_columns();
    let into_the_column = middle_of(harness.world(), 0, 600);
    press_and_drag(&mut harness, 2, into_the_column);
    let changed = |world: &mut World| {
        world
            .get_resource_change_ticks::<DragState>()
            .expect("drag state exists")
            .changed
    };
    let before = changed(harness.world());

    // The watchdog checks the held button on every one of these frames.
    for _ in 0..5 {
        harness.advance(FRAME);
    }

    assert!(matches!(
        drag_state(harness.world()),
        DragState::Dragging(_)
    ));
    assert_eq!(changed(harness.world()), before);
}

#[test]
fn a_still_drag_line_follows_its_stack_boundary_when_the_layout_moves_it() {
    let mut harness = three_columns();
    harness.run(vec![
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0, 1], vec![2]]);
    let boundary = |world: &mut World| frame(world, 1).min.y;

    // Just above the boundary between windows 0 and 1, so a drop lands
    // between them.
    let top = boundary(harness.world());
    let between = middle_of(harness.world(), 0, top - 14);
    press_and_drag(&mut harness, 2, between);
    let line_at = |world: &mut World| {
        let column = frame(world, 0);
        let y = boundary(world);
        Some(OverlayShape::Line(IRect::new(
            column.min.x,
            y - 1,
            column.max.x,
            y + 2,
        )))
    };
    assert_eq!(feedback(harness.world()), line_at(harness.world()));

    // The app shrinks the top window while the pointer stays still, and the
    // layout moves the boundary up. No display or drag input changes.
    harness.mock_state.os_resize_window(0, Size::new(400, 200));
    harness.advance(SETTLE);

    assert!(matches!(
        drag_state(harness.world()),
        DragState::Dragging(_)
    ));
    assert_ne!(boundary(harness.world()), top, "the boundary moved");
    assert_eq!(feedback(harness.world()), line_at(harness.world()));
}

#[test]
fn a_still_drag_line_takes_the_new_color_when_the_appearance_changes() {
    const RED: (f64, f64, f64) = (1.0, 0.0, 0.0);
    const GREEN: (f64, f64, f64) = (0.0, 1.0, 0.0);
    let mut harness = harness_with(
        "[decorations.active.border]\ncolor = '#FF0000'\ncolor_night = '#00FF00'\n",
        TEST_DISPLAY_HEIGHT,
        3,
    );
    harness
        .world()
        .insert_resource(crate::ecs::SystemTheme { is_dark: false });
    let log = record_overlay(&mut harness);
    let above_the_top = middle_of(harness.world(), 0, 30);
    press_and_drag(&mut harness, 2, above_the_top);
    let Some(OverlayShape::Line(line)) = feedback(harness.world()) else {
        panic!("{:?}", feedback(harness.world()));
    };
    assert_eq!(log.borrow().last(), Some(&OverlayCall::Line(line, RED)));

    // macOS switches to dark mode while the pointer stays still.
    harness
        .world()
        .resource_mut::<crate::ecs::SystemTheme>()
        .is_dark = true;
    harness.advance(FRAME);

    assert_eq!(log.borrow().last(), Some(&OverlayCall::Line(line, GREEN)));
}

/// The stack `[0, 1] | [2]` with window 2 held still just above the boundary
/// between windows 0 and 1, so a drop lands between them.
fn still_drag_over_a_stack_boundary() -> TestHarness {
    let mut harness = harness_with("", 900, 3);
    harness.run(vec![
        command(Operation::Focus(Direction::First)),
        command(Operation::Focus(Direction::East)),
        command(Operation::Stack(Side::West)),
    ]);
    harness.advance(SETTLE);
    assert_eq!(layout(harness.world()), vec![vec![0, 1], vec![2]]);
    let top = frame(harness.world(), 1).min.y;
    let between = middle_of(harness.world(), 0, top - 14);
    press_and_drag(&mut harness, 2, between);
    harness
}

/// The insertion line at the top of window 1, across its column.
fn line_above_window_1(world: &mut World) -> IRect {
    let column = frame(world, 0);
    let y = frame(world, 1).min.y;
    IRect::new(column.min.x, y - 1, column.max.x, y + 2)
}

#[test]
fn a_still_drag_line_follows_its_stack_boundary_on_the_first_frame_of_a_display_resize() {
    let mut harness = still_drag_over_a_stack_boundary();
    let log = record_overlay(&mut harness);
    assert_eq!(
        feedback(harness.world()),
        Some(OverlayShape::Line(line_above_window_1(harness.world())))
    );
    let top = frame(harness.world(), 1).min.y;
    let display_bottom = |world: &mut World| {
        world
            .query::<&crate::manager::Display>()
            .single(world)
            .expect("one display")
            .bounds()
            .max
            .y
    };
    let before = display_bottom(harness.world());

    // Short enough that the layout must move the boundary up.
    harness
        .mock_state
        .set_display_bounds(TEST_DISPLAY_ID, IRect::new(0, 0, TEST_DISPLAY_WIDTH, 650));
    harness.world().write_message(Event::DisplayConfigured {
        display_id: TEST_DISPLAY_ID,
    });
    // Stops on the frame the engine reconciles the display. A later frame
    // changes the display again, which would redraw stale feedback anyway.
    let mut frames = 0;
    while display_bottom(harness.world()) == before {
        harness.advance(FRAME);
        frames += 1;
        assert!(frames < 100, "the display never reconciled");
    }

    assert!(matches!(
        drag_state(harness.world()),
        DragState::Dragging(_)
    ));
    assert_ne!(frame(harness.world(), 1).min.y, top, "the boundary moved");
    let line = line_above_window_1(harness.world());
    assert_eq!(feedback(harness.world()), Some(OverlayShape::Line(line)));
    assert_eq!(log.borrow().last(), Some(&OverlayCall::Line(line, WHITE)));
}

#[test]
fn a_still_drag_line_takes_the_new_color_when_the_config_reloads() {
    const RED: (f64, f64, f64) = (1.0, 0.0, 0.0);
    const BLUE: (f64, f64, f64) = (0.0, 0.0, 1.0);
    let mut harness = harness_with(
        "[decorations.active.border]\ncolor = '#FF0000'\n",
        TEST_DISPLAY_HEIGHT,
        3,
    );
    let log = record_overlay(&mut harness);
    let above_the_top = middle_of(harness.world(), 0, 30);
    press_and_drag(&mut harness, 2, above_the_top);
    let Some(OverlayShape::Line(line)) = feedback(harness.world()) else {
        panic!("{:?}", feedback(harness.world()));
    };
    assert_eq!(log.borrow().last(), Some(&OverlayCall::Line(line, RED)));

    // The user saves a new border color while the pointer stays still. The
    // engine reloads it the way `refresh_configuration_trigger` does.
    let path = std::env::temp_dir().join(format!("paneru-drag-reload-{}.toml", std::process::id()));
    std::fs::write(&path, "[decorations.active.border]\ncolor = '#0000FF'\n")
        .expect("writing the new config");
    let reloaded = harness
        .world()
        .resource_mut::<crate::config::Config>()
        .reload_config(&path);
    std::fs::remove_file(&path).expect("removing the new config");
    reloaded.expect("reloading the config");
    harness.advance(FRAME);

    assert_eq!(log.borrow().last(), Some(&OverlayCall::Line(line, BLUE)));
}
