/* K1K user program: static ELF64, segments page-aligned so the kernel can
   apply distinct R/RX/RW permissions per PT_LOAD. */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    text    PT_LOAD FLAGS(5); /* R X */
    rodata  PT_LOAD FLAGS(4); /* R   */
    data    PT_LOAD FLAGS(6); /* RW  */
}

SECTIONS
{
    . = 0x400000;

    .text : {
        *(.text.entry)
        *(.text .text.*)
    } :text

    . = ALIGN(0x1000);

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(0x1000);

    .data : {
        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
