#ifndef _STDIO_H
#define _STDIO_H
#include <stdarg.h>
#include <stddef.h>
#define EOF (-1)
#define BUFSIZ 512
typedef struct K1KFile FILE;
extern FILE *stdin, *stdout, *stderr;

int printf(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
int fprintf(FILE *f, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
int snprintf(char *buf, size_t n, const char *fmt, ...) __attribute__((format(printf, 3, 4)));
int sprintf(char *buf, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
int vprintf(const char *fmt, va_list ap);
int vfprintf(FILE *f, const char *fmt, va_list ap);
int vsnprintf(char *buf, size_t n, const char *fmt, va_list ap);
int putchar(int c);
int puts(const char *s);
int fputc(int c, FILE *f);
int fputs(const char *s, FILE *f);
size_t fwrite(const void *p, size_t size, size_t n, FILE *f);
int fflush(FILE *f);
int getchar(void);
int fgetc(FILE *f);
char *fgets(char *buf, int n, FILE *f);
size_t fread(void *p, size_t size, size_t n, FILE *f);
FILE *fopen(const char *path, const char *mode);
int fclose(FILE *f);
int feof(FILE *f);
long ftell(FILE *f);
#endif
