//! Replay fixtures: recorded raw-event streams replayed through [`Input`] and
//! checked against the `Action` sequence they must produce.
//!
//! The plan's testing note is explicit — controller tests use a
//! [`ReplaySource`], never a real device — and a fixture makes that literal: a
//! JSON transcript in `fixtures/input/` names a pad and a list of button, key
//! and axis events, and the expected action transitions. The same
//! [`ReplaySource`] the app's pump uses in tests drives them, so a recorded
//! session and a unit test are one code path.
//!
//! The fixtures pin behaviour a device would otherwise only show on hardware:
//! the pad's face/Start/Mode/bumper buttons, the keyboard's arrows/WASD and its
//! shortcuts, and the radial deadzone plus the dominant-axis direction change
//! on a stick.

use std::time::Instant;

use pitagames_input::{
    Action, ActionEvent, ActionKind, Axis, Button, Device, DeviceId, Input, InputMaps, InputSource,
    Key, RawEvent, ReplaySource,
};
use serde::Deserialize;

/// One recorded event, tagged by kind so a fixture reads like a transcript.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Event {
    Button { button: Button, pressed: bool },
    Key { key: Key, pressed: bool },
    Axis { axis: Axis, value: f32 },
}

/// One expected action transition: the part of an [`ActionEvent`] a fixture
/// pins. A press/release transcript never repeats, so `repeat` is checked once
/// for the whole stream rather than per line.
#[derive(Debug, Deserialize)]
struct Expected {
    action: Action,
    kind: ActionKind,
}

/// A recorded session: the device it came from (absent for the keyboard), the
/// events, and the actions they must produce.
#[derive(Debug, Deserialize)]
struct Fixture {
    #[serde(default)]
    device: Option<Device>,
    events: Vec<Event>,
    expected: Vec<Expected>,
}

impl Fixture {
    fn parse(json: &str) -> Self {
        serde_json::from_str(json).expect("fixture is valid JSON")
    }
}

/// Replay a fixture's events through a fresh [`Input`] and return the actions.
fn replay(fixture: &Fixture) -> Vec<ActionEvent> {
    let device = DeviceId(0);
    let devices: Vec<Device> = fixture.device.iter().cloned().collect();
    let raw: Vec<RawEvent> = fixture
        .events
        .iter()
        .map(|event| match event {
            Event::Button { button, pressed } => RawEvent::button(device, *button, *pressed),
            Event::Key { key, pressed } => RawEvent::key(*key, *pressed),
            Event::Axis { axis, value } => RawEvent::axis(device, *axis, *value),
        })
        .collect();

    let mut source = ReplaySource::with_devices(devices.clone(), raw);
    let mut input = Input::with_devices(InputMaps::new(), devices);
    let now = Instant::now();
    let mut actions = Vec::new();
    while let Some(event) = source.poll() {
        actions.extend(input.handle(&event, now));
    }
    actions
}

/// Assert a fixture replays to exactly the actions it records.
fn assert_fixture(json: &str) {
    let fixture = Fixture::parse(json);
    let actual = replay(&fixture);

    let expected: Vec<(Action, ActionKind)> = fixture
        .expected
        .iter()
        .map(|expected| (expected.action, expected.kind))
        .collect();
    let actual_pairs: Vec<(Action, ActionKind)> = actual
        .iter()
        .map(|event| (event.action, event.kind))
        .collect();
    assert_eq!(actual_pairs, expected);
    assert!(
        actual.iter().all(|event| !event.repeat),
        "a press/release transcript produces no repeat events"
    );
}

#[test]
fn a_recorded_pad_session_replays_to_its_actions() {
    assert_fixture(include_str!("../../../fixtures/input/pad-menu-walk.json"));
}

#[test]
fn a_recorded_keyboard_session_replays_to_its_actions() {
    assert_fixture(include_str!("../../../fixtures/input/keyboard-walk.json"));
}

#[test]
fn a_recorded_stick_session_replays_the_deadzone_and_a_direction_change() {
    assert_fixture(include_str!("../../../fixtures/input/stick-deadzone.json"));
}
