use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const MAGIC_NUMBER: u16 = 0x3d5c;
const RECV_INPUT_SIZE: usize = std::mem::size_of::<RecvInput>();

// Linux input event type codes
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;

// Linux input event codes for sync
const SYN_REPORT: u16 = 0;

// Linux input event button codes (correct BTN_GAMEPAD codes)
const BTN_SOUTH: u16 = 0x130; // BTN_A / Cross
const BTN_EAST: u16 = 0x131; // BTN_B / Circle
const BTN_NORTH: u16 = 0x133; // BTN_Y / Triangle
const BTN_WEST: u16 = 0x134; // BTN_X / Square
const BTN_TL: u16 = 0x136; // L1
const BTN_TR: u16 = 0x137; // R1
const BTN_SELECT: u16 = 0x13a; // Share/Select
const BTN_START: u16 = 0x13b; // Options/Start
const BTN_MODE: u16 = 0x13c; // PS button
const BTN_THUMBL: u16 = 0x13d; // L3
const BTN_THUMBR: u16 = 0x13e; // R3

// Linux input event absolute axis codes
const ABS_X: u16 = 0x00; // Left stick X
const ABS_Y: u16 = 0x01; // Left stick Y
const ABS_Z: u16 = 0x02; // L2 analog
const ABS_RX: u16 = 0x03; // Right stick X
const ABS_RY: u16 = 0x04; // Right stick Y
const ABS_RZ: u16 = 0x05; // R2 analog
const ABS_HAT0X: u16 = 0x10; // D-pad X
const ABS_HAT0Y: u16 = 0x11; // D-pad Y

// uinput ioctl constants
const UI_SET_EVBIT: libc::c_ulong = 0x40045564;
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565;
const UI_SET_ABSBIT: libc::c_ulong = 0x40045567;
const UI_ABS_SETUP: libc::c_ulong = 0x401c5504;
const UI_DEV_SETUP: libc::c_ulong = 0x405c5503;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;

/// Maps a u16 axis value (0-255 range) to i32 (-32768 to 32767)
/// 0 = leftmost/upmost = -32768
/// 127/128 = center = 0
/// 255 = rightmost/downmost = 32767
#[inline]
fn map_axis_i16_to_i32(value: i16) -> i32 {
    // Scale from -256-255 to -32768 to 32767
    let normalized = value as i32;
    (normalized * 128).clamp(-32768, 32767)
}

fn ioctl_set_bit(
    device: &File,
    request: libc::c_ulong,
    value: u32,
) -> std::io::Result<()> {
    unsafe {
        use std::os::unix::io::AsRawFd;
        let ret = libc::ioctl(device.as_raw_fd(), request as _, value);
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// New 3DS button bit flags
#[repr(transparent)]
#[derive(Clone, Copy)]
struct N3DSButtons(u32);

#[allow(unused)]
impl N3DSButtons {
    const KEY_A: u32 = 1 << 0;
    const KEY_B: u32 = 1 << 1;
    const KEY_SELECT: u32 = 1 << 2;
    const KEY_START: u32 = 1 << 3;
    const KEY_DRIGHT: u32 = 1 << 4;
    const KEY_DLEFT: u32 = 1 << 5;
    const KEY_DUP: u32 = 1 << 6;
    const KEY_DDOWN: u32 = 1 << 7;
    const KEY_R: u32 = 1 << 8;
    const KEY_L: u32 = 1 << 9;
    const KEY_X: u32 = 1 << 10;
    const KEY_Y: u32 = 1 << 11;
    const KEY_ZL: u32 = 1 << 14;
    const KEY_ZR: u32 = 1 << 15;
    const KEY_TOUCH: u32 = 1 << 20;
    const KEY_CSTICK_RIGHT: u32 = 1 << 24;
    const KEY_CSTICK_LEFT: u32 = 1 << 25;
    const KEY_CSTICK_UP: u32 = 1 << 26;
    const KEY_CSTICK_DOWN: u32 = 1 << 27;
    const KEY_CPAD_RIGHT: u32 = 1 << 28;
    const KEY_CPAD_LEFT: u32 = 1 << 29;
    const KEY_CPAD_UP: u32 = 1 << 30;
    const KEY_CPAD_DOWN: u32 = 1 << 31;

    // Composite keys
    const KEY_UP: u32 = Self::KEY_DUP | Self::KEY_CPAD_UP;
    const KEY_DOWN: u32 = Self::KEY_DDOWN | Self::KEY_CPAD_DOWN;
    const KEY_LEFT: u32 = Self::KEY_DLEFT | Self::KEY_CPAD_LEFT;
    const KEY_RIGHT: u32 = Self::KEY_DRIGHT | Self::KEY_CPAD_RIGHT;

    #[inline]
    pub const fn a(&self) -> bool { self.0 & Self::KEY_A != 0 }

    #[inline]
    pub const fn b(&self) -> bool { self.0 & Self::KEY_B != 0 }

    #[inline]
    pub const fn select(&self) -> bool { self.0 & Self::KEY_SELECT != 0 }

    #[inline]
    pub const fn start(&self) -> bool { self.0 & Self::KEY_START != 0 }

    #[inline]
    pub const fn dpad_right(&self) -> bool { self.0 & Self::KEY_DRIGHT != 0 }

    #[inline]
    pub const fn dpad_left(&self) -> bool { self.0 & Self::KEY_DLEFT != 0 }

    #[inline]
    pub const fn dpad_up(&self) -> bool { self.0 & Self::KEY_DUP != 0 }

    #[inline]
    pub const fn dpad_down(&self) -> bool { self.0 & Self::KEY_DDOWN != 0 }

    #[inline]
    pub const fn r(&self) -> bool { self.0 & Self::KEY_R != 0 }

    #[inline]
    pub const fn l(&self) -> bool { self.0 & Self::KEY_L != 0 }

    #[inline]
    pub const fn x(&self) -> bool { self.0 & Self::KEY_X != 0 }

    #[inline]
    pub const fn y(&self) -> bool { self.0 & Self::KEY_Y != 0 }

    #[inline]
    pub const fn zl(&self) -> bool { self.0 & Self::KEY_ZL != 0 }

    #[inline]
    pub const fn zr(&self) -> bool { self.0 & Self::KEY_ZR != 0 }

    #[inline]
    pub const fn touch(&self) -> bool { self.0 & Self::KEY_TOUCH != 0 }

    #[inline]
    pub const fn cstick_right(&self) -> bool { self.0 & Self::KEY_CSTICK_RIGHT != 0 }

    #[inline]
    pub const fn cstick_left(&self) -> bool { self.0 & Self::KEY_CSTICK_LEFT != 0 }

    #[inline]
    pub const fn cstick_up(&self) -> bool { self.0 & Self::KEY_CSTICK_UP != 0 }

    #[inline]
    pub const fn cstick_down(&self) -> bool { self.0 & Self::KEY_CSTICK_DOWN != 0 }

    #[inline]
    pub const fn cpad_right(&self) -> bool { self.0 & Self::KEY_CPAD_RIGHT != 0 }

    #[inline]
    pub const fn cpad_left(&self) -> bool { self.0 & Self::KEY_CPAD_LEFT != 0 }

    #[inline]
    pub const fn cpad_up(&self) -> bool { self.0 & Self::KEY_CPAD_UP != 0 }

    #[inline]
    pub const fn cpad_down(&self) -> bool { self.0 & Self::KEY_CPAD_DOWN != 0 }

    #[inline]
    pub const fn up(&self) -> bool { self.0 & Self::KEY_UP != 0 }

    #[inline]
    pub const fn down(&self) -> bool { self.0 & Self::KEY_DOWN != 0 }

    #[inline]
    pub const fn left(&self) -> bool { self.0 & Self::KEY_LEFT != 0 }

    #[inline]
    pub const fn right(&self) -> bool { self.0 & Self::KEY_RIGHT != 0 }
}

#[repr(C, packed)]
struct Vec2<T> {
    pub x: T,
    pub y: T,
}

#[repr(C, packed)]
struct Vec3<T> {
    pub x: T,
    pub y: T,
    pub z: T,
}

#[repr(C, packed)]
struct RecvInput {
    magic_number: u16,
    version: u16,
    keys_up: N3DSButtons,
    keys_down: N3DSButtons,
    keys_held: N3DSButtons,
    touchscreen: Vec2<u16>,
    circlepad: Vec2<i16>,
    cstick: Vec2<i16>,
    gyroscope: Vec3<i16>,
    accelerometer: Vec3<i16>,
}

impl RecvInput {
    /// Converts a byte slice received from the network into a RecvInput struct.
    /// The bytes are expected to be in network (big-endian) byte order.
    pub fn from_network_bytes(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() != RECV_INPUT_SIZE {
            return Err("Invalid byte length for RecvInput");
        }

        // Read raw bytes into the packed struct
        let mut recv_input: Self =
            unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const Self) };

        // Convert all fields from network (big-endian) to host endianness
        recv_input.magic_number = u16::from_be(recv_input.magic_number);
        recv_input.version = u16::from_be(recv_input.version);
        recv_input.keys_up = N3DSButtons(u32::from_be(recv_input.keys_up.0));
        recv_input.keys_down =
            N3DSButtons(u32::from_be(recv_input.keys_down.0));
        recv_input.keys_held =
            N3DSButtons(u32::from_be(recv_input.keys_held.0));

        recv_input.touchscreen.x = u16::from_be(recv_input.touchscreen.x);
        recv_input.touchscreen.y = u16::from_be(recv_input.touchscreen.y);

        recv_input.circlepad.x = i16::from_be(recv_input.circlepad.x);
        recv_input.circlepad.y = i16::from_be(recv_input.circlepad.y);

        recv_input.cstick.x = i16::from_be(recv_input.cstick.x);
        recv_input.cstick.y = i16::from_be(recv_input.cstick.y);

        recv_input.gyroscope.x = i16::from_be(recv_input.gyroscope.x);
        recv_input.gyroscope.y = i16::from_be(recv_input.gyroscope.y);
        recv_input.gyroscope.z = i16::from_be(recv_input.gyroscope.z);

        recv_input.accelerometer.x = i16::from_be(recv_input.accelerometer.x);
        recv_input.accelerometer.y = i16::from_be(recv_input.accelerometer.y);
        recv_input.accelerometer.z = i16::from_be(recv_input.accelerometer.z);

        Ok(recv_input)
    }
}

/// Linux uinput structures for controller emulation
#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsInfo,
}

struct VirtualXboxController {
    device: File,
    controller_id: usize,
}

impl VirtualXboxController {
    fn new(controller_id: usize) -> std::io::Result<Self> {
        // Open "/dev/uinput"
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")?;

        // Enable event types
        ioctl_set_bit(&device, UI_SET_EVBIT, EV_KEY as u32)?;
        ioctl_set_bit(&device, UI_SET_EVBIT, EV_ABS as u32)?;
        ioctl_set_bit(&device, UI_SET_EVBIT, EV_SYN as u32)?;

        // Enable Xbox 360 buttons
        let buttons = [
            BTN_SOUTH,  // A
            BTN_EAST,   // B
            BTN_WEST,   // X
            BTN_NORTH,  // Y
            BTN_TL,     // LB
            BTN_TR,     // RB
            BTN_SELECT, // Back
            BTN_START,  // Start
            BTN_MODE,   // Guide
            BTN_THUMBL, // LS
            BTN_THUMBR, // RS
        ];

        for button in buttons {
            ioctl_set_bit(&device, UI_SET_KEYBIT, button as u32)?;
        }

        // Enable analog sticks and triggers
        let axes = [
            ABS_X,     // Left stick X
            ABS_Y,     // Left stick Y
            ABS_Z,     // LB
            ABS_RX,    // Right stick X
            ABS_RY,    // Right stick Y
            ABS_RZ,    // RB
            ABS_HAT0X, // D-pad X
            ABS_HAT0Y, // D-pad Y
        ];

        for axis in axes {
            ioctl_set_bit(&device, UI_SET_ABSBIT, axis as u32)?;
        }

        let mut abs = UinputAbsSetup {
            code: 0,
            absinfo: InputAbsInfo {
                value: 0,
                minimum: -32768,
                maximum: 32767,
                fuzz: 0,
                flat: 4096,
                resolution: 0,
            },
        };

        // Sticks
        for axis in [ABS_X, ABS_Y, ABS_RX, ABS_RY] {
            abs.code = axis;
            unsafe {
                let ret = libc::ioctl(device.as_raw_fd(), UI_ABS_SETUP, &abs);
                if ret < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }

        // Triggers
        abs.absinfo.flat = 0;
        abs.absinfo.minimum = 0;
        abs.absinfo.maximum = 255;
        for trigger in [ABS_Z, ABS_RZ] {
            abs.code = trigger;
            unsafe {
                let ret = libc::ioctl(device.as_raw_fd(), UI_ABS_SETUP, &abs);
                if ret < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }

        // D-pad
        abs.absinfo.flat = 0;
        abs.absinfo.minimum = -1;
        abs.absinfo.maximum = 1;
        for trigger in [ABS_HAT0X, ABS_HAT0Y] {
            abs.code = trigger;
            unsafe {
                let ret = libc::ioctl(device.as_raw_fd(), UI_ABS_SETUP, &abs);
                if ret < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }

        // Create the device
        let name = b"Xbox 360 Controller\0";
        let mut name_bytes = [0u8; 80];
        let name_len = name.len().min(79);
        name_bytes[..name_len].copy_from_slice(&name[..name_len]);

        let setup = UinputSetup {
            id: InputId {
                bustype: 0x03,   // USB
                vendor: 0x045e,  // Microsoft
                product: 0x028e, // Xbox 360 Controller
                version: 0x0114,
            },
            name: name_bytes,
            ff_effects_max: 0,
        };

        unsafe {
            let ret = libc::ioctl(
                device.as_raw_fd(),
                UI_DEV_SETUP,
                &setup as *const UinputSetup,
            );
            if ret < 0 {
                return Err(std::io::Error::last_os_error());
            }

            let ret = libc::ioctl(device.as_raw_fd(), UI_DEV_CREATE);
            if ret < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        println!(
            "Created virtual Xbox 360 controller {} for new client",
            controller_id
        );

        Ok(Self {
            device,
            controller_id,
        })
    }

    fn update_from_input(&mut self, input: &RecvInput) -> std::io::Result<()> {
        // Copy fields from packed struct to avoid alignment issues
        let keys_held = input.keys_held;
        let circlepad_x = input.circlepad.x;
        let circlepad_y = input.circlepad.y;
        let cstick_x = input.cstick.x;
        let cstick_y = input.cstick.y;

        // Map 3DS buttons to Xbox 360 buttons

        // Cross (BTN_SOUTH) <- B
        self.send_key(BTN_SOUTH, keys_held.b())?;

        // Circle (BTN_EAST) <- A
        self.send_key(BTN_EAST, keys_held.a())?;

        // Square (BTN_WEST) <- X
        self.send_key(BTN_WEST, keys_held.x())?;

        // Triangle (BTN_NORTH) <- Y
        self.send_key(BTN_NORTH, keys_held.y())?;

        // L1 button (BTN_TL) <- L
        self.send_key(BTN_TL, keys_held.l())?;

        // R1 button (BTN_TR) <- R
        self.send_key(BTN_TR, keys_held.r())?;

        // L2 trigger (BTN_TL2) <- ZL
        self.send_axis(ABS_Z, keys_held.zl() as i32 * 255)?;

        // R2 trigger (BTN_TR2) <- ZR
        self.send_axis(ABS_RZ, keys_held.zr() as i32 * 255)?;

        // D-pad
        let dpad_x =
            (keys_held.dpad_right() as i32) - (keys_held.dpad_left() as i32);
        let dpad_y =
            (keys_held.dpad_down() as i32) - (keys_held.dpad_up() as i32);
        self.send_axis(ABS_HAT0X, dpad_x)?;
        self.send_axis(ABS_HAT0Y, dpad_y)?;

        // Select (BTN_SELECT) <- SELECT
        self.send_key(BTN_SELECT, keys_held.select())?;

        // Start (BTN_START) <- START
        self.send_key(BTN_START, keys_held.start())?;

        // Map Circle Pad to Left Stick
        // 3DS: x = 0 (left) to 255 (right), y = 0 (up) to 255 (down)
        // Linux: -32768 (left/up) to 32767 (right/down)
        let left_stick_x = map_axis_i16_to_i32(circlepad_x);
        let left_stick_y = map_axis_i16_to_i32(1 - circlepad_y);

        self.send_axis(ABS_X, left_stick_x)?;
        self.send_axis(ABS_Y, left_stick_y)?;

        // Map C-Stick to Right Stick
        let right_stick_x = map_axis_i16_to_i32(cstick_x);
        let right_stick_y = map_axis_i16_to_i32(1 - cstick_y);

        self.send_axis(ABS_RX, right_stick_x)?;
        self.send_axis(ABS_RY, right_stick_y)?;

        // Send a sync event to indicate update is complete
        self.send_event(EV_SYN, SYN_REPORT, 0)?;
        Ok(())
    }

    #[inline]
    fn send_key(&mut self, code: u16, value: bool) -> std::io::Result<()> {
        self.send_event(EV_KEY, code, value as i32)
    }

    #[inline]
    fn send_axis(&mut self, code: u16, value: i32) -> std::io::Result<()> {
        self.send_event(EV_ABS, code, value)
    }

    fn send_event(
        &mut self,
        type_: u16,
        code: u16,
        value: i32,
    ) -> std::io::Result<()> {
        #[repr(C)]
        struct InputEvent {
            time: libc::timeval,
            type_: u16,
            code: u16,
            value: i32,
        }

        let event = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };

        unsafe {
            let bytes = std::slice::from_raw_parts(
                &event as *const InputEvent as *const u8,
                std::mem::size_of::<InputEvent>(),
            );
            self.device.write_all(bytes)?;
        }

        Ok(())
    }
}

impl Drop for VirtualXboxController {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.device.as_raw_fd(), UI_DEV_DESTROY);
        }
        println!(
            "Destroyed virtual Xbox 360 controller {}",
            self.controller_id
        );
    }
}

/// Represents a connected client with their controller
struct ConnectedClient {
    controller: VirtualXboxController,
    last_input: RecvInput,
}

/// Stores the server state
struct ServerState {
    /// Maps IP addresses to their controller and data
    clients: HashMap<SocketAddr, ConnectedClient>,
    /// Next controller ID to assign
    next_controller_id: usize,
    /// Total number of packets received
    total_packets: u64,
    /// Number of invalid packets (wrong magic number)
    invalid_packets: u64,
}

impl ServerState {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
            next_controller_id: 0,
            total_packets: 0,
            invalid_packets: 0,
        }
    }

    fn get_or_create_client(
        &mut self,
        addr: SocketAddr,
    ) -> std::io::Result<&mut ConnectedClient> {
        if !self.clients.contains_key(&addr) {
            let controller =
                VirtualXboxController::new(self.next_controller_id)?;
            let order = self.next_controller_id;
            self.next_controller_id += 1;

            // Create a dummy input for initialization
            let dummy_input = unsafe { std::mem::zeroed() };

            self.clients.insert(
                addr,
                ConnectedClient {
                    controller,
                    last_input: dummy_input,
                },
            );

            println!(
                "New client connected from {} assigned controller ID {}",
                addr, order
            );
        }

        Ok(self.clients.get_mut(&addr).unwrap())
    }

    fn update_input(
        &mut self,
        addr: SocketAddr,
        input: RecvInput,
    ) -> std::io::Result<()> {
        let client = self.get_or_create_client(addr)?;
        client.controller.update_from_input(&input)?;
        client.last_input = input;
        self.total_packets += 1;
        Ok(())
    }

    fn record_invalid_packet(&mut self) { self.invalid_packets += 1; }
}

struct N3DSServer {
    state: Arc<RwLock<ServerState>>,
    socket: Arc<UdpSocket>,
    cancellation_token: CancellationToken,
}

impl N3DSServer {
    async fn new(
        bind_addr: &str,
        cancellation_token: CancellationToken,
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(bind_addr).await?;
        println!("Server listening on {}", bind_addr);

        Ok(Self {
            state: Arc::new(RwLock::new(ServerState::new())),
            socket: Arc::new(socket),
            cancellation_token,
        })
    }

    async fn run(&self) -> std::io::Result<()> {
        // Buffer sized exactly for one RecvInput datagram
        let mut buffer = vec![0u8; RECV_INPUT_SIZE];

        loop {
            let result = tokio::select! {
                _ = self.cancellation_token.cancelled() => break Ok(()),
                x = self.socket.recv_from(&mut buffer) => x,
            };
            match result {
                Ok((size, addr)) => {
                    // Ensure we received exactly one complete RecvInput
                    if size != RECV_INPUT_SIZE {
                        eprintln!(
                            "Warning: Received {} bytes from {} (expected {} bytes for RecvInput)",
                            size, addr, RECV_INPUT_SIZE
                        );
                        continue;
                    }

                    self.handle_packet(&buffer[..size], addr).await;
                },
                Err(e) => {
                    eprintln!("Error receiving packet: {}", e);
                },
            }
        }
    }

    async fn handle_packet(&self, data: &[u8], addr: SocketAddr) {
        match RecvInput::from_network_bytes(data) {
            Ok(input) => {
                // Copy magic_number to avoid borrowing packed field
                let magic = input.magic_number;

                // Verify magic number
                if magic != MAGIC_NUMBER {
                    eprintln!(
                        "Warning: Invalid magic number 0x{:04x} from {} (expected 0x{:04x})",
                        magic, addr, MAGIC_NUMBER
                    );
                    let mut state = self.state.write().await;
                    state.record_invalid_packet();
                    return;
                }

                // Valid input, update state and controller
                let mut state = self.state.write().await;
                if let Err(e) = state.update_input(addr, input) {
                    eprintln!("Error updating controller for {}: {}", addr, e);
                }
            },
            Err(e) => {
                eprintln!("Error parsing input from {}: {}", addr, e);
            },
        }
    }

    /// Get a snapshot of the current state
    pub async fn get_state_snapshot(&self) -> (usize, u64, u64) {
        let state = self.state.read().await;
        (
            state.clients.len(),
            state.total_packets,
            state.invalid_packets,
        )
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cancellation_token = CancellationToken::new();
    let server =
        N3DSServer::new("0.0.0.0:15708", cancellation_token.clone()).await?;
    let server_clone = Arc::new(server);

    let ctrlc_token = cancellation_token.clone();
    ctrlc::set_handler(move || {
        ctrlc_token.cancel();
    })
    .expect("Could not set Ctrl+C handler");

    // Spawn a task to periodically print statistics
    let stats_server = server_clone.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = interval.tick() => (),
                _ = cancellation_token.cancelled() => break,
            };
            let (clients, total, invalid) =
                stats_server.get_state_snapshot().await;
            println!(
                "Stats - Connected clients: {}, Total packets: {}, Invalid packets: {}",
                clients, total, invalid
            );
        }
    });

    // Run the main server loop
    server_clone.run().await
}
