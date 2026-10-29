//! RISC-V DTM backed by the HPM Prober vendor tunnel.

use std::time::Duration;

use crate::architecture::riscv::communication_interface::{
    RiscvCommunicationInterface, RiscvDebugInterfaceState, RiscvError, RiscvInterfaceBuilder,
};
use crate::architecture::riscv::dtm::DtmAccess;
use crate::architecture::riscv::dtm::jtag_dtm::DmiOperation;
use crate::probe::hpmprobe::{
    HpmProbe,
    protocol::{CAP_MEMORY_PROGBUF, CAP_MEMORY_SBA, HpmProbeError, STATUS_IO_FAULT},
};
use crate::probe::queue::{Handle, HandleId, Results};
use crate::probe::{CommandResult, DebugProbe, DebugProbeError};

#[derive(Debug, Default)]
pub(crate) struct HpmDtmState {
    pending: Vec<(Option<HandleId>, DmiOperation)>,
    results: Results,
    idcode: Option<u32>,
    capabilities: u32,
}

pub(crate) struct HpmProbeDtmBuilder<'probe>(&'probe mut HpmProbe);

impl<'probe> HpmProbeDtmBuilder<'probe> {
    pub(crate) fn new(probe: &'probe mut HpmProbe) -> Self {
        Self(probe)
    }
}

impl<'probe> RiscvInterfaceBuilder<'probe> for HpmProbeDtmBuilder<'probe> {
    fn create_state(&self) -> RiscvDebugInterfaceState {
        RiscvDebugInterfaceState::new(Box::<HpmDtmState>::default())
    }

    fn attach<'state>(
        self: Box<Self>,
        state: &'state mut RiscvDebugInterfaceState,
    ) -> Result<RiscvCommunicationInterface<'state>, DebugProbeError>
    where
        'probe: 'state,
    {
        let dtm_state = state.dtm_state.downcast_mut::<HpmDtmState>().unwrap();

        Ok(RiscvCommunicationInterface::new(
            Box::new(HpmDtm::new(self.0, dtm_state)),
            &mut state.interface_state,
        ))
    }
}

#[derive(Debug)]
struct HpmDtm<'probe> {
    probe: &'probe mut HpmProbe,
    state: &'probe mut HpmDtmState,
}

impl<'probe> HpmDtm<'probe> {
    fn new(probe: &'probe mut HpmProbe, state: &'probe mut HpmDtmState) -> Self {
        Self { probe, state }
    }
}

fn probe_error(error: HpmProbeError) -> RiscvError {
    RiscvError::DebugProbe(DebugProbeError::from(error))
}

fn dmi_address(address: u64) -> Result<u32, RiscvError> {
    u32::try_from(address).map_err(|_| {
        RiscvError::DebugProbe(DebugProbeError::Other(format!(
            "DMI address {address:#x} exceeds 32 bits"
        )))
    })
}

fn memory_error(error: HpmProbeError) -> RiscvError {
    match error {
        HpmProbeError::Firmware {
            status: STATUS_IO_FAULT,
            ..
        } => RiscvError::SystemBusAccess,
        error => probe_error(error),
    }
}

impl DtmAccess for HpmDtm<'_> {
    fn init(&mut self) -> Result<(), RiscvError> {
        let info = self.probe.interface_info().map_err(probe_error)?;
        self.state.idcode = Some(info.idcode);
        self.state.capabilities = info.capabilities;
        tracing::debug!(
            "HPM Prober DTM: protocol {}.{}, idcode {:#010x}, dtmcs {:#010x}",
            info.major,
            info.minor,
            info.idcode,
            info.dtmcs
        );
        Ok(())
    }

    fn target_reset_assert(&mut self) -> Result<(), DebugProbeError> {
        self.probe.target_reset_assert()
    }

    fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError> {
        self.probe.target_reset_deassert()
    }

    fn clear_error_state(&mut self) -> Result<(), RiscvError> {
        self.probe.dmi_reset().map_err(probe_error)
    }

    fn read_deferred_result(
        &mut self,
        index: Handle<CommandResult>,
    ) -> Result<CommandResult, RiscvError> {
        match self.state.results.take(index) {
            Ok(result) => Ok(result),
            Err(handle) => {
                self.execute()?;
                self.state
                    .results
                    .take(handle)
                    .map_err(|_| RiscvError::BatchedResultNotAvailable)
            }
        }
    }

    fn execute(&mut self) -> Result<(), RiscvError> {
        for (id, operation) in std::mem::take(&mut self.state.pending) {
            let result = match operation {
                DmiOperation::Read { address } => self
                    .probe
                    .dmi_read(dmi_address(address)?)
                    .map_err(probe_error)?,
                DmiOperation::Write { address, value } => {
                    self.probe
                        .dmi_write(dmi_address(address)?, value)
                        .map_err(probe_error)?;
                    0
                }
                DmiOperation::NoOp => 0,
            };
            if let Some(handle_id) = id
                && handle_id.should_capture()
            {
                self.state
                    .results
                    .push(&handle_id, CommandResult::U32(result));
            }
        }
        Ok(())
    }

    fn schedule_write(
        &mut self,
        address: u64,
        value: u32,
    ) -> Result<Option<Handle<CommandResult>>, RiscvError> {
        let id = HandleId::new();
        self.state
            .pending
            .push((Some(id.clone()), DmiOperation::Write { address, value }));
        Ok(Some(Handle::from_id(id)))
    }

    fn schedule_read(&mut self, address: u64) -> Result<Handle<CommandResult>, RiscvError> {
        let id = HandleId::new();
        self.state
            .pending
            .push((Some(id.clone()), DmiOperation::Read { address }));
        Ok(Handle::from_id(id))
    }

    fn read_with_timeout(&mut self, address: u64, _timeout: Duration) -> Result<u32, RiscvError> {
        self.execute()?;
        self.probe
            .dmi_read(dmi_address(address)?)
            .map_err(probe_error)
    }

    fn write_with_timeout(
        &mut self,
        address: u64,
        value: u32,
        _timeout: Duration,
    ) -> Result<Option<u32>, RiscvError> {
        self.execute()?;
        self.probe
            .dmi_write(dmi_address(address)?, value)
            .map_err(probe_error)?;
        Ok(None)
    }

    fn read_idcode(&mut self) -> Result<Option<u32>, DebugProbeError> {
        if let Some(idcode) = self.state.idcode {
            return Ok(Some(idcode));
        }
        let info = self.probe.interface_info().map_err(DebugProbeError::from)?;
        self.state.idcode = Some(info.idcode);
        Ok(Some(info.idcode))
    }

    fn supports_memory_access(&self, progbuf: bool) -> bool {
        let capability = if progbuf {
            CAP_MEMORY_PROGBUF
        } else {
            CAP_MEMORY_SBA
        };
        self.state.capabilities & capability != 0
    }

    fn read_memory_sba(
        &mut self,
        access_size: u8,
        address: u64,
        data: &mut [u8],
    ) -> Option<Result<(), RiscvError>> {
        Some(self.execute().and_then(|()| {
            self.probe
                .read_memory_sba(access_size, address, data)
                .map_err(memory_error)
        }))
    }

    fn write_memory_sba(
        &mut self,
        access_size: u8,
        address: u64,
        data: &[u8],
    ) -> Option<Result<(), RiscvError>> {
        Some(self.execute().and_then(|()| {
            self.probe
                .write_memory_sba(access_size, address, data)
                .map_err(memory_error)
        }))
    }

    fn read_memory_progbuf(
        &mut self,
        hart: u32,
        access_size: u8,
        address: u64,
        data: &mut [u8],
    ) -> Option<Result<(), RiscvError>> {
        Some(
            self.execute()
                .and_then(|()| self.probe.select_hart(hart).map_err(probe_error))
                .and_then(|()| {
                    self.probe
                        .read_memory_progbuf(access_size, address, data)
                        .map_err(probe_error)
                }),
        )
    }

    fn write_memory_progbuf(
        &mut self,
        hart: u32,
        access_size: u8,
        address: u64,
        data: &[u8],
    ) -> Option<Result<(), RiscvError>> {
        Some(
            self.execute()
                .and_then(|()| self.probe.select_hart(hart).map_err(probe_error))
                .and_then(|()| {
                    self.probe
                        .write_memory_progbuf(access_size, address, data)
                        .map_err(probe_error)
                }),
        )
    }
}
