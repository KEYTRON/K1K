/* netget - fetches a file over plain HTTP and over HTTPS from the host.
 *
 * The first goes through the TCP calls of the C library (k1kc_net_*); the
 * second runs BearSSL on top of the same calls. The server certificate is the
 * test-only one in tools/testcert, pinned by its public key: there is no clock
 * and no trust store on K1K yet, so there is nothing to validate a chain with. */
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <k1k.h>
#include <curl/curl.h>
#include "bearssl.h"

#define HOST_IP 0x0A000202u /* 10.0.2.2, the host as QEMU's network shows it */

/* P-256 public key of tools/testcert/cert.pem (uncompressed point). */
static const unsigned char PINNED_Q[65] = { 0x04, 0x3b, 0xf1, 0x0d, 0xfa, 0x1e, 0x13, 0xa4, 0x11, 0x49, 0x23, 0x37, 0xf0, 0x39, 0x6d, 0xc2, 0xac, 0x2b, 0x93, 0x68, 0xa1, 0x85, 0xfb, 0x55, 0xb1, 0x66, 0x54, 0x23, 0xf6, 0xfb, 0xf3, 0x3d, 0x2c, 0x9f, 0x73, 0xa9, 0xc6, 0x60, 0xe9, 0x63, 0x50, 0xb1, 0x44, 0x01, 0xb8, 0x94, 0xce, 0xbe, 0x26, 0x3a, 0x44, 0xf0, 0x79, 0x3e, 0x3f, 0x34, 0x11, 0x93, 0xd5, 0xd0, 0x09, 0xab, 0xe1, 0xdd, 0x9c };

static unsigned char body[4096];
static unsigned char iobuf[BR_SSL_BUFSIZE_BIDI];

static const char *request(int tls) {
    return tls ? "GET /k1k-net.txt HTTP/1.0\r\nHost: k1k-test\r\nConnection: close\r\n\r\n"
               : "GET /k1k-net.txt HTTP/1.0\r\nHost: 10.0.2.2\r\nConnection: close\r\n\r\n";
}

/* A response is good if it is a 200 and the body holds the marker. */
static int check(const char *what, size_t n) {
    body[n < sizeof body ? n : sizeof body - 1] = 0;
    char *eol = strchr((char *)body, '\r');
    if (eol) *eol = 0;
    int ok = strstr((char *)body, " 200") != NULL;
    if (eol) *eol = '\r';
    ok = ok && strstr((char *)body, "K1K-NET-OK") != NULL;
    printf("netget: %s: %u bytes, %s\n", what, (unsigned)n, ok ? "200 and the marker" : "unexpected response");
    return ok;
}

static int plain(void) {
    int c = k1kc_net_connect(HOST_IP, 18791);
    if (c != 0) {
        printf("netget: http: connect failed (%d)\n", c);
        return 0;
    }
    const char *req = request(0);
    if (k1kc_net_send(req, strlen(req)) < 0) return 0;
    size_t n = 0;
    for (;;) {
        long r = k1kc_net_recv(body + n, sizeof body - 1 - n);
        if (r <= 0) break;
        n += (size_t)r;
    }
    k1kc_net_close();
    return check("http", n);
}

static int low_read(void *ctx, unsigned char *data, size_t len) {
    (void)ctx;
    long r = k1kc_net_recv(data, len);
    return r > 0 ? (int)r : -1;
}

static int low_write(void *ctx, const unsigned char *data, size_t len) {
    (void)ctx;
    long r = k1kc_net_send(data, len);
    return r > 0 ? (int)r : -1;
}

static int secure(void) {
    static br_ssl_client_context sc;
    static br_x509_minimal_context xc;
    static br_x509_knownkey_context pin;
    static br_sslio_context ioc;
    unsigned char seed[32];

    int c = k1kc_net_connect(HOST_IP, 18792);
    if (c != 0) {
        printf("netget: https: connect failed (%d)\n", c);
        return 0;
    }
    br_ssl_client_init_full(&sc, &xc, NULL, 0);
    br_ec_public_key pk = { BR_EC_secp256r1, (unsigned char *)PINNED_Q, sizeof PINNED_Q };
    br_x509_knownkey_init_ec(&pin, &pk, BR_KEYTYPE_SIGN);
    br_ssl_engine_set_x509(&sc.eng, &pin.vtable);
    br_ssl_engine_set_buffer(&sc.eng, iobuf, sizeof iobuf, 1);
    k1kc_entropy(seed, sizeof seed);
    br_ssl_engine_inject_entropy(&sc.eng, seed, sizeof seed);
    br_ssl_client_reset(&sc, "k1k-test", 0);
    br_sslio_init(&ioc, &sc.eng, low_read, NULL, low_write, NULL);

    const char *req = request(1);
    if (br_sslio_write_all(&ioc, req, strlen(req)) < 0 || br_sslio_flush(&ioc) < 0) {
        printf("netget: https: handshake or write failed (engine error %d)\n", br_ssl_engine_last_error(&sc.eng));
        k1kc_net_close();
        return 0;
    }
    size_t n = 0;
    for (;;) {
        int r = br_sslio_read(&ioc, body + n, sizeof body - 1 - n);
        if (r <= 0) break;
        n += (size_t)r;
    }
    br_ssl_session_parameters sp;
    br_ssl_engine_get_session_parameters(&sc.eng, &sp);
    printf("netget: https: TLS version %#x, cipher suite %#x\n", (unsigned)sp.version, (unsigned)sp.cipher_suite);
    br_sslio_close(&ioc);
    k1kc_net_close();
    return check("https", n);
}

/* ---- the libcurl subset ---- */

struct sink {
    char data[1024];
    size_t len;
    int calls;
};

static size_t collect(char *p, size_t size, size_t n, void *user) {
    struct sink *s = user;
    size_t bytes = size * n;
    if (s->len + bytes < sizeof s->data) {
        memcpy(s->data + s->len, p, bytes);
        s->len += bytes;
        s->data[s->len] = 0;
    }
    return bytes;
}

static int progress(void *user, curl_off_t dt, curl_off_t dn, curl_off_t ut, curl_off_t un) {
    (void)dt; (void)ut; (void)un;
    ((struct sink *)user)->calls += dn > 0;
    return 0;
}

static int fetch(const char *url, struct sink *s, int follow, int fail, int head, long *code) {
    memset(s, 0, sizeof *s);
    CURL *h = curl_easy_init();
    curl_easy_setopt(h, CURLOPT_URL, url);
    curl_easy_setopt(h, CURLOPT_USERAGENT, "netget/1");
    curl_easy_setopt(h, CURLOPT_WRITEFUNCTION, collect);
    curl_easy_setopt(h, CURLOPT_WRITEDATA, s);
    curl_easy_setopt(h, CURLOPT_NOPROGRESS, 0L);
    curl_easy_setopt(h, CURLOPT_XFERINFOFUNCTION, progress);
    curl_easy_setopt(h, CURLOPT_XFERINFODATA, s);
    curl_easy_setopt(h, CURLOPT_FOLLOWLOCATION, (long)follow);
    curl_easy_setopt(h, CURLOPT_FAILONERROR, (long)fail);
    curl_easy_setopt(h, CURLOPT_NOBODY, (long)head);
    curl_easy_setopt(h, CURLOPT_TIMEOUT, 20L);
    int rc = curl_easy_perform(h);
    curl_easy_getinfo(h, CURLINFO_RESPONSE_CODE, code);
    curl_easy_cleanup(h);
    return rc;
}

static int curl_tests(void) {
    struct sink s;
    long code = 0;
    int ok = 1;
#define EXPECT(cond, what) do { if (!(cond)) { printf("netget: curl: FAILED %s\n", what); ok = 0; } } while (0)

    int rc = fetch("http://10.0.2.2:18791/k1k-net.txt", &s, 0, 0, 0, &code);
    EXPECT(rc == 0 && code == 200 && strstr(s.data, "K1K-NET-OK") && s.calls > 0, "http get with progress");
    rc = fetch("https://10.0.2.2:18792/k1k-net.txt", &s, 0, 0, 0, &code);
    EXPECT(rc == 0 && code == 200 && strstr(s.data, "K1K-NET-OK"), "https get");
    rc = fetch("http://10.0.2.2:18791/k1k-net.txt", &s, 0, 0, 1, &code);
    EXPECT(rc == 0 && code == 200 && s.len == 0, "head request");
    rc = fetch("http://10.0.2.2:18791/missing", &s, 0, 1, 0, &code);
    EXPECT(rc == CURLE_HTTP_RETURNED_ERROR && code == 404, "404 with failonerror");
    rc = fetch("http://10.0.2.2:18791/sub", &s, 1, 1, 0, &code);
    EXPECT(rc == 0 && code == 200 && strstr(s.data, "K1K-REDIR-OK"), "redirect followed");
    rc = fetch("ftp://10.0.2.2/x", &s, 0, 0, 0, &code);
    EXPECT(rc == CURLE_UNSUPPORTED_PROTOCOL, "unsupported scheme");
    printf("netget: curl: %s\n", ok ? "ok" : "failed");
    return ok;
}

int main(void) {
    int a = plain();
    if (a) printf("netget: http ok\n");
    int b = secure();
    if (b) printf("netget: https ok\n");
    int c = curl_tests();
    return a && b && c ? 0 : 1;
}
