//! USB/IP HID injection backend.
//!
//! Injects input at the kernel level by exporting a composite virtual HID
//! device (keyboard + mouse) through a loopback USB/IP server; the receiver
//! attaches it once with `vhci-hcd` and the OS input stack consumes the
//! reports like a real USB device. Unlike libei/XTEST paths this does not
//! depend on any compositor cooperation — the Wayland portal can be
//! disabled entirely and injection still works.
//!
//! Requirements on the receiving machine (administrator): the `vhci_hcd`
//! kernel module and a one-time `usbip attach -r 127.0.0.1 -b <busid>`.
//! See `scripts/usbip-receiver-setup.sh`.

pub(crate) mod hid;
pub(crate) mod keymap;

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use input_event::{Event, KeyboardEvent, PointerEvent};
use tokio::task::JoinHandle;
use usbip::{ClassCode, UsbDevice, UsbEndpoint, UsbIpServer};

use super::{Emulation, EmulationHandle};
use crate::error::{EmulationCreationError, EmulationError, UsbipEmulationCreationError};

/// Bus id the composite device exports. `usbip attach` takes `-b` with this
/// value, so it is kept stable and printed at startup.
pub(crate) const BUS_ID: &str = "1-1";

/// Ports probed in order for the loopback server. 3240 first because that
/// is the `usbip` tool's default.
const PROBE_PORTS: std::ops::RangeInclusive<u16> = 3240..=3250;

pub struct UsbipEmulation {
    keyboard: Arc<Mutex<hid::KeyboardState>>,
    mouse: Arc<Mutex<hid::MouseState>>,
    server_addr: SocketAddr,
    server_task: JoinHandle<()>,
}

impl UsbipEmulation {
    pub(crate) fn new() -> Result<Self, EmulationCreationError> {
        let keyboard = Arc::new(Mutex::new(hid::KeyboardState::new()));
        let mouse = Arc::new(Mutex::new(hid::MouseState::new()));

        let device = build_device(keyboard.clone(), mouse.clone());
        let server_addr = probe_port()?;
        let server = Arc::new(UsbIpServer::new_simulated(vec![device]));
        let server_task = tokio::spawn(usbip::server(server_addr, server));

        log::info!(
            "usbip: virtual HID device exported on {server_addr} (busid {BUS_ID}); \
             on this machine run once as root: \
             usbip attach -r 127.0.0.1 -b {BUS_ID}"
        );
        Ok(Self {
            keyboard,
            mouse,
            server_addr,
            server_task,
        })
    }

    #[cfg(test)]
    async fn start_for_test() -> (Self, SocketAddr) {
        // Bind an ephemeral port, drop it and reuse the address: usbip's
        // `server()` binds internally and panics on failure, so the probe
        // pattern is also what `new()` uses (on a fixed port range there).
        use tokio::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let keyboard = Arc::new(Mutex::new(hid::KeyboardState::new()));
        let mouse = Arc::new(Mutex::new(hid::MouseState::new()));
        let device = build_device(keyboard.clone(), mouse.clone());
        let server = Arc::new(UsbIpServer::new_simulated(vec![device]));
        let server_task = tokio::spawn(usbip::server(addr, server));
        (
            Self {
                keyboard,
                mouse,
                server_addr: addr,
                server_task,
            },
            addr,
        )
    }
}

fn probe_port() -> Result<SocketAddr, EmulationCreationError> {
    for port in PROBE_PORTS {
        match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
            // Drop the probe before usbip::server rebinds; the race window is
            // negligible on loopback and its failure surfaces as a task panic.
            Ok(listener) => {
                drop(listener);
                return Ok(SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)));
            }
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(err) => {
                return Err(UsbipEmulationCreationError::Io(err).into());
            }
        }
    }
    Err(UsbipEmulationCreationError::NoPortAvailable.into())
}

fn build_device(
    keyboard: Arc<Mutex<hid::KeyboardState>>,
    mouse: Arc<Mutex<hid::MouseState>>,
) -> UsbDevice {
    let keyboard_handler = Arc::new(Mutex::new(Box::new(hid::HidDeviceHandler::new(
        hid::Role::Keyboard,
        keyboard.clone(),
        mouse.clone(),
    ))
        as Box<dyn usbip::UsbInterfaceHandler + Send>));
    let mouse_handler = Arc::new(Mutex::new(Box::new(hid::HidDeviceHandler::new(
        hid::Role::Mouse,
        keyboard,
        mouse,
    ))
        as Box<dyn usbip::UsbInterfaceHandler + Send>));

    let mut device = UsbDevice::new(1);
    device.bus_num = 1;
    device.bus_id = BUS_ID.to_string();
    device.path = format!("/sys/devices/pci0000:00/usb1/{BUS_ID}");
    device.vendor_id = 0x3553;
    device.product_id = 0x0001;
    device.device_class = 0x00; // class defined per-interface (composite)

    device
        .with_interface(
            ClassCode::HID as u8,
            1, // boot subclass
            1, // keyboard protocol
            Some("Mousehop Keyboard"),
            vec![UsbEndpoint {
                address: hid::KEYBOARD_EP,
                attributes: 0x03, // interrupt
                max_packet_size: 8,
                interval: 10,
            }],
            keyboard_handler,
        )
        .with_interface(
            ClassCode::HID as u8,
            1, // boot subclass
            2, // mouse protocol
            Some("Mousehop Mouse"),
            vec![UsbEndpoint {
                address: hid::MOUSE_EP,
                attributes: 0x03, // interrupt
                max_packet_size: 8,
                interval: 2,
            }],
            mouse_handler,
        )
}

#[async_trait]
impl Emulation for UsbipEmulation {
    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                match keymap::linux_to_usage(key) {
                    Some(usage) => {
                        let mut kb = lock(&self.keyboard);
                        if state == 0 {
                            kb.pressed.remove(&usage);
                        } else {
                            kb.pressed.insert(usage);
                        }
                    }
                    None => {
                        // Media/consumer-page keys are outside the boot
                        // keyboard descriptor; dropping keeps reports clean.
                        log::debug!("usbip: no HID usage for evdev key {key}, dropped");
                    }
                }
            }
            // The OS derives modifier state from the key reports themselves;
            // wayland-style modifier summaries are redundant here.
            Event::Keyboard(KeyboardEvent::Modifiers { .. }) => {}
            Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => {
                let mut mouse = lock(&self.mouse);
                mouse.accum_x += dx;
                mouse.accum_y += dy;
            }
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                const BTN_LEFT: u32 = 0x110;
                const BTN_RIGHT: u32 = 0x111;
                const BTN_MIDDLE: u32 = 0x112;
                const BTN_BACK: u32 = 0x113;
                const BTN_FORWARD: u32 = 0x114;
                let bit = match button {
                    BTN_LEFT => Some(0),
                    BTN_RIGHT => Some(1),
                    BTN_MIDDLE => Some(2),
                    BTN_BACK => Some(3),
                    BTN_FORWARD => Some(4),
                    _ => None,
                };
                if let Some(bit) = bit {
                    let mut mouse = lock(&self.mouse);
                    if state == 0 {
                        mouse.buttons &= !(1 << bit);
                    } else {
                        mouse.buttons |= 1 << bit;
                    }
                } else {
                    log::debug!("usbip: unknown mouse button {button:#x}, dropped");
                }
            }
            // Smooth/touchpad scroll is intentionally unused; discrete
            // 120-unit events carry mouse wheel ticks.
            Event::Pointer(PointerEvent::Axis { .. }) => {}
            Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
                const AXIS_HORIZONTAL: u8 = 1;
                // Input events carry wayland-sign deltas (positive =
                // scroll down / pan left). HID wheel is positive-up, AC Pan
                // is positive-right, hence the wheel sign flip.
                let ticks = (value / 120).clamp(i32::from(i16::MIN), i32::from(i16::MAX));
                let mut mouse = lock(&self.mouse);
                if axis == AXIS_HORIZONTAL {
                    mouse.pan = mouse.pan.saturating_add(ticks);
                } else {
                    mouse.wheel = mouse.wheel.saturating_sub(ticks);
                }
            }
            // Clipboard never reaches backends (intercepted by InputEmulation).
            Event::Clipboard(_) => {}
        }
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, _handle: EmulationHandle) {}

    async fn terminate(&mut self) {
        self.server_task.abort();
        lock(&self.keyboard).pressed.clear();
        {
            let mut mouse = lock(&self.mouse);
            mouse.buttons = 0;
            mouse.accum_x = 0.0;
            mouse.accum_y = 0.0;
            mouse.wheel = 0;
            mouse.pan = 0;
        }
        log::info!(
            "usbip: server on {} stopped; detach on this machine with \
             usbip detach -r 127.0.0.1 -b {BUS_ID}",
            self.server_addr
        );
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    /// `usbip::server` binds inside the spawned task, so the first connect
    /// can race the listener; poll instead of assuming.
    async fn connect_with_retry(addr: SocketAddr) -> TcpStream {
        for _ in 0..200 {
            if let Ok(stream) = TcpStream::connect(addr).await {
                return stream;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("usbip test server never came up at {addr}");
    }

    /// OP_REQ_DEVLIST (usbip crate framing: version 0x0111, code 0x8005,
    /// status u32) followed by OP_REP_DEVLIST (0x0005): 12-byte header
    /// (version, code, status, device count) + 0x138/device + 4/interface.
    #[tokio::test]
    async fn devlist_exposes_the_composite_hid_device() {
        let (mut emulation, addr) = UsbipEmulation::start_for_test().await;

        let mut stream = connect_with_retry(addr).await;
        stream
            .write_all(&[0x01, 0x11, 0x80, 0x05, 0, 0, 0, 0])
            .await
            .unwrap();

        let mut header = [0u8; 12];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(&header[0..2], &[0x01, 0x11], "version");
        assert_eq!(&header[2..4], &[0x00, 0x05], "OP_REP_DEVLIST");
        assert_eq!(&header[4..8], &[0, 0, 0, 0], "status");
        assert_eq!(&header[8..12], &[0, 0, 0, 1], "one device");

        let mut device = vec![0u8; 0x138];
        stream.read_exact(&mut device).await.unwrap();
        // path (256) + busid (32, zero-padded) — busid comes right after path
        let busid = &device[256..256 + BUS_ID.len()];
        assert_eq!(std::str::from_utf8(busid).unwrap(), BUS_ID);
        // interfaces are appended as 4 bytes each (class, sub, protocol, padding)
        let mut interfaces = [0u8; 8];
        stream.read_exact(&mut interfaces).await.unwrap();
        assert_eq!(interfaces[0], ClassCode::HID as u8);
        assert_eq!(interfaces[4], ClassCode::HID as u8);

        emulation.terminate().await;
    }

    /// OP_REQ_IMPORT (0x8003) + 32-byte busid must yield OP_REP_IMPORT
    /// success (0x140-byte device echo) — the operation `usbip attach`
    /// performs before URB traffic starts.
    #[tokio::test]
    async fn import_claims_the_device() {
        let (mut emulation, addr) = UsbipEmulation::start_for_test().await;

        let mut stream = connect_with_retry(addr).await;
        let mut req = [0u8; 8 + 32];
        req[0..2].copy_from_slice(&0x0111u16.to_be_bytes());
        req[2..4].copy_from_slice(&0x8003u16.to_be_bytes());
        req[4..8].copy_from_slice(&0u32.to_be_bytes());
        req[8..8 + BUS_ID.len()].copy_from_slice(BUS_ID.as_bytes());
        stream.write_all(&req).await.unwrap();

        let mut reply = vec![0u8; 0x140];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply[0..2], &[0x01, 0x11], "version");
        assert_eq!(&reply[2..4], &[0x00, 0x03], "OP_REP_IMPORT");
        assert_eq!(&reply[4..8], &[0, 0, 0, 0], "success status");

        emulation.terminate().await;
    }
}
