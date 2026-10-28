//! A press on an edge slice panel: the engine focuses the window the panel
//! shows and scrolls it into view, then posts the press again at the pressed
//! point once the window is there.

use std::time::Duration;

use bevy::app::{App, PostUpdate, Update};
use bevy::ecs::message::MessageReader;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::{on_message, resource_exists};
use bevy::prelude::{Commands, Entity, Has, Query, Res, Resource, Time};
use objc2_core_foundation::CGPoint;
use tracing::debug;

use crate::commands::focus_with_verification;
use crate::ecs::params::Windows;
use crate::ecs::{PostUpdateSet, RepositionMarker, ResizeMarker, SpawnCommandsExt as _, UpdateSet};
use crate::events::Event;
use crate::manager::{Display, Window, WindowManager, origin_from};
use crate::platform::slice_tap::SlicePress;

/// How long a press waits for its window to reach the pressed point. Focus
/// and the scroll stay when it gives up.
pub(crate) const GIVE_UP: Duration = Duration::from_millis(1500);

#[derive(Resource, Debug)]
pub(crate) struct PendingPress {
    press: SlicePress,
    window: Entity,
    give_up_at: Duration,
}

pub(crate) fn register(app: &mut App) {
    app.add_systems(
        Update,
        reveal_pressed
            .in_set(UpdateSet::Input)
            .run_if(on_message::<Event>),
    )
    .add_systems(
        PostUpdate,
        deliver_press
            .in_set(PostUpdateSet::Ui)
            .run_if(resource_exists::<PendingPress>),
    );
}

/// Focuses and reveals the window of each press, as a focus command does.
/// A later press replaces an earlier one.
fn reveal_pressed(
    mut messages: MessageReader<Event>,
    windows: Windows,
    time: Res<Time>,
    mut commands: Commands,
) {
    for event in messages.read() {
        let Event::SlicePress { press } = event else {
            continue;
        };
        commands.remove_resource::<PendingPress>();
        let Some((_, entity)) = windows.find(press.window_id) else {
            debug!(
                "edge slice press dropped: window {} is gone",
                press.window_id
            );
            continue;
        };
        focus_with_verification(entity, windows.focused().map(|(_, e)| e), &mut commands);
        commands.reshuffle_around(entity);
        commands.insert_resource(PendingPress {
            press: *press,
            window: entity,
            give_up_at: time.elapsed() + GIVE_UP,
        });
        debug!(
            "edge slice press revealing: press {} window {}",
            press.id, press.window_id
        );
    }
}

/// Posts the press once its window rests on a display where the window
/// server sends a press at the pressed point to it.
fn deliver_press(
    pending: Res<PendingPress>,
    windows: Query<(&Window, Has<RepositionMarker>, Has<ResizeMarker>)>,
    displays: Query<&Display>,
    window_manager: Res<WindowManager>,
    time: Res<Time>,
    mut commands: Commands,
) {
    let press = &pending.press;
    if time.elapsed() >= pending.give_up_at {
        debug!(
            "edge slice press gave up: press {} window {} did not reach the pressed point",
            press.id, press.window_id
        );
        commands.remove_resource::<PendingPress>();
        return;
    }
    let Ok((window, moving, resizing)) = windows.get(pending.window) else {
        debug!(
            "edge slice press dropped: window {} is gone",
            press.window_id
        );
        commands.remove_resource::<PendingPress>();
        return;
    };
    if moving || resizing || window.write_pending() {
        return;
    }
    let origin = window.margins().shown(window.frame()).min;
    let point = CGPoint::new(
        f64::from(origin.x) + press.at.x,
        f64::from(origin.y) + press.at.y,
    );
    let on_display = displays
        .iter()
        .any(|display| display.bounds().contains(origin_from(point)));
    if !on_display || window_manager.find_window_at_point(&point).ok() != Some(window.id()) {
        return;
    }
    window_manager.post_press(press, point);
    debug!(
        "edge slice press arrived: press {} window {} at ({:.0}, {:.0})",
        press.id, press.window_id, point.x, point.y
    );
    commands.remove_resource::<PendingPress>();
}
