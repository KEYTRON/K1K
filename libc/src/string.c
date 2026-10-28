#include <string.h>
#include <stdlib.h>

void *memcpy(void *restrict d, const void *restrict s, size_t n) {
    unsigned char *dp = d; const unsigned char *sp = s;
    while (n--) *dp++ = *sp++;
    return d;
}
void *memmove(void *d, const void *s, size_t n) {
    unsigned char *dp = d; const unsigned char *sp = s;
    if (dp == sp || n == 0) return d;
    if (dp < sp) { while (n--) *dp++ = *sp++; }
    else { dp += n; sp += n; while (n--) *--dp = *--sp; }
    return d;
}
void *memset(void *d, int c, size_t n) {
    unsigned char *dp = d;
    while (n--) *dp++ = (unsigned char)c;
    return d;
}
int memcmp(const void *a, const void *b, size_t n) {
    const unsigned char *x = a, *y = b;
    for (; n; n--, x++, y++) if (*x != *y) return *x - *y;
    return 0;
}
void *memchr(const void *s, int c, size_t n) {
    const unsigned char *p = s;
    for (; n; n--, p++) if (*p == (unsigned char)c) return (void *)p;
    return NULL;
}
size_t strlen(const char *s) { size_t n = 0; while (s[n]) n++; return n; }
size_t strnlen(const char *s, size_t m) { size_t n = 0; while (n < m && s[n]) n++; return n; }
int strcmp(const char *a, const char *b) {
    while (*a && *a == *b) { a++; b++; }
    return (unsigned char)*a - (unsigned char)*b;
}
int strncmp(const char *a, const char *b, size_t n) {
    for (; n; n--, a++, b++) {
        if (*a != *b) return (unsigned char)*a - (unsigned char)*b;
        if (!*a) return 0;
    }
    return 0;
}
char *strcpy(char *restrict d, const char *restrict s) {
    char *r = d; while ((*d++ = *s++)) {} return r;
}
char *strncpy(char *restrict d, const char *restrict s, size_t n) {
    size_t i = 0;
    for (; i < n && s[i]; i++) d[i] = s[i];
    for (; i < n; i++) d[i] = 0;
    return d;
}
char *strcat(char *restrict d, const char *restrict s) { strcpy(d + strlen(d), s); return d; }
char *strchr(const char *s, int c) {
    for (;; s++) { if (*s == (char)c) return (char *)s; if (!*s) return NULL; }
}
char *strrchr(const char *s, int c) {
    const char *last = NULL;
    for (;; s++) { if (*s == (char)c) last = s; if (!*s) return (char *)last; }
}
char *strstr(const char *h, const char *n) {
    size_t m = strlen(n);
    if (!m) return (char *)h;
    for (; *h; h++) if (!strncmp(h, n, m)) return (char *)h;
    return NULL;
}
char *strdup(const char *s) {
    size_t n = strlen(s) + 1; char *p = malloc(n);
    return p ? memcpy(p, s, n) : NULL;
}
