#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <k1k.h>

int k1kc_vsnprintf(char *buf, size_t cap, const char *fmt, va_list ap);

/* A stream is the console (output), the keyboard (input) or a file that was read
   whole into memory when it was opened: the file service takes one client at a
   time, so nothing keeps a connection to it between calls. */
struct K1KFile {
    int kind;                 /* 0 stdin, 1 stdout, 2 stderr, 3 file in memory */
    unsigned char *data;
    size_t len, pos;
    int eof;
};
static FILE in_f = { 0, NULL, 0, 0, 0 }, out_f = { 1, NULL, 0, 0, 0 }, err_f = { 2, NULL, 0, 0, 0 };
FILE *stdin = &in_f, *stdout = &out_f, *stderr = &err_f;

clock_t clock(void) { return (clock_t)k1kc_uptime_ms(); }

int vsnprintf(char *buf, size_t n, const char *fmt, va_list ap) { return k1kc_vsnprintf(buf, n, fmt, ap); }
int snprintf(char *buf, size_t n, const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    int r = k1kc_vsnprintf(buf, n, fmt, ap);
    va_end(ap); return r;
}
int sprintf(char *buf, const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    int r = k1kc_vsnprintf(buf, (size_t)-1 >> 1, fmt, ap);
    va_end(ap); return r;
}

int fwrite_raw(FILE *f, const char *s, size_t n) {
    if (f->kind != 1 && f->kind != 2) return -1;
    return k1kc_write(f->kind, s, n) < 0 ? -1 : (int)n;
}
size_t fwrite(const void *p, size_t size, size_t n, FILE *f) {
    size_t total = size * n;
    return total && fwrite_raw(f, p, total) < 0 ? 0 : n;
}
int vfprintf(FILE *f, const char *fmt, va_list ap) {
    char stack[256];
    va_list copy; va_copy(copy, ap);
    int n = k1kc_vsnprintf(stack, sizeof stack, fmt, copy);
    va_end(copy);
    if (n < 0) return n;
    if ((size_t)n < sizeof stack) return fwrite_raw(f, stack, (size_t)n);
    char *big = malloc((size_t)n + 1);
    if (!big) return -1;
    k1kc_vsnprintf(big, (size_t)n + 1, fmt, ap);
    int r = fwrite_raw(f, big, (size_t)n);
    free(big);
    return r;
}
int vprintf(const char *fmt, va_list ap) { return vfprintf(stdout, fmt, ap); }
int printf(const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    int r = vfprintf(stdout, fmt, ap);
    va_end(ap); return r;
}
int fprintf(FILE *f, const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    int r = vfprintf(f, fmt, ap);
    va_end(ap); return r;
}
int fputc(int c, FILE *f) { char ch = (char)c; return fwrite_raw(f, &ch, 1) < 0 ? EOF : (unsigned char)ch; }
int putchar(int c) { return fputc(c, stdout); }
int fputs(const char *s, FILE *f) { return fwrite_raw(f, s, strlen(s)) < 0 ? EOF : 0; }
int puts(const char *s) {
    size_t n = strlen(s);
    char *line = malloc(n + 1);
    if (!line) return EOF;
    memcpy(line, s, n); line[n] = '\n';
    int r = fwrite_raw(stdout, line, n + 1);
    free(line);
    return r < 0 ? EOF : 0;
}
int fflush(FILE *f) { (void)f; return 0; }

int fgetc(FILE *f) {
    if (f->kind == 0) {
        int k = k1kc_getkey();
        if (k < 0) { f->eof = 1; return EOF; }
        if (k == '\r') k = '\n';
        if (k == '\n' || (k >= 0x20 && k < 0x7f)) k1kc_write(1, &(char){ (char)k }, 1);   /* the terminal does no echo of its own */
        return k;
    }
    if (f->kind != 3 || f->pos >= f->len) { f->eof = 1; return EOF; }
    return f->data[f->pos++];
}
int getchar(void) { return fgetc(stdin); }
char *fgets(char *buf, int n, FILE *f) {
    int i = 0;
    while (i < n - 1) {
        int c = fgetc(f);
        if (c == EOF) break;
        buf[i++] = (char)c;
        if (c == '\n') break;
    }
    if (i == 0) return NULL;
    buf[i] = '\0';
    return buf;
}
size_t fread(void *p, size_t size, size_t n, FILE *f) {
    if (f->kind != 3 || !size) return 0;
    size_t want = size * n, have = f->len - f->pos;
    if (want > have) { want = have; f->eof = 1; }
    memcpy(p, f->data + f->pos, want);
    f->pos += want;
    return want / size;
}
int feof(FILE *f) { return f->eof; }
long ftell(FILE *f) { return f->kind == 3 ? (long)f->pos : -1; }

FILE *fopen(const char *path, const char *mode) {
    if (mode[0] != 'r') return NULL;   /* the file service is read-only */
    FILE *f = malloc(sizeof *f);
    if (!f) return NULL;
    f->kind = 3; f->pos = 0; f->eof = 0;
    if (k1kc_load(path, strlen(path), &f->data, &f->len) != 0) { free(f); return NULL; }
    return f;
}
int fclose(FILE *f) {
    if (f->kind != 3) return 0;
    k1kc_unload(f->data, f->len);
    free(f);
    return 0;
}
