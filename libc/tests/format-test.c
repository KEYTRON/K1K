/* Differential test: the K1K formatter against the host's C library, on the
   conversions K1K supports (everything but floating point). Host build only. */
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int k1kc_vsnprintf(char *buf, size_t cap, const char *fmt, va_list ap);
static int mine(char *b, size_t n, const char *f, ...) {
    va_list ap; va_start(ap, f); int r = k1kc_vsnprintf(b, n, f, ap); va_end(ap); return r;
}
static int bad;
#define CHECK(...) do { \
    char a[256], b[256]; \
    int ra = snprintf(a, sizeof a, __VA_ARGS__), rb = mine(b, sizeof b, __VA_ARGS__); \
    if (ra != rb || strcmp(a, b)) { bad++; printf("MISMATCH %s: host \"%s\"(%d) k1k \"%s\"(%d)\n", #__VA_ARGS__, a, ra, b, rb); } \
} while (0)

int main(void) {
    CHECK("plain text");
    CHECK("%d|%i|%u", -42, 42, 42u);
    CHECK("%5d|%-5d|%05d|%+d|% d", 42, 42, 42, 42, 42);
    CHECK("%5d|%-5d|%05d|%+d", -42, -42, -42, -42);
    CHECK("%.3d|%8.3d|%-8.3d|%.0d|%.0d", 7, 7, 7, 0, 5);
    CHECK("%x|%X|%#x|%#X|%08x|%#010x", 255, 255, 255, 255, 255, 255);
    CHECK("%#x|%#o|%o|%#o", 0, 0, 8, 8);
    CHECK("%ld|%lu|%lld|%llu|%lx", -1234567890123L, 1234567890123UL, -1LL << 62, ~0ULL, 0xdeadbeefcafeUL);
    CHECK("%zu|%zd|%td", (size_t)99, (ssize_t)-99, (ptrdiff_t)-5);
    CHECK("%hd|%hhd|%hu|%hhu", 70000, 300, 70000, 300);
    CHECK("%s|%10s|%-10s|%.3s|%10.3s|%-10.3s|", "hello", "hello", "hello", "hello", "hello", "hello");
    CHECK("%c|%5c|%-5c|", 'x', 'y', 'z');
    CHECK("%%|%5%");
    CHECK("%*d|%-*d|%.*s", 6, 42, 6, 42, 2, "abcdef");
    CHECK("%p", (void *)0x1234);
    CHECK("%d %s %x", INT32_MIN, "", 0);
    CHECK("%u", UINT32_MAX);
    CHECK("%lld", INT64_MIN);
    for (int i = -300; i <= 300; i += 7) CHECK("[%4d|%-4d|%04d|%x|%o]", i, i, i, (unsigned)i, (unsigned)i);
    /* truncation: the return value is the length that was wanted, the buffer stays terminated */
    char t1[8], t2[8];
    int r1 = snprintf(t1, sizeof t1, "%s", "0123456789"), r2 = mine(t2, sizeof t2, "%s", "0123456789");
    if (r1 != r2 || strcmp(t1, t2)) { bad++; printf("MISMATCH truncation\n"); }
    if (mine(t2, 0, "%d", 5) != 1) { bad++; printf("MISMATCH zero-size\n"); }
    if (bad) { printf("%d mismatch(es)\n", bad); return 1; }
    puts("format-test: all matched");
    return 0;
}
