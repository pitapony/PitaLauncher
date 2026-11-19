//! The real [`InputSource`], over `gilrs`.
//!
//! `gilrs` speaks evdev on Linux (through udev/SDL-derived mappings), so this is
//! the only place in the launcher that touches a controller device. Everything
//! else sees [`RawEvent`]s of our own types, which is what keeps the mapper
//! testable on a machine with no gamepad attached.

use std::collections::BTreeMap;

use gilrs::{EventType, Gilrs};

use crate::binding::{Axis, Button};
use crate::source::{Battery, Device, DeviceId, InputSource, RawEvent};

/// An [`InputSource`] backed by a live `gilrs` context.
///
/// One instance owns the device handles for the life of the process. `gamemon`
/// devices that are not gamepads are not surfaced here: `gilrs` already filters
/// to joystick-capable evdev nodes, so every device this yields is a gamepad.
pub struct GilrsSource {
    gilrs: Gilrs,
    power: BTreeMap<DeviceId, Battery>,
}

/// The input backend could not be started.
///
/// `gilrs::Error` is 200+ bytes, so it is boxed rather than returned by value.
#[derive(Debug, thiserror::Error)]
#[error("controller backend: {0}")]
pub struct SourceError(Box<gilrs::Error>);

impl From<gilrs::Error> for SourceError {
    fn from(error: gilrs::Error) -> Self {
        Self(Box::new(error))
    }
}

impl GilrsSource {
    /// Open the input backend. A machine with no controller still succeeds;
    /// devices arrive as [`RawEvent::Connected`] as they are plugged in.
    pub fn new() -> Result<Self, SourceError> {
        Ok(Self::from_gilrs(Gilrs::new()?))
    }

    /// Adopt an already-built `gilrs` context (tests, or a caller that configured
    /// its filter chain).
    pub fn from_gilrs(gilrs: Gilrs) -> Self {
        Self {
            gilrs,
            power: BTreeMap::new(),
        }
    }

    /// The last battery state seen for a device, if any.
    pub fn battery(&self, device: DeviceId) -> Option<Battery> {
        self.power.get(&device).copied()
    }

    /// Emit a battery change for the first pad whose power state moved.
    ///
    /// `gilrs` reports power by polling, not as an event, so this runs before the
    /// event queue every `poll`: a pad that has never been seen has its state
    /// recorded without an event (`Unknown` is noise), and only a real change is
    /// reported.
    fn poll_battery(&mut self) -> Option<RawEvent> {
        let observed: Vec<(DeviceId, Battery)> = self
            .gilrs
            .gamepads()
            .map(|(id, pad)| (device_id(id), battery(pad.power_info())))
            .collect();
        for (device, state) in observed {
            if self.power.get(&device) == Some(&state) {
                continue;
            }
            let first = self.power.insert(device, state).is_none();
            if !first || state != Battery::Unknown {
                return Some(RawEvent::Battery { device, state });
            }
        }
        None
    }

    /// Everything a device event needs to describe the device.
    fn device(&self, id: gilrs::GamepadId) -> Device {
        let pad = self.gilrs.gamepad(id);
        Device {
            id: device_id(id),
            name: pad.name().to_string(),
            guid: guid(pad.uuid()),
            is_gamepad: true,
        }
    }
}

impl InputSource for GilrsSource {
    fn poll(&mut self) -> Option<RawEvent> {
        if let Some(event) = self.poll_battery() {
            return Some(event);
        }
        while let Some(event) = self.gilrs.next_event() {
            let id = device_id(event.id);
            match event.event {
                EventType::ButtonPressed(button, _) => {
                    if let Some(button) = convert_button(button) {
                        return Some(RawEvent::Button {
                            device: id,
                            button,
                            pressed: true,
                        });
                    }
                }
                EventType::ButtonReleased(button, _) => {
                    if let Some(button) = convert_button(button) {
                        return Some(RawEvent::Button {
                            device: id,
                            button,
                            pressed: false,
                        });
                    }
                }
                EventType::AxisChanged(axis, value, _) => {
                    if let Some(axis) = convert_axis(axis) {
                        return Some(RawEvent::Axis {
                            device: id,
                            axis,
                            value,
                        });
                    }
                }
                EventType::Connected => return Some(RawEvent::Connected(self.device(event.id))),
                EventType::Disconnected => {
                    self.power.remove(&id);
                    return Some(RawEvent::Disconnected(id));
                }
                // Analog trigger movement is also an axis change; a bare
                // `ButtonChanged` is a value we do not map, and a `Dropped`
                // event was filtered upstream.
                _ => continue,
            }
        }
        None
    }

    fn devices(&self) -> Vec<Device> {
        self.gilrs
            .gamepads()
            .map(|(id, pad)| Device {
                id: device_id(id),
                name: pad.name().to_string(),
                guid: guid(pad.uuid()),
                is_gamepad: true,
            })
            .collect()
    }
}

fn device_id(id: gilrs::GamepadId) -> DeviceId {
    DeviceId(usize::from(id) as u64)
}

fn guid(uuid: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in uuid {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `gilrs::Button` to ours, resolving the L1/L2 naming swap in one place.
///
/// `None` for a control we have no action for: `gilrs`'s `C`/`Z` (non-Xbox face
/// buttons), `Unknown`, and any variant a future release adds.
fn convert_button(button: gilrs::Button) -> Option<Button> {
    Some(match button {
        gilrs::Button::South => Button::South,
        gilrs::Button::East => Button::East,
        gilrs::Button::West => Button::West,
        gilrs::Button::North => Button::North,
        // gilrs names the *shoulder* button `LeftTrigger` and the *analog
        // trigger* `LeftTrigger2`; our names use the SDL reading.
        gilrs::Button::LeftTrigger => Button::LeftBumper,
        gilrs::Button::LeftTrigger2 => Button::LeftTrigger,
        gilrs::Button::RightTrigger => Button::RightBumper,
        gilrs::Button::RightTrigger2 => Button::RightTrigger,
        gilrs::Button::Select => Button::Select,
        gilrs::Button::Start => Button::Start,
        gilrs::Button::Mode => Button::Mode,
        gilrs::Button::LeftThumb => Button::LeftThumb,
        gilrs::Button::RightThumb => Button::RightThumb,
        gilrs::Button::DPadUp => Button::DPadUp,
        gilrs::Button::DPadDown => Button::DPadDown,
        gilrs::Button::DPadLeft => Button::DPadLeft,
        gilrs::Button::DPadRight => Button::DPadRight,
        _ => return None,
    })
}

/// `gilrs::Axis` to ours. `None` for `Unknown`.
fn convert_axis(axis: gilrs::Axis) -> Option<Axis> {
    Some(match axis {
        gilrs::Axis::LeftStickX => Axis::LeftStickX,
        gilrs::Axis::LeftStickY => Axis::LeftStickY,
        gilrs::Axis::LeftZ => Axis::LeftZ,
        gilrs::Axis::RightStickX => Axis::RightStickX,
        gilrs::Axis::RightStickY => Axis::RightStickY,
        gilrs::Axis::RightZ => Axis::RightZ,
        gilrs::Axis::DPadX => Axis::DpadX,
        gilrs::Axis::DPadY => Axis::DpadY,
        _ => return None,
    })
}

/// `gilrs::PowerInfo` to ours.
fn battery(info: gilrs::PowerInfo) -> Battery {
    match info {
        gilrs::PowerInfo::Unknown => Battery::Unknown,
        gilrs::PowerInfo::Wired => Battery::Wired,
        gilrs::PowerInfo::Discharging(percent) => Battery::Discharging { percent },
        gilrs::PowerInfo::Charging(percent) => Battery::Charging { percent },
        gilrs::PowerInfo::Charged => Battery::Charged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shoulder_and_trigger_buttons_do_not_swap() {
        // gilrs' `LeftTrigger` is the shoulder (SDL `leftshoulder`).
        assert_eq!(
            convert_button(gilrs::Button::LeftTrigger),
            Some(Button::LeftBumper)
        );
        assert_eq!(
            convert_button(gilrs::Button::LeftTrigger2),
            Some(Button::LeftTrigger)
        );
        assert_eq!(
            convert_button(gilrs::Button::RightTrigger2),
            Some(Button::RightTrigger)
        );
        assert_eq!(convert_button(gilrs::Button::South), Some(Button::South));
        assert_eq!(convert_button(gilrs::Button::DPadUp), Some(Button::DPadUp));
        // A control we have no action for is dropped, not guessed at.
        assert_eq!(convert_button(gilrs::Button::Unknown), None);
        assert_eq!(convert_button(gilrs::Button::C), None);
    }

    #[test]
    fn the_axes_keep_their_names() {
        assert_eq!(
            convert_axis(gilrs::Axis::LeftStickY),
            Some(Axis::LeftStickY)
        );
        assert_eq!(convert_axis(gilrs::Axis::RightZ), Some(Axis::RightZ));
        assert_eq!(convert_axis(gilrs::Axis::Unknown), None);
    }

    #[test]
    fn power_states_map_one_for_one() {
        assert_eq!(battery(gilrs::PowerInfo::Unknown), Battery::Unknown);
        assert_eq!(battery(gilrs::PowerInfo::Wired), Battery::Wired);
        assert_eq!(
            battery(gilrs::PowerInfo::Discharging(40)),
            Battery::Discharging { percent: 40 }
        );
        assert_eq!(
            battery(gilrs::PowerInfo::Charging(90)),
            Battery::Charging { percent: 90 }
        );
        assert_eq!(battery(gilrs::PowerInfo::Charged), Battery::Charged);
    }

    #[test]
    fn a_uuid_becomes_its_hex_guid() {
        let uuid = [
            0x03, 0x00, 0x00, 0x00, 0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
            0x07, 0x08,
        ];
        assert_eq!(guid(uuid), "03000000deadbeef0102030405060708");
        assert_eq!(guid([0; 16]), "00000000000000000000000000000000");
    }
}
