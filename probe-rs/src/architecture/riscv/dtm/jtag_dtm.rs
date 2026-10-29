//! Debug Transport Module (DTM) handling
//!
//! The DTM is responsible for access to the debug module.
//! Currently, only JTAG is supported.

use bitfield::bitfield;
use bitvec::field::BitField;
use bitvec::slice::BitSlice;
use bitvec::store::BitStore;
use std::time::{Duration, Instant};

use crate::architecture::riscv::communication_interface::{
    RiscvCommunicationInterface, RiscvDebugInterfaceState, RiscvError, RiscvInterfaceBuilder,
};
use crate::architecture::riscv::dtm::DtmAccess;
use crate::probe::DebugProbeError;
use crate::probe::common::valid_idcode;
use crate::probe::jtag::chain::JtagChain;
use crate::probe::queue::{BatchError, Handle, HandleId, JtagQueue, Results};
use crate::probe::{
    BitSequence, CommandResult, JtagBatch, JtagCommand, JtagWriteCommand, JtagWriteData,
    ShiftDrCommand, ShiftDrData,
};

#[derive(Debug, Default)]
struct DtmState {
    queued_commands: JtagQueue<DmiOperationError>,
    jtag_results: Results,
    // A scan reports the preceding request's status even when its own result
    // was discarded. Keep all scan responses until the batch has been checked.
    scan_checks: Vec<HandleId>,
    pipelined: bool,
    last_is_nop: bool,

    /// Number of address bits in the DMI register
    abits: u32,

    /// TCK cycles in Run-Test/Idle after each DMI access.
    idle_cycles: u8,
}

/// Object that can be used to build a RISC-V DTM interface
/// from a JTAG transport.
pub struct JtagDtmBuilder<'f> {
    chain: JtagChain<'f>,
}

impl<'f> JtagDtmBuilder<'f> {
    /// Create a new DTM Builder via a JTAG transport.
    pub fn new(probe: &'f mut dyn crate::probe::JtagChainAccess) -> Self {
        Self {
            chain: JtagChain::new(probe),
        }
    }
}

impl<'probe> RiscvInterfaceBuilder<'probe> for JtagDtmBuilder<'probe> {
    fn create_state(&self) -> RiscvDebugInterfaceState {
        RiscvDebugInterfaceState::new(Box::<DtmState>::default())
    }

    fn attach<'state>(
        self: Box<Self>,
        state: &'state mut RiscvDebugInterfaceState,
    ) -> Result<RiscvCommunicationInterface<'state>, DebugProbeError>
    where
        'probe: 'state,
    {
        let dtm_state = state.dtm_state.downcast_mut::<DtmState>().unwrap();

        Ok(RiscvCommunicationInterface::new(
            Box::new(JtagDtm::new(self.chain, dtm_state)),
            &mut state.interface_state,
        ))
    }

    fn attach_tunneled<'state>(
        self: Box<Self>,
        tunnel_ir_id: u32,
        tunnel_ir_width: u32,
        state: &'state mut RiscvDebugInterfaceState,
    ) -> Result<RiscvCommunicationInterface<'state>, DebugProbeError>
    where
        'probe: 'state,
    {
        let dtm_state = state.dtm_state.downcast_mut::<DtmState>().unwrap();

        Ok(RiscvCommunicationInterface::new(
            Box::new(TunneledJtagDtm::new(
                self.chain,
                tunnel_ir_id,
                tunnel_ir_width,
                dtm_state,
            )),
            &mut state.interface_state,
        ))
    }
}

/// Access to the Debug Transport Module (DTM),
/// which is used to communicate with the RISC-V debug module.
#[derive(Debug)]
pub struct JtagDtm<'probe> {
    probe: JtagChain<'probe>,
    state: &'probe mut DtmState,
}

impl<'probe> JtagDtm<'probe> {
    fn new(probe: JtagChain<'probe>, state: &'probe mut DtmState) -> Self {
        Self { probe, state }
    }

    fn tap_reset(&mut self) -> Result<(), DebugProbeError> {
        let mut batch = JtagBatch::new();
        self.probe.tap_reset(&mut batch);
        self.probe.run(batch).map(|_| ())
    }

    fn exchange_register(
        &mut self,
        address: u32,
        dr: BitSequence,
        idle_cycles: u32,
    ) -> Result<BitSequence, DebugProbeError> {
        if address > self.probe.params().max_ir_address() {
            return Err(DebugProbeError::Other(format!(
                "Invalid instruction register access: {address}"
            )));
        }

        let ir_len = self.probe.params().irlen;
        let mut batch = JtagBatch::new();
        let ir = BitSequence::from_bytes(&address.to_le_bytes(), ir_len);
        self.probe.shift_ir(&mut batch, &ir);
        let handle = self.probe.exchange_dr(&mut batch, &dr);
        self.probe.run_test_idle(&mut batch, idle_cycles);
        let mut results = self.probe.run(batch)?;
        results
            .take(handle)
            .map_err(|_| DebugProbeError::Other("missing JTAG capture result".into()))
    }

    fn transform_dmi_result<T: BitStore>(
        response_bits: &BitSlice<T>,
    ) -> Result<u32, DmiOperationError> {
        let response_value = response_bits.load_le::<u128>();

        // Verify that the transfer was ok
        let op = (response_value & DMI_OP_MASK) as u8;

        let status = DmiOperationStatus::parse(op).expect("INVALID DMI OP status");

        match status {
            DmiOperationStatus::Ok => Ok((response_value >> 2) as u32),

            DmiOperationStatus::Reserved => Err(DmiOperationError::Reserved),
            DmiOperationStatus::RequestInProgress => Err(DmiOperationError::RequestInProgress),
            DmiOperationStatus::OperationFailed => Err(DmiOperationError::OperationFailed),
        }
    }

    fn schedule_dmi_register_access(
        &mut self,
        op: DmiOperation,
    ) -> Result<Handle<CommandResult>, RiscvError> {
        self.schedule_dmi_register_access_with_idle(op, 0)
    }

    fn schedule_dmi_register_access_with_idle(
        &mut self,
        op: DmiOperation,
        extra_idle: u8,
    ) -> Result<Handle<CommandResult>, RiscvError> {
        let bytes = op.to_byte_batch();

        let bit_size = self.state.abits + DMI_ADDRESS_BIT_OFFSET;

        let handle = self.state.queued_commands.schedule(JtagWriteCommand {
            data: JtagWriteData {
                address: DMI_ADDRESS,
                data: BitSequence::from_bytes(&bytes, bit_size as usize),
                idle_cycles: self.state.idle_cycles.saturating_add(extra_idle) as u32,
            },
            transform: |_, result| Self::transform_dmi_result(result).map(CommandResult::U32),
        });
        self.state.scan_checks.push(handle.id().clone());
        self.state.last_is_nop = matches!(op, DmiOperation::NoOp);
        Ok(handle)
    }

    fn execute_with_timeout(&mut self, timeout: Duration) -> Result<(), RiscvError> {
        if !self.state.queued_commands.is_empty() && !self.state.last_is_nop {
            // The last request's status arrives on the next scan.
            self.schedule_dmi_register_access(DmiOperation::NoOp)?;
        }
        self.state.last_is_nop = false;
        let mut cmds = std::mem::take(&mut self.state.queued_commands);
        let checks = std::mem::take(&mut self.state.scan_checks);
        let pipelined = std::mem::take(&mut self.state.pipelined);

        let mut started = Instant::now();
        let mut previous_queue_len = cmds.len();
        while !cmds.is_empty() {
            match cmds.execute(|queue| self.probe.run_command_batch(queue)) {
                Ok(r) => {
                    drop(checks);
                    drop(cmds);
                    self.state.jtag_results.merge_missing_from(r);
                    return Ok(());
                }
                Err(e) => {
                    match e.error {
                        BatchError::Specific(error) => {
                            // error is now DmiOperationError, not Box<dyn Error>
                            match error {
                                DmiOperationError::RequestInProgress => {
                                    self.clear_error_state()?;

                                    if pipelined {
                                        // The target may have already consumed part of a
                                        // side-effecting read stream; do not replay it.
                                        return Err(RiscvError::DtmOperationFailed);
                                    }

                                    // A DMI scan reports the *previous* scan. The
                                    // request that returned busy must be sent again,
                                    // not just the scan that observed its response.
                                    cmds.consume(e.results.len().saturating_sub(1));
                                    self.state.jtag_results.merge_missing_from(e.results);

                                    self.state.idle_cycles =
                                        self.state.idle_cycles.saturating_add(1);
                                    for (_, command) in cmds.iter_mut() {
                                        match command {
                                            JtagCommand::WriteRegister(write) => {
                                                write.inner.idle_cycles =
                                                    write.inner.idle_cycles.saturating_add(1);
                                            }
                                            JtagCommand::ShiftDr(shift) => {
                                                shift.inner.idle_cycles =
                                                    shift.inner.idle_cycles.saturating_add(1);
                                            }
                                        }
                                    }
                                }
                                DmiOperationError::Reserved
                                | DmiOperationError::OperationFailed => {
                                    self.clear_error_state()?;
                                    return Err(RiscvError::DtmOperationFailed);
                                }
                            }
                        }
                        BatchError::Probe(error) => {
                            return Err(error.into());
                        }
                    }
                }
            }

            // If progress was made, reset the timeout.
            if cmds.len() != previous_queue_len {
                started = Instant::now();
                previous_queue_len = cmds.len();
            }

            let elapsed_time = started.elapsed();

            if elapsed_time > timeout {
                tracing::error!(
                    "Timeout ({timeout:?}) exceeded executing RISCV commands (elapsed: {elapsed_time:?})"
                );
                return Err(RiscvError::Timeout);
            }
        }

        Ok(())
    }
}

impl DtmAccess for JtagDtm<'_> {
    fn init(&mut self) -> Result<(), RiscvError> {
        self.tap_reset()?;
        let dtmcs_raw = self.exchange_register(
            DTMCS_ADDRESS,
            BitSequence::repeat(false, DTMCS_WIDTH as usize),
            0,
        )?;

        let raw_dtmcs = dtmcs_raw.as_bits().load_le::<u32>();

        if raw_dtmcs == 0 {
            return Err(RiscvError::NoRiscvTarget);
        }

        let dtmcs = Dtmcs(raw_dtmcs);

        tracing::debug!("{:?}", dtmcs);

        let abits = dtmcs.abits();
        let idle_cycles = dtmcs.idle();

        if dtmcs.version() != 1 {
            return Err(RiscvError::UnsupportedDebugTransportModuleVersion(
                dtmcs.version() as u8,
            ));
        }

        // Setup the number of idle cycles between JTAG accesses
        self.state.idle_cycles = idle_cycles as u8;
        self.state.abits = abits;

        Ok(())
    }

    fn target_reset_assert(&mut self) -> Result<(), DebugProbeError> {
        self.probe.target_reset_assert()
    }

    fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError> {
        self.probe.target_reset_deassert()
    }

    fn clear_error_state(&mut self) -> Result<(), RiscvError> {
        let mut dtmcs = Dtmcs(0);

        dtmcs.set_dmireset(true);

        let Dtmcs(reg_value) = dtmcs;

        let bytes = reg_value.to_le_bytes();

        self.exchange_register(
            DTMCS_ADDRESS,
            BitSequence::from_bytes(&bytes, DTMCS_WIDTH as usize),
            self.state.idle_cycles as u32,
        )?;

        Ok(())
    }

    fn read_deferred_result(
        &mut self,
        index: Handle<CommandResult>,
    ) -> Result<CommandResult, RiscvError> {
        match self.state.jtag_results.take(index) {
            Ok(result) => Ok(result),
            Err(handle) => {
                self.execute()?;
                // We can lose data if `execute` fails.
                self.state
                    .jtag_results
                    .take(handle)
                    .map_err(|_| RiscvError::BatchedResultNotAvailable)
            }
        }
    }

    fn execute(&mut self) -> Result<(), RiscvError> {
        self.execute_with_timeout(JTAG_COMMAND_TIMEOUT)
    }

    fn schedule_write(
        &mut self,
        address: u64,
        value: u32,
    ) -> Result<Option<Handle<CommandResult>>, RiscvError> {
        self.schedule_dmi_register_access(DmiOperation::Write { address, value })
            .map(Some)
    }

    fn schedule_read(&mut self, address: u64) -> Result<Handle<CommandResult>, RiscvError> {
        // Prepare the read by sending a read request with the register address
        self.schedule_dmi_register_access(DmiOperation::Read { address })?;

        // Read back the response from the previous request.
        self.schedule_dmi_register_access(DmiOperation::NoOp)
    }

    fn schedule_read_repeated(
        &mut self,
        address: u64,
        count: usize,
        extra_idle: u8,
    ) -> Result<Vec<Handle<CommandResult>>, RiscvError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        self.schedule_dmi_register_access_with_idle(DmiOperation::Read { address }, extra_idle)?;
        self.state.pipelined = true;
        let mut results = Vec::with_capacity(count);
        for _ in 1..count {
            results.push(self.schedule_dmi_register_access_with_idle(
                DmiOperation::Read { address },
                extra_idle,
            )?);
        }
        results.push(self.schedule_dmi_register_access(DmiOperation::NoOp)?);
        Ok(results)
    }

    fn supports_abstract_batching(&self) -> bool {
        true
    }

    fn schedule_read_sequence(
        &mut self,
        requests: &[(u64, u8)],
    ) -> Result<Vec<Handle<CommandResult>>, RiscvError> {
        let Some((&(first_address, first_idle), rest)) = requests.split_first() else {
            return Ok(Vec::new());
        };
        self.schedule_dmi_register_access_with_idle(
            DmiOperation::Read {
                address: first_address,
            },
            first_idle,
        )?;
        self.state.pipelined = true;
        let mut results = Vec::with_capacity(requests.len());
        for &(address, idle) in rest {
            results.push(
                self.schedule_dmi_register_access_with_idle(DmiOperation::Read { address }, idle)?,
            );
        }
        results.push(self.schedule_dmi_register_access(DmiOperation::NoOp)?);
        Ok(results)
    }

    fn schedule_write_sequence(&mut self, requests: &[(u64, u32, u8)]) -> Result<(), RiscvError> {
        if requests.is_empty() {
            return Ok(());
        }
        for &(address, value, idle) in requests {
            self.schedule_dmi_register_access_with_idle(
                DmiOperation::Write { address, value },
                idle,
            )?;
        }
        self.schedule_dmi_register_access(DmiOperation::NoOp)?;
        self.state.pipelined = true;
        Ok(())
    }

    fn schedule_write_repeated(
        &mut self,
        address: u64,
        values: &[u32],
        extra_idle: u8,
    ) -> Result<(), RiscvError> {
        if values.is_empty() {
            return Ok(());
        }
        for &value in values {
            self.schedule_dmi_register_access_with_idle(
                DmiOperation::Write { address, value },
                extra_idle,
            )?;
        }
        // DMI errors are sticky, so the final NOP reports an error from any
        // write in this batch. An error is returned without replaying writes.
        self.schedule_dmi_register_access(DmiOperation::NoOp)?;
        self.state.pipelined = true;
        Ok(())
    }

    fn read_with_timeout(&mut self, address: u64, timeout: Duration) -> Result<u32, RiscvError> {
        let handle = self.schedule_read(address)?;
        self.execute_with_timeout(timeout)?;
        Ok(self.read_deferred_result(handle)?.into_u32())
    }

    fn write_with_timeout(
        &mut self,
        address: u64,
        value: u32,
        timeout: Duration,
    ) -> Result<Option<u32>, RiscvError> {
        self.schedule_write(address, value)?;
        let handle = self.schedule_dmi_register_access(DmiOperation::NoOp)?;
        self.execute_with_timeout(timeout)?;
        Ok(Some(self.read_deferred_result(handle)?.into_u32()))
    }

    fn read_idcode(&mut self) -> Result<Option<u32>, DebugProbeError> {
        // Reset the JTAG state machine to Test-Logic-Reset before reading the IDCODE.
        // This is required when read_idcode() is called without a prior dtm.init() (e.g.
        // from `probe-rs info`), because the TAP state may be indeterminate after attach.
        // After reset, the IDCODE instruction is automatically loaded into IR, and we then
        // explicitly load it again via read_register(0x1, …) to be consistent with all paths.
        self.tap_reset()?;
        let value = self.exchange_register(0x1, BitSequence::repeat(false, 32), 0)?;

        Ok(valid_idcode(value.as_bits().load_le::<u32>()))
    }
}

/// Access to the tunneled Debug Transport Module (DTM),
/// which is used to communicate with the RISC-V debug module.
///
/// The protocol was originally created by SiFive for their IP, but many others implement it. For
/// reference, see the `riscv use_bscan_tunnel` command of riscv-openocd.
///
/// Tunneled DR scan:
/// 1. Select IR (0) or DR (1): 1 bit
/// 2. Width of tunneled scan: 7 bits
/// 3. Tunneled scan bits: width + 1 bits
/// 4. Set tunnel to idle: 3 zero bits
#[derive(Debug)]
pub struct TunneledJtagDtm<'probe> {
    probe: JtagChain<'probe>,
    state: &'probe mut DtmState,
    select_dtmcs: JtagWriteData,
    select_dmi: JtagWriteData,
}

impl<'probe> TunneledJtagDtm<'probe> {
    fn new(
        probe: JtagChain<'probe>,
        tunnel_ir_id: u32,
        tunnel_ir_width: u32,
        state: &'probe mut DtmState,
    ) -> Self {
        Self {
            probe,
            state,
            select_dtmcs: tunnel_select_data(tunnel_ir_id, tunnel_ir_width, DTMCS_ADDRESS),
            select_dmi: tunnel_select_data(tunnel_ir_id, tunnel_ir_width, DMI_ADDRESS),
        }
    }

    fn tap_reset(&mut self) -> Result<(), DebugProbeError> {
        let mut batch = JtagBatch::new();
        self.probe.tap_reset(&mut batch);
        self.probe.run(batch).map(|_| ())
    }

    fn exchange_register(
        &mut self,
        write: &JtagWriteData,
        idle_cycles: u32,
    ) -> Result<BitSequence, DebugProbeError> {
        self.exchange_register_at(write.address, write.data.clone(), idle_cycles)
    }

    fn exchange_register_at(
        &mut self,
        address: u32,
        dr: BitSequence,
        idle_cycles: u32,
    ) -> Result<BitSequence, DebugProbeError> {
        if address > self.probe.params().max_ir_address() {
            return Err(DebugProbeError::Other(format!(
                "Invalid instruction register access: {address}"
            )));
        }

        let ir_len = self.probe.params().irlen;
        let mut batch = JtagBatch::new();
        let ir = BitSequence::from_bytes(&address.to_le_bytes(), ir_len);
        self.probe.shift_ir(&mut batch, &ir);
        let handle = self.probe.exchange_dr(&mut batch, &dr);
        self.probe.run_test_idle(&mut batch, idle_cycles);
        let mut results = self.probe.run(batch)?;
        results
            .take(handle)
            .map_err(|_| DebugProbeError::Other("missing JTAG capture result".into()))
    }

    fn exchange_dr(
        &mut self,
        dr: BitSequence,
        idle_cycles: u32,
    ) -> Result<BitSequence, DebugProbeError> {
        let mut batch = JtagBatch::new();
        let handle = self.probe.exchange_dr(&mut batch, &dr);
        self.probe.run_test_idle(&mut batch, idle_cycles);
        let mut results = self.probe.run(batch)?;
        results
            .take(handle)
            .map_err(|_| DebugProbeError::Other("missing JTAG capture result".into()))
    }

    fn write_dtmcs(&mut self, data: u32) -> Result<u32, RiscvError> {
        let select = self.select_dtmcs.clone();
        self.exchange_register(&select, self.state.idle_cycles as u32)?;
        let cmd = tunnel_dtmcs_data(data);
        let result = self
            .exchange_dr(cmd.data.clone(), self.state.idle_cycles as u32)
            .map(|r| tunnel_dtmcs_transform(&cmd, r.as_bits()))?;

        match result {
            Ok(CommandResult::U32(d)) => Ok(d),
            _ => {
                unreachable!(
                    "The transform function of tunnel_dtmcs always returns Ok(CommandResult::U32)"
                )
            }
        }
    }

    fn transform_tunneled_dr_result<T: BitStore>(response_bits: &BitSlice<T>) -> &BitSlice<T> {
        &response_bits[4..]
    }

    fn dmi_register_access(
        &mut self,
        op: DmiOperation,
    ) -> Result<Result<u32, DmiOperationError>, DebugProbeError> {
        let select = self.select_dmi.clone();
        self.exchange_register(&select, self.state.idle_cycles as u32)?;

        let dmi_bits = self.state.abits + DMI_ADDRESS_BIT_OFFSET;
        let (bit_size, bytes) = op.to_tunneled_byte_batch(dmi_bits);
        let result = self.exchange_dr(
            BitSequence::from_bytes(&bytes, bit_size as usize),
            self.state.idle_cycles as u32,
        )?;
        let tunneled_result = Self::transform_tunneled_dr_result(result.as_bits());
        Ok(JtagDtm::transform_dmi_result(tunneled_result))
    }

    fn make_select_command(&self) -> JtagWriteCommand<DmiOperationError> {
        JtagWriteCommand {
            data: JtagWriteData {
                idle_cycles: self.state.idle_cycles as u32,
                ..self.select_dmi.clone()
            },
            transform: |_, _| Ok(CommandResult::None),
        }
    }

    fn schedule_dmi_register_access(
        &mut self,
        op: DmiOperation,
    ) -> Result<Handle<CommandResult>, RiscvError> {
        self.state
            .queued_commands
            .schedule(self.make_select_command());

        let dmi_bits = self.state.abits + DMI_ADDRESS_BIT_OFFSET;
        let (bit_size, bytes) = op.to_tunneled_byte_batch(dmi_bits);

        Ok(self.state.queued_commands.schedule(ShiftDrCommand {
            inner: ShiftDrData {
                data: BitSequence::from_bytes(&bytes, bit_size as usize),
                idle_cycles: self.state.idle_cycles as u32,
            },
            transform: |_, raw_result| {
                let result = TunneledJtagDtm::transform_tunneled_dr_result(raw_result);
                JtagDtm::transform_dmi_result(result).map(CommandResult::U32)
            },
        }))
    }

    fn dmi_register_access_with_timeout(
        &mut self,
        op: DmiOperation,
        timeout: Duration,
    ) -> Result<u32, RiscvError> {
        let start_time = Instant::now();

        self.execute()?;

        loop {
            match self.dmi_register_access(op)? {
                Ok(result) => return Ok(result),
                Err(DmiOperationError::RequestInProgress) => {
                    // Operation still in progress, reset dmi status and try again.
                    self.clear_error_state()?;
                    self.state.idle_cycles = self.state.idle_cycles.saturating_add(1);
                }
                // A reserved status is defined to be interpreted like a failure.
                Err(DmiOperationError::Reserved | DmiOperationError::OperationFailed) => {
                    // The failure is sticky. Clear it, or the DTM ignores every later access.
                    self.clear_error_state()?;
                    return Err(RiscvError::DtmOperationFailed);
                }
            };

            if start_time.elapsed() > timeout {
                return Err(RiscvError::Timeout);
            }
        }
    }
}

impl DtmAccess for TunneledJtagDtm<'_> {
    fn init(&mut self) -> Result<(), RiscvError> {
        self.tap_reset()?;
        let raw_dtmcs = self.write_dtmcs(0)?;

        if raw_dtmcs == 0 {
            return Err(RiscvError::NoRiscvTarget);
        }

        let dtmcs = Dtmcs(raw_dtmcs);

        tracing::debug!("{:?}", dtmcs);

        let abits = dtmcs.abits();
        let idle_cycles = dtmcs.idle();

        if dtmcs.version() != 1 {
            return Err(RiscvError::UnsupportedDebugTransportModuleVersion(
                dtmcs.version() as u8,
            ));
        }

        // Setup the number of idle cycles between JTAG accesses
        self.state.idle_cycles = idle_cycles as u8;
        self.state.abits = abits;

        Ok(())
    }

    fn target_reset_assert(&mut self) -> Result<(), DebugProbeError> {
        self.probe.target_reset_assert()
    }

    fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError> {
        self.probe.target_reset_deassert()
    }

    fn clear_error_state(&mut self) -> Result<(), RiscvError> {
        let mut dtmcs = Dtmcs(0);

        dtmcs.set_dmireset(true);

        let Dtmcs(reg_value) = dtmcs;

        self.write_dtmcs(reg_value)?;

        Ok(())
    }

    fn read_deferred_result(
        &mut self,
        index: Handle<CommandResult>,
    ) -> Result<CommandResult, RiscvError> {
        match self.state.jtag_results.take(index) {
            Ok(result) => Ok(result),
            Err(handle) => {
                self.execute()?;
                // We can lose data if `execute` fails.
                self.state
                    .jtag_results
                    .take(handle)
                    .map_err(|_| RiscvError::BatchedResultNotAvailable)
            }
        }
    }

    fn execute(&mut self) -> Result<(), RiscvError> {
        let mut cmds = std::mem::take(&mut self.state.queued_commands);

        let mut started = Instant::now();
        let mut previous_queue_len = cmds.len();
        while !cmds.is_empty() {
            match cmds.execute(|queue| self.probe.run_command_batch(queue)) {
                Ok(r) => {
                    self.state.jtag_results.merge_from(r);
                    return Ok(());
                }
                Err(e) => {
                    match e.error {
                        BatchError::Specific(error) => {
                            // error is now DmiOperationError, not Box<dyn Error>
                            match error {
                                DmiOperationError::RequestInProgress => {
                                    self.clear_error_state()?;

                                    // queue up the remaining commands when we retry
                                    cmds.consume(e.results.len());
                                    self.state.jtag_results.merge_from(e.results);

                                    self.state.idle_cycles =
                                        self.state.idle_cycles.saturating_add(1);
                                }
                                DmiOperationError::Reserved
                                | DmiOperationError::OperationFailed => {
                                    self.clear_error_state()?;
                                    return Err(RiscvError::DtmOperationFailed);
                                }
                            }
                        }
                        BatchError::Probe(debug_probe_error) => {
                            return Err(RiscvError::DebugProbe(debug_probe_error));
                        }
                    }
                }
            }

            // If progress was made, reset the timeout.
            if cmds.len() != previous_queue_len {
                started = Instant::now();
                previous_queue_len = cmds.len();
            }

            let elapsed_time = started.elapsed();

            if elapsed_time > JTAG_COMMAND_TIMEOUT {
                tracing::error!(
                    "Timeout ({JTAG_COMMAND_TIMEOUT:?}) exceeded executing RISCV commands (elapsed: {elapsed_time:?})"
                );
                return Err(RiscvError::Timeout);
            }
        }

        Ok(())
    }

    fn schedule_write(
        &mut self,
        address: u64,
        value: u32,
    ) -> Result<Option<Handle<CommandResult>>, RiscvError> {
        self.schedule_dmi_register_access(DmiOperation::Write { address, value })
            .map(Some)
    }

    fn schedule_read(&mut self, address: u64) -> Result<Handle<CommandResult>, RiscvError> {
        // Prepare the read by sending a read request with the register address
        self.schedule_dmi_register_access(DmiOperation::Read { address })?;

        // Read back the response from the previous request.
        self.schedule_dmi_register_access(DmiOperation::NoOp)
    }

    fn read_with_timeout(&mut self, address: u64, timeout: Duration) -> Result<u32, RiscvError> {
        // Prepare the read by sending a read request with the register address
        self.schedule_dmi_register_access(DmiOperation::Read { address })?;

        self.dmi_register_access_with_timeout(DmiOperation::NoOp, timeout)
    }

    fn write_with_timeout(
        &mut self,
        address: u64,
        value: u32,
        timeout: Duration,
    ) -> Result<Option<u32>, RiscvError> {
        self.dmi_register_access_with_timeout(DmiOperation::Write { address, value }, timeout)
            .map(Some)
    }

    fn read_idcode(&mut self) -> Result<Option<u32>, DebugProbeError> {
        // Reset the JTAG state machine to Test-Logic-Reset before reading the IDCODE.
        // This is required when read_idcode() is called without a prior dtm.init() (e.g.
        // from `probe-rs info`), because the TAP state may be indeterminate after attach.
        self.tap_reset()?;
        let value = self.exchange_register_at(0x1, BitSequence::repeat(false, 32), 0)?;
        Ok(valid_idcode(value.as_bits().load_le::<u32>()))
    }
}

fn tunnel_select_data(tunnel_ir_id: u32, tunnel_ir_width: u32, address: u32) -> JtagWriteData {
    let tunneled_ir: u32 = (tunnel_ir_width << (tunnel_ir_width + 3)) | (address << 3);
    let tunneled_ir_len = 1 + 7 + tunnel_ir_width + 3;
    JtagWriteData {
        address: tunnel_ir_id,
        data: BitSequence::from_bytes(&tunneled_ir.to_le_bytes(), tunneled_ir_len as usize),
        idle_cycles: 0,
    }
}

fn tunnel_dtmcs_data(data: u32) -> ShiftDrData {
    let width_offset = 1 + (DTMCS_WIDTH as u128) + 3;
    let msb_offset = 7 + width_offset;
    let tunneled_dr: u128 =
        (1 << msb_offset) | ((DTMCS_WIDTH as u128) << width_offset) | ((data as u128) << 3);
    ShiftDrData {
        data: BitSequence::from_bytes(&tunneled_dr.to_le_bytes(), (msb_offset as u32 + 1) as usize),
        idle_cycles: 0,
    }
}

fn tunnel_dtmcs_transform<T: BitStore>(
    _data: &ShiftDrData,
    result: &BitSlice<T>,
) -> Result<CommandResult, DmiOperationError> {
    let response = result[4..36].load_le::<u32>();
    Ok(CommandResult::U32(response))
}

/// DMI operation.
#[derive(Copy, Clone, Debug)]
pub enum DmiOperation {
    /// No operation, usually to read back the response to the previous operation.
    NoOp,

    /// Read operation.
    Read {
        /// Address of the register to read.
        address: u64,
    },

    /// Write operation.
    Write {
        /// Address of the register to write.
        address: u64,

        /// Value to write to the register.
        value: u32,
    },
}

impl DmiOperation {
    fn opcode(&self) -> u8 {
        match self {
            Self::NoOp => 0,
            Self::Read { .. } => 1,
            Self::Write { .. } => 2,
        }
    }

    fn register_value(&self) -> u128 {
        let (opcode, address, value): (u128, u128, u128) = match self {
            Self::NoOp => (self.opcode() as u128, 0, 0),
            Self::Read { address } => (self.opcode() as u128, *address as u128, 0),
            Self::Write { address, value } => {
                (self.opcode() as u128, *address as u128, *value as u128)
            }
        };
        (address << DMI_ADDRESS_BIT_OFFSET) | (value << DMI_VALUE_BIT_OFFSET) | opcode
    }

    /// Converts the DMI operation into a byte array.
    pub fn to_byte_batch(self) -> [u8; 16] {
        self.register_value().to_le_bytes()
    }

    /// Converts the DMI operation into a byte array for tunneling.
    pub fn to_tunneled_byte_batch(self, dmi_bits: u32) -> (u32, [u8; 16]) {
        let width_offset = 1 + (dmi_bits) + 3;
        let msb_offset = 7 + width_offset;
        let bits = (1 << (msb_offset as u128))
            | (((dmi_bits + 1) as u128) << (width_offset as u128))
            | (self.register_value() << 3);
        (msb_offset + 1, bits.to_le_bytes())
    }
}

/// Possible return values in the op field of the dmi register.
#[derive(Debug)]
pub enum DmiOperationStatus {
    /// OK
    Ok = 0,
    /// Reserved
    Reserved = 1,
    /// Operation failed
    OperationFailed = 2,
    /// Request in progress
    RequestInProgress = 3,
}

// TODO Better error names

/// DMI operation error.
#[derive(Debug, thiserror::Error, docsplay::Display)]
pub enum DmiOperationError {
    /// Reserved
    Reserved,
    /// Operation failed
    OperationFailed,
    /// Request in progress
    RequestInProgress,
}

impl DmiOperationStatus {
    pub(crate) fn parse(value: u8) -> Option<Self> {
        let status = match value {
            0 => Self::Ok,
            1 => Self::Reserved,
            2 => Self::OperationFailed,
            3 => Self::RequestInProgress,
            _ => return None,
        };

        Some(status)
    }
}

/// Timeout for the execution of a batch of scheduled `dmi` accesses.
///
/// Observed with a HiFive Rev B Board: 1.4 sec to execute commands when a reset is involved.
const JTAG_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// Address of the `dtmcs` JTAG register.
const DTMCS_ADDRESS: u32 = 0x10;

/// Width of the `dtmcs` JTAG register.
const DTMCS_WIDTH: u32 = 32;

/// Address of the `dmi` JTAG register
const DMI_ADDRESS: u32 = 0x11;

/// Offset of the `address` field in the `dmi` JTAG register.
const DMI_ADDRESS_BIT_OFFSET: u32 = 34;

/// Offset of the `value` field in the `dmi` JTAG register.
const DMI_VALUE_BIT_OFFSET: u32 = 2;

const DMI_OP_MASK: u128 = 0x3;

bitfield! {
    /// The `dtmcs` register is
    pub struct Dtmcs(pub u32);
    impl Debug;


    /// Hard-reset the DTM
    ///
    /// Writing 1 to this bit does a hard reset of the DTM,
    /// causing the DTM to forget about any outstanding DMI
    /// transactions. In general this should only be used
    /// when the Debugger has reason to expect that the
    /// outstanding DMI transaction will never complete
    /// (e.g. a reset condition caused an inflight DMI
    /// transaction to be cancelled).
    pub _, set_dmihardreset: 17;

    /// Writing 1 to this bit clears the sticky error state
    /// and allows the DTM to retry or complete the previous
    /// outstanding transaction.
    pub _, set_dmireset: 16;

    /// Idle cycles hint.
    ///
    /// This is a hint to the debugger of the minimum
    /// number of cycles a debugger should spend in Run
    /// Test/Idle after every DMI scan to avoid a ‘busy’
    /// return code (dmistat of 3). A debugger must still
    /// check dmistat when necessary.
    ///
    /// 0: It is not necessary to enter Run-Test/Idle at
    ///    all.
    /// 1: Enter Run-Test/Idle and leave it immediately.
    /// 2: Enter Run-Test/Idle and stay there for 1 cycle
    ///    before leaving.
    ///
    /// And so on.
    pub idle, _: 14, 12;

    /// DMI status
    ///
    /// 0: No error.
    /// 1: Reserved. Interpret the same as 2.
    /// 2: An operation failed (resulted in op of 2).
    /// 3: An operation was attempted while a DMI access was still
    ///    in progress (resulted in op of 3).
    pub dmistat, _: 11,10;

    /// The size of address in dmi.
    pub abits, _: 9,4;

    /// Version of the implemented RISCV-V debug specification
    ///
    /// 0: Version described in spec version 0.11.
    /// 1: Version described in spec version 0.13.
    /// 15: Version not described in any available version
    /// of this spec.
    pub version, _: 3,0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{
        BatchExecutionError, DebugProbe, JtagChainAccess, JtagChainState, JtagOp, JtagProbe,
        TapState, WireProtocol,
    };

    #[derive(Debug)]
    struct DmiProbe {
        state: JtagChainState,
        ir: u32,
        response: u64,
        sticky: bool,
        busy_on_read: usize,
        busy_on_write: usize,
        reads: Vec<u64>,
        writes: Vec<(u64, u32)>,
        max_idle: u32,
    }

    impl DmiProbe {
        fn new(busy_on_read: usize) -> Self {
            let mut state = JtagChainState::default();
            state.chain_params.irlen = 5;
            Self {
                state,
                ir: 0,
                response: 0,
                sticky: false,
                busy_on_read,
                busy_on_write: usize::MAX,
                reads: Vec::new(),
                writes: Vec::new(),
                max_idle: 0,
            }
        }
    }

    impl DebugProbe for DmiProbe {
        fn get_name(&self) -> &str {
            "DMI test"
        }
        fn speed_khz(&self) -> u32 {
            0
        }
        fn set_speed(&mut self, speed: u32) -> Result<u32, DebugProbeError> {
            Ok(speed)
        }
        fn attach(&mut self) -> Result<(), DebugProbeError> {
            Ok(())
        }
        fn detach(&mut self) -> Result<(), crate::Error> {
            Ok(())
        }
        fn target_reset(&mut self) -> Result<(), DebugProbeError> {
            Ok(())
        }
        fn target_reset_assert(&mut self) -> Result<(), DebugProbeError> {
            Ok(())
        }
        fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError> {
            Ok(())
        }
        fn set_reset_line_connected(&mut self, _: bool) {}
        fn select_protocol(&mut self, _: WireProtocol) -> Result<(), DebugProbeError> {
            Ok(())
        }
        fn active_protocol(&self) -> Option<WireProtocol> {
            Some(WireProtocol::Jtag)
        }
        fn into_probe(self: Box<Self>) -> Box<dyn DebugProbe> {
            self
        }
    }

    impl JtagChainAccess for DmiProbe {
        fn chain_state(&mut self) -> &mut JtagChainState {
            &mut self.state
        }
        fn chain_state_ref(&self) -> &JtagChainState {
            &self.state
        }
    }

    impl JtagProbe for DmiProbe {
        fn run_batch(
            &mut self,
            batch: &JtagBatch,
        ) -> Result<Results, BatchExecutionError<DebugProbeError>> {
            let mut results = Results::new();
            for (id, op) in batch.iter() {
                match op {
                    JtagOp::EnterState(state) => self.state.tap_state = *state,
                    JtagOp::ClockTck { count } => self.max_idle = self.max_idle.max(*count),
                    JtagOp::Exchange { data, capture } => {
                        let bits = data.as_bits().load_le::<u64>();
                        let mut response = 0;
                        match self.state.tap_state {
                            TapState::ShiftIr => self.ir = bits as u32,
                            TapState::ShiftDr if self.ir == DMI_ADDRESS => {
                                response = self.response;
                                let operation = bits & 3;
                                let address = bits >> DMI_ADDRESS_BIT_OFFSET;
                                if operation == 1 {
                                    self.reads.push(address);
                                    if self.reads.len() == self.busy_on_read {
                                        self.sticky = true;
                                    }
                                    self.response = if self.sticky {
                                        3
                                    } else {
                                        (0xa500_0000 | address) << 2
                                    };
                                } else if operation == 2 {
                                    self.writes.push((address, (bits >> 2) as u32));
                                    if self.writes.len() == self.busy_on_write {
                                        self.sticky = true;
                                    }
                                    self.response = if self.sticky { 3 } else { 0 };
                                } else {
                                    self.response = if self.sticky { 3 } else { 0 };
                                }
                            }
                            TapState::ShiftDr
                                if self.ir == DTMCS_ADDRESS && bits & (1 << 16) != 0 =>
                            {
                                self.sticky = false;
                                self.response = 0;
                            }
                            _ => {}
                        }
                        if *capture && id.should_capture() {
                            results.push(id, CommandResult::VecU8(response.to_le_bytes().to_vec()));
                        }
                    }
                }
            }
            Ok(results)
        }
    }

    #[test]
    fn dmi_busy_retries_the_request_and_preserves_previous_read() {
        let mut probe = DmiProbe::new(2);
        let mut state = DtmState {
            abits: 7,
            ..Default::default()
        };
        let mut dtm = JtagDtm::new(JtagChain::new(&mut probe), &mut state);
        let first = dtm.schedule_read(0x11).unwrap();
        let second = dtm.schedule_read(0x12).unwrap();
        assert_eq!(
            dtm.read_deferred_result(first).unwrap().into_u32(),
            0xa500_0011
        );
        assert_eq!(
            dtm.read_deferred_result(second).unwrap().into_u32(),
            0xa500_0012
        );
        assert_eq!(probe.reads, [0x11, 0x12, 0x12]);
        assert!(probe.max_idle > 0);
    }

    #[test]
    fn dmi_busy_does_not_replay_a_side_effecting_read_stream() {
        let mut probe = DmiProbe::new(1);
        let mut state = DtmState {
            abits: 7,
            ..Default::default()
        };
        let mut dtm = JtagDtm::new(JtagChain::new(&mut probe), &mut state);
        let _handles = dtm.schedule_read_repeated(0x3c, 3, 0).unwrap();
        assert!(matches!(dtm.execute(), Err(RiscvError::DtmOperationFailed)));
        assert_eq!(probe.reads, [0x3c, 0x3c, 0x3c]);
    }

    #[test]
    fn pipelined_register_sequence_returns_each_response_in_order() {
        let mut probe = DmiProbe::new(usize::MAX);
        let mut state = DtmState {
            abits: 7,
            ..Default::default()
        };
        let mut dtm = JtagDtm::new(JtagChain::new(&mut probe), &mut state);
        let handles = dtm
            .schedule_read_sequence(&[(4, 0), (5, 0), (6, 9), (4, 0)])
            .unwrap();
        let values: Vec<u32> = handles
            .into_iter()
            .map(|handle| dtm.read_deferred_result(handle).unwrap().into_u32())
            .collect();
        assert_eq!(values, [0xa500_0004, 0xa500_0005, 0xa500_0006, 0xa500_0004]);
        assert_eq!(probe.reads, [4, 5, 6, 4]);
        assert!(probe.max_idle >= 9);
    }

    #[test]
    fn dmi_busy_does_not_replay_a_side_effecting_write_stream() {
        let mut probe = DmiProbe::new(usize::MAX);
        probe.busy_on_write = 2;
        let mut state = DtmState {
            abits: 7,
            ..Default::default()
        };
        let mut dtm = JtagDtm::new(JtagChain::new(&mut probe), &mut state);
        dtm.schedule_write_sequence(&[(4, 11, 0), (5, 22, 0), (6, 33, 7)])
            .unwrap();
        assert!(matches!(dtm.execute(), Err(RiscvError::DtmOperationFailed)));
        assert_eq!(probe.writes, [(4, 11), (5, 22), (6, 33)]);
    }

    #[test]
    fn dmi_busy_retries_the_write_whose_response_was_busy() {
        let mut probe = DmiProbe::new(usize::MAX);
        probe.busy_on_write = 1;
        let mut state = DtmState {
            abits: 7,
            ..Default::default()
        };
        let mut dtm = JtagDtm::new(JtagChain::new(&mut probe), &mut state);
        dtm.write_with_timeout(4, 0x1234, Duration::from_secs(1))
            .unwrap();
        assert_eq!(probe.writes, [(4, 0x1234), (4, 0x1234)]);
        assert!(probe.max_idle > 0);
    }

    #[test]
    fn discarded_write_results_still_check_each_dmi_status() {
        for (busy_on, expected) in [
            (1, vec![(4, 11), (5, 22), (4, 11), (5, 22)]),
            (2, vec![(4, 11), (5, 22), (5, 22)]),
        ] {
            let mut probe = DmiProbe::new(usize::MAX);
            probe.busy_on_write = busy_on;
            let mut state = DtmState {
                abits: 7,
                ..Default::default()
            };
            let mut dtm = JtagDtm::new(JtagChain::new(&mut probe), &mut state);
            dtm.schedule_write(4, 11).unwrap();
            dtm.schedule_write(5, 22).unwrap();
            dtm.execute().unwrap();
            assert_eq!(probe.writes, expected);
        }
    }

    #[test]
    fn tunneled_dmi_batch_bit_width() {
        let dmi_bits = 7 + DMI_ADDRESS_BIT_OFFSET;
        let op = DmiOperation::Write {
            address: 0x100,
            value: 0x42,
        };
        let (bit_size, _) = op.to_tunneled_byte_batch(dmi_bits);
        let width_offset = 1 + dmi_bits + 3;
        let msb_offset = 7 + width_offset;
        assert_eq!(bit_size, msb_offset + 1);
    }
}
