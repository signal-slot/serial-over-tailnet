//! The USB/IP protocol, server side: the PC's `usbip attach` imports the
//! device on the board's USB port and its own drivers talk to it as if it
//! were plugged into the PC. Pure encoding and decoding, unit-tested on the
//! host; the firmware does the USB transfers.
//!
//! Written from the protocol's description (version 1.1.1, TCP port 3240,
//! every field big-endian), not from an implementation.
//!
//! A connection starts with one operation: `OP_REQ_DEVLIST` (list the
//! exportable devices, then the server closes) or `OP_REQ_IMPORT` (attach one
//! by its bus ID). After a successful import the connection carries URBs:
//! `CMD_SUBMIT` (answered by `RET_SUBMIT`) and `CMD_UNLINK` (answered by
//! `RET_UNLINK`), each behind a 48-byte header.

use alloc::string::String;
use alloc::vec::Vec;

/// The TCP port `usbip` connects to.
pub const USBIP_PORT: u16 = 3240;
/// Protocol version 1.1.1.
pub const VERSION: u16 = 0x0111;

pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

pub const CMD_SUBMIT: u32 = 1;
pub const CMD_UNLINK: u32 = 2;
pub const RET_SUBMIT: u32 = 3;
pub const RET_UNLINK: u32 = 4;

/// Version, code and status of an operation.
pub const OP_HEADER_LEN: usize = 8;
/// The bus ID that follows an `OP_REQ_IMPORT` header.
pub const BUSID_LEN: usize = 32;
const PATH_LEN: usize = 256;
/// Every URB command and reply starts with this many bytes.
pub const URB_HEADER_LEN: usize = 48;
/// One isochronous packet descriptor after a submit's data.
pub const ISO_DESCRIPTOR_LEN: usize = 16;

/// Linux errno values, negated, as URB statuses.
pub mod status {
    pub const OK: i32 = 0;
    pub const ENOMEM: i32 = -12;
    pub const ENODEV: i32 = -19;
    pub const EINVAL: i32 = -22;
    pub const EPIPE: i32 = -32;
    pub const ETIME: i32 = -62;
    pub const EPROTO: i32 = -71;
    pub const EOVERFLOW: i32 = -75;
    pub const ECONNRESET: i32 = -104;
    pub const ESHUTDOWN: i32 = -108;
}

/// Device speeds as USB/IP numbers them (Linux `enum usb_device_speed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Low = 1,
    Full = 2,
    High = 3,
}

/// The device as it is announced in a device list and an import reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub busid: String,
    pub path: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: Speed,
    pub vid: u16,
    pub pid: u16,
    pub bcd_device: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    /// Class, subclass and protocol of every interface (alternate setting 0).
    pub interfaces: Vec<(u8, u8, u8)>,
}

const DESC_DEVICE: u8 = 1;
const DESC_CONFIG: u8 = 2;
const DESC_INTERFACE: u8 = 4;

impl DeviceInfo {
    /// Builds the announcement from the device descriptor (18 bytes) and
    /// the active configuration descriptor (all `wTotalLength` bytes).
    /// `None` when either is not what it should be.
    pub fn from_descriptors(
        busid: &str,
        devnum: u32,
        speed: Speed,
        device: &[u8],
        config: &[u8],
    ) -> Option<DeviceInfo> {
        if device.len() < 18
            || device[1] != DESC_DEVICE
            || config.len() < 9
            || config[1] != DESC_CONFIG
        {
            return None;
        }
        let le = |i: usize| u16::from_le_bytes([device[i], device[i + 1]]);
        let mut interfaces = Vec::new();
        walk(config, |d| {
            if d[1] == DESC_INTERFACE && d.len() >= 9 && d[3] == 0 {
                interfaces.push((d[5], d[6], d[7]));
            }
        });
        Some(DeviceInfo {
            busid: busid.into(),
            path: alloc::format!("/sys/devices/platform/serial-over-tailnet/usb1/{busid}"),
            busnum: 1,
            devnum,
            speed,
            vid: le(8),
            pid: le(10),
            bcd_device: le(12),
            class: device[4],
            subclass: device[5],
            protocol: device[6],
            configuration_value: config[5],
            num_configurations: device[17],
            interfaces,
        })
    }

    /// `busnum << 16 | devnum`, the device ID the client puts in every URB.
    pub fn devid(&self) -> u32 {
        (self.busnum << 16) | (self.devnum & 0xffff)
    }

    fn encode(&self, out: &mut Vec<u8>) {
        put_str(out, &self.path, PATH_LEN);
        put_str(out, &self.busid, BUSID_LEN);
        out.extend_from_slice(&self.busnum.to_be_bytes());
        out.extend_from_slice(&self.devnum.to_be_bytes());
        out.extend_from_slice(&(self.speed as u32).to_be_bytes());
        out.extend_from_slice(&self.vid.to_be_bytes());
        out.extend_from_slice(&self.pid.to_be_bytes());
        out.extend_from_slice(&self.bcd_device.to_be_bytes());
        out.extend_from_slice(&[
            self.class,
            self.subclass,
            self.protocol,
            self.configuration_value,
            self.num_configurations,
            self.interfaces.len() as u8,
        ]);
    }
}

/// Calls `f` with every descriptor of a configuration descriptor; stops at
/// a truncated or malformed one.
fn walk(config: &[u8], mut f: impl FnMut(&[u8])) {
    let mut at = 0;
    while at + 2 <= config.len() {
        let len = config[at] as usize;
        if len < 2 || at + len > config.len() {
            break;
        }
        f(&config[at..at + len]);
        at += len;
    }
}

fn put_str(out: &mut Vec<u8>, s: &str, field: usize) {
    let bytes = &s.as_bytes()[..s.len().min(field - 1)];
    out.extend_from_slice(bytes);
    out.resize(out.len() + field - bytes.len(), 0);
}

fn op_header(out: &mut Vec<u8>, code: u16, status: u32) {
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(&status.to_be_bytes());
}

/// The operation a connection starts with.
#[derive(Debug, PartialEq, Eq)]
pub enum OpRequest {
    DevList,
    /// Followed by a [`BUSID_LEN`]-byte bus ID the caller reads next.
    Import,
    Unknown {
        version: u16,
        code: u16,
    },
}

pub fn parse_op(h: &[u8; OP_HEADER_LEN]) -> OpRequest {
    let version = u16::from_be_bytes([h[0], h[1]]);
    let code = u16::from_be_bytes([h[2], h[3]]);
    match code {
        OP_REQ_DEVLIST if version == VERSION => OpRequest::DevList,
        OP_REQ_IMPORT if version == VERSION => OpRequest::Import,
        _ => OpRequest::Unknown { version, code },
    }
}

/// The bus ID of an import request: the bytes up to the first NUL.
pub fn parse_busid(b: &[u8; BUSID_LEN]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(BUSID_LEN);
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// The device list: the one device, or none while nothing is plugged in.
pub fn rep_devlist(device: Option<&DeviceInfo>) -> Vec<u8> {
    let mut out = Vec::new();
    op_header(&mut out, OP_REP_DEVLIST, 0);
    out.extend_from_slice(&(device.is_some() as u32).to_be_bytes());
    if let Some(d) = device {
        d.encode(&mut out);
        for &(class, subclass, protocol) in &d.interfaces {
            out.extend_from_slice(&[class, subclass, protocol, 0]);
        }
    }
    out
}

/// The answer to an import: the device, or a failure status alone.
pub fn rep_import(device: Option<&DeviceInfo>) -> Vec<u8> {
    let mut out = Vec::new();
    match device {
        Some(d) => {
            op_header(&mut out, OP_REP_IMPORT, 0);
            d.encode(&mut out);
        }
        None => op_header(&mut out, OP_REP_IMPORT, 1),
    }
    out
}

/// A `CMD_SUBMIT`: one URB to run. OUT data (`length` bytes) and, for an
/// isochronous URB, the packet descriptors follow the header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submit {
    pub seqnum: u32,
    pub devid: u32,
    pub dir_in: bool,
    /// Endpoint number, 0 to 15 (the direction is `dir_in`).
    pub ep: u8,
    pub flags: u32,
    pub length: u32,
    /// Isochronous packets; 0 for anything else.
    pub packets: u32,
    pub setup: [u8; 8],
}

impl Submit {
    /// The endpoint address as USB writes it (bit 7 set for IN).
    pub fn address(&self) -> u8 {
        self.ep | if self.dir_in { 0x80 } else { 0 }
    }

    /// Bytes that follow the header: the OUT data and the isochronous
    /// packet descriptors.
    pub fn trailer_len(&self) -> usize {
        (if self.dir_in { 0 } else { self.length as usize })
            + self.packets as usize * ISO_DESCRIPTOR_LEN
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Submit(Submit),
    Unlink {
        seqnum: u32,
        devid: u32,
        victim: u32,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommandError {
    /// Not a command this side understands; the connection cannot go on.
    UnknownCommand(u32),
    /// A field out of range (endpoint past 15, a length past `max_length`).
    Invalid,
}

/// Decodes a URB command header. `max_length` caps a submit's buffer.
pub fn parse_command(h: &[u8; URB_HEADER_LEN], max_length: u32) -> Result<Command, CommandError> {
    let word = |i: usize| u32::from_be_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
    let (command, seqnum, devid, direction, ep) = (word(0), word(4), word(8), word(12), word(16));
    match command {
        CMD_SUBMIT => {
            let length = word(24);
            // Not isochronous: Linux sends 0 or, from 5.x on, 0xffffffff.
            let packets = match word(32) {
                0xffff_ffff => 0,
                n => n,
            };
            if ep > 15 || direction > 1 || length > max_length || packets > 1024 {
                return Err(CommandError::Invalid);
            }
            let mut setup = [0u8; 8];
            setup.copy_from_slice(&h[40..48]);
            Ok(Command::Submit(Submit {
                seqnum,
                devid,
                dir_in: direction == 1,
                ep: ep as u8,
                flags: word(20),
                length,
                packets,
                setup,
            }))
        }
        CMD_UNLINK => Ok(Command::Unlink {
            seqnum,
            devid,
            victim: word(20),
        }),
        other => Err(CommandError::UnknownCommand(other)),
    }
}

/// The reply to a submit: header, then the IN data actually received.
pub fn ret_submit(seqnum: u32, status: i32, actual: u32, data_in: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(URB_HEADER_LEN + data_in.len());
    for w in [RET_SUBMIT, seqnum, 0, 0, 0] {
        out.extend_from_slice(&w.to_be_bytes());
    }
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&actual.to_be_bytes());
    // start_frame, number_of_packets (0xffffffff: not isochronous), error_count.
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0xffff_ffffu32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.resize(URB_HEADER_LEN, 0);
    out.extend_from_slice(data_in);
    out
}

/// The reply to an unlink: `ECONNRESET` when the URB was cancelled (its
/// own reply is then never sent), 0 when it had already completed.
pub fn ret_unlink(seqnum: u32, status: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(URB_HEADER_LEN);
    for w in [RET_UNLINK, seqnum, 0, 0, 0] {
        out.extend_from_slice(&w.to_be_bytes());
    }
    out.extend_from_slice(&status.to_be_bytes());
    out.resize(URB_HEADER_LEN, 0);
    out
}

/// Standard control requests the server carries out itself instead of
/// passing them on: they change state the board's USB host keeps.
#[derive(Debug, PartialEq, Eq)]
pub enum Control {
    Forward,
    /// SET_CONFIGURATION: the board's host has already configured the
    /// device, so only the value it already has can be accepted.
    SetConfiguration(u8),
    /// SET_INTERFACE: the interface's pipes are made again.
    SetInterface {
        interface: u8,
        alt: u8,
    },
    /// CLEAR_FEATURE(ENDPOINT_HALT): the host's pipe must restart at
    /// DATA0 together with the device's endpoint.
    ClearHalt {
        endpoint: u8,
    },
    /// The client resetting the device: a hub-class SET_FEATURE(PORT_RESET)
    /// sent down the device's endpoint 0, which only the server can act on.
    PortReset,
}

pub fn classify_control(setup: &[u8; 8]) -> Control {
    let (request_type, request) = (setup[0], setup[1]);
    let value = u16::from_le_bytes([setup[2], setup[3]]);
    let index = u16::from_le_bytes([setup[4], setup[5]]);
    match (request_type, request) {
        (0x00, 0x09) => Control::SetConfiguration(value as u8),
        (0x01, 0x0b) => Control::SetInterface {
            interface: index as u8,
            alt: value as u8,
        },
        (0x02, 0x01) if value == 0 => Control::ClearHalt {
            endpoint: index as u8,
        },
        (0x23, 0x03) if value == 4 => Control::PortReset,
        _ => Control::Forward,
    }
}

/// A CMSIS-DAP v2 command asking for the probe's packet size (DAP_Info,
/// ID 0xFF). Sent on the bulk OUT endpoint of a vendor-class interface.
pub fn is_dap_packet_size_request(out: &[u8]) -> bool {
    out.len() >= 2 && out[0] == 0x00 && out[1] == 0xff
}

/// Lowers the packet size in a DAP_Info reply to `max`, the bulk endpoint's
/// packet size, and says whether it did.
///
/// A probe made for high speed can go on announcing 512 bytes when the
/// board's full-speed host enumerates it. The MCU-Link (firmware V3.128)
/// then runs each 64-byte USB packet of a longer command as a command of
/// its own, and the replies stop matching the commands. Commands no longer
/// than one packet work on every probe.
pub fn cap_dap_packet_size(reply: &mut [u8], max: u16) -> bool {
    if reply.len() < 4 || reply[0] != 0x00 || reply[1] != 2 {
        return false;
    }
    if u16::from_le_bytes([reply[2], reply[3]]) <= max {
        return false;
    }
    reply[2..4].copy_from_slice(&max.to_le_bytes());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn dap_packet_size_is_capped_to_the_endpoint() {
        assert!(is_dap_packet_size_request(&[0x00, 0xff]));
        assert!(!is_dap_packet_size_request(&[0x00, 0xfe]));
        assert!(!is_dap_packet_size_request(&[0x00]));
        // MCU-Link at full speed: 512 announced, 64 per packet.
        let mut r = [0x00, 0x02, 0x00, 0x02];
        assert!(cap_dap_packet_size(&mut r, 64));
        assert_eq!(r, [0x00, 0x02, 64, 0x00]);
        // Already within the endpoint: left alone.
        let mut r = [0x00, 0x02, 64, 0x00];
        assert!(!cap_dap_packet_size(&mut r, 64));
        // Not a two-byte DAP_Info answer.
        let mut r = [0x00, 0x01, 0x08];
        assert!(!cap_dap_packet_size(&mut r, 64));
        let mut r = [0x05, 0x02, 0x00, 0x02];
        assert!(!cap_dap_packet_size(&mut r, 64));
    }

    /// A Raspberry Pi Debug Probe: CMSIS-DAP v2 on interface 0, CDC-ACM on
    /// 1 and 2.
    fn probe_device() -> Vec<u8> {
        vec![
            18, 1, 0x10, 0x02, 0xef, 0x02, 0x01, 64, 0x8a, 0x2e, 0x0c, 0x00, 0x01, 0x02, 1, 2, 3, 1,
        ]
    }

    fn probe_config() -> Vec<u8> {
        let body: Vec<u8> = [
            vec![9, 4, 0, 0, 2, 0xff, 0, 0, 0],
            vec![7, 5, 0x01, 2, 64, 0, 0],
            vec![7, 5, 0x82, 2, 64, 0, 0],
            vec![9, 4, 1, 0, 1, 0x02, 0x02, 0, 0],
            vec![5, 0x24, 0, 0x10, 1],
            vec![7, 5, 0x83, 3, 8, 0, 10],
            vec![9, 4, 2, 0, 2, 0x0a, 0, 0, 0],
            vec![7, 5, 0x04, 2, 64, 0, 0],
            vec![7, 5, 0x85, 2, 64, 0, 0],
        ]
        .concat();
        let total = (9 + body.len()) as u16;
        let mut c = vec![9, 2, total as u8, (total >> 8) as u8, 3, 1, 0, 0x80, 50];
        c.extend(body);
        c
    }

    fn probe() -> DeviceInfo {
        DeviceInfo::from_descriptors("1-1", 2, Speed::Full, &probe_device(), &probe_config())
            .unwrap()
    }

    #[test]
    fn device_info_from_descriptors() {
        let d = probe();
        assert_eq!((d.vid, d.pid, d.bcd_device), (0x2e8a, 0x000c, 0x0201));
        assert_eq!((d.class, d.subclass, d.protocol), (0xef, 0x02, 0x01));
        assert_eq!((d.configuration_value, d.num_configurations), (1, 1));
        assert_eq!(d.interfaces, vec![(0xff, 0, 0), (2, 2, 0), (0x0a, 0, 0)]);
        assert_eq!(d.devid(), 0x0001_0002);
        assert!(DeviceInfo::from_descriptors(
            "1-1",
            2,
            Speed::Full,
            &probe_device()[..17],
            &probe_config()
        )
        .is_none());
        assert!(
            DeviceInfo::from_descriptors("1-1", 2, Speed::Full, &probe_device(), &[9, 4]).is_none()
        );
    }

    #[test]
    fn devlist_layout() {
        let r = rep_devlist(Some(&probe()));
        assert_eq!(&r[..12], &[0x01, 0x11, 0x00, 0x05, 0, 0, 0, 0, 0, 0, 0, 1]);
        let dev = &r[12..];
        assert_eq!(&dev[..PATH_LEN][..10], b"/sys/devic");
        assert_eq!(dev[PATH_LEN - 1], 0);
        assert_eq!(&dev[PATH_LEN..PATH_LEN + 4], b"1-1\0");
        let at = PATH_LEN + BUSID_LEN;
        assert_eq!(&dev[at..at + 12], &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 2]);
        assert_eq!(
            &dev[at + 12..at + 18],
            &[0x2e, 0x8a, 0x00, 0x0c, 0x02, 0x01]
        );
        assert_eq!(&dev[at + 18..at + 24], &[0xef, 0x02, 0x01, 1, 1, 3]);
        // 312-byte device record, then 4 bytes per interface.
        assert_eq!(dev.len(), 312 + 3 * 4);
        assert_eq!(&dev[312..316], &[0xff, 0, 0, 0]);
        assert_eq!(
            rep_devlist(None),
            vec![1, 0x11, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn import_reply() {
        let r = rep_import(Some(&probe()));
        assert_eq!(&r[..8], &[1, 0x11, 0, 3, 0, 0, 0, 0]);
        assert_eq!(r.len(), 8 + 312);
        assert_eq!(rep_import(None), vec![1, 0x11, 0, 3, 0, 0, 0, 1]);
    }

    #[test]
    fn op_requests() {
        assert_eq!(
            parse_op(&[1, 0x11, 0x80, 0x05, 0, 0, 0, 0]),
            OpRequest::DevList
        );
        assert_eq!(
            parse_op(&[1, 0x11, 0x80, 0x03, 0, 0, 0, 0]),
            OpRequest::Import
        );
        assert_eq!(
            parse_op(&[1, 0x06, 0x80, 0x03, 0, 0, 0, 0]),
            OpRequest::Unknown {
                version: 0x0106,
                code: 0x8003
            }
        );
        let mut b = [0u8; BUSID_LEN];
        b[..3].copy_from_slice(b"1-1");
        assert_eq!(parse_busid(&b), "1-1");
        assert_eq!(parse_busid(&[b'x'; BUSID_LEN]).len(), BUSID_LEN);
    }

    fn header(words: [u32; 10], setup: [u8; 8]) -> [u8; URB_HEADER_LEN] {
        let mut h = [0u8; URB_HEADER_LEN];
        for (i, w) in words.iter().enumerate() {
            h[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        h[40..].copy_from_slice(&setup);
        h
    }

    #[test]
    fn submit_commands() {
        // GET_DESCRIPTOR(device) on endpoint 0, IN, 18 bytes.
        let setup = [0x80, 6, 0, 1, 0, 0, 18, 0];
        let h = header([1, 7, 0x10002, 1, 0, 0x200, 18, 0, 0, 0], setup);
        let Ok(Command::Submit(s)) = parse_command(&h, 4096) else {
            panic!()
        };
        assert_eq!(
            (s.seqnum, s.devid, s.dir_in, s.ep, s.length),
            (7, 0x10002, true, 0, 18)
        );
        assert_eq!((s.address(), s.trailer_len(), s.setup), (0x80, 0, setup));
        // Bulk OUT of 100 bytes on endpoint 1, number_of_packets 0xffffffff.
        let h = header([1, 8, 0x10002, 0, 1, 0, 100, 0, 0xffff_ffff, 0], [0; 8]);
        let Ok(Command::Submit(s)) = parse_command(&h, 4096) else {
            panic!()
        };
        assert_eq!((s.address(), s.packets, s.trailer_len()), (0x01, 0, 100));
        // Too long, endpoint out of range.
        let h = header([1, 9, 0, 0, 1, 0, 8192, 0, 0, 0], [0; 8]);
        assert_eq!(parse_command(&h, 4096), Err(CommandError::Invalid));
        let h = header([1, 9, 0, 0, 16, 0, 8, 0, 0, 0], [0; 8]);
        assert_eq!(parse_command(&h, 4096), Err(CommandError::Invalid));
        let h = header([9, 9, 0, 0, 1, 0, 8, 0, 0, 0], [0; 8]);
        assert_eq!(
            parse_command(&h, 4096),
            Err(CommandError::UnknownCommand(9))
        );
    }

    #[test]
    fn unlink_command() {
        let h = header([2, 12, 0x10002, 0, 0, 7, 0, 0, 0, 0], [0; 8]);
        assert_eq!(
            parse_command(&h, 4096),
            Ok(Command::Unlink {
                seqnum: 12,
                devid: 0x10002,
                victim: 7
            })
        );
    }

    #[test]
    fn replies() {
        let r = ret_submit(7, status::OK, 3, &[1, 2, 3]);
        assert_eq!(r.len(), URB_HEADER_LEN + 3);
        assert_eq!(&r[..8], &[0, 0, 0, 3, 0, 0, 0, 7]);
        assert_eq!(&r[20..28], &[0, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(&r[32..36], &[0xff; 4]);
        assert_eq!(&r[48..], &[1, 2, 3]);
        let r = ret_submit(8, status::EPIPE, 0, &[]);
        assert_eq!(&r[20..24], &(-32i32).to_be_bytes());
        let r = ret_unlink(12, status::ECONNRESET);
        assert_eq!(r.len(), URB_HEADER_LEN);
        assert_eq!(&r[..8], &[0, 0, 0, 4, 0, 0, 0, 12]);
        assert_eq!(&r[20..24], &(-104i32).to_be_bytes());
    }

    #[test]
    fn control_requests_the_server_handles() {
        assert_eq!(
            classify_control(&[0, 9, 1, 0, 0, 0, 0, 0]),
            Control::SetConfiguration(1)
        );
        assert_eq!(
            classify_control(&[1, 0x0b, 1, 0, 2, 0, 0, 0]),
            Control::SetInterface {
                interface: 2,
                alt: 1
            }
        );
        assert_eq!(
            classify_control(&[2, 1, 0, 0, 0x82, 0, 0, 0]),
            Control::ClearHalt { endpoint: 0x82 }
        );
        assert_eq!(
            classify_control(&[0x23, 3, 4, 0, 1, 0, 0, 0]),
            Control::PortReset
        );
        // CLEAR_FEATURE of another feature, GET_DESCRIPTOR, a class request.
        assert_eq!(
            classify_control(&[2, 1, 1, 0, 0x82, 0, 0, 0]),
            Control::Forward
        );
        assert_eq!(
            classify_control(&[0x80, 6, 0, 1, 0, 0, 18, 0]),
            Control::Forward
        );
        assert_eq!(
            classify_control(&[0x21, 0x20, 0, 0, 1, 0, 7, 0]),
            Control::Forward
        );
    }
}
