/* The few calls that reach K1K itself; everything else in this library is plain C
   on top of them. Implemented in user/libc-sys (Rust). */
#ifndef K1K_H
#define K1K_H
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

_Noreturn void k1kc_exit(int code);
long k1kc_write(int fd, const void *buf, size_t len);
int k1kc_getkey(void);
uint64_t k1kc_uptime_ms(void);
uint32_t k1kc_task_id(void);
void *k1kc_alloc(size_t size);
void k1kc_free(void *ptr, size_t size);
int k1kc_load(const char *path, size_t path_len, unsigned char **out, size_t *out_len);
void k1kc_unload(unsigned char *ptr, size_t len);

#ifdef __cplusplus
}
#endif
#endif
