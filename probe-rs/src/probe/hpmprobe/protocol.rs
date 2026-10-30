//! HPM Prober vendor protocol.
//!
//! Wire format:
//! `[tunnel_id, flags, payload_len_le16, payload...]`
//!
//! The probe is a WinUSB composite device: interface 0 is the vendor tunnel,
//! bulk OUT `0x02`, bulk IN `0x81`. This driver uses the RISC-V debug tunnel
//! (`RISCV_DEBUG_OP_DMI_*`) so probe-rs's normal RISC-V layer handles the
//! Debug Module, abstracts, progbuf and SBA.

use crate::probe::ProbeError;

pub const VENDOR_ID: u16 = 0x34B7;
pub const PRODUCT_ID: u16 = 0x8003;
/// PID used by transitional HPM Prober firmware.
pub const LEGACY_PRODUCT_ID: u16 = 0xE001;

pub const TUNNEL_CONTROL: u8 = 0x00;
pub const TUNNEL_RISCV: u8 = 0x01;
pub const TUNNEL_JTAG: u8 = 0x02;

pub const FRAME_HEADER_SIZE: usize = 4;
pub const MAX_PAYLOAD: usize = 8192;
pub const MAX_FRAME_SIZE: usize = FRAME_HEADER_SIZE + MAX_PAYLOAD;

pub const CONTROL_HELLO: u8 = 0x01;
pub const CONTROL_CONFIGURE_DEBUG_TRANSPORT: u8 = 0x21;
pub const CONTROL_RELEASE_DEBUG_TRANSPORT: u8 = 0x23;

pub const RISCV_GET_INTERFACE_INFO: u8 = 0x01;
pub const RISCV_DMI_RESET: u8 = 0x02;
pub const RISCV_DMI_READ: u8 = 0x03;
pub const RISCV_DMI_WRITE: u8 = 0x04;
pub const RISCV_SELECT_HART: u8 = 0x06;

pub const CAP_MEMORY_PROGBUF: u32 = 1 << 19;
pub const CAP_MEMORY_SBA: u32 = 1 << 30;
pub const JTAG_GET_INTERFACE_INFO: u8 = 0x01;
pub const JTAG_SEQUENCE: u8 = 0x07;
pub const JTAG_CAP_SEQUENCE: u32 = 1 << 2;
pub const JTAG_CAP_CJTAG_RAW: u32 = 1 << 3;

pub const RISCV_READ_MEMORY_PROGBUF: u8 = 0x1e;
pub const RISCV_WRITE_MEMORY_PROGBUF: u8 = 0x1f;
pub const RISCV_READ_MEMORY_SBA: u8 = 0x27;
pub const RISCV_WRITE_MEMORY_SBA: u8 = 0x28;

pub const STATUS_OK: u8 = 0x00;
pub const STATUS_UNSUPPORTED_OPCODE: u8 = 0x01;
pub const STATUS_INVALID_ARGUMENT: u8 = 0x02;
pub const STATUS_IO_FAULT: u8 = 0x08;

pub const JTAG_TRANSPORT: u8 = 0x01;
pub const CJTAG_TRANSPORT: u8 = 0x02;
pub const DEFAULT_SPEED_KHZ: u32 = 1_000;

#[derive(Debug, thiserror::Error)]
pub enum HpmProbeError {
    /// USB transfer failed.
    #[error("HPM probe USB transfer failed: {0}")]
    Usb(#[from] std::io::Error),
    /// The USB reader timed out.
    #[error("HPM probe USB transfer timed out")]
    Timeout,
    /// The USB session was lost.
    #[error("HPM probe USB session was lost")]
    SessionLost,
    /// The probe returned an invalid frame.
    #[error("HPM probe protocol error: {0}")]
    Protocol(String),
    /// The probe rejected a request with a firmware status.
    #[error(
        "HPM probe tunnel 0x{tunnel:02x} opcode 0x{opcode:02x} failed: {status_name} (status 0x{status:02x})"
    )]
    Firmware {
        tunnel: u8,
        opcode: u8,
        status: u8,
        status_name: &'static str,
    },
    /// The probe firmware protocol version is not supported.
    #[error("HPM probe firmware protocol {major}.{minor} is not supported")]
    VersionMismatch { major: u8, minor: u8 },
}

impl ProbeError for HpmProbeError {}

pub(crate) fn status_name(status: u8) -> &'static str {
    match status {
        0x00 => "OK",
        0x01 => "unsupported opcode",
        0x02 => "invalid argument",
        0x03 => "invalid state",
        0x04 => "busy",
        0x05 => "timeout",
        0x06 => "cancelled",
        0x07 => "resource conflict",
        0x08 => "I/O fault",
        0x09 => "length mismatch",
        0x0A => "not configured",
        0x0B => "safety interlock",
        0x0C => "overflow",
        0x0D => "unavailable",
        0x0E => "version mismatch",
        0x0F => "internal fault",
        _ => "unknown status",
    }
}

pub(crate) fn encode_frame(tunnel: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
    frame.push(tunnel);
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Hello {
    pub major: u8,
    pub minor: u8,
    pub max_payload: usize,
    pub nonce: u32,
}

pub(crate) fn parse_hello(response: &[u8]) -> Result<Hello, HpmProbeError> {
    if response.len() < 11 {
        return Err(HpmProbeError::Protocol(format!(
            "HELLO response is {} bytes, expected at least 11",
            response.len()
        )));
    }
    let max_payload = u16::from_le_bytes([response[5], response[6]]) as usize;
    if max_payload < 16 {
        return Err(HpmProbeError::Protocol(format!(
            "probe advertises an unusable payload limit of {max_payload} bytes"
        )));
    }
    if response[2] != 1 {
        return Err(HpmProbeError::VersionMismatch {
            major: response[2],
            minor: response[3],
        });
    }
    Ok(Hello {
        major: response[2],
        minor: response[3],
        max_payload: max_payload.min(MAX_PAYLOAD),
        nonce: u32::from_le_bytes([response[7], response[8], response[9], response[10]]),
    })
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InterfaceInfo {
    pub major: u8,
    pub minor: u8,
    pub idcode: u32,
    pub dtmcs: u32,
    pub capabilities: u32,
}

pub(crate) fn parse_interface_info(response: &[u8]) -> Result<InterfaceInfo, HpmProbeError> {
    if response.len() < 27 {
        return Err(HpmProbeError::Protocol(format!(
            "GET_INTERFACE_INFO response is {} bytes, expected at least 27",
            response.len()
        )));
    }
    let word = |offset: usize| {
        u32::from_le_bytes([
            response[offset],
            response[offset + 1],
            response[offset + 2],
            response[offset + 3],
        ])
    };
    Ok(InterfaceInfo {
        major: response[2],
        minor: response[3],
        idcode: word(13),
        dtmcs: word(17),
        capabilities: word(9),
    })
}

pub(crate) fn configure_request(transport: u8, clock_hz: u32) -> [u8; 6] {
    let clock = clock_hz.to_le_bytes();
    [
        CONTROL_CONFIGURE_DEBUG_TRANSPORT,
        transport,
        clock[0],
        clock[1],
        clock[2],
        clock[3],
    ]
}

pub(crate) fn parse_configure_clock(response: &[u8], transport: u8) -> Result<u32, HpmProbeError> {
    if response.len() != 11 {
        return Err(HpmProbeError::Protocol(format!(
            "CONFIGURE_DEBUG_TRANSPORT response is {} bytes, expected 11",
            response.len()
        )));
    }
    if response[2] != transport {
        return Err(HpmProbeError::Protocol(format!(
            "probe configured transport {}, expected {transport}",
            response[2]
        )));
    }
    let clock = u32::from_le_bytes([response[7], response[8], response[9], response[10]]);
    if clock == 0 {
        return Err(HpmProbeError::Protocol(
            "probe configured a zero JTAG clock".into(),
        ));
    }
    Ok(clock)
}

pub(crate) fn parse_dmi_read(response: &[u8]) -> Result<u32, HpmProbeError> {
    if response.len() < 6 {
        return Err(HpmProbeError::Protocol(format!(
            "DMI_READ response is {} bytes, expected at least 6",
            response.len()
        )));
    }
    Ok(u32::from_le_bytes([
        response[2],
        response[3],
        response[4],
        response[5],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_matches_the_firmware_header() {
        let frame = encode_frame(TUNNEL_RISCV, &[0x10, 0x04, 0xB1, 0x07]);
        assert_eq!(frame, vec![0x01, 0x00, 0x04, 0x00, 0x10, 0x04, 0xB1, 0x07]);
    }

    #[test]
    fn hello_is_parsed() {
        let response = [
            0x01, 0x00, 0x01, 0x02, 0x00, 0x00, 0x20, 0x11, 0x22, 0x33, 0x44,
        ];
        let hello = parse_hello(&response).unwrap();
        assert_eq!(hello.major, 1);
        assert_eq!(hello.minor, 2);
        assert_eq!(hello.max_payload, 8192);
        assert_eq!(hello.nonce, 0x4433_2211);
    }

    #[test]
    fn interface_info_uses_the_documented_offsets() {
        let mut response = [0u8; 27];
        response[0] = 0x01;
        response[2] = 1;
        response[3] = 28;
        response[9..13].copy_from_slice(&(CAP_MEMORY_PROGBUF | CAP_MEMORY_SBA).to_le_bytes());
        response[13..17].copy_from_slice(&0x1000_563d_u32.to_le_bytes());
        response[17..21].copy_from_slice(&0x0000_0071_u32.to_le_bytes());
        let info = parse_interface_info(&response).unwrap();
        assert_eq!(info.major, 1);
        assert_eq!(info.minor, 28);
        assert_eq!(info.idcode, 0x1000_563d);
        assert_eq!(info.dtmcs, 0x71);
        assert_eq!(info.capabilities, CAP_MEMORY_PROGBUF | CAP_MEMORY_SBA);
    }

    #[test]
    fn configure_transport_checks_type_and_actual_clock() {
        assert_eq!(
            configure_request(CJTAG_TRANSPORT, 25_000_000),
            [0x21, 0x02, 0x40, 0x78, 0x7d, 0x01]
        );
        let mut reply = [0x21, 0, CJTAG_TRANSPORT, 0, 0, 0, 0, 0x40, 0x78, 0x7d, 0x01];
        assert_eq!(
            parse_configure_clock(&reply, CJTAG_TRANSPORT).unwrap(),
            25_000_000
        );
        assert!(parse_configure_clock(&reply, JTAG_TRANSPORT).is_err());
        assert!(parse_configure_clock(&reply[..10], CJTAG_TRANSPORT).is_err());
        reply[7..11].fill(0);
        assert!(parse_configure_clock(&reply, CJTAG_TRANSPORT).is_err());
    }
}
