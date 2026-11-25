OUTPUT_ARCH(riscv)
ENTRY(_start)

SECTIONS {
    . = 0x80200000;
    __hv_begin = .;
    .text : { KEEP(*(.text.entry)) *(.text .text.*) }
    .rodata : ALIGN(4096) { *(.rodata .rodata.*) *(.srodata .srodata.*) }
    .data : ALIGN(4096) {
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.data .data.* .sdata .sdata.* .got .got.*)
        /* Offset the 64-byte wire header so each 128-byte owner record starts aligned. */
        . = ALIGN(128) + 64;
        KEEP(*(.pauli_hs_stats))
    }
    .bss (NOLOAD) : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.* .sbss .sbss.* COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }
    .boot_stack (NOLOAD) : ALIGN(16) { . += 256K; __boot_stack = .; }
    __hv_end = ALIGN(4096);
    /DISCARD/ : { *(.eh_frame .eh_frame.* .gcc_except_table*) }
    ASSERT((PAULI_HS_STATS + 64) % 128 == 0, "HS stats owner records must be 128-byte aligned")
}
