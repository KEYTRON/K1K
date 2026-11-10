/* chello - an ordinary C program running as a K1K service.
 *
 * Nothing here is K1K-specific: it is stdio, stdlib and string.h, linked against
 * the small C library in libc/. Its manifest line (`fs,console`) is all the
 * authority it has: files through the file service and text on the console. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>
#include <errno.h>
#include <sys/stat.h>
#include <unistd.h>
#include <k1k.h>

static int by_value(const void *a, const void *b) {
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}

int main(void) {
    printf("chello: a C program in ring 3 (%s)\n", "libc/k1kc");

    int v[] = { 42, 7, 19, -3, 100, 0, 7 };
    qsort(v, sizeof v / sizeof v[0], sizeof v[0], by_value);
    printf("chello: sorted:");
    for (size_t i = 0; i < sizeof v / sizeof v[0]; i++) printf(" %d", v[i]);
    printf("\n");

    char *text = malloc(8);
    strcpy(text, "grow");
    text = realloc(text, 64);
    strcat(text, "ing heap");
    printf("chello: %s, %zu bytes, strtol(\"0x1f\") = %ld\n", text, strlen(text), strtol("0x1f", NULL, 0));
    free(text);

    char line[96];
    snprintf(line, sizeof line, "%-8s|%6.3s|%#06x|%+d", "left", "truncated", 255, 7);
    printf("chello: [%s]\n", line);

    FILE *f = fopen("/README.TXT", "r");
    if (!f) {
        printf("chello: cannot open /README.TXT\n");
        return 1;
    }
    char first[80];
    if (fgets(first, sizeof first, f)) {
        { char *nl = strchr(first, '\n'); if (nl) *nl = '\0'; }
        printf("chello: /README.TXT says \"%s\"\n", first);
    }
    fclose(f);

    /* The writable FAT16 API from C: mkdir/unlink/rename wear their POSIX
       names, creating a file with its content is the K1K-specific
       k1kc_create. Names are pre-cleaned and a directory from a previous
       boot of the same image is fine, like the fatwrite service does. */
    const char *dir = "/CHELLO", *a = "/CHELLO/A.TXT", *b = "/CHELLO/B.TXT";
    unlink(a);
    unlink(b);
    if (mkdir(dir, 0) != 0 && errno != EEXIST) {
        printf("chello: mkdir %s: errno %d\n", dir, errno);
        return 1;
    }
    if (k1kc_create(a, strlen(a), "hello, FAT16", 12, 0) != 0) {
        printf("chello: create %s failed\n", a);
        return 1;
    }
    if (rename(a, b) != 0) {
        printf("chello: rename: errno %d\n", errno);
        return 1;
    }
    f = fopen(b, "r");
    if (!f) {
        printf("chello: cannot reopen %s\n", b);
        return 1;
    }
    char back[16] = { 0 };
    size_t got = fread(back, 1, 12, f);
    fclose(f);
    if (got != 12 || memcmp(back, "hello, FAT16", 12) != 0) {
        printf("chello: read back \"%s\"\n", back);
        return 1;
    }
    if (unlink(b) != 0) {
        printf("chello: unlink: errno %d\n", errno);
        return 1;
    }
    printf("chello: writes: ok\n");
    printf("chello: done\n");
    return 0;
}
