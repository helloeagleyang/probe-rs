# HPM Probe X

Probe discovery opens Probe X in direct mode. It uses the probe's RISC-V DMI
commands for control and uses firmware SBA or Program Buffer memory commands
only when the firmware advertises the respective opcode. Program Buffer
offloads are limited to a halted hart with sufficient Debug Module resources;
other accesses use the generic RISC-V implementation. The selected hart is
communicated to the probe before a Program Buffer offload, and its cached
program is invalidated afterwards.

Applications can select direct or raw mode and four-wire JTAG or two-wire
cJTAG. The default probe discovery uses direct mode and four-wire JTAG:

```rust,no_run
use probe_rs::probe::{DebugProbeSelector, Probe, hpmprobe::{HpmProbe, HpmProbeMode, HpmProbeTransport}};

let selector: DebugProbeSelector = "34b7:8003:PROBER0001".parse()?;
let mut probe = Probe::new(HpmProbe::open_with_transport(
    &selector, HpmProbeMode::RawJtag, HpmProbeTransport::Cjtag,
)?);
probe.set_speed(25_000)?; // kHz
# Ok::<(), Box<dyn std::error::Error>>(())
```

The CLI can select the same settings in `.probe-rs.toml`:

```toml
[probes."34b7:8003".driver]
type = "hpmprobe"
transport = "cjtag" # or "jtag"
mode = "raw"        # or "direct"
```

cJTAG requires firmware with the cJTAG backend enabled. Raw mode also requires
the native JTAG sequence command; raw cJTAG additionally requires the raw
cJTAG capability. The firmware performs the physical cJTAG activation, and
the driver reacquires it when a raw batch requests a TAP reset. Reacquisition
leaves the TAP in Idle; clocks requested while holding Test-Logic-Reset are
therefore unsupported in raw cJTAG mode. These settings still use the logical
JTAG protocol in probe-rs; physical TRST/SRST control is not supported. A USB
failure or malformed raw-JTAG reply requires reopening
the probe: an interrupted transfer may already have affected the target. DMI
errors in a batched memory stream are reported without replaying the stream.
Program Buffer cleanup waits for abstract commands to become idle and verifies
that autoexec is disabled before restoring scratch registers. If cleanup fails,
a hart that was stopped for the access remains halted.

The optional `MemoryAccessConfig::set_progbuf_multiword(true)` path is intended
for RAM. On abstract-command busy, it may reread the uncommitted batch. It is
off by default and is limited to a usable hart-visible data window, supported
autoexec registers, RV32, and a sufficiently large Program Buffer.
This option does not change SBA accesses: raw JTAG SBA independently batches
DMI reads and uses the Debug Module's system-bus auto-increment support.

## Hardware test

`probe-rs/examples/hpm_probex_hil.rs` tests direct and raw modes with SBA and
Program Buffer reads and writes at 25 MHz. It tests 8-, 16-, and 32-bit
accesses, short transfers, and, when the scratch area is large enough, transfers
spanning the 8,176-byte packet boundary. It checks writes using the other
memory engine, checks 64-byte guard regions, the halted/autoexec state,
selected scratch registers and DPC, and
restores and independently verifies the saved RAM. The original run state is
restored on successful cleanup.

Choose a known writable scratch area and a **new** backup file. The test
temporarily overwrites the area and both guards must also be accessible RAM.
For the HPM6800EVK fixture used during development:

```sh
cargo run --release -p probe-rs --example hpm_probex_hil -- \
  --probe 34b7:8003:PROBER0001 --expected-idcode 0x1000563d \
  --address 0x10000 --words 4096 --repeats 3 \
  --backup /tmp/probex-ram-backup.bin
```

Use `--mode raw --method progbuf --multiword` to test the optional multiword
path. Add `--transport cjtag` to run the same checks and 25 MHz measurements
through cJTAG. Use `--words 256` or `--words 1024` for 1 KiB or 4 KiB
transfers. The backup is retained if restoration fails; a disconnected probe
can prevent automatic restoration.

### HPM5100EVK cJTAG results

On 2026-10-30, an HPM5100EVK target was connected to Probe X
(`34b7:8003:PROBER0001`). The probe reported a 25 MHz cJTAG clock, control
protocol 1.6, RISC-V tunnel 1.28 and raw JTAG tunnel 1.2. The target IDCODE was
`0x1000563d`; this IDCODE alone does not distinguish HPM5100 from other
boards. Tests used 16 KiB at `0x20000` and 64-byte guards on both sides.

Each row is the median of three independent sessions; each session's value is
the median of three timed transfers after the correctness checks. The speeds
include host, USB, probe and target overhead. The reported clock is the
configured value; the physical waveform was not measured in these runs.

| Mode | Memory method | Read KiB/s | Write KiB/s |
| --- | --- | ---: | ---: |
| Direct cJTAG | SBA | 444.3 | 440.4 |
| Direct cJTAG | Program Buffer | 243.7 | 242.1 |
| Raw cJTAG | SBA | 189.0 | 187.3 |
| Raw cJTAG | Program Buffer | 174.8 | 173.9 |
| Raw cJTAG | SBA, Program Buffer multiword enabled | 186.4 | 186.2 |
| Raw cJTAG | Program Buffer, multiword opt-in | 169.7 | 168.7 |

All nine sessions passed their read/write, guard, register and debug-state
checks and independently verified RAM restoration. Separate runs used
host-side breakpoints to confirm all four direct SBA/Program Buffer offloads
and both raw multiword batch functions executed; they are excluded from the
throughput table. Multiword was slightly slower on this fixture and remains
opt-in. Enabling it does not change SBA.

The first raw cJTAG attempt exposed a reset issue: sending five logical reset
clocks after firmware reacquisition caused this target to exit OScan1. The
driver now uses the firmware's physical reset and resumes from Idle; the
successful raw runs above use that fix. To reproduce on this fixture, use the
hardware test command above with `--transport cjtag --address 0x20000` and a
new backup path. A separate HIL run also reset the TAP in one batch and read
the IDCODE in the next, and verified that clocks requested while holding reset
are rejected.

### HPM5100EVK four-wire JTAG results

The same target and probe were tested over JTAG on 2026-10-30. The probe
reported 25 MHz, and the tests used 16 KiB at `0x20000` with 64-byte guards.
Each row is the median of three independent sessions, each containing three
timed transfers after the correctness checks. The physical waveform was not
measured in these runs.

| Mode | Memory method | Read KiB/s | Write KiB/s |
| --- | --- | ---: | ---: |
| Direct JTAG | SBA | 1,412.0 | 1,423.1 |
| Direct JTAG | Program Buffer | 601.7 | 596.3 |
| Raw JTAG | SBA | 521.1 | 513.7 |
| Raw JTAG | Program Buffer | 177.0 | 176.5 |
| Raw JTAG | SBA, Program Buffer multiword enabled | 527.0 | 517.2 |
| Raw JTAG | Program Buffer, multiword opt-in | 298.3 | 294.9 |

All nine sessions passed read/write, guard, register and debug-state checks;
the original RAM and run state were restored and verified. Separate runs with
host-side breakpoints confirmed all four Direct offloads and both Raw multiword
batch functions executed; those runs are excluded from the timing table. On
this fixture, multiword increased Raw Program Buffer read and write throughput
by approximately 69% and 67%, respectively. It does not change SBA.

The final `dmstatus` was `0x004f0ca2`: the hart was running, but its sticky
`anyhavereset` and `allhavereset` bits were set. The test did not issue a
target-reset command, and those bits were not sampled immediately before the
JTAG sessions, so when they were set is unknown. `abstractauto` was zero.

### HPM5100EVK small-transfer comparison

The same fixture was tested at a probe-reported 25 MHz with 1 KiB and 4 KiB
transfers at `0x20000`. Each value below is the median of three independent
sessions; each session's value is the median of five timed transfers after the
correctness checks. Speeds are in KiB/s and include host, USB, probe and target
overhead. The physical clock waveform was not measured.

| Size | Mode | Memory method | JTAG read | JTAG write | cJTAG read | cJTAG write |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| 1 KiB | Direct | SBA | 1,196.9 | 1,253.8 | 406.3 | 414.3 |
| 1 KiB | Direct | Program Buffer | 433.8 | 423.0 | 181.3 | 176.3 |
| 1 KiB | Raw | SBA | 463.6 | 490.7 | 179.7 | 179.5 |
| 1 KiB | Raw | SBA, multiword enabled | 500.3 | 488.3 | 180.4 | 179.1 |
| 1 KiB | Raw | Program Buffer | 134.6 | 144.5 | 126.8 | 134.9 |
| 1 KiB | Raw | Program Buffer, multiword enabled | 193.5 | 185.5 | 122.4 | 120.7 |
| 4 KiB | Direct | SBA | 1,392.8 | 1,421.5 | 445.7 | 442.3 |
| 4 KiB | Direct | Program Buffer | 578.2 | 572.3 | 236.4 | 234.4 |
| 4 KiB | Raw | SBA | 504.1 | 503.6 | 183.6 | 183.3 |
| 4 KiB | Raw | SBA, multiword enabled | 497.7 | 503.0 | 180.7 | 182.9 |
| 4 KiB | Raw | Program Buffer | 164.8 | 165.6 | 160.7 | 161.8 |
| 4 KiB | Raw | Program Buffer, multiword enabled | 266.0 | 256.2 | 157.5 | 155.2 |

All 36 sessions passed the data, guard, register and debug-state checks and
independently verified RAM restoration. The retained backups matched the
corresponding prefix of the earlier 16 KiB backup. `dmstatus` was
`0x004f0ca2` before and after the small-transfer tests: the hart was running,
and the previously noted sticky reset bits were set both times. `abstractauto`
was zero before and after. Multiword changes only the Program Buffer path, not
SBA; on this fixture it improved Raw JTAG throughput but was slower over cJTAG.

### Measured fixture

On 2026-10-30, an HPM Probe X (`34b7:8003:PROBER0001`) was connected to the
HPM6800EVK fixture, IDCODE `0x1000563d`. The probe reported control protocol
1.6, RISC-V tunnel 1.28, an 8,192-byte maximum payload, and a 25 MHz JTAG
clock using four-wire JTAG. A release build was tested with 16 KiB transfers at
`0x10000`. These measurements predate the cJTAG and DMI status-check changes;
the current implementation's performance has not yet been remeasured.

Each row is the median of three fresh sessions; each session's value is the
median of three timed transfers after the correctness checks. Timing includes
the probe-rs memory operation but excludes verification. These are fixture
measurements, not a throughput guarantee for other targets or firmware.

| Mode | Memory method | Read KiB/s | Write KiB/s |
| --- | --- | ---: | ---: |
| Direct | SBA | 1,406 | 1,435 |
| Direct | Program Buffer | 632 | 667 |
| Raw JTAG | SBA | 518 | 683 |
| Raw JTAG | SBA, Program Buffer multiword enabled | 505 | 668 |
| Raw JTAG | Program Buffer | 245 | 287 |
| Raw JTAG | Program Buffer, multiword opt-in | 347 | 410 |

The SBA rows use the same SBA implementation; enabling Program Buffer
multiword changes the verification path, not the timed SBA transfer. All nine
sessions completed their checks and independently verified restoration.

## Audit notes

The original direct path could dispatch memory operations without checking
firmware capability bits or synchronizing the firmware's selected hart. It
could also let immediate DMI operations overtake scheduled writes, retry an
ambiguous timeout, and leave the host's Program Buffer cache stale after a
probe-side operation. The former reset implementation wrote `haltreq` while
claiming to assert a physical reset. Those paths now preserve ordering, gate
offloads, synchronize the hart and cache, and report unsupported physical
reset control.

The generic JTAG path previously retried the scan that observed DMI busy instead
of the preceding request whose response it carried. Program Buffer reads also
accessed `DATA0` before an abstract command completed, and the multiword path
did not bound the hart-visible data window by `hartinfo.datasize`. The updated
paths correct those cases and batch ordinary and multiword transfers with
adaptive idle timing.
