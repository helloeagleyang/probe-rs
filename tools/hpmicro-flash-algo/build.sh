#!/usr/bin/env bash
# Build the HPM XPI NOR flash algorithms used by probe-rs target YAMLs.
#
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Requirements:
#   - riscv32-unknown-elf-gcc with rv32imafc_zicsr_zifencei support
#   - HPM SDK headers (HPM_SDK_BASE, default /mnt/d/repos/hpm_sdk)
#   - python3 + PyYAML
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
sdk=${HPM_SDK_BASE:-/mnt/d/repos/hpm_sdk}
gcc=${RISCV_GCC:-/opt/rv32imac_zicsr_zifencei_multilib_b_ext-elf/bin/riscv32-unknown-elf-gcc}
build_root=${HPM_FLASH_ALGO_BUILD_DIR:-"$root/build"}

if [[ ! -x "$gcc" ]]; then
    echo "RISC-V GCC not found: $gcc (set RISCV_GCC)" >&2
    exit 1
fi
if [[ ! -d "$sdk/soc/HPM5100/HPM5151" ]]; then
    echo "HPM SDK not found: $sdk (set HPM_SDK_BASE)" >&2
    exit 1
fi

includes=(
    -I"$root/include"
    -I"$sdk/soc/HPM5100/HPM5151"
    -I"$sdk/soc/HPM5100/ip"
    -I"$sdk/drivers/inc"
    -I"$sdk/arch"
    -I"$sdk/arch/riscv"
    -I"$sdk/arch/riscv/l1c"
    -I"$sdk/arch/riscv/intc"
    -I"$sdk/utils"
)

common_flags=(
    -march=rv32imafc_zicsr_zifencei
    -mabi=ilp32f
    -Os
    -DNDEBUG
    -fpic
    -ffreestanding
    -fno-builtin
    -ffunction-sections
    -fdata-sections
    -msmall-data-limit=0
    -Wall
    -Wno-format
)

mkdir -p "$build_root"

python3 - "$root/configs.yaml" <<'PY' > "$build_root/configs.tsv"
import sys, yaml
for c in yaml.safe_load(open(sys.argv[1])):
    print("|".join(str(c.get(k, "")) for k in (
        "kind", "name", "flash_base", "device_size", "xpi_base",
        "header", "opt0", "opt1")))
PY

while IFS='|' read -r kind name flash_base device_size xpi_base header opt0 opt1; do
    if [[ "$kind" == "small" ]]; then
        continue
    fi
    out="$build_root/$name"
    mkdir -p "$out"
    defs=(
        -DFLASH_BASE="$flash_base"
        -DFLASH_DEVICE_START="$flash_base"
        -DFLASH_DEVICE_SIZE="$device_size"
        -DXPI_BASE="$xpi_base"
        -DOPT_HEADER="$header"
        -DOPT0="$opt0"
        -DOPT1="$opt1"
    )

    "$gcc" "${common_flags[@]}" "${defs[@]}" "${includes[@]}" \
        -c "$root/src/loader.c" -o "$out/loader.o"
    "$gcc" "${common_flags[@]}" "${defs[@]}" "${includes[@]}" \
        -c "$sdk/arch/riscv/l1c/hpm_l1c_drv.c" -o "$out/l1c.o"
    "$gcc" "${common_flags[@]}" "${defs[@]}" "${includes[@]}" \
        -c "$root/src/wrapper.c" -o "$out/wrapper.o"
    "$gcc" "${common_flags[@]}" "${defs[@]}" "${includes[@]}" \
        -c "$root/src/flash_device.c" -o "$out/flash_device.o"

    "$gcc" \
        -march=rv32imafc_zicsr_zifencei -mabi=ilp32f \
        -nostdlib -T "$root/src/linker.ld" \
        -Wl,--gc-sections \
        -Wl,--build-id=none \
        -Wl,--no-warn-rwx-segments \
        -Wl,--defsym=__global_pointer\$=0 \
        -o "$out/hpm.elf" \
        "$out/loader.o" "$out/l1c.o" "$out/wrapper.o" "$out/flash_device.o" \
        -lgcc

    echo "built $name -> $out/hpm.elf"
done < "$build_root/configs.tsv"

"$root/build_small.sh"
