#!/usr/bin/env python3
"""Generate probe-rs target YAMLs for the HPMicro HPM series.

The chip/flash parameter source is hpm_socs.yaml from hpm_iar_enablement.
Memory maps are transcribed from the generated IAR DDF/ICF files in that
repository. Run build.sh first to produce tools/hpmicro-flash-algo/build/*/hpm.elf,
then run this script; it fills each placeholder algorithm with target-gen.
"""

from __future__ import annotations

import argparse
import base64
import json
import re
import subprocess
import sys
from pathlib import Path

import yaml

DUAL_OVERRIDES = {
    # hpm_socs.yaml contains contradictory duplicate entries for these parts.
    "HPM6754xVMx": 2,
    "HPM6E8YxGNx": 2,
}

# Families with a second XPI controller window.
XPI1_FAMILIES = {"HPM6700", "HPM6300"}

# RAM regions transcribed from the IAR DDF/ICF memory maps.
# (name, start, end, role) where role is c0 / c1 / shared.
RAM_REGIONS = {
    "HPM6700": [
        ("ILM0", 0x0000_0000, 0x0004_0000, "c0"),
        ("DLM0", 0x0008_0000, 0x000C_0000, "c0"),
        ("XRAM0", 0x0108_0000, 0x0110_0000, "shared"),
        ("XRAM1", 0x0110_0000, 0x0118_0000, "shared"),
        ("CPU1_ILM", 0x0118_0000, 0x011C_0000, "c1"),
        ("CPU1_DLM", 0x011C_0000, 0x0120_0000, "c1"),
        ("HRAM", 0xF030_0000, 0xF030_8000, "shared"),
    ],
    "HPM6300": [
        ("ILM0", 0x0000_0000, 0x0002_0000, "c0"),
        ("DLM0", 0x0008_0000, 0x000A_0000, "c0"),
        ("XRAM0", 0x0108_0000, 0x0110_0000, "shared"),
        ("HRAM", 0xF030_0000, 0xF030_8000, "shared"),
    ],
    "HPM6200": [
        ("ILM0", 0x0000_0000, 0x0002_0000, "c0"),
        ("DLM0", 0x0008_0000, 0x000A_0000, "c0"),
        ("XRAM0", 0x0108_0000, 0x010C_0000, "shared"),
        ("CPU1_ILM", 0x010C_0000, 0x010E_0000, "c1"),
        ("CPU1_DLM", 0x010E_0000, 0x0110_0000, "c1"),
        ("HRAM", 0xF030_0000, 0xF030_8000, "shared"),
    ],
    "HPM6800": [
        ("ILM0", 0x0000_0000, 0x0004_0000, "c0"),
        ("DLM0", 0x0008_0000, 0x000C_0000, "c0"),
        ("XRAM0", 0x0120_0000, 0x0128_0000, "shared"),
        ("HRAM", 0xF040_0000, 0xF040_8000, "shared"),
    ],
    "HPM6E00": [
        ("ILM0", 0x0000_0000, 0x0004_0000, "c0"),
        ("CPU1_ILM", 0x0004_0000, 0x0008_0000, "c1"),
        ("DLM0", 0x0020_0000, 0x0024_0000, "c0"),
        ("CPU1_DLM", 0x0024_0000, 0x0028_0000, "c1"),
        ("XRAM0", 0x0120_0000, 0x0128_0000, "shared"),
        ("XRAM1", 0x0128_0000, 0x0130_0000, "shared"),
        ("HRAM", 0xF020_0000, 0xF020_8000, "shared"),
    ],
    "HPM6P00": [
        ("ILM0", 0x0000_0000, 0x0002_0000, "c0"),
        ("CPU1_ILM", 0x0004_0000, 0x0006_0000, "c1"),
        ("DLM0", 0x0020_0000, 0x0022_0000, "c0"),
        ("CPU1_DLM", 0x0024_0000, 0x0026_0000, "c1"),
        ("XRAM0", 0x0120_0000, 0x0124_0000, "shared"),
        ("HRAM", 0xF020_0000, 0xF020_8000, "shared"),
    ],
    "HPM5E00": [
        ("CPU0_ILM", 0x0000_0000, 0x0002_0000, "c0"),
        ("CPU0_DLM", 0x0020_0000, 0x0022_0000, "c0"),
        ("XRAM0", 0x0120_0000, 0x0124_0000, "shared"),
        ("HRAM", 0xF020_0000, 0xF020_8000, "shared"),
    ],
    "HPM5100": [
        ("ILM0", 0x0000_0000, 0x0004_0000, "c0"),
        ("DLM0", 0x0020_0000, 0x0022_0000, "c0"),
        ("XRAM0", 0x0120_0000, 0x0122_0000, "shared"),
        ("HRAM", 0xF020_0000, 0xF020_8000, "shared"),
    ],
}

FAMILY_ORDER = [
    "HPM6700",
    "HPM6300",
    "HPM6200",
    "HPM6800",
    "HPM6E00",
    "HPM6P00",
    "HPM5E00",
    "HPM5100",
]

HPM5300_PACKAGES = {
    "HPM5361": ["HPM5361xCBx", "HPM5361xCFx", "HPM5361xEGx"],
    "HPM5331": ["HPM5331xCBx", "HPM5331xCFx", "HPM5331xEGx"],
    "HPM5321": ["HPM5321xCBx", "HPM5321xCFx", "HPM5321xEGx"],
    "HPM5301": ["HPM5301xCBx", "HPM5301xCFx", "HPM5301xEGx"],
}


def parse_sip(value) -> int:
    s = str(value).strip().upper()
    if s in ("", "0", "NONE"):
        return 0
    for suffix, scale in (("MB", 1024 * 1024), ("KB", 1024), ("B", 1)):
        if s.endswith(suffix):
            return int(s[: -len(suffix)], 0) * scale
    raise ValueError(f"unsupported sip_flash value: {value!r}")


def parse_xpi(value: str):
    fields = [int(part.strip(), 0) for part in value.split(",")]
    if len(fields) == 2:
        return fields[0], fields[1], 0, 0xFCF90001
    if len(fields) == 3:
        return fields[0], fields[1], fields[2], 0xFCF90002
    raise ValueError(f"unsupported xpi0_flash_param: {value!r}")


def load_configs(path: Path):
    configs = yaml.safe_load(path.read_text())
    by_name = {c["name"]: c for c in configs}
    by_key = {
        (
            c["flash_base"],
            c["device_size"],
            c["xpi_base"],
            c["header"],
            c["opt0"],
            c["opt1"],
        ): c
        for c in configs
    }
    return configs, by_name, by_key


def load_families(path: Path):
    data = yaml.safe_load(path.read_text())
    families = {}
    for entry in data["families"]:
        for soc, contents in entry.items():
            info = {}
            for item in contents:
                info.update(item)
            name = info["family_name"]
            order = []
            parts = {}
            for part_entry in info["part_numbers"]:
                for part, props_list in part_entry.items():
                    if part not in parts:
                        order.append(part)
                    props = {}
                    for item in props_list:
                        props.update(item)
                    parts[part] = props
            families[name] = {
                "soc": soc,
                "info": info,
                "order": order,
                "parts": parts,
            }
    return families


def core_count(part: str, props: dict) -> int:
    if part in DUAL_OVERRIDES:
        return DUAL_OVERRIDES[part]
    count = int(props.get("num_of_cores", 1))
    if count not in (1, 2):
        raise ValueError(f"{part}: unsupported core count {count}")
    return count


def regions_for(family: str, dual: bool):
    result = []
    for name, start, end, role in RAM_REGIONS[family]:
        if role == "c1":
            if not dual:
                continue
            cores = ["core1"]
        elif role == "c0":
            cores = ["core0"]
        else:
            cores = ["core0", "core1"] if dual else ["core0"]
        result.append((name, start, end, cores))
    return result


def nvm_for(family: str, dual: bool, sip_size: int):
    shared = ["core0", "core1"] if dual else ["core0"]
    size = sip_size if sip_size else 0x200_0000
    result = [
        ("XPI0", 0x8000_0000, 0x8000_0000 + size, shared, True),
    ]
    if family in XPI1_FAMILIES:
        result.append(("XPI1", 0x9000_0000, 0x9200_0000, shared, False))
    if family == "HPM5E00":
        result.append(("HYBRID_XPI0", 0xB000_0000, 0xB010_0000, shared, False))
    return result


def algorithm_for(family: str, props: dict, by_key) -> str:
    xpi_base, _source_opt0, opt1, header = parse_xpi(props["xpi0_flash_param"])
    # HPM freq_opt (option0) is unified to 0x5 for every part.
    opt0 = 0x5
    sip_size = parse_sip(props.get("sip_flash", 0))
    device_size = sip_size if sip_size else 0x200_0000
    key = (0x8000_0000, device_size, xpi_base, header, opt0, opt1)
    config = by_key.get(key)
    if config is None:
        raise KeyError(
            f"{family}: no algorithm config for "
            f"base=0x{key[0]:x} size=0x{device_size:x} xpi=0x{xpi_base:x} "
            f"header=0x{header:x} opt0=0x{opt0:x} opt1=0x{opt1:x}"
        )
    return config["name"]


def emit_ram(name: str, start: int, end: int, cores):
    lines = [
        "  - !Ram",
        f"    name: {name}",
        "    range:",
        f"      start: 0x{start:x}",
        f"      end: 0x{end:x}",
        "    cores:",
    ]
    lines += [f"    - {core}" for core in cores]
    return lines


def emit_nvm(name: str, start: int, end: int, cores, boot: bool):
    lines = [
        "  - !Nvm",
        f"    name: {name}",
        "    range:",
        f"      start: 0x{start:x}",
        f"      end: 0x{end:x}",
        "    cores:",
    ]
    lines += [f"    - {core}" for core in cores]
    if boot:
        lines += ["    access:", "      boot: true"]
    return lines


def emit_algorithm(config: dict):
    start = config["flash_base"]
    end = start + config["device_size"]
    default = "true" if "xpi0" in config["name"] and "hybrid" not in config["name"] else "false"
    # target-gen compacts algorithms with identical content. Keep each
    # placeholder distinct until its real ELF has been merged.
    placeholder = base64.b64encode(
        f"placeholder:{config['name']}".encode()
    ).decode()
    return [
        f"- name: {config['name']}",
        f"  description: {json.dumps(config['description'])}",
        f"  default: {default}",
        f'  instructions: "{placeholder}"',
        "  load_address: 0x20",
        "  flash_properties:",
        "    address_range:",
        f"      start: 0x{start:x}",
        f"      end: 0x{end:x}",
        "    page_size: 0x100",
        "    erased_byte_value: 0xff",
        "    program_page_timeout: 1000",
        "    erase_sector_timeout: 2000",
        "    sectors:",
        "    - size: 0x1000",
        "      address: 0x0",
        "  cores:",
        "  - core0",
        "  stack_size: 2048",
    ]


def emit_family(family: str, data: dict, by_name, by_key):
    info = data["info"]
    parts = data["parts"]
    order = data["order"]

    variant_algorithms = {}
    used_algorithms = []
    for part in order:
        props = parts[part]
        algo = algorithm_for(family, props, by_key)
        algos = [algo]
        if family in XPI1_FAMILIES:
            algos.append("hpm-xpi1-f3044000-opt5-32mb")
        if family == "HPM5E00":
            algos.append("hpm-hybrid-f300-opt5-1000-1mb")
        variant_algorithms[part] = algos
        for name in algos:
            if name not in used_algorithms:
                used_algorithms.append(name)

    lines = [
        f"name: {family} Series",
        "generated_from_pack: false",
        "variants:",
    ]

    seen_bases = set()
    for part in order:
        props = parts[part]
        dual = core_count(part, props) == 2
        base = part[:7]
        alias = base if base != part and base not in seen_bases else None
        if alias:
            seen_bases.add(base)

        lines.append(f"- name: {part}")
        if alias:
            lines += ["  package_variants:", f"  - {alias}"]
        lines.append("  cores:")
        for index in range(2 if dual else 1):
            lines += [
                f"  - name: core{index}",
                "    type: riscv",
                "    core_access_options: !Riscv",
                f"      hart_id: {index}",
            ]

        lines.append("  memory_map:")
        for name, start, end, cores in regions_for(family, dual):
            lines += emit_ram(name, start, end, cores)
        for name, start, end, cores, boot in nvm_for(
            family, dual, parse_sip(props.get("sip_flash", 0))
        ):
            lines += emit_nvm(name, start, end, cores, boot)

        lines.append("  flash_algorithms:")
        lines += [f"  - {algo}" for algo in variant_algorithms[part]]

    lines.append("flash_algorithms:")
    for algo in used_algorithms:
        lines += emit_algorithm(by_name[algo])

    return lines, used_algorithms


def add_hpm5300_aliases(path: Path):
    text = path.read_text()
    if "package_variants:" in text:
        return
    for base, packages in HPM5300_PACKAGES.items():
        marker = f"- name: {base}\n"
        if marker not in text:
            continue
        block = [f"  package_variants:"] + [f"  - {pkg}" for pkg in packages]
        text = text.replace(marker, marker + "\n".join(block) + "\n", 1)
    path.write_text(text)


def update_hpm5300(path: Path, build_dir: Path, target_gen: Path):
    text = path.read_text().replace("  stack_size: 1024\n", "  stack_size: 2048\n")
    path.write_text(text)

    elf = build_dir / "flash-algo-hpm-xpi0-f300-1mb" / "hpm.elf"
    if not elf.is_file():
        raise FileNotFoundError(f"missing HPM5300 algorithm ELF: {elf}")
    subprocess.run(
        [
            str(target_gen),
            "elf",
            "--fixed-load-address",
            "--update",
            "--name",
            "flash-algo-hpm53x1",
            str(elf),
            str(path),
        ],
        check=True,
    )
    subprocess.run([str(target_gen), "reformat", str(path)], check=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--socs",
        type=Path,
        default=Path("/mnt/d/repos/hpm_iar_enablement/scripts/hpm_socs.yaml"),
    )
    parser.add_argument(
        "--configs",
        type=Path,
        default=Path(__file__).resolve().parent / "configs.yaml",
    )
    parser.add_argument(
        "--targets-dir",
        type=Path,
        default=Path(__file__).resolve().parents[2] / "probe-rs/targets",
    )
    parser.add_argument(
        "--build-dir",
        type=Path,
        default=Path(__file__).resolve().parent / "build",
    )
    parser.add_argument(
        "--target-gen",
        type=Path,
        default=Path(__file__).resolve().parents[2] / "target/debug/target-gen",
    )
    parser.add_argument("--no-update", action="store_true")
    args = parser.parse_args()

    _, by_name, by_key = load_configs(args.configs)
    families = load_families(args.socs)

    missing = [f for f in FAMILY_ORDER if f not in families]
    if missing:
        print(f"missing families in {args.socs}: {missing}", file=sys.stderr)
        return 1

    if not args.no_update and not args.target_gen.is_file():
        print(
            f"target-gen not found at {args.target_gen}; build it or pass --target-gen",
            file=sys.stderr,
        )
        return 1

    total_variants = 0
    for family in FAMILY_ORDER:
        lines, algorithms = emit_family(
            family, families[family], by_name, by_key
        )
        path = args.targets_dir / f"{family}_Series.yaml"
        path.write_text("\n".join(lines) + "\n")
        total_variants += len(families[family]["order"])
        print(f"wrote {path} ({len(families[family]['order'])} variants, {len(algorithms)} algorithms)")

        if args.no_update:
            continue

        for algo in algorithms:
            elf = args.build_dir / algo / "hpm.elf"
            if not elf.is_file():
                print(f"missing algorithm ELF: {elf}; run build.sh", file=sys.stderr)
                return 1
            subprocess.run(
                [
                    str(args.target_gen),
                    "elf",
                    "--fixed-load-address",
                    "--update",
                    "--name",
                    algo,
                    str(elf),
                    str(path),
                ],
                check=True,
            )
        subprocess.run([str(args.target_gen), "reformat", str(path)], check=True)

    hpm5300 = args.targets_dir / "HPM5300_Series.yaml"
    if hpm5300.is_file():
        add_hpm5300_aliases(hpm5300)
        if not args.no_update:
            update_hpm5300(hpm5300, args.build_dir, args.target_gen)
        print(f"updated {hpm5300} package aliases")

    print(f"generated {len(FAMILY_ORDER)} families, {total_variants} variants")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
