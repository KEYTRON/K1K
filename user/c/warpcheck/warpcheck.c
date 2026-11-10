/* warpcheck - a read-only native WARP trust-chain probe for K1K.
 *
 * It fetches the same pair WARP uses (index.json plus detached Ed25519
 * signature), verifies the exact received bytes, then checks that the index
 * contains a k1k-x86_64 build. Package installation remains intentionally
 * out of scope until K1K has a writable filesystem with atomic activation.
 *
 * The verifier is Monocypher 4.0.2 from WARP/src/vendor (BSD-2-Clause OR
 * CC0-1.0), linked from that checkout by the K1K Makefile. The fixture key
 * below is RFC 8032's public test key, not a production WARP release key. */
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <curl/curl.h>
#include "monocypher-ed25519.h"

#define INDEX_CAP 4096

static const uint8_t FIXTURE_PUBKEY[32] = {
    0x3d,0x40,0x17,0xc3,0xe8,0x43,0x89,0x5a,
    0x92,0xb7,0x0a,0xa7,0x4d,0x1b,0x7e,0xbc,
    0x9c,0x98,0x2c,0xcf,0x2e,0xc4,0x96,0x8c,
    0xc0,0xcd,0x55,0xf1,0x2a,0xf4,0x66,0x0c,
};

struct buffer { char data[INDEX_CAP]; size_t len; int overflow; };

static size_t collect(char *src, size_t size, size_t count, void *user) {
    struct buffer *dst = user;
    size_t n = size * count;
    if (n > sizeof dst->data - 1 - dst->len) {
        dst->overflow = 1;
        return 0;
    }
    memcpy(dst->data + dst->len, src, n);
    dst->len += n;
    dst->data[dst->len] = '\0';
    return n;
}

static int fetch(const char *path, struct buffer *out) {
    memset(out, 0, sizeof *out);
    char url[128];
    snprintf(url, sizeof url, "http://10.0.2.2:18791/%s", path);
    CURL *curl = curl_easy_init();
    if (!curl) return 0;
    curl_easy_setopt(curl, CURLOPT_URL, url);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, collect);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, out);
    curl_easy_setopt(curl, CURLOPT_FAILONERROR, 1L);
    curl_easy_setopt(curl, CURLOPT_TIMEOUT, 20L);
    int rc = curl_easy_perform(curl);
    curl_easy_cleanup(curl);
    return rc == CURLE_OK && !out->overflow;
}

static int b64(char c) {
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return c - 'a' + 26;
    if (c >= '0' && c <= '9') return c - '0' + 52;
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
}

static int decode_sig(const char *in, uint8_t sig[64]) {
    size_t n = strlen(in), out = 0;
    while (n && (in[n - 1] == '\n' || in[n - 1] == '\r' || in[n - 1] == ' ')) n--;
    if (n != 88) return 0;
    for (size_t i = 0; i < n; i += 4) {
        int a = b64(in[i]), b = b64(in[i + 1]);
        int c = in[i + 2] == '=' ? 0 : b64(in[i + 2]);
        int d = in[i + 3] == '=' ? 0 : b64(in[i + 3]);
        if (a < 0 || b < 0 || c < 0 || d < 0) return 0;
        uint32_t v = ((uint32_t)a << 18) | ((uint32_t)b << 12) | ((uint32_t)c << 6) | (uint32_t)d;
        if (out < 64) sig[out++] = (uint8_t)(v >> 16);
        if (in[i + 2] != '=' && out < 64) sig[out++] = (uint8_t)(v >> 8);
        if (in[i + 3] != '=' && out < 64) sig[out++] = (uint8_t)v;
    }
    return out == 64;
}

static int verifies(const struct buffer *index, const struct buffer *sig_text) {
    uint8_t sig[64];
    return decode_sig(sig_text->data, sig)
        && crypto_ed25519_check(sig, FIXTURE_PUBKEY, (const uint8_t *)index->data, index->len) == 0;
}

int main(void) {
    struct buffer index, sig;
    if (!fetch("warp-index.json", &index) || !fetch("warp-index.json.sig", &sig)) {
        printf("warpcheck: fetch failed\n");
        return 1;
    }
    if (!verifies(&index, &sig)) {
        printf("warpcheck: signature: refused\n");
        return 1;
    }
    if (!strstr(index.data, "\"k1k-x86_64\"")) {
        printf("warpcheck: signed index has no k1k-x86_64 build\n");
        return 1;
    }
    printf("warpcheck: signed k1k-x86_64 index: ok\n");

    index.data[0] ^= 1;
    if (verifies(&index, &sig)) {
        printf("warpcheck: tampered index: ACCEPTED\n");
        return 1;
    }
    printf("warpcheck: tampered index: refused\n");
    return 0;
}
