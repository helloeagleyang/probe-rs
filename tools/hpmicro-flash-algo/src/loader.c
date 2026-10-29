/*
 * Copyright (c) 2021,2026 HPMicro
 *
 * SPDX-License-Identifier: BSD-3-Clause
 *
 * Adapted from:
 *   hpm_sdk/samples/openocd_algo/src/openocd_flash_algo.c
 *   hpm-prober/flashloader/unified/loader.c
 *
 * The XPI NOR configuration is passed by wrapper.c at runtime; this file only
 * contains the ROM-API based erase/program primitives.
 */

#include "board.h"
#include "hpm_common.h"
#include "hpm_l1c_drv.h"
#include "hpm_romapi.h"

#define XPI_USE_PORT_B_MASK (0x100)

/* HPM6200 BootROM 0x56010000 needs the FPU enabled during auto_config(). */
#define HPM6200_FPU_WORKAROUND_ROM_VERSION (0x56010000UL)

#define ROMAPI_SUPPORTS_HYBRIDXPI()                                            \
    (((ROM_API_TABLE_ROOT->xpi_nor_driver_if->version >= 0x56010300)) &&       \
     (ROM_API_TABLE_ROOT->xpi_nor_driver_if->enable_hybrid_xpi != NULL))

__attribute__((section(".flash_algo.data"))) xpi_nor_config_t nor_config;
__attribute__((section(".flash_algo.data"))) bool xpi_inited = false;
__attribute__((section(".flash_algo.data"))) uint32_t channel = xpi_channel_a1;
__attribute__((section(".flash_algo.data"))) XPI_Type *xpi_base;
__attribute__((section(".flash_algo.data"))) uint32_t saved_mcache_ctl;
__attribute__((section(".flash_algo.data"))) bool cache_context_saved = false;
__attribute__((section(".flash_algo.data"))) uint32_t saved_mstatus;
__attribute__((section(".flash_algo.data"))) uint32_t saved_fcsr;
__attribute__((section(".flash_algo.data"))) bool fpu_workaround_context_saved = false;

__attribute__((section(".flash_algo.text"))) void refresh_device_size(
    XPI_Type *base, xpi_nor_config_option_t *option)
{
    volatile uint32_t *dev_size = (volatile uint32_t *)((uint32_t)base + 0x60);
    bool enable_channelb = false;

    if (option->header.words > 1) {
        enable_channelb = option->option1.connection_sel == xpi_nor_connection_sel_chnb_cs0;
    }
    if (enable_channelb) {
        dev_size[0] = 0;
        dev_size[1] = 0;
    }
}

static void restore_debug_context(void)
{
    if (cache_context_saved) {
        /* No application code runs while D-cache is disabled. Discard stale
         * XIP lines before restoring the original cache-control word. */
        l1c_dc_invalidate_all();
        l1c_fence_i();
        write_csr(CSR_MCACHE_CTL, saved_mcache_ctl);
        cache_context_saved = false;
    }
    if (fpu_workaround_context_saved) {
        uint32_t current_mstatus = enable_fpu();

        write_fcsr(saved_fcsr);
        current_mstatus = (current_mstatus & ~CSR_MSTATUS_FS_MASK) |
                          (saved_mstatus & CSR_MSTATUS_FS_MASK);
        write_csr(CSR_MSTATUS, current_mstatus);
        fpu_workaround_context_saved = false;
    }
}

__attribute__((section(".flash_algo.text"))) uint32_t flash_init(
    uint32_t flash_base, uint32_t header, uint32_t opt0, uint32_t opt1,
    uint32_t xpi_base_addr)
{
    uint32_t i;
    xpi_nor_config_option_t cfg_option;
    hpm_stat_t stat = status_success;

    xpi_base = (XPI_Type *)xpi_base_addr;
    if (xpi_inited) {
        return status_success;
    }

    if ((ROM_API_TABLE_ROOT->version == HPM6200_FPU_WORKAROUND_ROM_VERSION) &&
        !fpu_workaround_context_saved) {
        saved_mstatus = enable_fpu();
        saved_fcsr = read_fcsr();
        write_fcsr(0);
        fpu_workaround_context_saved = true;
    }

    if (!cache_context_saved) {
        saved_mcache_ctl = l1c_get_control();
        if (l1c_dc_is_enabled()) {
            l1c_dc_writeback_all();
        }
        l1c_dc_disable();
        cache_context_saved = true;
    }

    for (i = 0; i < sizeof(cfg_option); i++) {
        *((uint8_t *)&cfg_option + i) = 0;
    }
    for (i = 0; i < sizeof(nor_config); i++) {
        *((uint8_t *)&nor_config + i) = 0;
    }

    cfg_option.header.U = header;
    cfg_option.option0.U = opt0;
    cfg_option.option1.U = opt1;

    channel = (opt1 & XPI_USE_PORT_B_MASK) ? xpi_channel_b1 : xpi_channel_a1;

    stat = ROM_API_TABLE_ROOT->xpi_nor_driver_if->auto_config(
        xpi_base, &nor_config, &cfg_option);
    if (stat != status_success) {
        restore_debug_context();
        return stat;
    }

    if (ROMAPI_SUPPORTS_HYBRIDXPI()) {
        ROM_API_TABLE_ROOT->xpi_nor_driver_if->enable_hybrid_xpi(xpi_base);
    }

    refresh_device_size(xpi_base, &cfg_option);
    nor_config.device_info.clk_freq_for_non_read_cmd = 0;
    xpi_inited = true;

    return stat;
}

__attribute__((section(".flash_algo.text"))) uint32_t flash_erase(
    uint32_t flash_base, uint32_t address, uint32_t size)
{
    hpm_stat_t stat = status_success;
    uint32_t left = size;
    uint32_t start = address;
    uint32_t block_size;
    uint32_t align;

    if (ROMAPI_SUPPORTS_HYBRIDXPI()) {
        start += flash_base;
    }

    block_size = nor_config.device_info.block_size_kbytes * 1024U;
    if (block_size == 0U) {
        return status_invalid_argument;
    }

    if (left >= block_size) {
        align = block_size - (start % block_size);
        if (align != block_size) {
            stat = ROM_API_TABLE_ROOT->xpi_nor_driver_if->erase(
                xpi_base, channel, &nor_config, start, align);
            if (stat != status_success) {
                return stat;
            }
            left -= align;
            start += align;
        }
        while (left >= block_size) {
            stat = ROM_API_TABLE_ROOT->xpi_nor_driver_if->erase_block(
                xpi_base, channel, &nor_config, start);
            if (stat != status_success) {
                return stat;
            }
            left -= block_size;
            start += block_size;
        }
    }

    if (left != 0U) {
        stat = ROM_API_TABLE_ROOT->xpi_nor_driver_if->erase(
            xpi_base, channel, &nor_config, start, left);
    }

    return stat;
}

__attribute__((section(".flash_algo.text"))) uint32_t flash_program(
    uint32_t flash_base, uint32_t address, uint32_t *buf, uint32_t size)
{
    if (ROMAPI_SUPPORTS_HYBRIDXPI()) {
        address += flash_base;
    }
    return ROM_API_TABLE_ROOT->xpi_nor_driver_if->program(
        xpi_base, channel, &nor_config, buf, address, size);
}

__attribute__((section(".flash_algo.text"))) uint32_t flash_erase_chip(
    uint32_t flash_base)
{
    (void)flash_base;
    return rom_xpi_nor_erase_chip(xpi_base, channel, &nor_config);
}

__attribute__((section(".flash_algo.text"))) uint32_t flash_deinit(void)
{
    restore_debug_context();
    xpi_inited = false;
    return status_success;
}
