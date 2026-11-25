OUTPUT_ARCH(riscv)
ENTRY(_start)

/* FW_DYNAMIC's next address up to (but not including) the initrd slot. */
MEMORY { PAYLOAD (rwx) : ORIGIN = 0x80200000, LENGTH = 14M }
SECTIONS {
    .text : { KEEP(*(.text.entry)) *(.text .text.*) } > PAYLOAD
    .rodata : ALIGN(4096) { *(.rodata .rodata.*) *(.srodata .srodata.*) } > PAYLOAD
    .data : ALIGN(4096) {
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.data .data.* .sdata .sdata.* .got .got.*)
        /* Offset the 64-byte wire header so each 128-byte owner record starts aligned. */
        . = ALIGN(128) + 64;
        KEEP(*(.pauli_hs_stats))
    } > PAYLOAD
    .bss (NOLOAD) : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.* .sbss .sbss.* COMMON)
        . = ALIGN(8);
        __bss_end = .;
    } > PAYLOAD
    .boot_stack (NOLOAD) : ALIGN(16) { . += 256K; __boot_stack = .; } > PAYLOAD
    __hv_end = ALIGN(4096);
    /* A separate private arena per possible slot. Of this linker section,
       only the final 4 KiB page is mapped into VS (in probe mode). */
    .guest_storage (NOLOAD) : ALIGN(16K) {
        KEEP(*(.wave8.arena))
        . = ALIGN(4K);
        KEEP(*(.wave8.page))
    } > PAYLOAD
    ASSERT(_start == ORIGIN(PAYLOAD), "Wave8 HS entry must match FW_DYNAMIC")
    ASSERT(wave8_guest_end - wave8_guest_start <= 4K, "embedded guest does not fit its RX page")
    ASSERT(wave8_service_start % 4K == 0 && wave8_service_end - wave8_service_start <= 4K, "invalid service RX page")
    ASSERT(. <= ORIGIN(PAYLOAD) + LENGTH(PAYLOAD), "Wave8 HS payload overlaps initrd")
    /DISCARD/ : { *(.eh_frame .eh_frame.* .gcc_except_table*) }
    ASSERT((PAULI_HS_STATS + 64) % 128 == 0, "HS stats owner records must be 128-byte aligned")
}
