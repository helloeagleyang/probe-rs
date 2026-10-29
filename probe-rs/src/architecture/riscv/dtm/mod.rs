//! Debug Transport Module (DTM) access abstraction

pub(crate) mod hpm_probe_dtm;
pub mod jtag_dtm;
pub(crate) mod mem_ap_dtm;
pub(crate) mod wlink_dtm;

use crate::architecture::riscv::communication_interface::RiscvError;
use crate::probe::queue::Handle;
use crate::probe::{CommandResult, DebugProbeError};
use std::fmt;
use std::time::Duration;

/// Debug Transport Module (DTM) access abstraction
pub trait DtmAccess: fmt::Debug {
    /// Perform interface-specific initialisation upon attaching.
    fn init(&mut self) -> Result<(), RiscvError> {
        Ok(())
    }

    /// Asserts a reset of the physical pins
    fn target_reset_assert(&mut self) -> Result<(), DebugProbeError>;

    /// Deasserts a reset of the physical pins
    fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError>;

    /// Clear the sticky error state, if applicable
    fn clear_error_state(&mut self) -> Result<(), RiscvError>;

    /// Read previously scheduled `dmi` register accesses
    fn read_deferred_result(
        &mut self,
        index: Handle<CommandResult>,
    ) -> Result<CommandResult, RiscvError>;

    /// Execute scheduled dmi accesses
    fn execute(&mut self) -> Result<(), RiscvError>;

    /// Schedule a write to an address on the `dmi` bus.
    ///
    /// Returns None if the underlying transport protocol does
    /// not return the value at the address on write
    fn schedule_write(
        &mut self,
        address: u64,
        value: u32,
    ) -> Result<Option<Handle<CommandResult>>, RiscvError>;

    /// Schedule a read from an address on the `dmi` bus.
    fn schedule_read(&mut self, address: u64) -> Result<Handle<CommandResult>, RiscvError>;

    /// Schedule consecutive reads of one register, preserving their order.
    fn schedule_read_repeated(
        &mut self,
        address: u64,
        count: usize,
        _extra_idle: u8,
    ) -> Result<Vec<Handle<CommandResult>>, RiscvError> {
        (0..count).map(|_| self.schedule_read(address)).collect()
    }

    /// Whether repeated abstract-command triggers can be batched with idle clocks.
    fn supports_abstract_batching(&self) -> bool {
        false
    }

    /// Queue register reads with the indicated extra idle clocks after each request.
    fn schedule_read_sequence(
        &mut self,
        requests: &[(u64, u8)],
    ) -> Result<Vec<Handle<CommandResult>>, RiscvError> {
        requests
            .iter()
            .map(|&(address, _)| self.schedule_read(address))
            .collect()
    }

    /// Queue register writes with the indicated extra idle clocks after each request.
    fn schedule_write_sequence(&mut self, requests: &[(u64, u32, u8)]) -> Result<(), RiscvError> {
        for &(address, value, _) in requests {
            self.schedule_write(address, value)?;
        }
        Ok(())
    }

    /// Queue repeated writes to an autoexec register, with additional idle clocks.
    fn schedule_write_repeated(
        &mut self,
        address: u64,
        values: &[u32],
        _extra_idle: u8,
    ) -> Result<(), RiscvError> {
        for value in values {
            self.schedule_write(address, *value)?;
        }
        Ok(())
    }

    /// Read an address on the `dmi` bus. If a busy value is returned, the access is
    /// retried until the transfer either succeeds, or the timeout expires.
    fn read_with_timeout(&mut self, address: u64, timeout: Duration) -> Result<u32, RiscvError>;

    /// Write an address to the `dmi` bus. If a busy value is returned, the access is
    /// retried until the transfer either succeeds, or the timeout expires.
    ///
    /// Returns None if the underlying protocol does not return the value on write
    fn write_with_timeout(
        &mut self,
        address: u64,
        value: u32,
        timeout: Duration,
    ) -> Result<Option<u32>, RiscvError>;

    /// Returns an idcode used for chip detection
    fn read_idcode(&mut self) -> Result<Option<u32>, DebugProbeError>;

    /// Whether the transport advertises a probe-side memory engine.
    fn supports_memory_access(&self, _progbuf: bool) -> bool {
        false
    }

    /// Optional probe-side block read through the target's System Bus Access
    /// engine. Returning `None` keeps the generic DMI-driven SBA path.
    fn read_memory_sba(
        &mut self,
        _access_size: u8,
        _address: u64,
        _data: &mut [u8],
    ) -> Option<Result<(), RiscvError>> {
        None
    }

    /// Optional probe-side block write through the target's System Bus Access
    /// engine. Returning `None` keeps the generic DMI-driven SBA path.
    fn write_memory_sba(
        &mut self,
        _access_size: u8,
        _address: u64,
        _data: &[u8],
    ) -> Option<Result<(), RiscvError>> {
        None
    }

    /// Optional probe-side block read through the target's Program Buffer.
    /// Returning `None` keeps the generic DMI-driven Program Buffer path.
    fn read_memory_progbuf(
        &mut self,
        _hart: u32,
        _access_size: u8,
        _address: u64,
        _data: &mut [u8],
    ) -> Option<Result<(), RiscvError>> {
        None
    }

    /// Optional probe-side block write through the target's Program Buffer.
    /// Returning `None` keeps the generic DMI-driven Program Buffer path.
    fn write_memory_progbuf(
        &mut self,
        _hart: u32,
        _access_size: u8,
        _address: u64,
        _data: &[u8],
    ) -> Option<Result<(), RiscvError>> {
        None
    }
}
