//! USB transport for the HPM Prober vendor tunnel.

use std::fmt;
use std::io::{Read, Write};
use std::time::Duration;

use nusb::io::{EndpointRead, EndpointWrite};
use nusb::transfer::{Bulk, In, Out};
use nusb::{Device, Interface, MaybeFuture};

use super::protocol::{
    self, HpmProbeError, LEGACY_PRODUCT_ID, MAX_FRAME_SIZE, MAX_PAYLOAD, PRODUCT_ID, VENDOR_ID,
};

const INTERFACE: u8 = 0;
const EP_OUT: u8 = 0x02;
const EP_IN: u8 = 0x81;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct HpmUsbTransport {
    _device: Device,
    _interface: Interface,
    reader: EndpointRead<Bulk>,
    writer: EndpointWrite<Bulk>,
    max_payload: usize,
    failed: bool,
}

impl fmt::Debug for HpmUsbTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HpmUsbTransport")
            .field("max_payload", &self.max_payload)
            .finish()
    }
}

impl HpmUsbTransport {
    pub(crate) fn open(info: &nusb::DeviceInfo) -> Result<Self, nusb::Error> {
        let device = info.open().wait()?;

        #[cfg(target_os = "macos")]
        if device.active_configuration().is_err() {
            device.set_configuration(1).wait()?;
        }

        #[cfg(target_os = "linux")]
        let interface = device.detach_and_claim_interface(INTERFACE).wait()?;
        #[cfg(not(target_os = "linux"))]
        let interface = device.claim_interface(INTERFACE).wait()?;

        let reader = interface
            .endpoint::<Bulk, In>(EP_IN)?
            .reader(MAX_FRAME_SIZE)
            .with_num_transfers(4)
            .with_read_timeout(IO_TIMEOUT);
        let writer = interface
            .endpoint::<Bulk, Out>(EP_OUT)?
            .writer(MAX_FRAME_SIZE)
            .with_num_transfers(4)
            .with_write_timeout(IO_TIMEOUT);

        Ok(Self {
            _device: device,
            _interface: interface,
            reader,
            writer,
            max_payload: MAX_PAYLOAD,
            failed: false,
        })
    }

    pub(crate) fn set_max_payload(&mut self, maximum: usize) {
        self.max_payload = maximum.min(MAX_PAYLOAD);
    }

    pub(crate) fn max_payload(&self) -> usize {
        self.max_payload
    }

    pub(crate) fn exchange(
        &mut self,
        tunnel: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, HpmProbeError> {
        if self.failed {
            return Err(HpmProbeError::SessionLost);
        }
        let result = self.exchange_inner(tunnel, payload);
        if let Err(error) = &result
            && !matches!(error, HpmProbeError::Firmware { .. })
        {
            // A late reply after an I/O error could otherwise be mistaken for
            // the next request's response. Reopen instead of replaying it.
            self.failed = true;
        }
        result
    }

    fn exchange_inner(&mut self, tunnel: u8, payload: &[u8]) -> Result<Vec<u8>, HpmProbeError> {
        if payload.is_empty() {
            return Err(HpmProbeError::Protocol("empty request payload".into()));
        }
        if payload.len() > self.max_payload {
            return Err(HpmProbeError::Protocol(format!(
                "request payload {} exceeds negotiated maximum {}",
                payload.len(),
                self.max_payload
            )));
        }

        let frame = protocol::encode_frame(tunnel, payload);
        self.writer.write_all(&frame)?;
        // Each tunnel frame must end at a USB transfer boundary. submit_end
        // emits the short packet / ZLP the firmware parser expects.
        self.writer.submit_end();
        self.writer.flush()?;

        let mut header = [0u8; protocol::FRAME_HEADER_SIZE];
        self.read_exact(&mut header)?;
        let response_tunnel = header[0];
        let flags = header[1];
        let payload_len = u16::from_le_bytes([header[2], header[3]]) as usize;
        if payload_len > self.max_payload {
            return Err(HpmProbeError::Protocol(format!(
                "response payload {payload_len} exceeds negotiated maximum {}",
                self.max_payload
            )));
        }

        let mut response = vec![0u8; payload_len];
        self.read_exact(&mut response)?;

        if response_tunnel != tunnel {
            return Err(HpmProbeError::Protocol(format!(
                "response tunnel 0x{response_tunnel:02x}, expected 0x{tunnel:02x}"
            )));
        }
        if flags & !0x04 != 0x01 {
            return Err(HpmProbeError::Protocol(format!(
                "unexpected response flags 0x{flags:02x}"
            )));
        }
        if response.len() < 2 {
            return Err(HpmProbeError::Protocol(
                "response does not contain opcode and status".into(),
            ));
        }
        if response[0] != payload[0] {
            return Err(HpmProbeError::Protocol(format!(
                "response opcode 0x{:02x}, expected 0x{:02x}",
                response[0], payload[0]
            )));
        }
        if response[1] != protocol::STATUS_OK {
            return Err(HpmProbeError::Firmware {
                tunnel,
                opcode: response[0],
                status: response[1],
                status_name: protocol::status_name(response[1]),
            });
        }

        Ok(response)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), HpmProbeError> {
        let mut offset = 0;
        while offset < buf.len() {
            match self.reader.read(&mut buf[offset..]) {
                Ok(0) => return Err(HpmProbeError::SessionLost),
                Ok(read) => offset += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Err(HpmProbeError::Timeout);
                }
                Err(error) => return Err(HpmProbeError::Usb(error)),
            }
        }
        Ok(())
    }
}

pub(crate) fn is_probe(info: &nusb::DeviceInfo) -> bool {
    info.vendor_id() == VENDOR_ID && matches!(info.product_id(), PRODUCT_ID | LEGACY_PRODUCT_ID)
}
