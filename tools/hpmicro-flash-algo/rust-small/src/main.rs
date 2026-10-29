#![no_std]
#![no_main]
#![allow(unused)]

//! Minimal HPM 1 MiB SiP XPI0 flash algorithm, following the hpm53x1 flavor.
//!
//! The only chip-specific inputs are the XPI controller base and the fixed
//! 1 MiB flash window. The HPM BootROM XPI NOR API does the configuration,
//! erase and program work. No SYSCTL or cache manipulation is performed here;
//! the SDK OpenOCD loader for HPM5300 also skips SYSCTL.

use flash_algorithm::*;
use panic_halt as _;

use self::romapi::*;

#[allow(non_upper_case_globals, non_snake_case, non_camel_case_types)]
pub mod romapi;

pub const FLASH_BASE: u32 = 0x8000_0000;
pub const FLASH_SIZE: u32 = 0x0010_0000;
pub const PAGE_SIZE: u32 = 256;

#[cfg(feature = "xpi-f300")]
pub const XPI_BASE: u32 = 0xF300_0000;
#[cfg(feature = "xpi-f00e")]
pub const XPI_BASE: u32 = 0xF00E_0000;
#[cfg(not(any(feature = "xpi-f300", feature = "xpi-f00e")))]
compile_error!("select exactly one of the `xpi-f300` or `xpi-f00e` features");

pub const ROM_API_TABLE_ROOT: *const bootloader_api_table_t =
    0x2001_FF00 as *const bootloader_api_table_t;

const OPT_HEADER: u32 = 0xFCF9_0002;
const OPT0: u32 = 0x05; // frequency option, 5 = 100 MHz
const OPT1: u32 = 0x1000; // pin group 1, channel A

struct Algorithm {
    nor_config: xpi_nor_config_t,
    xpi_base: *mut XPI_Type,
}

algorithm!(Algorithm, {
    flash_address: FLASH_BASE,
    flash_size: FLASH_SIZE,
    page_size: PAGE_SIZE,
    empty_value: 0xFF,
    sectors: [{
        size: 4096,
        address: 0x0,
    }]
});

impl FlashAlgorithm for Algorithm {
    fn new(_address: u32, _clock: u32, _function: Function) -> Result<Self, ErrorCode> {
        unsafe {
            let xpi_base = XPI_BASE as *mut XPI_Type;
            let mut nor_config: xpi_nor_config_t = core::mem::zeroed();
            let mut option: xpi_nor_config_option_t = core::mem::zeroed();

            option.header.U = OPT_HEADER;
            option.option0.U = OPT0;
            option.option1.U = OPT1;

            let xpi_nor_driver = &*(*ROM_API_TABLE_ROOT).xpi_nor_driver_if;
            let ret = xpi_nor_driver.auto_config.unwrap()(xpi_base, &mut nor_config, &mut option);
            if ret != 0 {
                return Err(ErrorCode::new(ret).unwrap());
            }

            nor_config.device_info.clk_freq_for_non_read_cmd = 0;

            Ok(Self {
                nor_config,
                xpi_base,
            })
        }
    }

    fn erase_all(&mut self) -> Result<(), ErrorCode> {
        unsafe {
            let xpi_nor_driver = &*(*ROM_API_TABLE_ROOT).xpi_nor_driver_if;
            let ret = xpi_nor_driver.erase_chip.unwrap()(
                self.xpi_base,
                xpi_xfer_channel_auto,
                &self.nor_config,
            );
            if ret != 0 {
                return Err(ErrorCode::new(ret).unwrap());
            }
        }
        Ok(())
    }

    fn erase_sector(&mut self, address: u32) -> Result<(), ErrorCode> {
        unsafe {
            let xpi_nor_driver = &*(*ROM_API_TABLE_ROOT).xpi_nor_driver_if;
            let address = address - FLASH_BASE;

            let ret = xpi_nor_driver.erase_sector.unwrap()(
                self.xpi_base,
                xpi_xfer_channel_auto,
                &mut self.nor_config,
                address,
            );
            if ret != 0 {
                return Err(ErrorCode::new(ret).unwrap());
            }
        }
        Ok(())
    }

    fn program_page(&mut self, address: u32, data: &[u8]) -> Result<(), ErrorCode> {
        unsafe {
            let xpi_nor_driver = &*(*ROM_API_TABLE_ROOT).xpi_nor_driver_if;
            let address = address - FLASH_BASE;

            let ret = xpi_nor_driver.program.unwrap()(
                self.xpi_base,
                xpi_xfer_channel_auto,
                &mut self.nor_config,
                data.as_ptr() as *const _,
                address,
                data.len() as u32,
            );
            if ret != 0 {
                return Err(ErrorCode::new(ret).unwrap());
            }
        }
        Ok(())
    }
}

impl Drop for Algorithm {
    fn drop(&mut self) {}
}
