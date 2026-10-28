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

/* TCP through the `net` service (needs the `net` capability in the manifest).
   connect: 0, or a positive net status (2 timeout, 3 refused), negative without
   the capability. recv: count, 0 = the peer closed, -1 error, -2 timeout. */
int k1kc_net_connect(uint32_t ip_be, uint16_t port);
long k1kc_net_send(const void *data, size_t len);
long k1kc_net_recv(void *out, size_t max);
void k1kc_net_close(void);
/* NOT secure randomness: timing only, until K1K has an entropy source. */
void k1kc_entropy(void *out, size_t len);

#ifdef __cplusplus
}
#endif
#endif
