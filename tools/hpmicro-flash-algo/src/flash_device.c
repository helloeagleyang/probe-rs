/*
 * CMSIS FlashDevice descriptor consumed by target-gen.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * Layout (160 bytes, followed by {size, address} sector records):
 *   u16 version; char name[128]; u16 typ; u32 start; u32 size; u32 page;
 *   u32 reserved; u8 erased; u8 pad[3]; u32 program_timeout;
 *   u32 erase_timeout
 */

#include <stdint.h>

#ifndef FLASH_DEVICE_START
#error "FLASH_DEVICE_START must be defined by the build"
#endif
#ifndef FLASH_DEVICE_SIZE
#error "FLASH_DEVICE_SIZE must be defined by the build"
#endif

typedef struct __attribute__((packed)) {
    uint16_t version;
    char name[128];
    uint16_t typ;
    uint32_t start_address;
    uint32_t device_size;
    uint32_t page_size;
    uint32_t reserved;
    uint8_t erased_default_value;
    uint8_t pad[3];
    uint32_t program_page_timeout;
    uint32_t erase_sector_timeout;
} FlashDevice_t;

typedef struct {
    FlashDevice_t device;
    uint32_t sectors[4];
} FlashDeviceBlob_t;

_Static_assert(sizeof(FlashDevice_t) == 160, "FlashDevice layout must be 160 bytes");

__attribute__((section(".flash_device"), used))
const FlashDeviceBlob_t FlashDevice = {
    .device = {
        .version = 1,
        .name = "HPM XPI NOR",
        .typ = 0,
        .start_address = FLASH_DEVICE_START,
        .device_size = FLASH_DEVICE_SIZE,
        .page_size = 0x100,
        .reserved = 0,
        .erased_default_value = 0xFF,
        .pad = {0, 0, 0},
        .program_page_timeout = 1000,
        .erase_sector_timeout = 2000,
    },
    .sectors = {0x1000, 0x0, 0xFFFFFFFF, 0xFFFFFFFF},
};
