/* RP2350 memory layout: single application image, no OTA partitions.
 *
 * The boot-ROM/picotool boot sections below (.start_block / .bi_entries /
 * .end_block) are required on RP2350 — embassy-rp's `binary-info` feature fills
 * `.start_block` with the image definition the ROM checks before booting.
 *
 * This is the standalone single-image layout (default build). The OTA build
 * (`--features ota`) uses `memory-ota.x` instead, placing the firmware in the
 * ACTIVE partition under the `bootloader` crate. */
MEMORY {
    /* 2 MiB is a safe default (a Pico 2 has 4 MiB; the RedBoard has 16 MiB —
     * either way this image fits comfortably). */
    FLASH : ORIGIN = 0x10000000, LENGTH = 2048K
    /* Main SRAM: 512 KiB across the striped banks SRAM0-7. */
    RAM   : ORIGIN = 0x20000000, LENGTH = 512K
    /* Direct-mapped banks 8 & 9 (unused here). */
    SRAM8 : ORIGIN = 0x20080000, LENGTH = 4K
    SRAM9 : ORIGIN = 0x20081000, LENGTH = 4K
}

SECTIONS {
    /* Boot ROM info — kept in the first 4K of flash where the ROM/picotool
     * look for it. */
    .start_block : ALIGN(4)
    {
        __start_block_addr = .;
        KEEP(*(.start_block));
        KEEP(*(.boot_info));
    } > FLASH
} INSERT AFTER .vector_table;

/* Move .text to start after the boot info. */
_stext = ADDR(.start_block) + SIZEOF(.start_block);

SECTIONS {
    /* Picotool 'Binary Info' entries. */
    .bi_entries : ALIGN(4)
    {
        __bi_entries_start = .;
        KEEP(*(.bi_entries));
        . = ALIGN(4);
        __bi_entries_end = .;
    } > FLASH
} INSERT AFTER .text;

SECTIONS {
    /* Boot ROM extra info — after everything, can hold a signature. */
    .end_block : ALIGN(4)
    {
        __end_block_addr = .;
        KEEP(*(.end_block));
    } > FLASH
} INSERT AFTER .uninit;

PROVIDE(start_to_end = __end_block_addr - __start_block_addr);
PROVIDE(end_to_start = __start_block_addr - __end_block_addr);
