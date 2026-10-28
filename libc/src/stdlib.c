#include <stdlib.h>
#include <string.h>
#include <ctype.h>
#include <errno.h>
#include <limits.h>
#include <k1k.h>

int k1kc_snprintf(char *buf, size_t n, const char *fmt, ...);

int errno;

/* Every block carries its size in front: the heap underneath wants it back on free. */
#define HDR 16
void *malloc(size_t n) {
    if (n > (size_t)-1 - HDR) { errno = ENOMEM; return NULL; }
    unsigned char *p = k1kc_alloc(n + HDR);
    if (!p) { errno = ENOMEM; return NULL; }
    *(size_t *)p = n + HDR;
    return p + HDR;
}
void free(void *q) {
    if (!q) return;
    unsigned char *p = (unsigned char *)q - HDR;
    k1kc_free(p, *(size_t *)p);
}
void *calloc(size_t n, size_t m) {
    if (m && n > (size_t)-1 / m) { errno = ENOMEM; return NULL; }
    void *p = malloc(n * m);
    return p ? memset(p, 0, n * m) : NULL;
}
void *realloc(void *q, size_t n) {
    if (!q) return malloc(n);
    if (!n) { free(q); return NULL; }
    size_t old = *(size_t *)((unsigned char *)q - HDR) - HDR;
    if (n <= old) return q;
    void *p = malloc(n);
    if (!p) return NULL;
    memcpy(p, q, old);
    free(q);
    return p;
}

_Noreturn void exit(int code) { k1kc_exit(code); }
_Noreturn void abort(void) { k1kc_exit(134); }
char *getenv(const char *name) { (void)name; return NULL; }

int abs(int x) { return x < 0 ? -x : x; }
long labs(long x) { return x < 0 ? -x : x; }

unsigned long strtoul(const char *s, char **end, int base) {
    const char *p = s;
    while (isspace((unsigned char)*p)) p++;
    int neg = 0;
    if (*p == '+' || *p == '-') neg = *p++ == '-';
    if ((base == 0 || base == 16) && p[0] == '0' && (p[1] == 'x' || p[1] == 'X') && isxdigit((unsigned char)p[2])) {
        p += 2; base = 16;
    } else if (base == 0) {
        base = p[0] == '0' ? 8 : 10;
    }
    unsigned long v = 0; int any = 0, over = 0;
    for (;; p++) {
        int c = (unsigned char)*p, d;
        if (isdigit(c)) d = c - '0';
        else if (isalpha(c)) d = tolower(c) - 'a' + 10;
        else break;
        if (d >= base) break;
        any = 1;
        if (v > (ULONG_MAX - d) / base) over = 1; else v = v * base + d;
    }
    if (end) *end = (char *)(any ? p : s);
    if (over) { errno = ERANGE; return ULONG_MAX; }
    return neg ? -v : v;
}
long strtol(const char *s, char **end, int base) {
    const char *p = s;
    while (isspace((unsigned char)*p)) p++;
    int neg = *p == '-';
    unsigned long v = strtoul(s, end, base);
    if (neg) { if (v > (unsigned long)LONG_MAX + 1) { errno = ERANGE; return LONG_MIN; } return -(long)v; }
    if (v > (unsigned long)LONG_MAX) { errno = ERANGE; return LONG_MAX; }
    return (long)v;
}
int atoi(const char *s) { return (int)strtol(s, NULL, 10); }
long atol(const char *s) { return strtol(s, NULL, 10); }

/* Shell sort: no recursion, no allocation, good enough for what a service sorts. */
void qsort(void *base, size_t n, size_t size, int (*cmp)(const void *, const void *)) {
    unsigned char *a = base;
    unsigned char tmp[64];
    if (size > sizeof tmp || n < 2) {
        if (size > sizeof tmp) abort();
        return;
    }
    for (size_t gap = n / 2; gap > 0; gap /= 2)
        for (size_t i = gap; i < n; i++) {
            memcpy(tmp, a + i * size, size);
            size_t j = i;
            for (; j >= gap && cmp(a + (j - gap) * size, tmp) > 0; j -= gap)
                memcpy(a + j * size, a + (j - gap) * size, size);
            memcpy(a + j * size, tmp, size);
        }
}
void *bsearch(const void *key, const void *base, size_t n, size_t size,
              int (*cmp)(const void *, const void *)) {
    const unsigned char *a = base;
    size_t lo = 0, hi = n;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        int c = cmp(key, a + mid * size);
        if (c == 0) return (void *)(a + mid * size);
        if (c < 0) hi = mid; else lo = mid + 1;
    }
    return NULL;
}

static unsigned long rng = 1;
void srand(unsigned seed) { rng = seed; }
int rand(void) { rng = rng * 6364136223846793005UL + 1442695040888963407UL; return (int)((rng >> 33) & RAND_MAX); }

_Noreturn void __assert_fail(const char *expr, const char *file, int line) {
    char buf[200];
    int n = k1kc_snprintf(buf, sizeof buf, "assertion failed: %s (%s:%d)\n", expr, file, line);
    k1kc_write(2, buf, (size_t)n);
    abort();
}
