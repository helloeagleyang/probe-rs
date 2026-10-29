/*
 * probe-rs flash algorithm layout for HPM XPI NOR.
 *
 * probe-rs prepends two EBREAK words at load_address - 8, so code starts at
 * 0x20. PrgCode/PrgData are the section names target-gen extracts.
 */

ENTRY(Init)

SECTIONS
{
    . = 0x20;

    PrgCode : {
        KEEP(*(.flash_algo.text*))
        KEEP(*(.text*))
        KEEP(*(.rodata*))
        /* target-gen requires PrgData to follow PrgCode with no gap. */
        . = ALIGN(4);
    }

    PrgData : {
        KEEP(*(.flash_algo.data*))
        KEEP(*(.data*))
        KEEP(*(.sdata*))
        KEEP(*(.got*))
        KEEP(*(.flash_device))
    }

    /DISCARD/ : {
        *(.bss*)
        *(.sbss*)
        *(.comment)
        *(.riscv.attributes)
        *(.eh_frame*)
    }
}
