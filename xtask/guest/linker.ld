OUTPUT_ARCH(riscv)
ENTRY(_start)
PHDRS { code PT_LOAD FLAGS(5); data PT_LOAD FLAGS(6); }
SECTIONS {
    . = 0x10000;
    .text : { KEEP(*(.text.entry)) *(.text .text.*) } :code
    .rodata : { *(.rodata .rodata.* .srodata .srodata.*) } :code
    . = ALIGN(4096);
    .data : {
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.data .data.* .sdata .sdata.* .got .got.*)
    } :data
    .bss (NOLOAD) : { *(.bss .bss.* .sbss .sbss.* COMMON) } :data
    /DISCARD/ : { *(.eh_frame .eh_frame.* .gcc_except_table*) }
}
