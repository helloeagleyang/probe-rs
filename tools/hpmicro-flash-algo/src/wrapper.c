/*
 * probe-rs / CMSIS flash-algorithm entry points for HPM XPI NOR.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * The per-variant constants are supplied by build.sh:
 *   FLASH_BASE, XPI_BASE, OPT_HEADER, OPT0, OPT1
 */

#include <stdint.h>

#define SECTOR_SIZE 0x1000U

extern uint32_t flash_init(uint32_t flash_base, uint32_t header, uint32_t opt0,
                           uint32_t opt1, uint32_t xpi_base_addr);
extern uint32_t flash_erase(uint32_t flash_base, uint32_t address,
                            uint32_t size);
extern uint32_t flash_program(uint32_t flash_base, uint32_t address,
                              uint32_t *buf, uint32_t size);
extern uint32_t flash_erase_chip(uint32_t flash_base);
extern uint32_t flash_deinit(void);

#define FLASH_ALGO_TEXT __attribute__((section(".flash_algo.text"), used))

FLASH_ALGO_TEXT uint32_t Init(uint32_t base, uint32_t clock, uint32_t operation)
{
    (void)base;
    (void)clock;
    (void)operation;
    return flash_init(FLASH_BASE, OPT_HEADER, OPT0, OPT1, XPI_BASE);
}

FLASH_ALGO_TEXT uint32_t UnInit(uint32_t operation)
{
    (void)operation;
    return flash_deinit();
}

FLASH_ALGO_TEXT uint32_t EraseSector(uint32_t address)
{
    return flash_erase(FLASH_BASE, address - FLASH_BASE, SECTOR_SIZE);
}

FLASH_ALGO_TEXT uint32_t ProgramPage(uint32_t address, uint32_t size,
                                    uint32_t *buffer)
{
    return flash_program(FLASH_BASE, address - FLASH_BASE, buffer, size);
}

FLASH_ALGO_TEXT uint32_t EraseChip(void)
{
    return flash_erase_chip(FLASH_BASE);
}
