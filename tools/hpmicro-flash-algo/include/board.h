/*
 * Minimal HPM SDK board header for the standalone probe-rs flash loader.
 *
 * The loader only needs the SoC CSR/L1C/ROM-API definitions. Pulling in the
 * full EVK board.h would drag in console/pinmux drivers that the algorithm
 * does not use.
 */

#ifndef HPM_FLASHLOADER_BOARD_H
#define HPM_FLASHLOADER_BOARD_H

#include "hpm_common.h"
#include "hpm_soc.h"
#include "hpm_soc_feature.h"
#include "hpm_csr_drv.h"
#include "riscv/riscv_core.h"
#include "hpm_l1c_drv.h"
#include "hpm_romapi.h"

#endif
