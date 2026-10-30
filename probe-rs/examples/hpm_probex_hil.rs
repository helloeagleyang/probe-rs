//! Destructive-to-RAM Probe X or FTDI HIL test. Supply scratch RAM and a backup path.

use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use bitvec::field::BitField;
use clap::Parser;
use probe_rs::{
    MemoryInterface,
    architecture::riscv::communication_interface::{
        Abstractauto, AccessRegisterCommand, MemoryAccessMethod, RiscvBusAccess,
        RiscvCommunicationInterface,
    },
    architecture::riscv::{Abstractcs, Data0, Dmstatus},
    probe::{
        BitSequence, DebugProbeError, DebugProbeSelector, JtagBatch, Probe, ProbeFactory, TapState,
        WireProtocol,
        ftdi::FtdiProbeFactory,
        hpmprobe::{HpmProbe, HpmProbeMode, HpmProbeTransport},
    },
};

#[derive(Parser)]
struct Args {
    /// Probe driver: `probex` or `ftdi`.
    #[arg(long, default_value = "probex")]
    adapter: String,
    /// Probe selector (VID:PID[:serial]).
    #[arg(long, default_value = "34b7:8003")]
    probe: DebugProbeSelector,
    /// Requested JTAG frequency in kHz.
    #[arg(long, default_value_t = 25_000)]
    speed: u32,
    /// Optional expected JTAG IDCODE; refuse to test a different target.
    #[arg(long, value_parser = parse_address)]
    expected_idcode: Option<u64>,
    /// Start of writable scratch RAM, excluding the 64-byte guard before it.
    #[arg(long, value_parser = parse_address)]
    address: u64,
    /// Scratch size in 32-bit words (at least one).
    #[arg(long, default_value_t = 4096)]
    words: usize,
    /// New file in which to save the original scratch RAM and guards.
    #[arg(long)]
    backup: PathBuf,
    /// Number of timed 32-bit transfers per mode and method.
    #[arg(long, default_value_t = 3)]
    repeats: usize,
    /// Run `direct`, `raw`, or `all` modes.
    #[arg(long, default_value = "all")]
    mode: String,
    /// Physical transport: `jtag` or `cjtag`.
    #[arg(long, default_value = "jtag")]
    transport: String,
    /// Run `sba`, `progbuf`, or `all` memory methods.
    #[arg(long, default_value = "all")]
    method: String,
    /// Enable the opt-in multiword Program Buffer RAM path.
    #[arg(long)]
    multiword: bool,
}

fn parse_address(value: &str) -> Result<u64> {
    Ok(u64::from_str_radix(value.trim_start_matches("0x"), 16)?)
}

fn with_interface<T>(
    args: &Args,
    mode: HpmProbeMode,
    physical_transport: HpmProbeTransport,
    speed: u32,
    f: impl FnOnce(&mut RiscvCommunicationInterface<'_>) -> Result<T>,
) -> Result<T> {
    let mut probe = if args.adapter == "ftdi" {
        Probe::from_specific_probe(FtdiProbeFactory.open(&args.probe)?)
    } else {
        Probe::new(HpmProbe::open_with_transport(
            &args.probe,
            mode,
            physical_transport,
        )?)
    };
    probe.select_protocol(WireProtocol::Jtag)?;
    probe.set_speed(speed)?;
    probe.attach_to_unspecified()?;
    let actual = probe.speed_khz();
    ensure!(
        actual > 0 && actual <= speed,
        "unexpected probe speed: {actual} kHz"
    );
    ensure!(
        args.adapter != "probex" || actual == speed,
        "requested {speed} kHz, Probe X reported {actual} kHz"
    );
    if args.adapter == "ftdi" {
        let mut chain = probe.try_as_jtag_chain().context("JTAG unavailable")?;
        chain.select(0).context("FTDI chain discovery failed")?;
        let idcodes = chain.idcodes();
        ensure!(idcodes.len() == 1, "expected a single TAP, got {idcodes:?}");
        if let Some(expected) = args.expected_idcode {
            ensure!(
                idcodes[0].map(u64::from) == Some(expected),
                "FTDI chain IDCODE {:?} does not match {expected:#010x}",
                idcodes[0]
            );
        }
    }
    if mode == HpmProbeMode::RawJtag && physical_transport == HpmProbeTransport::Cjtag {
        let mut chain = probe.try_as_jtag_chain().context("raw JTAG unavailable")?;
        chain
            .select(0)
            .context("raw cJTAG chain discovery failed")?;

        let mut invalid = JtagBatch::new();
        invalid.enter(TapState::TestLogicReset);
        invalid.clock(1);
        let error = match chain.run(invalid) {
            Err(error) => error,
            Ok(_) => bail!("raw cJTAG unexpectedly accepted clocks in reset"),
        };
        ensure!(
            matches!(&error, DebugProbeError::Other(message) if message.contains("cannot hold the TAP in reset")),
            "unexpected reset-clock error: {error}"
        );

        // Reacquire in one batch and read IDCODE in the next. Firmware leaves
        // the physical TAP in Idle even though the logical state is Reset.
        let mut reset = JtagBatch::new();
        reset.enter(TapState::TestLogicReset);
        chain.run(reset)?;
        let mut scan = JtagBatch::new();
        let idcode = chain.exchange_dr(&mut scan, &BitSequence::repeat(false, 32));
        chain.run_test_idle(&mut scan, 0);
        let mut results = chain.run(scan)?;
        let idcode = results
            .take(idcode)
            .map_err(|_| anyhow::anyhow!("raw cJTAG IDCODE result missing"))?
            .as_bits()
            .load_le::<u32>();
        if let Some(expected) = args.expected_idcode {
            ensure!(
                u64::from(idcode) == expected,
                "raw cJTAG IDCODE {idcode:#010x} does not match {expected:#010x}"
            );
        }
        println!("Raw cJTAG reset across batches: IDCODE {idcode:#010x}");
    }
    let builder = probe.try_get_riscv_interface_builder()?;
    let mut state = builder.create_state();
    let mut interface = builder.attach(&mut state)?;
    interface.enter_debug_mode()?;
    let idcode = interface
        .read_idcode()?
        .context("target has no JTAG IDCODE")?;
    if let Some(expected) = args.expected_idcode {
        ensure!(
            u64::from(idcode) == expected,
            "target IDCODE {idcode:#010x} does not match {expected:#010x}"
        );
    }
    if args.adapter == "ftdi" {
        println!(
            "FTDI JTAG target IDCODE {idcode:#010x}, requested {speed} kHz, configured {actual} kHz"
        );
    } else {
        println!("{mode:?} {physical_transport:?} target IDCODE {idcode:#010x}, {actual} kHz");
    }
    let result = f(&mut interface);
    drop(interface);
    probe.detach()?;
    result
}

fn method(interface: &mut RiscvCommunicationInterface<'_>, value: MemoryAccessMethod) {
    interface
        .memory_access_config()
        .set_default_method(RiscvBusAccess::A32, value);
}

fn halt(interface: &mut RiscvCommunicationInterface<'_>) -> Result<bool> {
    let running = !interface.core_halted()?;
    if running {
        interface.halt(Duration::from_secs(1))?;
    }
    Ok(running)
}

fn read_region(
    interface: &mut RiscvCommunicationInterface<'_>,
    address: u64,
    words: usize,
) -> Result<Vec<u32>> {
    let mut values = vec![0; words];
    interface.read_32(address, &mut values)?;
    Ok(values)
}

fn speed(bytes: usize, duration: Duration) -> f64 {
    bytes as f64 / 1024.0 / duration.as_secs_f64()
}

fn check_debug_state(interface: &mut RiscvCommunicationInterface<'_>) -> Result<()> {
    let auto: Abstractauto = interface.read_dm_register()?;
    let status: Abstractcs = interface.read_dm_register()?;
    let dmstatus: Dmstatus = interface.read_dm_register()?;
    ensure!(auto.0 == 0, "abstractauto remains enabled: {:#x}", auto.0);
    ensure!(
        status.0 & 0x1700 == 0,
        "abstract command is busy or failed: {:#x}",
        status.0
    );
    ensure!(
        dmstatus.0 & 0x200 != 0,
        "target did not remain halted: {:#x}",
        dmstatus.0
    );
    Ok(())
}

fn print_debug_state(interface: &mut RiscvCommunicationInterface<'_>, label: &str) -> Result<()> {
    let dmstatus: Dmstatus = interface.read_dm_register()?;
    let auto: Abstractauto = interface.read_dm_register()?;
    let status: Abstractcs = interface.read_dm_register()?;
    println!(
        "{label}: dmstatus={:#010x}, abstractauto={:#010x}, abstractcs={:#010x}",
        dmstatus.0, auto.0, status.0
    );
    Ok(())
}

fn scratch_registers(interface: &mut RiscvCommunicationInterface<'_>) -> Result<Vec<u32>> {
    check_debug_state(interface)?;
    let mut values = Vec::new();
    for register in [
        0x1005u32, 0x1008, 0x1009, 0x100a, 0x100b, 0x100c, 0x100d, 0x7b1,
    ] {
        interface.write_dm_register(AccessRegisterCommand((2 << 20) | (1 << 17) | register))?;
        let start = Instant::now();
        loop {
            let status: Abstractcs = interface.read_dm_register()?;
            if status.0 & (1 << 12) == 0 {
                ensure!(
                    status.0 & 0x700 == 0,
                    "reading register {register:#x} failed: {:#x}",
                    status.0
                );
                break;
            }
            ensure!(
                start.elapsed() < Duration::from_secs(1),
                "register read timed out"
            );
        }
        let value: Data0 = interface.read_dm_register()?;
        values.push(value.0);
    }
    Ok(values)
}

fn exercise_subwords(
    interface: &mut RiscvCommunicationInterface<'_>,
    mode: HpmProbeMode,
    args: &Args,
    access: MemoryAccessMethod,
    verify: MemoryAccessMethod,
    expected: &mut Vec<u32>,
) -> Result<()> {
    let region = args.address - 64;
    for width in [1usize, 2] {
        let bus_width = if width == 1 {
            RiscvBusAccess::A8
        } else {
            RiscvBusAccess::A16
        };
        interface
            .memory_access_config()
            .set_default_method(bus_width, access);
        let offset = width;
        let max_count = (args.words * 4 - offset).min(8180) / width;
        let mut counts = vec![1, 3, 17, max_count];
        counts.retain(|&count| count != 0 && count <= max_count);
        counts.sort_unstable();
        counts.dedup();
        for count in counts {
            let pattern: Vec<u16> = (0..count)
                .map(|index| {
                    (index as u16).wrapping_mul(0x2d) ^ (count as u16).rotate_left(3) ^ 0xa55a
                })
                .collect();
            let mut bytes: Vec<u8> = expected
                .iter()
                .flat_map(|word| word.to_le_bytes())
                .collect();
            let start = 64 + offset;
            if width == 1 {
                let values: Vec<u8> = pattern.iter().map(|&value| value as u8).collect();
                interface
                    .write_8(args.address + offset as u64, &values)
                    .with_context(|| format!("{mode:?} {access:?} 8-bit write {count}"))?;
                bytes[start..start + count].copy_from_slice(&values);
                let mut actual = vec![0u8; count];
                interface.read_8(args.address + offset as u64, &mut actual)?;
                ensure!(
                    actual == values,
                    "{mode:?} {access:?} 8-bit read mismatch at {count}"
                );
            } else {
                interface
                    .write_16(args.address + offset as u64, &pattern)
                    .with_context(|| format!("{mode:?} {access:?} 16-bit write {count}"))?;
                for (index, value) in pattern.iter().enumerate() {
                    bytes[start + index * 2..start + index * 2 + 2]
                        .copy_from_slice(&value.to_le_bytes());
                }
                let mut actual = vec![0u16; count];
                interface.read_16(args.address + offset as u64, &mut actual)?;
                ensure!(
                    actual == pattern,
                    "{mode:?} {access:?} 16-bit read mismatch at {count}"
                );
            }
            for (word, chunk) in expected.iter_mut().zip(bytes.as_chunks::<4>().0) {
                *word = u32::from_le_bytes(*chunk);
            }
            method(interface, verify);
            ensure!(
                read_region(interface, region, expected.len())? == *expected,
                "{mode:?} {access:?} {width}-byte write or guard mismatch at {count}"
            );
            check_debug_state(interface)?;
        }
    }
    Ok(())
}

fn exercise(
    interface: &mut RiscvCommunicationInterface<'_>,
    mode: HpmProbeMode,
    args: &Args,
    original: &[u32],
) -> Result<()> {
    halt(interface)?;
    interface
        .memory_access_config()
        .set_progbuf_multiword(args.multiword);
    let guard = 16;
    let region = args.address - guard as u64 * 4;
    method(interface, MemoryAccessMethod::SystemBus);
    interface.write_32(region, original)?;
    method(interface, MemoryAccessMethod::ProgramBuffer);
    ensure!(
        read_region(interface, region, original.len())? == original,
        "initial restoration failed"
    );
    let mut expected = original.to_vec();
    let mut sizes = vec![1, 2, 3, 17, 255, 2043, 2044, 2045, args.words];
    sizes.retain(|&size| size <= args.words);
    sizes.sort_unstable();
    sizes.dedup();

    for access in [
        MemoryAccessMethod::SystemBus,
        MemoryAccessMethod::ProgramBuffer,
    ] {
        if (args.method == "sba" && access != MemoryAccessMethod::SystemBus)
            || (args.method == "progbuf" && access != MemoryAccessMethod::ProgramBuffer)
        {
            continue;
        }
        println!("Testing {mode:?} {access:?}");
        let registers = scratch_registers(interface)?;
        let verify = match access {
            MemoryAccessMethod::SystemBus => MemoryAccessMethod::ProgramBuffer,
            _ => MemoryAccessMethod::SystemBus,
        };
        for count in sizes.iter().copied() {
            let pattern: Vec<u32> = (0..count)
                .map(|i| {
                    (i as u32).wrapping_mul(0x9e37_79b9)
                        ^ (count as u32).rotate_left(11)
                        ^ 0xa55a_1234
                })
                .collect();
            method(interface, access);
            interface
                .write_32(args.address, &pattern)
                .with_context(|| format!("{mode:?} {access:?} write {count} words"))?;
            expected[guard..guard + count].copy_from_slice(&pattern);
            method(interface, verify);
            let actual = read_region(interface, region, expected.len()).with_context(|| {
                format!("{mode:?} {access:?} independent verification at {count} words")
            })?;
            ensure!(
                actual == expected,
                "{mode:?} {access:?} write or guard mismatch at {count} words"
            );
            method(interface, access);
            let actual = read_region(interface, args.address, count)
                .with_context(|| format!("{mode:?} {access:?} read {count} words"))?;
            ensure!(
                actual == pattern,
                "{mode:?} {access:?} read mismatch at {count} words"
            );
            check_debug_state(interface)?;
        }

        exercise_subwords(interface, mode, args, access, verify, &mut expected)?;

        let pattern = &expected[guard..guard + args.words];
        for iteration in 0..args.repeats {
            method(interface, access);
            let start = Instant::now();
            let actual = read_region(interface, args.address, args.words)?;
            let read_time = start.elapsed();
            ensure!(actual == pattern, "{mode:?} {access:?} timed read mismatch");
            let start = Instant::now();
            interface.write_32(args.address, pattern)?;
            let write_time = start.elapsed();
            method(interface, verify);
            ensure!(
                read_region(interface, region, expected.len())? == expected,
                "{mode:?} {access:?} timed write or guard mismatch"
            );
            check_debug_state(interface)?;
            println!(
                "{mode:?} {access:?} {iteration}: read {:.1} KiB/s ({:.3} ms), write {:.1} KiB/s ({:.3} ms)",
                speed(args.words * 4, read_time),
                read_time.as_secs_f64() * 1000.0,
                speed(args.words * 4, write_time),
                write_time.as_secs_f64() * 1000.0
            );
        }
        ensure!(
            scratch_registers(interface)? == registers,
            "{mode:?} {access:?} changed target registers"
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        ["probex", "ftdi"].contains(&args.adapter.as_str()),
        "invalid adapter"
    );
    ensure!(args.speed > 0, "speed must be nonzero");
    ensure!(
        args.address >= 64 && args.address.is_multiple_of(4),
        "address must be aligned and leave room for the guard"
    );
    ensure!(
        args.words > 0 && args.repeats > 0,
        "need at least one word and one repetition"
    );
    ensure!(
        ["all", "direct", "raw"].contains(&args.mode.as_str()),
        "invalid mode"
    );
    let physical_transport = match args.transport.as_str() {
        "jtag" => HpmProbeTransport::Jtag,
        "cjtag" => HpmProbeTransport::Cjtag,
        _ => bail!("invalid transport: expected jtag or cjtag"),
    };
    if args.adapter == "ftdi" {
        ensure!(
            physical_transport == HpmProbeTransport::Jtag,
            "the FTDI HIL test supports JTAG only"
        );
        ensure!(args.mode != "direct", "FTDI has no Probe X direct mode");
    }
    ensure!(
        ["all", "sba", "progbuf"].contains(&args.method.as_str()),
        "invalid method"
    );
    let region = args.address - 64;
    let region_words = args.words.checked_add(32).context("region too large")?;
    ensure!(
        region
            .checked_add(
                (region_words as u64)
                    .checked_mul(4)
                    .context("region too large")?
            )
            .is_some(),
        "region overflows"
    );
    let mut backup_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.backup)
        .context("backup must be a new file")?;

    // Use a conservative FTDI clock for backup and recovery.
    let recovery_speed = if args.adapter == "ftdi" {
        args.speed.min(15_000)
    } else {
        args.speed
    };
    let (original, was_running) = with_interface(
        &args,
        HpmProbeMode::Direct,
        physical_transport,
        recovery_speed,
        |interface| {
            print_debug_state(interface, "Before test")?;
            let was_running = halt(interface)?;
            let result = (|| -> Result<Vec<u32>> {
                method(interface, MemoryAccessMethod::SystemBus);
                let original = read_region(interface, region, region_words)?;
                for value in &original {
                    backup_file.write_all(&value.to_le_bytes())?;
                }
                backup_file.sync_all()?;
                method(interface, MemoryAccessMethod::ProgramBuffer);
                ensure!(
                    read_region(interface, region, region_words)? == original,
                    "SBA and Program Buffer disagree before writing; backup retained"
                );
                Ok(original)
            })();
            if result.is_err() && was_running {
                interface.resume_core()?;
            }
            Ok((result?, was_running))
        },
    )?;
    println!(
        "Saved {} bytes including guards to {}",
        original.len() * 4,
        args.backup.display()
    );

    let test_result = (|| -> Result<()> {
        for mode in [HpmProbeMode::Direct, HpmProbeMode::RawJtag] {
            if (args.adapter == "ftdi" && mode == HpmProbeMode::Direct)
                || (args.mode == "direct" && mode != HpmProbeMode::Direct)
                || (args.mode == "raw" && mode != HpmProbeMode::RawJtag)
            {
                continue;
            }
            with_interface(&args, mode, physical_transport, args.speed, |interface| {
                exercise(interface, mode, &args, &original)
            })?;
        }
        Ok(())
    })();

    // Always try to restore through a fresh session, even after a failed test.
    let restore = with_interface(
        &args,
        HpmProbeMode::Direct,
        physical_transport,
        recovery_speed,
        |interface| {
            halt(interface)?;
            method(interface, MemoryAccessMethod::SystemBus);
            interface.write_32(region, &original)?;
            method(interface, MemoryAccessMethod::ProgramBuffer);
            ensure!(
                read_region(interface, region, original.len())? == original,
                "restored RAM differs from backup"
            );
            if was_running {
                interface.resume_core()?;
            }
            print_debug_state(interface, "After restoration")?;
            Ok(())
        },
    );
    if let Err(error) = restore {
        bail!(
            "RAM restoration failed: {error:#}; backup retained at {}; test result: {test_result:?}",
            args.backup.display()
        );
    }
    println!("Original RAM and guards restored and verified; original run state restored.");
    test_result
}
