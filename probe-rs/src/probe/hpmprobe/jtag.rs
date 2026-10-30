//! Native Probe X JTAG sequence packets.

use bitvec::{order::Lsb0, slice::BitSlice, store::BitStore, vec::BitVec, view::BitView};

use super::{HpmProbe, HpmProbeTransport, protocol};
use crate::probe::{
    BatchExecutionError, DebugProbeError, JtagBatch, JtagChainAccess, JtagChainState, JtagProbe,
    Results, TapState,
    jtag::{Step, distribute_captures, tms_runs, walk_batch},
};

#[derive(Debug)]
struct Packet {
    request: Vec<u8>,
    captures: Vec<usize>,
    reply_len: usize,
    clocks: usize,
    records: usize,
    last: Option<(usize, usize, bool)>,
}

impl Packet {
    fn new() -> Self {
        Self {
            request: vec![protocol::JTAG_SEQUENCE, 0],
            captures: Vec::new(),
            reply_len: 2,
            clocks: 0,
            records: 0,
            last: None,
        }
    }
}

#[derive(Debug)]
enum Action {
    Packet(Packet),
    Reacquire,
}

struct Encoder {
    actions: Vec<Action>,
    packet: Packet,
    max_payload: usize,
    max_clocks: usize,
}

impl Encoder {
    fn new(max_payload: usize, max_clocks: usize) -> Self {
        Self {
            actions: Vec::new(),
            packet: Packet::new(),
            max_payload,
            max_clocks,
        }
    }

    fn flush(&mut self) {
        if self.packet.records != 0 {
            self.packet.request[1] = self.packet.records as u8;
            self.actions.push(Action::Packet(std::mem::replace(
                &mut self.packet,
                Packet::new(),
            )));
        }
    }

    fn push<T: BitStore>(
        &mut self,
        tms: bool,
        capture: bool,
        bits: &BitSlice<T>,
    ) -> Result<(), DebugProbeError> {
        let mut offset = 0;
        while offset < bits.len() {
            let merge = !capture
                && self
                    .packet
                    .last
                    .is_some_and(|(_, len, last_tms)| len < 64 && last_tms == tms);
            let (old_offset, old_bits) = if merge {
                let (pos, len, _) = self.packet.last.unwrap();
                (pos, len)
            } else {
                (self.packet.request.len(), 0)
            };
            let count = (bits.len() - offset)
                .min(64 - old_bits)
                .min(self.max_clocks.saturating_sub(self.packet.clocks));
            let old_bytes = old_bits.div_ceil(8);
            let new_bytes = (old_bits + count).div_ceil(8);
            let request_bytes = usize::from(!merge) + new_bytes - old_bytes;
            let response_bytes = if capture { count.div_ceil(8) } else { 0 };
            if count == 0
                || (!merge && self.packet.records == u8::MAX as usize)
                || self.packet.request.len() + request_bytes > self.max_payload
                || self.packet.reply_len + response_bytes > self.max_payload
            {
                if self.packet.records == 0 {
                    return Err(DebugProbeError::Other(
                        "Probe X sequence limits are too small".into(),
                    ));
                }
                self.flush();
                continue;
            }
            if !merge {
                self.packet.request.push(0);
                self.packet.records += 1;
                if capture {
                    self.packet.captures.push(count);
                }
            }
            self.packet
                .request
                .resize(self.packet.request.len() + new_bytes - old_bytes, 0);
            for i in 0..count {
                if bits[offset + i] {
                    self.packet.request[old_offset + 1 + (old_bits + i) / 8] |=
                        1 << ((old_bits + i) % 8);
                }
            }
            let length = old_bits + count;
            self.packet.request[old_offset] =
                (length as u8 & 0x3f) | if tms { 0x40 } else { 0 } | if capture { 0x80 } else { 0 };
            self.packet.last = (!capture).then_some((old_offset, length, tms));
            self.packet.reply_len += response_bytes;
            self.packet.clocks += count;
            offset += count;
        }
        Ok(())
    }
}

fn encode_batch(
    start: TapState,
    batch: &JtagBatch,
    max_payload: usize,
    max_clocks: usize,
    physical_transport: HpmProbeTransport,
) -> Result<(TapState, Vec<Action>), DebugProbeError> {
    let mut state = start;
    let mut encoder = Encoder::new(max_payload, max_clocks);
    // Probe X cJTAG reacquisition resets the TAP and leaves it in Idle. Treat
    // that as the logical reset state until the next requested transition.
    let mut reset_in_idle =
        physical_transport == HpmProbeTransport::Cjtag && start == TapState::TestLogicReset;
    walk_batch(&mut state, batch, |step| match step {
        Step::Tms { path, tdi, target } => {
            if physical_transport == HpmProbeTransport::Cjtag && target == TapState::TestLogicReset
            {
                // Reacquisition performs the physical reset and OScan1 entry.
                // Sending ordinary reset clocks afterwards exits OScan1 on HPM5100.
                encoder.flush();
                encoder.actions.push(Action::Reacquire);
                reset_in_idle = true;
                return Ok(());
            }
            let path = if reset_in_idle {
                reset_in_idle = false;
                TapState::RunTestIdle.path_to(target)
            } else {
                path
            };
            for (tms, count) in tms_runs(path) {
                encoder.push(tms, false, &BitVec::<usize, Lsb0>::repeat(tdi, count))?;
            }
            Ok(())
        }
        Step::Shift {
            data,
            exit,
            capture,
        } => {
            let bits = data.as_bits();
            let normal = bits.len() - usize::from(exit);
            encoder.push(false, capture, &bits[..normal])?;
            if exit {
                encoder.push(true, capture, &bits[normal..])?;
            }
            Ok(())
        }
        Step::Clock { count, tms } => {
            if reset_in_idle && count != 0 {
                return Err(DebugProbeError::Other(
                    "Probe X cJTAG cannot hold the TAP in reset while clocking".into(),
                ));
            }
            // Keep large clock requests bounded without allocating their entire length.
            let mut remaining = count as usize;
            while remaining != 0 {
                let count = remaining.min(64);
                encoder.push(tms, false, &BitVec::<usize, Lsb0>::repeat(false, count))?;
                remaining -= count;
            }
            Ok(())
        }
    })?;
    encoder.flush();
    Ok((state, encoder.actions))
}

fn raw_jtag_clock_limit(
    info: &[u8],
    physical_transport: HpmProbeTransport,
) -> Result<usize, protocol::HpmProbeError> {
    if info.len() < 10 || info[2] != 1 {
        return Err(protocol::HpmProbeError::Protocol(
            "invalid JTAG interface information".into(),
        ));
    }
    let clocks = u16::from_le_bytes([info[4], info[5]]) as usize;
    let capabilities = u32::from_le_bytes(info[6..10].try_into().unwrap());
    if clocks == 0 || capabilities & protocol::JTAG_CAP_SEQUENCE == 0 {
        return Err(protocol::HpmProbeError::Protocol(
            "firmware does not advertise raw JTAG sequences".into(),
        ));
    }
    if physical_transport == HpmProbeTransport::Cjtag
        && capabilities & protocol::JTAG_CAP_CJTAG_RAW == 0
    {
        return Err(protocol::HpmProbeError::Protocol(
            "firmware does not advertise raw cJTAG sequences".into(),
        ));
    }
    Ok(clocks.min(255 * 64))
}

fn execute_actions(
    actions: &[Action],
    mut execute: impl FnMut(&Action) -> Result<Vec<u8>, DebugProbeError>,
) -> Result<BitVec, DebugProbeError> {
    let mut captured = BitVec::new();
    for action in actions {
        let response = execute(action)?;
        if let Action::Packet(packet) = action {
            if response.len() != packet.reply_len {
                return Err(DebugProbeError::Other(
                    "invalid Probe X JTAG response length".into(),
                ));
            }
            let mut offset = 2;
            for &length in &packet.captures {
                let bytes = length.div_ceil(8);
                captured.extend_from_bitslice(
                    &response[offset..offset + bytes].view_bits::<Lsb0>()[..length],
                );
                offset += bytes;
            }
        }
    }
    Ok(captured)
}

impl HpmProbe {
    pub(super) fn init_raw_jtag(
        &mut self,
        physical_transport: HpmProbeTransport,
    ) -> Result<(), protocol::HpmProbeError> {
        let info = self
            .transport
            .exchange(protocol::TUNNEL_JTAG, &[protocol::JTAG_GET_INTERFACE_INFO])?;
        self.jtag_max_clocks = raw_jtag_clock_limit(&info, physical_transport)?;
        Ok(())
    }

    fn reacquire_cjtag(&mut self) -> Result<(), protocol::HpmProbeError> {
        let clock_hz = self.clock_hz;
        let actual = self.configure_transport(clock_hz)?;
        if actual != clock_hz {
            return Err(protocol::HpmProbeError::Protocol(format!(
                "cJTAG clock changed from {clock_hz} Hz to {actual} Hz during reset"
            )));
        }
        Ok(())
    }
}

impl JtagChainAccess for HpmProbe {
    fn chain_state(&mut self) -> &mut JtagChainState {
        &mut self.jtag_state
    }
    fn chain_state_ref(&self) -> &JtagChainState {
        &self.jtag_state
    }
}

impl JtagProbe for HpmProbe {
    fn run_batch(
        &mut self,
        batch: &JtagBatch,
    ) -> Result<Results, BatchExecutionError<DebugProbeError>> {
        let failed = |error| BatchExecutionError::new_from_debug_probe(error, Results::new());
        if self.jtag_failed {
            return Err(failed(DebugProbeError::Other(
                "Probe X JTAG state is unknown; reconnect".into(),
            )));
        }
        let (state, actions) = encode_batch(
            self.jtag_state.tap_state,
            batch,
            self.transport.max_payload(),
            self.jtag_max_clocks,
            self.physical_transport,
        )
        .map_err(failed)?;
        let captured = execute_actions(&actions, |action| match action {
            Action::Packet(packet) => self
                .transport
                .exchange(protocol::TUNNEL_JTAG, &packet.request)
                .map_err(Into::into),
            Action::Reacquire => {
                self.reacquire_cjtag()?;
                Ok(Vec::new())
            }
        })
        .map_err(|error| {
            self.jtag_failed = true;
            failed(error)
        })?;
        self.jtag_state.tap_state = state;
        distribute_captures(batch.iter(), &captured, Results::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{
        BitSequence,
        jtag::{FullTapState, golden::lowering_batch},
    };

    #[test]
    fn raw_cjtag_requires_both_sequence_capabilities() {
        let mut info = [1, 0, 1, 0, 0xff, 0xff, 0, 0, 0, 0];
        info[6..10].copy_from_slice(&protocol::JTAG_CAP_SEQUENCE.to_le_bytes());
        assert_eq!(
            raw_jtag_clock_limit(&info, HpmProbeTransport::Jtag).unwrap(),
            255 * 64
        );
        assert!(raw_jtag_clock_limit(&info, HpmProbeTransport::Cjtag).is_err());
        info[6..10].copy_from_slice(
            &(protocol::JTAG_CAP_SEQUENCE | protocol::JTAG_CAP_CJTAG_RAW).to_le_bytes(),
        );
        assert_eq!(
            raw_jtag_clock_limit(&info, HpmProbeTransport::Cjtag).unwrap(),
            255 * 64
        );
        info[4..6].fill(0);
        assert!(raw_jtag_clock_limit(&info, HpmProbeTransport::Cjtag).is_err());
        info[2] = 2;
        assert!(raw_jtag_clock_limit(&info, HpmProbeTransport::Jtag).is_err());
        assert!(raw_jtag_clock_limit(&info[..9], HpmProbeTransport::Jtag).is_err());
    }

    #[test]
    fn cjtag_reacquires_between_captured_scans_and_preserves_results() {
        let mut batch = JtagBatch::new();
        batch.enter(TapState::ShiftDr);
        let _first = batch.exchange(BitSequence::from_u64(7, 0x59));
        batch.enter(TapState::TestLogicReset);
        batch.enter(TapState::ShiftIr);
        let _second = batch.exchange(BitSequence::from_u64(11, 0x35a));
        batch.enter(TapState::TestLogicReset);
        batch.enter(TapState::ShiftDr);
        let _third = batch.exchange(BitSequence::from_u64(13, 0x1234));
        batch.enter(TapState::RunTestIdle);

        for max_clocks in [4, 64] {
            let (end, actions) = encode_batch(
                TapState::RunTestIdle,
                &batch,
                64,
                max_clocks,
                HpmProbeTransport::Cjtag,
            )
            .unwrap();
            assert_eq!(end, TapState::RunTestIdle);
            let mut tap = FullTapState::RunTestIdle;
            let mut resets = 0;
            let mut after_reset = Vec::new();
            let mut tdi = BitVec::<usize, Lsb0>::new();
            let mut expected_capture = BitVec::<usize, Lsb0>::new();
            let captured = execute_actions(&actions, |action| {
                let Action::Packet(packet) = action else {
                    assert!(after_reset.is_empty());
                    assert_eq!(
                        tap,
                        if resets == 0 {
                            FullTapState::Exit1Dr
                        } else {
                            FullTapState::Exit1Ir
                        }
                    );
                    assert_eq!(tdi.len(), if resets == 0 { 7 } else { 18 });
                    resets += 1;
                    after_reset = if resets == 1 {
                        vec![true, true, false, false]
                    } else {
                        vec![true, false, false]
                    };
                    after_reset.reverse();
                    tap = FullTapState::RunTestIdle;
                    return Ok(Vec::new());
                };
                let mut response = vec![protocol::JTAG_SEQUENCE, 0];
                let mut pos = 2;
                for _ in 0..packet.records {
                    let info = packet.request[pos];
                    pos += 1;
                    let count = match info & 0x3f {
                        0 => 64,
                        n => n as usize,
                    };
                    let capture = info & 0x80 != 0;
                    let tms = info & 0x40 != 0;
                    let response_offset = response.len();
                    if capture {
                        response.resize(response_offset + count.div_ceil(8), 0);
                    }
                    for bit in 0..count {
                        let input = packet.request[pos + bit / 8] & (1 << (bit % 8)) != 0;
                        if let Some(expected_tms) = after_reset.pop() {
                            assert_eq!(tms, expected_tms);
                            assert!(!input && !capture);
                        }
                        if capture {
                            assert!(matches!(tap, FullTapState::ShiftDr | FullTapState::ShiftIr));
                            tdi.push(input);
                            let output = expected_capture.len().is_multiple_of(3);
                            expected_capture.push(output);
                            if output {
                                response[response_offset + bit / 8] |= 1 << (bit % 8);
                            }
                        }
                        tap = tap.step(tms);
                    }
                    pos += count.div_ceil(8);
                }
                Ok(response)
            })
            .unwrap();
            assert_eq!(resets, 2);
            assert!(after_reset.is_empty());
            assert_eq!(tap, FullTapState::RunTestIdle);
            assert_eq!(captured, expected_capture);
            let mut expected_tdi = BitVec::<usize, Lsb0>::new();
            for (length, value) in [(7, 0x59), (11, 0x35a), (13, 0x1234)] {
                expected_tdi.extend_from_bitslice(BitSequence::from_u64(length, value).as_bits());
            }
            assert_eq!(tdi, expected_tdi);
        }
    }

    #[test]
    fn sequence_execution_stops_on_reset_failure_or_malformed_reply() {
        let mut batch = JtagBatch::new();
        batch.enter(TapState::TestLogicReset);
        batch.enter(TapState::RunTestIdle);
        batch.clock(1);
        let (_, actions) = encode_batch(
            TapState::RunTestIdle,
            &batch,
            64,
            64,
            HpmProbeTransport::Cjtag,
        )
        .unwrap();
        let mut calls = 0;
        assert!(
            execute_actions(&actions, |_| {
                calls += 1;
                Err(DebugProbeError::Other("reset failed".into()))
            })
            .is_err()
        );
        assert_eq!(calls, 1);
        assert!(
            execute_actions(&actions, |action| match action {
                Action::Reacquire => Ok(Vec::new()),
                Action::Packet(_) => Ok(vec![protocol::JTAG_SEQUENCE]),
            })
            .is_err()
        );
    }

    #[test]
    fn cjtag_reset_state_survives_batch_boundaries_without_reset_clocks() {
        let mut reset = JtagBatch::new();
        reset.enter(TapState::TestLogicReset);
        let (end, reset_actions) = encode_batch(
            TapState::RunTestIdle,
            &reset,
            64,
            64,
            HpmProbeTransport::Cjtag,
        )
        .unwrap();
        assert_eq!(end, TapState::TestLogicReset);
        assert!(matches!(reset_actions.as_slice(), [Action::Reacquire]));

        let mut batch = JtagBatch::new();
        batch.enter(TapState::ShiftDr);
        let (_, actions) = encode_batch(end, &batch, 64, 64, HpmProbeTransport::Cjtag).unwrap();
        let [Action::Packet(packet)] = actions.as_slice() else {
            panic!("expected only the transition from Idle to Shift-DR");
        };
        assert_eq!(packet.request, [protocol::JTAG_SEQUENCE, 2, 0x41, 0, 2, 0]);

        let mut clocks = JtagBatch::new();
        clocks.clock(1);
        assert!(
            encode_batch(
                TapState::TestLogicReset,
                &clocks,
                64,
                64,
                HpmProbeTransport::Cjtag,
            )
            .is_err()
        );
        assert!(
            encode_batch(
                TapState::TestLogicReset,
                &clocks,
                64,
                64,
                HpmProbeTransport::Jtag,
            )
            .is_ok()
        );
    }

    #[test]
    fn sequence_packets_preserve_jtag_clocks_and_capture_across_limits() {
        let mut batch = JtagBatch::new();
        batch.enter(TapState::ShiftIr);
        batch.exchange_no_capture(BitSequence::from_u64(5, 0x11));
        batch.enter(TapState::RunTestIdle);
        batch.clock(71);
        batch.enter(TapState::ShiftDr);
        let _capture = batch.exchange(BitSequence::from_bytes(&[0x96; 700], 5501));
        batch.enter(TapState::RunTestIdle);
        batch.enter(TapState::TestLogicReset);

        let expected = lowering_batch(TapState::RunTestIdle, &batch);
        for (max_payload, max_clocks) in [(64, 70), (512, 512), (8192, 16320)] {
            let (end, actions) = encode_batch(
                TapState::RunTestIdle,
                &batch,
                max_payload,
                max_clocks,
                HpmProbeTransport::Jtag,
            )
            .unwrap();
            assert_eq!(end, TapState::TestLogicReset);
            let mut actual = Vec::new();
            for action in actions {
                let Action::Packet(packet) = action else {
                    panic!("unexpected cJTAG reset");
                };
                assert!(packet.request.len() <= max_payload && packet.reply_len <= max_payload);
                assert!(packet.clocks <= max_clocks && packet.records <= 255);
                let mut pos = 2;
                for _ in 0..packet.records {
                    let info = packet.request[pos];
                    pos += 1;
                    let count = match info & 0x3f {
                        0 => 64,
                        value => value as usize,
                    };
                    for bit in 0..count {
                        actual.push((
                            info & 0x40 != 0,
                            packet.request[pos + bit / 8] & (1 << (bit % 8)) != 0,
                            info & 0x80 != 0,
                        ));
                    }
                    pos += count.div_ceil(8);
                }
                assert_eq!(pos, packet.request.len());
            }
            assert_eq!(actual, expected);
        }
    }
}
