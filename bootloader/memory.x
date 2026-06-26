/* Bootloader linker layout. The bootloader image lives in the BOOTLOADER region
 * at the start of flash, where the RP2350 boot ROM looks (the image-def in
 * `.start_block` is filled by embassy-rp's `binary-info` feature). The other
 * regions are referenced only to export the partition symbols that
 * `BootLoaderConfig::from_linkerfile_blocking` reads. Keep these addresses in sync
 * with `firmware/memory-ota.x`. */
MEMORY {
    /* FLASH == the BOOTLOADER partition (cortex-m-rt places code here). */
    FLASH            : ORIGIN = 0x10000000, LENGTH = 128K
    BOOTLOADER_STATE : ORIGIN = 0x10020000, LENGTH = 4K
    ACTIVE           : ORIGIN = 0x10021000, LENGTH = 892K
    DFU              : ORIGIN = 0x10100000, LENGTH = 896K

    RAM   : ORIGIN = 0x20000000, LENGTH = 512K
    SRAM8 : ORIGIN = 0x20080000, LENGTH = 4K
    SRAM9 : ORIGIN = 0x20081000, LENGTH = 4K
}

/* These must be flash-relative OFFSETS, not absolute addresses: embassy-boot's
 * from_linkerfile_blocking feeds the symbol value straight to embassy-rp's Flash
 * as an offset-from-flash-start, so subtract the 0x10000000 flash base. */
__bootloader_state_start  = ORIGIN(BOOTLOADER_STATE) - 0x10000000;
__bootloader_state_end    = ORIGIN(BOOTLOADER_STATE) + LENGTH(BOOTLOADER_STATE) - 0x10000000;
__bootloader_active_start = ORIGIN(ACTIVE) - 0x10000000;
__bootloader_active_end   = ORIGIN(ACTIVE) + LENGTH(ACTIVE) - 0x10000000;
__bootloader_dfu_start    = ORIGIN(DFU) - 0x10000000;
__bootloader_dfu_end      = ORIGIN(DFU) + LENGTH(DFU) - 0x10000000;

/* RP2350 boot ROM image definition (same shape as the firmware's memory.x). */
SECTIONS {
    .start_block : ALIGN(4)
    {
        __start_block_addr = .;
        KEEP(*(.start_block));
        KEEP(*(.boot_info));
    } > FLASH
} INSERT AFTER .vector_table;

_stext = ADDR(.start_block) + SIZEOF(.start_block);

SECTIONS {
    .bi_entries : ALIGN(4)
    {
        __bi_entries_start = .;
        KEEP(*(.bi_entries));
        . = ALIGN(4);
        __bi_entries_end = .;
    } > FLASH
} INSERT AFTER .text;

SECTIONS {
    .end_block : ALIGN(4)
    {
        __end_block_addr = .;
        KEEP(*(.end_block));
    } > FLASH
} INSERT AFTER .uninit;

PROVIDE(start_to_end = __end_block_addr - __start_block_addr);
PROVIDE(end_to_start = __start_block_addr - __end_block_addr);
