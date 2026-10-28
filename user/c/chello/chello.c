/* chello - an ordinary C program running as a K1K service.
 *
 * Nothing here is K1K-specific: it is stdio, stdlib and string.h, linked against
 * the small C library in libc/. Its manifest line (`fs,console`) is all the
 * authority it has: files through the file service and text on the console. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>

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
    printf("chello: done\n");
    return 0;
}
