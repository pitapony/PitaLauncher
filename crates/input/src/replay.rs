//! A scripted [`InputSource`] for tests and the replay fixtures.
//!
//! The plan's testing note is explicit: controller tests use a replay source,
//! never a real device. The same type is what the RSX snapshot tests will feed,
//! so a recorded session and a unit test are the same code path.

use std::collections::VecDeque;

use crate::source::{Device, InputSource, RawEvent};

/// A source that yields a fixed list of events and then nothing.
#[derive(Debug, Clone, Default)]
pub struct ReplaySource {
    events: VecDeque<RawEvent>,
    devices: Vec<Device>,
}

impl ReplaySource {
    /// A source over a scripted event list, with no devices reported.
    pub fn new(events: impl IntoIterator<Item = RawEvent>) -> Self {
        Self {
            events: events.into_iter().collect(),
            devices: Vec::new(),
        }
    }

    /// A source that reports a device list (as if the pads were connected at
    /// startup) and then yields the scripted events.
    pub fn with_devices(devices: Vec<Device>, events: impl IntoIterator<Item = RawEvent>) -> Self {
        Self {
            events: events.into_iter().collect(),
            devices,
        }
    }

    /// Append an event to the script.
    pub fn push(&mut self, event: RawEvent) {
        self.events.push_back(event);
    }

    /// How many events are left.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl InputSource for ReplaySource {
    fn poll(&mut self) -> Option<RawEvent> {
        self.events.pop_front()
    }

    fn devices(&self) -> Vec<Device> {
        self.devices.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::Key;
    use crate::source::DeviceId;

    #[test]
    fn a_script_is_yielded_in_order_and_then_ends() {
        let mut source = ReplaySource::new([
            RawEvent::key(Key::ArrowDown, true),
            RawEvent::Disconnected(DeviceId(0)),
        ]);
        assert_eq!(source.poll(), Some(RawEvent::key(Key::ArrowDown, true)));
        assert_eq!(source.poll(), Some(RawEvent::Disconnected(DeviceId(0))));
        assert_eq!(source.poll(), None);
        assert!(source.is_empty());
    }

    #[test]
    fn reported_devices_are_available_before_any_event() {
        let device = Device::new(DeviceId(1), "Pad", "guid-1");
        let source = ReplaySource::with_devices(vec![device.clone()], []);
        assert_eq!(source.devices(), vec![device]);
    }
}
