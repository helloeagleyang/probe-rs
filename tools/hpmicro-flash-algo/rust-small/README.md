# HPM 1 MiB SiP flash algorithm (hpm53x1 flavor)

This is a small CMSIS flash algorithm for the HPM 1 MiB SiP XPI0 flash,
derived from [`hpmicro-rs/flash-algo`](https://github.com/hpmicro-rs/flash-algo)
(MIT OR Apache-2.0). `src/romapi.rs` is the bindgen output for HPMicro's
BootROM XPI NOR API (BSD-3-Clause).

Two builds are produced:

| Feature | XPI0 base | Used by |
|---|---|---|
| `xpi-f300` | `0xF3000000` | HPM5300, HPM6P00, HPM5E00 XPI0 |
| `xpi-f00e` | `0xF00E0000` | HPM5100 XPI0 |

Fixed constants: flash window `0x80000000..0x80100000` (1 MiB), page 256,
sector 4096, `option0=0x05`, `option1=0x1000`, `header=0xFCF90002`.

The algorithm only calls the HPM BootROM XPI NOR API. It deliberately does not
touch SYSCTL or the L1 cache; the SDK OpenOCD loader for HPM5300 also skips
SYSCTL. HPM5E00 hybrid (`0xB0000000`) is not handled here; the generic C loader
in the parent directory handles it.

Build:

```sh
rustup target add riscv32imac-unknown-none-elf
cargo build --release --features xpi-f300
cargo build --release --features xpi-f00e
```

or run `../build_small.sh`, which copies the resulting ELFs into
`../build/flash-algo-hpm-xpi0-f300-1mb/hpm.elf` and
`../build/flash-algo-hpm-xpi0-f00e-1mb/hpm.elf` for `target-gen`.

Hardware qualification is required for HPM6P00/HPM5E00/HPM5100 and for the
rebuilt HPM5300 opt5 image.
