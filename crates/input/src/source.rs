//! The hardware boundary: devices and the raw events a source yields.

use serde::{Deserialize, Serialize};

/// Stable identity of a connected device for the life of one [`InputSource`].
///
/// It is the source's own numbering (gilrs' `GamepadId`), not a SDL GUID: the
/// same physical pad reconnects under a new id, which is exactly why a
/// [`Device`] carries its `guid` separately — the [`InputMap`](crate::InputMap)
/// is keyed by the GUID so a remap survives a reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub u64);

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "device-{}", self.0)
    }
}

/// A connected input device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    /// The human-readable name, for the remap UI.
    pub name: String,
    /// The stable per-device identity (`gilrs`'s SDL GUID, hex), the key an
    /// [`InputMap`](crate::InputMap) is stored under.
    pub guid: String,
    /// Whether this is a gamepad rather than some other evdev node. Non-gamepads
    /// are still reported so the remap UI can say "seen, but not a controller";
    /// they never produce actions.
    pub is_gamepad: bool,
}

impl Device {
    pub fn new(id: DeviceId, name: impl Into<String>, guid: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            guid: guid.into(),
            is_gamepad: true,
        }
    }
}

/// A device's power supply state.
///
/// Mirrors `gilrs`'s `PowerInfo` without leaking it, and folds the two
/// battery-bearing cases into one so the overlay can show a percentage plus
/// whether it is charging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Battery {
    /// Power status could not be determined.
    Unknown,
    /// The device has no battery (wired).
    Wired,
    /// Running on battery, `percent` in `0..=100`.
    Discharging { percent: u8 },
    /// Charging, `percent` in `0..=100`.
    Charging { percent: u8 },
    /// Plugged in and full.
    Charged,
}

impl Battery {
    /// The charge percentage, when there is a battery to report one for.
    pub const fn percent(self) -> Option<u8> {
        match self {
            Self::Discharging { percent } | Self::Charging { percent } => Some(percent),
            _ => None,
        }
    }

    /// Whether the device is plugged in.
    pub const fn is_charging(self) -> bool {
        matches!(self, Self::Charging { .. } | Self::Charged | Self::Wired)
    }
}

/// One raw event, before any action mapping.
#[derive(Debug, Clone, PartialEq)]
pub enum RawEvent {
    /// A device appeared.
    Connected(Device),
    /// A device went away.
    Disconnected(DeviceId),
    /// A gamepad button changed state.
    Button {
        device: DeviceId,
        button: super::Button,
        pressed: bool,
    },
    /// A gamepad axis changed value.
    Axis {
        device: DeviceId,
        axis: super::Axis,
        value: f32,
    },
    /// A keyboard key changed state. Keys carry no device: there is one keyboard.
    Key { key: super::Key, pressed: bool },
    /// A device's power state changed.
    Battery { device: DeviceId, state: Battery },
}

impl RawEvent {
    pub const fn button(device: DeviceId, button: super::Button, pressed: bool) -> Self {
        Self::Button {
            device,
            button,
            pressed,
        }
    }

    pub const fn axis(device: DeviceId, axis: super::Axis, value: f32) -> Self {
        Self::Axis {
            device,
            axis,
            value,
        }
    }

    pub const fn key(key: super::Key, pressed: bool) -> Self {
        Self::Key { key, pressed }
    }

    /// The device this event came from, if it has one.
    pub const fn device(&self) -> Option<DeviceId> {
        match self {
            Self::Connected(device) => Some(device.id),
            Self::Disconnected(id)
            | Self::Button { device: id, .. }
            | Self::Axis { device: id, .. }
            | Self::Battery { device: id, .. } => Some(*id),
            Self::Key { .. } => None,
        }
    }
}

/// A stream of raw input events.
///
/// Polling rather than a callback because the caller owns the event loop and its
/// clock: both shells drain a source on their own tick, and [`Input`](crate::Input)
/// is fed the drained events with an explicit `now`. A [`ReplaySource`](crate::ReplaySource)
/// satisfies the same trait in tests.
pub trait InputSource {
    /// The next event, if one is waiting. Never blocks.
    fn poll(&mut self) -> Option<RawEvent>;

    /// The devices currently connected.
    fn devices(&self) -> Vec<Device>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::Button;

    #[test]
    fn a_device_event_names_its_device_but_a_key_does_not() {
        let id = DeviceId(3);
        assert_eq!(RawEvent::button(id, Button::South, true).device(), Some(id));
        assert_eq!(RawEvent::Disconnected(id).device(), Some(id));
        assert_eq!(
            RawEvent::key(crate::binding::Key::Enter, true).device(),
            None
        );
    }

    #[test]
    fn battery_reports_a_percentage_only_when_there_is_one() {
        assert_eq!(Battery::Discharging { percent: 40 }.percent(), Some(40));
        assert_eq!(Battery::Charging { percent: 80 }.percent(), Some(80));
        assert_eq!(Battery::Wired.percent(), None);
        assert_eq!(Battery::Unknown.percent(), None);
        assert_eq!(Battery::Charged.percent(), None);
        assert!(Battery::Charged.is_charging());
        assert!(Battery::Wired.is_charging());
        assert!(!Battery::Discharging { percent: 10 }.is_charging());
    }
}
