/* OTA-partitioned layout (selected by `--features ota`). The firmware runs from
 * the ACTIVE partition under the bootloader; the bootloader (booted by the ROM)
 * jumps here. Keep these addresses in sync with `bootloader/memory.x`.
 *
 * The DFU + STATE symbols are exported for `FirmwareUpdaterConfig` (the firmware
 * streams a new image into DFU; the bootloader swaps it on the next reset). */
MEMORY {
    /* FLASH == the ACTIVE partition. */
    FLASH            : ORIGIN = 0x10021000, LENGTH = 892K
    BOOTLOADER_STATE : ORIGIN = 0x10020000, LENGTH = 4K
    DFU              : ORIGIN = 0x10100000, LENGTH = 896K

    RAM   : ORIGIN = 0x20000000, LENGTH = 512K
    SRAM8 : ORIGIN = 0x20080000, LENGTH = 4K
    SRAM9 : ORIGIN = 0x20081000, LENGTH = 4K
}

/* Flash-relative OFFSETS (not absolute addresses) — embassy-boot feeds these
 * straight to embassy-rp's Flash as offset-from-start. See bootloader/memory.x. */
__bootloader_state_start = ORIGIN(BOOTLOADER_STATE) - 0x10000000;
__bootloader_state_end   = ORIGIN(BOOTLOADER_STATE) + LENGTH(BOOTLOADER_STATE) - 0x10000000;
__bootloader_dfu_start   = ORIGIN(DFU) - 0x10000000;
__bootloader_dfu_end     = ORIGIN(DFU) + LENGTH(DFU) - 0x10000000;

/* RP2350 boot blocks (binary-info fills the image-def). Unused by the ROM here
 * since the bootloader jumps to ACTIVE directly, but the firmware links with
 * `binary-info` so the section still needs a home. */
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
