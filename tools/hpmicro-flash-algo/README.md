# HPMicro HPM flash algorithms and target generator

This directory contains two flash-algorithm implementations and the generator
for the HPM target YAMLs:

1. **Small hpm53x1-flavor Rust algorithm** for 1 MiB SiP XPI0 flash
   (`rust-small/`). One build uses XPI0 `0xF3000000` for HPM5300/HPM6P00/HPM5E00;
   another uses `0xF00E0000` for HPM5100.
2. **Generic C ROM-API loader** for external XPI NOR, 4 MiB SiP, XPI1 and the
   HPM5E00 hybrid window (`src/`).

Both are CMSIS flash algorithms consumed by probe-rs.

## Provenance and license

- `src/loader.c` is adapted from HPMicro's
  `hpm_sdk/samples/openocd_algo/src/openocd_flash_algo.c` and the
  `hpm-prober` unified flashloader (both BSD-3-Clause). The original copyright
  header is retained.
- `rust-small/` is adapted from
  [`hpmicro-rs/flash-algo`](https://github.com/hpmicro-rs/flash-algo)
  (MIT OR Apache-2.0). Its `src/romapi.rs` is bindgen output for HPMicro's
  BootROM XPI NOR API (BSD-3-Clause).
- `src/wrapper.c`, `src/flash_device.c`, `src/linker.ld`, `build.sh`,
  `build_small.sh`, `configs.yaml`, `gen_targets.py` and the small-algorithm
  wrapper are new and dual-licensed under MIT OR Apache-2.0, matching probe-rs.

## Prerequisites

For the generic C loader:

- `riscv32-unknown-elf-gcc` with `rv32imafc_zicsr_zifencei` support.
  Default: `/opt/rv32imac_zicsr_zifencei_multilib_b_ext-elf/bin/riscv32-unknown-elf-gcc`
  (override with `RISCV_GCC`).
- HPM SDK headers. Default: `/mnt/d/repos/hpm_sdk` (override with
  `HPM_SDK_BASE`). The HPM5151 headers are used as the reference build; the
  ROM API structures are shared across the HPM families.

For the small Rust loader:

- `cargo` and the `riscv32imac-unknown-none-elf` target
  (`rustup target add riscv32imac-unknown-none-elf`).

For both:

- Python 3 with PyYAML.
- A built `target-gen` (`cargo build -p target-gen`).

## Build algorithms

```sh
./build.sh
```

`build.sh` builds the generic C configs (those without `kind: small`) and then
calls `build_small.sh`, which builds the two Rust algorithms. To build only one
side:

```sh
./build.sh       # generic C configs, then small
./build_small.sh # small Rust algorithms only
```

Outputs land in `build/<algorithm>/hpm.elf`. The generic C ELF layout is
`PrgCode` at `0x20`, `PrgData` after it, and a 160-byte CMSIS `FlashDevice`
descriptor. The small Rust ELF puts code and zero-initialized state in
`PrgCode` and the `FlashDevice` descriptor in `DeviceData`; `target-gen`
handles both layouts. `target-gen` fills the algorithm entries in the YAMLs.

## Regenerate target YAMLs

```sh
python3 gen_targets.py \
  --socs /mnt/d/repos/hpm_iar_enablement/scripts/hpm_socs.yaml
```

The generator writes `probe-rs/targets/HPM*_Series.yaml`, fills each
placeholder algorithm with `target-gen elf --update`, and runs
`target-gen reformat`. Use `--no-update` to only emit the base YAML.

`HPM5300_Series.yaml` keeps the `flash-algo-hpm53x1` name for compatibility,
but its body is the small 1 MiB opt5 algorithm (rebuilt from the hpm53x1
flavor) and package aliases are added.

## Generated coverage

- HPM6700 (11 parts, single/dual core, XPI0 + XPI1): generic C
- HPM6300 (10 parts, single core, XPI0 + XPI1): generic C
- HPM6200 (12 parts, single/dual core, XPI0): generic C
- HPM6800 (3 parts, single core, XPI0): generic C
- HPM6E00 (10 parts, single/dual core, XPI0): generic C
- HPM6P00 (3 parts, single/dual core, XPI0): small F300
- HPM5E00 (4 parts, single core, XPI0 + hybrid XPI0): small F300 + generic hybrid
- HPM5100 (4 parts, single core, XPI0): small F00E
- HPM5300 (4 existing variants): small F300 opt5, package aliases

NVM windows are 32 MiB for external NOR, 4 MiB for 4 MiB SiP parts and 1 MiB
for 1 MiB SiP parts. The ROM API queries the actual flash size, so an 8 MiB or
16 MiB board works inside the 32 MiB window.

## Known caveats

- All algorithms use freq_opt (option0) `0x5`. Option1 still varies per family
  (`0`, `0xE`, `0x10E`, `0x1000`, `0x100E`).
- The small Rust algorithm calls only the BootROM XPI NOR API. It does **not**
  touch SYSCTL or the L1 cache. The SDK OpenOCD HPM5300 loader also skips
  SYSCTL, but the HPM5300 opt5 image and the HPM6P00/HPM5E00/HPM5100 images
  are not hardware-qualified yet.
- HPM5300 previously used a 1128-byte opt0 `0x6` blob. It now uses the
  same-source small opt5 image; run HPM5300EVK flash/verify before merge.
- No automatic chip detection: HPM's JTAG IDCODE is the generic D25/D45 ID,
  and `hpm_socs.yaml` has no per-part OTP CHIP_ID table. Select a part with
  `--chip`.
- HPM6264 is modeled with its two-argument XPI0 configuration. If a board
  needs `0x1000`, use `hpm-xpi0-f304-opt5-1000-4mb`.
- HPM6P00/HPM5E00 parts use option0 `0x5`. Some EVK board entries say `0x6`;
  add a board-specific algorithm if hardware requires it.
- HPM5E00 hybrid (`0xB0000000`) still uses the generic C loader; the small
  algorithm only covers XPI0 `0x80000000`.
- Dual-core parts expose hart 0/1 on the same TAP. Default RISC-V reset is
  used; CPU1 may remain held by SYSCTL. Add a vendor debug sequence only if
  hardware testing shows it is needed.
- Package base-name aliases resolve to the first package in `hpm_socs.yaml`.
  For HPM6700 VMx vs ANx, `--chip HPM6750` selects the VMx/option-0xE
  algorithm; use `HPM6750xANx` for the channel-B board.
- **No hardware validation was performed for this change.** Recommended order:
  HPM6P00EVK, HPM5100EVK, HPM5300EVK, HPM6750EVK, HPM6E00EVK, HPM6800EVK,
  then HPM6300/HPM6200/HPM5E00. For each: flash, verify, reset, run; dual-core
  parts also attach `--core 1`.
