//! HPM Prober (WinUSB vendor tunnel) support.
//!
//! The probe exposes a vendor bulk interface and a RISC-V DMI tunnel. Control,
//! register and single-word traffic uses DMI; block memory accesses use the
//! firmware's standard SBA or Program Buffer commands when probe-rs selects
//! those engines.

pub(crate) mod protocol;
mod usb;

use std::fmt;

use nusb::MaybeFuture;

use self::protocol::{
    CONTROL_RELEASE_DEBUG_TRANSPORT, DEFAULT_SPEED_KHZ, HpmProbeError, InterfaceInfo,
    RISCV_DMI_READ, RISCV_DMI_RESET, RISCV_DMI_WRITE, RISCV_GET_INTERFACE_INFO,
    RISCV_READ_MEMORY_PROGBUF, RISCV_READ_MEMORY_SBA, RISCV_SELECT_HART,
    RISCV_WRITE_MEMORY_PROGBUF, RISCV_WRITE_MEMORY_SBA, STATUS_INVALID_ARGUMENT,
    STATUS_UNSUPPORTED_OPCODE, TUNNEL_CONTROL, TUNNEL_RISCV,
};
use self::usb::{HpmUsbTransport, is_probe};

use crate::architecture::riscv::dtm::hpm_probe_dtm::HpmProbeDtmBuilder;
use crate::probe::{
    DebugProbe, DebugProbeError, DebugProbeInfo, DebugProbeSelector, ProbeCreationError,
    ProbeFactory, ResetLine, WireProtocol,
    list::{ProbeListItem, usb_probe_accessibility},
};

const PROBE_NAME: &str = "HPM Probe";

/// Selects probe-side RISC-V commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HpmProbeMode {
    /// Use the probe's RISC-V DMI and memory commands.
    Direct,
}

/// Physical transport used for the JTAG protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HpmProbeTransport {
    /// Four-wire JTAG.
    Jtag,
    /// Two-wire IEEE 1149.7 cJTAG.
    Cjtag,
}

impl HpmProbeTransport {
    fn id(self) -> u8 {
        match self {
            Self::Jtag => protocol::JTAG_TRANSPORT,
            Self::Cjtag => protocol::CJTAG_TRANSPORT,
        }
    }
}

/// Probe-specific settings, applied before attaching to a target.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HpmProbeConfig {
    /// Choose probe-accelerated commands.
    pub mode: Option<HpmProbeMode>,
    /// Choose the physical JTAG transport.
    pub transport: Option<HpmProbeTransport>,
}

fn validate_memory_block_request(
    access_size: u8,
    address: u64,
    data_len: usize,
) -> Result<(), HpmProbeError> {
    if !matches!(access_size, 1 | 2 | 4 | 8) {
        return Err(HpmProbeError::Protocol(format!(
            "unsupported memory access size {access_size}"
        )));
    }
    if !address.is_multiple_of(access_size as u64) {
        return Err(HpmProbeError::Protocol(format!(
            "memory address {address:#x} is not aligned to {access_size} bytes"
        )));
    }
    if !data_len.is_multiple_of(access_size as usize) {
        return Err(HpmProbeError::Protocol(format!(
            "memory data length {data_len} is not a multiple of access size {access_size}"
        )));
    }
    if data_len != 0 && address.checked_add(data_len as u64 - 1).is_none() {
        return Err(HpmProbeError::Protocol(
            "memory range overflows the address space".into(),
        ));
    }
    Ok(())
}

/// Factory for HPM Prober devices.
#[derive(Debug)]
pub struct HpmProbeFactory;

impl fmt::Display for HpmProbeFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PROBE_NAME)
    }
}

impl ProbeFactory for HpmProbeFactory {
    fn open(&self, selector: &DebugProbeSelector) -> Result<Box<dyn DebugProbe>, DebugProbeError> {
        Ok(Box::new(HpmProbe::open_inner(
            selector,
            HpmProbeMode::Direct,
            HpmProbeTransport::Jtag,
            false,
        )?))
    }

    fn list_probes(&self) -> Vec<ProbeListItem> {
        let devices = match nusb::list_devices().wait() {
            Ok(devices) => devices,
            Err(error) => {
                tracing::warn!("failed to list HPM Prober devices: {error}");
                return Vec::new();
            }
        };

        devices
            .filter(is_probe)
            .map(|device| {
                let info = DebugProbeInfo::new(
                    device.product_string().unwrap_or(PROBE_NAME),
                    protocol::VENDOR_ID,
                    device.product_id(),
                    device.serial_number().map(str::to_owned),
                    &HpmProbeFactory,
                    None,
                    false,
                );
                ProbeListItem {
                    info,
                    accessibility: usb_probe_accessibility(&device),
                }
            })
            .collect()
    }
}

/// HPM Probe X driver. The standard probe discovery uses [`HpmProbeMode::Direct`].
pub struct HpmProbe {
    transport: HpmUsbTransport,
    name: String,
    speed_khz: u32,
    clock_hz: u32,
    configured: bool,
    release_needed: bool,
    attached: bool,
    interface_info: Option<InterfaceInfo>,
    reset_line: ResetLine,
    mode: HpmProbeMode,
    physical_transport: HpmProbeTransport,
}

impl fmt::Debug for HpmProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HpmProbe")
            .field("name", &self.name)
            .field("speed_khz", &self.speed_khz)
            .field("configured", &self.configured)
            .field("mode", &self.mode)
            .field("physical_transport", &self.physical_transport)
            .field("transport", &self.transport)
            .finish()
    }
}

impl HpmProbe {
    /// Open a Probe X with the requested RISC-V transport mode.
    pub fn open(
        selector: &DebugProbeSelector,
        mode: HpmProbeMode,
    ) -> Result<Self, DebugProbeError> {
        Self::open_with_transport(selector, mode, HpmProbeTransport::Jtag)
    }

    /// Open a Probe X with the requested mode and physical transport.
    pub fn open_with_transport(
        selector: &DebugProbeSelector,
        mode: HpmProbeMode,
        physical_transport: HpmProbeTransport,
    ) -> Result<Self, DebugProbeError> {
        Self::open_inner(selector, mode, physical_transport, true)
    }

    fn open_inner(
        selector: &DebugProbeSelector,
        mode: HpmProbeMode,
        physical_transport: HpmProbeTransport,
        configure_now: bool,
    ) -> Result<Self, DebugProbeError> {
        let mut devices = nusb::list_devices().wait().map_err(|error| {
            DebugProbeError::ProbeCouldNotBeCreated(ProbeCreationError::Usb(error.into()))
        })?;

        let device = devices
            .find(|device| is_probe(device) && selector.matches(device))
            .ok_or(DebugProbeError::ProbeCouldNotBeCreated(
                ProbeCreationError::NotFound,
            ))?;

        let name = device.product_string().unwrap_or(PROBE_NAME).to_string();
        let transport = HpmUsbTransport::open(&device).map_err(|error| {
            DebugProbeError::ProbeCouldNotBeCreated(ProbeCreationError::Usb(error.into()))
        })?;

        Ok(Self::new(
            transport,
            name,
            mode,
            physical_transport,
            configure_now,
        )?)
    }

    fn new(
        transport: HpmUsbTransport,
        name: String,
        mode: HpmProbeMode,
        physical_transport: HpmProbeTransport,
        configure_now: bool,
    ) -> Result<Self, HpmProbeError> {
        let mut probe = Self {
            transport,
            name,
            speed_khz: DEFAULT_SPEED_KHZ,
            clock_hz: DEFAULT_SPEED_KHZ * 1_000,
            configured: false,
            release_needed: false,
            attached: false,
            interface_info: None,
            reset_line: ResetLine::default(),
            mode,
            physical_transport,
        };

        probe.handshake()?;
        if configure_now {
            probe.configure_transport(DEFAULT_SPEED_KHZ * 1_000)?;
        }
        Ok(probe)
    }

    /// Apply driver settings before attaching. Unspecified settings retain their values.
    pub fn configure(&mut self, config: &HpmProbeConfig) -> Result<(), DebugProbeError> {
        if self.attached {
            return Err(DebugProbeError::Attached);
        }
        let mode = config.mode.unwrap_or(self.mode);
        let physical_transport = config.transport.unwrap_or(self.physical_transport);
        if mode == self.mode && physical_transport == self.physical_transport {
            return Ok(());
        }
        self.mode = mode;
        self.physical_transport = physical_transport;
        if self.configured {
            self.configure_transport(self.clock_hz)?;
        }
        Ok(())
    }

    fn handshake(&mut self) -> Result<(), HpmProbeError> {
        let response = self.transport.exchange(
            TUNNEL_CONTROL,
            &[protocol::CONTROL_HELLO, 0x01, 0x01, 0x00, 0x00, 0x20],
        )?;
        let hello = protocol::parse_hello(&response)?;
        self.transport.set_max_payload(hello.max_payload);
        tracing::debug!(
            "HPM Prober protocol {}.{}, payload limit {}, nonce {:#010x}",
            hello.major,
            hello.minor,
            hello.max_payload,
            hello.nonce
        );
        Ok(())
    }

    fn configure_transport(&mut self, clock_hz: u32) -> Result<u32, HpmProbeError> {
        let payload = protocol::configure_request(self.physical_transport.id(), clock_hz);
        // Firmware releases the previous transport before attempting a reconfiguration.
        self.configured = false;
        // Even a malformed or interrupted reply may follow a successful acquisition.
        self.release_needed = true;
        self.interface_info = None;
        let response = self.transport.exchange(TUNNEL_CONTROL, &payload)?;
        let actual_hz = protocol::parse_configure_clock(&response, self.physical_transport.id())?;
        self.configured = true;
        self.clock_hz = actual_hz;
        self.speed_khz = ((u64::from(actual_hz) + 500) / 1_000) as u32;
        Ok(actual_hz)
    }

    fn ensure_configured(&mut self) -> Result<(), HpmProbeError> {
        if !self.configured {
            self.configure_transport(self.clock_hz)?;
        }
        Ok(())
    }

    pub(crate) fn interface_info(&mut self) -> Result<InterfaceInfo, HpmProbeError> {
        if let Some(info) = self.interface_info {
            return Ok(info);
        }
        self.ensure_configured()?;
        let response = self
            .transport
            .exchange(TUNNEL_RISCV, &[RISCV_GET_INTERFACE_INFO])?;
        let info = protocol::parse_interface_info(&response)?;
        self.interface_info = Some(info);
        Ok(info)
    }

    pub(crate) fn dmi_read(&mut self, address: u32) -> Result<u32, HpmProbeError> {
        self.ensure_configured()?;
        let mut payload = vec![RISCV_DMI_READ];
        payload.extend_from_slice(&address.to_le_bytes());
        let response = self.transport.exchange(TUNNEL_RISCV, &payload)?;
        protocol::parse_dmi_read(&response)
    }

    pub(crate) fn dmi_write(&mut self, address: u32, value: u32) -> Result<(), HpmProbeError> {
        self.ensure_configured()?;
        let mut payload = vec![RISCV_DMI_WRITE];
        payload.extend_from_slice(&address.to_le_bytes());
        payload.extend_from_slice(&value.to_le_bytes());
        self.transport.exchange(TUNNEL_RISCV, &payload)?;
        Ok(())
    }

    pub(crate) fn dmi_reset(&mut self) -> Result<(), HpmProbeError> {
        self.ensure_configured()?;
        self.transport.exchange(TUNNEL_RISCV, &[RISCV_DMI_RESET])?;
        Ok(())
    }

    pub(crate) fn select_hart(&mut self, hart: u32) -> Result<(), HpmProbeError> {
        let mut payload = vec![RISCV_SELECT_HART];
        payload.extend_from_slice(&hart.to_le_bytes());
        let reply = self.transport.exchange(TUNNEL_RISCV, &payload)?;
        if reply.len() != 11 || u32::from_le_bytes(reply[2..6].try_into().unwrap()) != hart {
            return Err(HpmProbeError::Protocol(
                "invalid SELECT_HART response".into(),
            ));
        }
        Ok(())
    }

    /// Largest data payload carried by one probe-side memory command, matching
    /// the firmware's public memory-command payload limit.
    const MEMORY_BLOCK_BYTES: usize = 8176;

    pub(crate) fn read_memory_sba(
        &mut self,
        access_size: u8,
        address: u64,
        data: &mut [u8],
    ) -> Result<(), HpmProbeError> {
        self.read_memory_block(RISCV_READ_MEMORY_SBA, access_size, address, data)
    }

    pub(crate) fn read_memory_progbuf(
        &mut self,
        access_size: u8,
        address: u64,
        data: &mut [u8],
    ) -> Result<(), HpmProbeError> {
        self.read_memory_block(RISCV_READ_MEMORY_PROGBUF, access_size, address, data)
    }

    fn read_memory_block(
        &mut self,
        opcode: u8,
        access_size: u8,
        address: u64,
        data: &mut [u8],
    ) -> Result<(), HpmProbeError> {
        self.ensure_configured()?;
        validate_memory_block_request(access_size, address, data.len())?;

        let block_bytes = Self::memory_block_bytes(self.transport.max_payload(), access_size)?;
        for (index, chunk) in data.chunks_mut(block_bytes).enumerate() {
            let chunk_address = address + (index * block_bytes) as u64;
            let count = (chunk.len() / access_size as usize) as u16;

            let mut payload = Vec::with_capacity(12);
            payload.push(opcode);
            payload.push(access_size);
            payload.extend_from_slice(&count.to_le_bytes());
            payload.extend_from_slice(&chunk_address.to_le_bytes());

            let response = self.transport.exchange(TUNNEL_RISCV, &payload)?;
            let expected = 2 + chunk.len();
            if response.len() != expected {
                return Err(HpmProbeError::Protocol(format!(
                    "memory read returned {} bytes, expected {expected}",
                    response.len()
                )));
            }
            chunk.copy_from_slice(&response[2..expected]);
        }

        Ok(())
    }

    pub(crate) fn write_memory_sba(
        &mut self,
        access_size: u8,
        address: u64,
        data: &[u8],
    ) -> Result<(), HpmProbeError> {
        self.write_memory_block(RISCV_WRITE_MEMORY_SBA, access_size, address, data)
    }

    pub(crate) fn write_memory_progbuf(
        &mut self,
        access_size: u8,
        address: u64,
        data: &[u8],
    ) -> Result<(), HpmProbeError> {
        self.write_memory_block(RISCV_WRITE_MEMORY_PROGBUF, access_size, address, data)
    }

    fn write_memory_block(
        &mut self,
        opcode: u8,
        access_size: u8,
        address: u64,
        data: &[u8],
    ) -> Result<(), HpmProbeError> {
        self.ensure_configured()?;
        validate_memory_block_request(access_size, address, data.len())?;

        let block_bytes = Self::memory_block_bytes(self.transport.max_payload(), access_size)?;
        for (index, chunk) in data.chunks(block_bytes).enumerate() {
            let chunk_address = address + (index * block_bytes) as u64;
            let count = (chunk.len() / access_size as usize) as u16;

            let mut payload = Vec::with_capacity(12 + chunk.len());
            payload.push(opcode);
            payload.push(access_size);
            payload.extend_from_slice(&count.to_le_bytes());
            payload.extend_from_slice(&chunk_address.to_le_bytes());
            payload.extend_from_slice(chunk);

            let response = self.transport.exchange(TUNNEL_RISCV, &payload)?;
            if response.len() != 2 {
                return Err(HpmProbeError::Protocol(
                    "invalid memory write response".into(),
                ));
            }
        }

        Ok(())
    }

    fn memory_block_bytes(max_payload: usize, access_size: u8) -> Result<usize, HpmProbeError> {
        let bytes = Self::MEMORY_BLOCK_BYTES.min(max_payload.saturating_sub(12));
        let aligned = bytes / access_size as usize * access_size as usize;
        if aligned == 0 {
            return Err(HpmProbeError::Protocol(
                "negotiated payload is too small for memory access".into(),
            ));
        }
        Ok(aligned)
    }

    fn release(&mut self) -> Result<(), HpmProbeError> {
        if self.release_needed {
            self.configured = false;
            self.release_needed = false;
            self.interface_info = None;
            self.transport
                .exchange(TUNNEL_CONTROL, &[CONTROL_RELEASE_DEBUG_TRANSPORT])?;
        }
        Ok(())
    }
}

impl Drop for HpmProbe {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            tracing::warn!("failed to release the HPM Prober debug transport: {error}");
        }
    }
}

impl DebugProbe for HpmProbe {
    fn get_name(&self) -> &str {
        &self.name
    }

    fn speed_khz(&self) -> u32 {
        self.speed_khz
    }

    fn set_speed(&mut self, speed_khz: u32) -> Result<u32, DebugProbeError> {
        if speed_khz == 0 {
            return Err(DebugProbeError::UnsupportedSpeed(speed_khz));
        }
        let hz = speed_khz
            .checked_mul(1_000)
            .ok_or(DebugProbeError::UnsupportedSpeed(speed_khz))?;
        match self.configure_transport(hz) {
            Ok(_) => Ok(self.speed_khz),
            Err(HpmProbeError::Firmware {
                status: STATUS_UNSUPPORTED_OPCODE | STATUS_INVALID_ARGUMENT,
                ..
            }) => Err(DebugProbeError::UnsupportedSpeed(speed_khz)),
            Err(error) => Err(error.into()),
        }
    }

    fn attach(&mut self) -> Result<(), DebugProbeError> {
        if !self.configured {
            self.ensure_configured()?;
        }
        self.attached = true;
        Ok(())
    }

    fn detach(&mut self) -> Result<(), crate::Error> {
        self.attached = false;
        self.release().map_err(DebugProbeError::from)?;
        Ok(())
    }

    fn target_reset(&mut self) -> Result<(), DebugProbeError> {
        self.reset_line.check_assert()?;
        Err(DebugProbeError::CommandNotSupportedByProbe {
            command_name: "physical reset",
        })
    }

    fn target_reset_assert(&mut self) -> Result<(), DebugProbeError> {
        self.reset_line.check_assert()?;
        Err(DebugProbeError::CommandNotSupportedByProbe {
            command_name: "physical reset",
        })
    }

    fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError> {
        Err(DebugProbeError::CommandNotSupportedByProbe {
            command_name: "physical reset",
        })
    }

    fn set_reset_line_connected(&mut self, connected: bool) {
        self.reset_line.set_connected(connected);
    }

    fn select_protocol(&mut self, protocol: WireProtocol) -> Result<(), DebugProbeError> {
        match protocol {
            WireProtocol::Jtag => Ok(()),
            WireProtocol::Swd => Err(DebugProbeError::UnsupportedProtocol(protocol)),
        }
    }

    fn active_protocol(&self) -> Option<WireProtocol> {
        Some(WireProtocol::Jtag)
    }

    fn has_riscv_interface(&self) -> bool {
        true
    }

    fn try_get_riscv_interface_builder<'probe>(
        &'probe mut self,
    ) -> Result<
        Box<
            dyn crate::architecture::riscv::communication_interface::RiscvInterfaceBuilder<'probe>
                + 'probe,
        >,
        crate::architecture::riscv::communication_interface::RiscvError,
    > {
        Ok(Box::new(HpmProbeDtmBuilder::new(self)))
    }

    fn into_probe(self: Box<Self>) -> Box<dyn DebugProbe> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_requests_check_alignment_overflow_and_packet_limits() {
        for size in [1, 2, 4, 8] {
            assert!(validate_memory_block_request(size, 0x1000, 32).is_ok());
            assert_eq!(HpmProbe::memory_block_bytes(8192, size).unwrap(), 8176);
        }
        assert!(validate_memory_block_request(3, 0x1000, 12).is_err());
        assert!(validate_memory_block_request(4, 0x1001, 4).is_err());
        assert!(validate_memory_block_request(4, 0x1000, 6).is_err());
        assert!(validate_memory_block_request(4, u64::MAX - 3, 8).is_err());
        assert_eq!(HpmProbe::memory_block_bytes(64, 8).unwrap(), 48);
        assert!(HpmProbe::memory_block_bytes(19, 8).is_err());
    }
}
