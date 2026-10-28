/* The formatter behind printf, snprintf and friends: no I/O, no allocation, no
   floating point. Checked against the host's C library in libc/tests. */
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct { char *buf; size_t cap, len; } Out;

static void put(Out *o, char c) {
    if (o->len + 1 < o->cap) o->buf[o->len] = c;
    o->len++;
}
static void pad(Out *o, char c, int n) { while (n-- > 0) put(o, c); }

static int convert(char *tmp, unsigned long long v, unsigned base, int upper) {
    const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    int n = 0;
    do { tmp[n++] = digits[v % base]; v /= base; } while (v);
    return n;   /* digits are in reverse */
}

int k1kc_vsnprintf(char *buf, size_t cap, const char *fmt, va_list ap) {
    Out o = { buf, cap, 0 };
    for (; *fmt; fmt++) {
        if (*fmt != '%') { put(&o, *fmt); continue; }
        fmt++;
        int left = 0, zero = 0, plus = 0, space = 0, alt = 0;
        for (;; fmt++) {
            if (*fmt == '-') left = 1; else if (*fmt == '0') zero = 1;
            else if (*fmt == '+') plus = 1; else if (*fmt == ' ') space = 1;
            else if (*fmt == '#') alt = 1; else break;
        }
        int width = 0, prec = -1;
        if (*fmt == '*') { width = va_arg(ap, int); if (width < 0) { left = 1; width = -width; } fmt++; }
        else while (*fmt >= '0' && *fmt <= '9') width = width * 10 + (*fmt++ - '0');
        if (*fmt == '.') {
            fmt++; prec = 0;
            if (*fmt == '*') { prec = va_arg(ap, int); fmt++; }
            else while (*fmt >= '0' && *fmt <= '9') prec = prec * 10 + (*fmt++ - '0');
        }
        int lng = 0;   /* 0 int, 1 long, 2 long long, -1 short, -2 char, 3 size_t/ptrdiff */
        for (;; fmt++) {
            if (*fmt == 'l') lng = lng == 1 ? 2 : 1;
            else if (*fmt == 'h') lng = lng == -1 ? -2 : -1;
            else if (*fmt == 'z' || *fmt == 't' || *fmt == 'j') lng = 3;
            else break;
        }
        char tmp[24];
        switch (*fmt) {
        case '%': put(&o, '%'); break;
        case 'c': {
            char c = (char)va_arg(ap, int);
            if (!left) pad(&o, ' ', width - 1);
            put(&o, c);
            if (left) pad(&o, ' ', width - 1);
            break;
        }
        case 's': {
            const char *s = va_arg(ap, const char *);
            if (!s) s = "(null)";
            size_t n = prec >= 0 ? strnlen(s, (size_t)prec) : strlen(s);
            if (!left) pad(&o, ' ', width - (int)n);
            for (size_t i = 0; i < n; i++) put(&o, s[i]);
            if (left) pad(&o, ' ', width - (int)n);
            break;
        }
        case 'd': case 'i': case 'u': case 'x': case 'X': case 'o': case 'p': {
            int sign = (*fmt == 'd' || *fmt == 'i');
            unsigned base = (*fmt == 'x' || *fmt == 'X' || *fmt == 'p') ? 16 : (*fmt == 'o' ? 8 : 10);
            unsigned long long v; int neg = 0;
            if (*fmt == 'p') { v = (uintptr_t)va_arg(ap, void *); alt = 1; lng = 1; }
            else if (sign) {
                long long sv = lng >= 2 ? va_arg(ap, long long) : lng == 1 ? va_arg(ap, long)
                             : lng == 3 ? (long long)va_arg(ap, ptrdiff_t) : va_arg(ap, int);
                if (lng == -1) sv = (short)sv; else if (lng == -2) sv = (signed char)sv;
                neg = sv < 0; v = neg ? 0ULL - (unsigned long long)sv : (unsigned long long)sv;
            } else {
                v = lng >= 2 ? va_arg(ap, unsigned long long) : lng == 1 ? va_arg(ap, unsigned long)
                  : lng == 3 ? (unsigned long long)va_arg(ap, size_t) : va_arg(ap, unsigned);
                if (lng == -1) v = (unsigned short)v; else if (lng == -2) v = (unsigned char)v;
            }
            int nd = (v == 0 && prec == 0) ? 0 : convert(tmp, v, base, *fmt == 'X');
            if (v == 0 && prec == 0) nd = 0;
            int zeros = prec > nd ? prec - nd : 0;
            const char *prefix = ""; int plen = 0;
            if (neg) { prefix = "-"; plen = 1; }
            else if (sign && plus) { prefix = "+"; plen = 1; }
            else if (sign && space) { prefix = " "; plen = 1; }
            else if (alt && base == 16 && v != 0) { prefix = *fmt == 'X' ? "0X" : "0x"; plen = 2; }
            else if (alt && base == 8 && zeros == 0 && (nd == 0 || tmp[nd - 1] != '0')) zeros = 1;
            int total = plen + zeros + nd;
            if (zero && !left && prec < 0 && width > total) zeros += width - total, total = width;
            if (!left) pad(&o, ' ', width - total);
            for (int i = 0; i < plen; i++) put(&o, prefix[i]);
            pad(&o, '0', zeros);
            while (nd) put(&o, tmp[--nd]);
            if (left) pad(&o, ' ', width - total);
            break;
        }
        default:   /* unknown conversion: print it as written */
            put(&o, '%'); if (*fmt) put(&o, *fmt); else fmt--;
        }
    }
    if (cap) o.buf[o.len < cap ? o.len : cap - 1] = '\0';
    return (int)o.len;
}

int k1kc_snprintf(char *buf, size_t n, const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    int r = k1kc_vsnprintf(buf, n, fmt, ap);
    va_end(ap);
    return r;
}
