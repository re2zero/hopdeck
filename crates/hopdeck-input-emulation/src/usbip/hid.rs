//! Simulated HID devices for the USB/IP loopback injection backend.
//!
//! One composite USB device exposes two boot-compatible HID interfaces:
//! a keyboard (interface 0) and a mouse (interface 1). Both handlers are
//! snapshot-based: the host polls the interrupt-IN endpoint and receives
//! the *current* state, so held keys auto-repeat via the OS and there are
//! no event queues to overflow. Control (ep0) class requests are
//! dispatched by `setup.index` (wIndex = interface number) because the
//! usbip crate routes interface-recipient requests that way.

use std::{
    any::Any,
    collections::HashSet,
    sync::{Arc, Mutex, MutexGuard},
};

use usbip::{Direction, SetupPacket, UsbEndpoint, UsbInterface, UsbInterfaceHandler};

/// Which interface a handler instance serves. Two instances (one per
/// interface) share the same state Arcs; the role only selects the
/// interface's own HID class descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Role {
    Keyboard,
    Mouse,
}

#[derive(Debug)]
pub(crate) struct KeyboardState {
    /// Pressed HID usage ids, including modifiers (0xE0..=0xE7).
    pub pressed: HashSet<u8>,
}

impl KeyboardState {
    pub fn new() -> Self {
        Self {
            pressed: HashSet::new(),
        }
    }

    /// 8-byte boot-keyboard report: modifier byte, reserved byte, 6 key slots.
    /// Overflowing keys (> 6 non-modifier) are dropped with a warning;
    /// HopDeck's release tracking keeps the state machine honest so the
    /// window is transient.
    fn snapshot(&self) -> [u8; 8] {
        let mut modifier = 0u8;
        let mut keys = [0u8; 6];
        let mut n = 0usize;
        let mut overflow = 0usize;
        for &usage in &self.pressed {
            if (0xE0..=0xE7).contains(&usage) {
                modifier |= 1 << (usage - 0xE0);
            } else if n < 6 {
                keys[n] = usage;
                n += 1;
            } else {
                overflow += 1;
            }
        }
        if overflow > 0 {
            log::warn!("usbip keyboard: dropping {overflow} keys beyond the 6-key rollover");
        }
        keys[..n].sort_unstable();
        [
            modifier, 0, keys[0], keys[1], keys[2], keys[3], keys[4], keys[5],
        ]
    }
}

#[derive(Debug)]
pub(crate) struct MouseState {
    /// Bit 0..=4: left, right, middle, back, forward.
    pub buttons: u8,
    pub accum_x: f64,
    pub accum_y: f64,
    /// Pending wheel ticks, positive = scroll up (HID convention).
    pub wheel: i32,
    /// Pending AC Pan ticks, positive = pan right.
    pub pan: i32,
}

impl MouseState {
    pub fn new() -> Self {
        Self {
            buttons: 0,
            accum_x: 0.0,
            accum_y: 0.0,
            wheel: 0,
            pan: 0,
        }
    }

    /// 7-byte report: buttons, wheel, AC Pan, X (i16 LE), Y (i16 LE).
    /// Relative deltas drain from the accumulator one poll at a time and
    /// are clamped per report to the descriptor's logical range; residue
    /// is delivered by the next poll, so fast motion never gets lost.
    fn take_report(&mut self) -> [u8; 7] {
        let dx = clamp_i16(&mut self.accum_x);
        let dy = clamp_i16(&mut self.accum_y);
        let wheel = take_i8(&mut self.wheel);
        let pan = take_i8(&mut self.pan);
        [
            self.buttons,
            wheel as u8,
            pan as u8,
            (dx & 0xFF) as u8,
            (dx >> 8) as u8,
            (dy & 0xFF) as u8,
            (dy >> 8) as u8,
        ]
    }
}

fn clamp_i16(accum: &mut f64) -> i16 {
    let value = *accum;
    let clamped = value.clamp(-32767.0, 32767.0) as i16;
    *accum = value - f64::from(clamped);
    clamped
}

fn take_i8(accum: &mut i32) -> i8 {
    let clamped = (*accum).clamp(-127, 127);
    *accum -= clamped;
    clamped as i8
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock still holds valid state for a device snapshot; the
    // alternative (unwrap) would turn one panicked reader into dead input.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Shared handler for both HID interfaces of the composite device.
#[derive(Debug)]
pub(crate) struct HidDeviceHandler {
    keyboard: Arc<Mutex<KeyboardState>>,
    mouse: Arc<Mutex<MouseState>>,
    /// This instance's 9-byte HID class descriptor (contains the report
    /// descriptor length), returned by `get_class_specific_descriptor`.
    class_descriptor: [u8; 9],
}

impl HidDeviceHandler {
    pub fn new(
        role: Role,
        keyboard: Arc<Mutex<KeyboardState>>,
        mouse: Arc<Mutex<MouseState>>,
    ) -> Self {
        let report_descriptor = match role {
            Role::Keyboard => KEYBOARD_REPORT_DESCRIPTOR,
            Role::Mouse => MOUSE_REPORT_DESCRIPTOR,
        };
        let len = report_descriptor.len() as u16;
        Self {
            keyboard,
            mouse,
            class_descriptor: [
                0x09, // bLength
                0x21, // bDescriptorType: HID
                0x11,
                0x01, // bcdHID 1.11
                0x00, // bCountryCode
                0x01, // bNumDescriptors
                0x22, // bDescriptorType: Report
                (len & 0xFF) as u8,
                (len >> 8) as u8,
            ],
        }
    }

    fn report_descriptor(&self, interface: u8) -> &'static [u8] {
        if interface == 0 {
            KEYBOARD_REPORT_DESCRIPTOR
        } else {
            MOUSE_REPORT_DESCRIPTOR
        }
    }

    fn handle_control_to_interface(
        &self,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        let interface = (setup.index & 0xFF) as u8;
        match (setup.request_type, setup.request) {
            // GET_DESCRIPTOR(Interface), class request: HID (0x21) or Report (0x22)
            (0b1000_0001, 0x06) => match setup.value >> 8 {
                0x22 => Ok(self.report_descriptor(interface).to_vec()),
                0x21 => Ok(self.class_descriptor_for(interface).to_vec()),
                other => {
                    log::warn!("usbip hid: unhandled descriptor type {other:#04x}");
                    Ok(vec![])
                }
            },
            // SET_IDLE / SET_PROTOCOL / SET_REPORT (keyboard LEDs): accepted, no-op
            (0b0010_0001, 0x0A) | (0b0010_0001, 0x0B) | (0b0010_0001, 0x09) => Ok(vec![]),
            _ => {
                log::warn!(
                    "usbip hid: unhandled control request type={:#04x} request={:#04x}",
                    setup.request_type,
                    setup.request
                );
                Ok(vec![])
            }
        }
    }

    fn class_descriptor_for(&self, interface: u8) -> [u8; 9] {
        // The interface's own descriptor was captured at registration time,
        // but a request may arrive on the peer instance; rebuild by role.
        let len = self.report_descriptor(interface).len() as u16;
        let mut desc = self.class_descriptor;
        desc[7] = (len & 0xFF) as u8;
        desc[8] = (len >> 8) as u8;
        desc
    }
}

impl UsbInterfaceHandler for HidDeviceHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        self.class_descriptor.to_vec()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        _transfer_buffer_length: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if ep.is_ep0() {
            return self.handle_control_to_interface(setup, req);
        }
        match ep.direction() {
            Direction::In if ep.address == KEYBOARD_EP => {
                Ok(lock(&self.keyboard).snapshot().to_vec())
            }
            Direction::In if ep.address == MOUSE_EP => Ok(lock(&self.mouse).take_report().to_vec()),
            _ => Ok(vec![]),
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Keyboard interface endpoint (interrupt IN).
pub(crate) const KEYBOARD_EP: u8 = 0x81;
/// Mouse interface endpoint (interrupt IN).
pub(crate) const MOUSE_EP: u8 = 0x82;

/// Boot-compatible keyboard: 8 modifier bits, 1 reserved byte, 6-key array.
pub(crate) static KEYBOARD_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x05, 0x07, //   Usage Page (Key Codes)
    0x19, 0xE0, //   Usage Min (224)
    0x29, 0xE7, //   Usage Max (231)
    0x15, 0x00, //   Logical Min (0)
    0x25, 0x01, //   Logical Max (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x08, //   Report Count (8)
    0x81, 0x02, //   Input (Data, Var, Abs) — modifier byte
    0x95, 0x01, //   Report Count (1)
    0x75, 0x08, //   Report Size (8)
    0x81, 0x01, //   Input (Const) — reserved byte
    0x95, 0x06, //   Report Count (6)
    0x75, 0x08, //   Report Size (8)
    0x25, 0x65, //   Logical Max (101)
    0x05, 0x07, //   Usage Page (Key Codes)
    0x19, 0x00, //   Usage Min (0)
    0x29, 0x65, //   Usage Max (101)
    0x81, 0x00, //   Input (Data, Array) — key slots
    0xC0, // End Collection
];

/// 5-button mouse: buttons, 8-bit wheel + AC Pan, 16-bit relative X/Y.
pub(crate) static MOUSE_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Buttons)
    0x19, 0x01, //     Usage Min (1)
    0x29, 0x05, //     Usage Max (5)
    0x15, 0x00, //     Logical Min (0)
    0x25, 0x01, //     Logical Max (1)
    0x95, 0x05, //     Report Count (5)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Var, Abs) — button bits
    0x95, 0x01, //     Report Count (1)
    0x75, 0x03, //     Report Size (3)
    0x81, 0x01, //     Input (Const) — padding
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Min (-127)
    0x25, 0x7F, //     Logical Max (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Var, Rel)
    0x05, 0x0C, //     Usage Page (Consumer)
    0x0A, 0x38, 0x02, // Usage (AC Pan)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Var, Rel)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x16, 0x01, 0x80, // Logical Min (-32767)
    0x26, 0xFF, 0x7F, // Logical Max (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x06, //     Input (Data, Var, Rel)
    0xC0, //   End Collection
    0xC0, // End Collection
];

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(role: Role) -> HidDeviceHandler {
        HidDeviceHandler::new(
            role,
            Arc::new(Mutex::new(KeyboardState::new())),
            Arc::new(Mutex::new(MouseState::new())),
        )
    }

    fn setup(request_type: u8, request: u8, value: u16, index: u16) -> SetupPacket {
        SetupPacket {
            request_type,
            request,
            value,
            index,
            length: 0,
        }
    }

    fn mouse_ep(address: u8) -> UsbEndpoint {
        UsbEndpoint {
            address,
            attributes: 0x03,
            max_packet_size: 8,
            interval: 2,
        }
    }

    #[test]
    fn class_descriptor_carries_report_length() {
        let kb = handler(Role::Keyboard);
        let desc = kb.get_class_specific_descriptor();
        assert_eq!(desc.len(), 9);
        assert_eq!(&desc[0..2], &[0x09, 0x21]);
        let len = u16::from_le_bytes([desc[7], desc[8]]);
        assert_eq!(len as usize, KEYBOARD_REPORT_DESCRIPTOR.len());

        let mouse = handler(Role::Mouse);
        let len = {
            let desc = mouse.get_class_specific_descriptor();
            u16::from_le_bytes([desc[7], desc[8]])
        };
        assert_eq!(len as usize, MOUSE_REPORT_DESCRIPTOR.len());
    }

    #[test]
    fn control_requests_dispatch_by_windex() {
        let mut h = handler(Role::Keyboard); // a keyboard-role instance must serve both
        // Report descriptor of the mouse interface (wIndex = 1)
        let resp = h
            .handle_urb(
                &test_interface(),
                mouse_ep(0x00),
                MOUSE_REPORT_DESCRIPTOR.len() as u32,
                setup(0b1000_0001, 0x06, 0x2200, 1),
                &[],
            )
            .unwrap();
        assert_eq!(resp.as_slice(), MOUSE_REPORT_DESCRIPTOR);

        // SET_PROTOCOL on the keyboard interface is a no-op
        let resp = h
            .handle_urb(
                &test_interface(),
                mouse_ep(0x00),
                0,
                setup(0b0010_0001, 0x0B, 0, 0),
                &[],
            )
            .unwrap();
        assert!(resp.is_empty());
    }

    #[test]
    fn keyboard_snapshot_is_boot_layout_and_sorted() {
        let keyboard = Arc::new(Mutex::new(KeyboardState::new()));
        let mut h = HidDeviceHandler::new(
            Role::Keyboard,
            keyboard.clone(),
            Arc::new(Mutex::new(MouseState::new())),
        );
        {
            let mut state = lock(&keyboard);
            // insert unsorted on purpose
            state.pressed.insert(0x0C); // I
            state.pressed.insert(0x04); // A
            state.pressed.insert(0xE1); // LeftShift
            state.pressed.insert(0x09); // F
        }
        let report = h
            .handle_urb(
                &test_interface(),
                mouse_ep(KEYBOARD_EP),
                8,
                setup(0, 0, 0, 0),
                &[],
            )
            .unwrap();
        assert_eq!(report.len(), 8);
        assert_eq!(report[0], 0b0000_0010); // LeftShift bit
        assert_eq!(&report[2..5], &[0x04, 0x09, 0x0C]); // sorted usages
        assert!(report[5..].iter().all(|&b| b == 0));
    }

    #[test]
    fn keyboard_snapshot_clears_on_release() {
        let keyboard = Arc::new(Mutex::new(KeyboardState::new()));
        keyboard.lock().unwrap().pressed.insert(0x04);
        let mut h = HidDeviceHandler::new(
            Role::Keyboard,
            keyboard.clone(),
            Arc::new(Mutex::new(MouseState::new())),
        );
        keyboard.lock().unwrap().pressed.clear();
        let report = h
            .handle_urb(
                &test_interface(),
                mouse_ep(KEYBOARD_EP),
                8,
                setup(0, 0, 0, 0),
                &[],
            )
            .unwrap();
        assert!(report.iter().all(|&b| b == 0));
    }

    #[test]
    fn mouse_report_deltas_drain_with_clamping() {
        let mouse = Arc::new(Mutex::new(MouseState::new()));
        let mut h = HidDeviceHandler::new(
            Role::Mouse,
            Arc::new(Mutex::new(KeyboardState::new())),
            mouse.clone(),
        );
        {
            let mut state = lock(&mouse);
            state.buttons = 0b0000_0001; // left button
            state.accum_x = 500.0;
            state.accum_y = -100_000.0; // overflows one report
            state.wheel = -2; // two ticks down, both fit in one report
        }
        let report = h
            .handle_urb(
                &test_interface(),
                mouse_ep(MOUSE_EP),
                8,
                setup(0, 0, 0, 0),
                &[],
            )
            .unwrap();
        assert_eq!(report.len(), 7);
        assert_eq!(report[0], 0x01);
        assert_eq!(i8::from_le_bytes([report[1]]), -2);
        let dx = i16::from_le_bytes([report[3], report[4]]);
        let dy = i16::from_le_bytes([report[5], report[6]]);
        assert_eq!(dx, 500);
        assert_eq!(dy, -32767);
        // residue survives for the next poll
        let state = lock(&mouse);
        assert!((state.accum_y - (-67233.0)).abs() < 1e-6);
        assert_eq!(state.wheel, 0);
        drop(state);

        // a burst beyond one report's range drains in 127-tick chunks
        lock(&mouse).wheel = -200;
        let report = h
            .handle_urb(
                &test_interface(),
                mouse_ep(MOUSE_EP),
                8,
                setup(0, 0, 0, 0),
                &[],
            )
            .unwrap();
        assert_eq!(i8::from_le_bytes([report[1]]), -127);
        assert_eq!(lock(&mouse).wheel, -73);
    }

    #[test]
    fn mouse_report_when_idle_is_zero_motion() {
        let mut h = handler(Role::Mouse);
        let report = h
            .handle_urb(
                &test_interface(),
                mouse_ep(MOUSE_EP),
                8,
                setup(0, 0, 0, 0),
                &[],
            )
            .unwrap();
        assert_eq!(report, [0u8; 7]);
    }

    fn test_interface() -> UsbInterface {
        UsbInterface {
            interface_class: 0x03,
            interface_subclass: 1,
            interface_protocol: 1,
            endpoints: vec![],
            string_interface: 0,
            class_specific_descriptor: vec![],
            handler: Arc::new(Mutex::new(Box::new(handler(Role::Keyboard)))),
        }
    }
}
