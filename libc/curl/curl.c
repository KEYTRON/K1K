/* The easy interface of libcurl, the part WARP uses, on top of the C library's
 * TCP calls (k1kc_net_*) and BearSSL. See curl/curl.h for what is missing.
 *
 * TLS: the server's certificate chain is NOT validated (K1K has no clock and no
 * trust store yet); CURLOPT_CAINFO is accepted and ignored. What is fetched must
 * be protected some other way, as WARP's signed indexes and hashes are. */
#include <curl/curl.h>
#include <k1k.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "bearssl.h"

#define HDR_MAX 16384
#define MAX_REDIRECTS 10

struct easy {
    char *url;
    char *user_agent;
    char *post;
    struct curl_slist *headers;
    curl_write_callback write_fn;
    void *write_data;
    curl_xferinfo_callback progress_fn;
    void *progress_data;
    long timeout_s, connect_timeout_s;
    int follow, fail_on_error, no_progress, nobody;
    long response_code;
};

/* ---- slist -------------------------------------------------------------- */

struct curl_slist *curl_slist_append(struct curl_slist *list, const char *s) {
    struct curl_slist *n = malloc(sizeof *n);
    if (!n) return NULL;
    size_t len = strlen(s);
    n->data = malloc(len + 1);
    if (!n->data) {
        free(n);
        return NULL;
    }
    memcpy(n->data, s, len + 1);
    n->next = NULL;
    if (!list) return n;
    struct curl_slist *t = list;
    while (t->next) t = t->next;
    t->next = n;
    return list;
}

void curl_slist_free_all(struct curl_slist *list) {
    while (list) {
        struct curl_slist *next = list->next;
        free(list->data);
        free(list);
        list = next;
    }
}

/* ---- handle ------------------------------------------------------------- */

CURLcode curl_global_init(long flags) {
    (void)flags;
    return CURLE_OK;
}
void curl_global_cleanup(void) {}

CURL *curl_easy_init(void) {
    struct easy *e = calloc(1, sizeof *e);
    if (e) e->no_progress = 1;
    return e;
}

static char *dup(const char *s) {
    size_t n = strlen(s) + 1;
    char *d = malloc(n);
    if (d) memcpy(d, s, n);
    return d;
}

void curl_easy_cleanup(CURL *h) {
    struct easy *e = h;
    if (!e) return;
    free(e->url);
    free(e->user_agent);
    free(e->post);
    free(e);
}

CURLcode curl_easy_setopt(CURL *h, CURLoption opt, ...) {
    struct easy *e = h;
    va_list ap;
    va_start(ap, opt);
    CURLcode rc = CURLE_OK;
    switch (opt) {
    case CURLOPT_URL: free(e->url); e->url = dup(va_arg(ap, const char *)); break;
    case CURLOPT_USERAGENT: free(e->user_agent); e->user_agent = dup(va_arg(ap, const char *)); break;
    case CURLOPT_POSTFIELDS: free(e->post); e->post = dup(va_arg(ap, const char *)); break;
    case CURLOPT_HTTPHEADER: e->headers = va_arg(ap, struct curl_slist *); break;
    case CURLOPT_WRITEFUNCTION: e->write_fn = va_arg(ap, curl_write_callback); break;
    case CURLOPT_WRITEDATA: e->write_data = va_arg(ap, void *); break;
    case CURLOPT_XFERINFOFUNCTION: e->progress_fn = va_arg(ap, curl_xferinfo_callback); break;
    case CURLOPT_XFERINFODATA: e->progress_data = va_arg(ap, void *); break;
    case CURLOPT_TIMEOUT: e->timeout_s = va_arg(ap, long); break;
    case CURLOPT_CONNECTTIMEOUT: e->connect_timeout_s = va_arg(ap, long); break;
    case CURLOPT_FOLLOWLOCATION: e->follow = va_arg(ap, long) != 0; break;
    case CURLOPT_FAILONERROR: e->fail_on_error = va_arg(ap, long) != 0; break;
    case CURLOPT_NOPROGRESS: e->no_progress = va_arg(ap, long) != 0; break;
    case CURLOPT_NOBODY: e->nobody = va_arg(ap, long) != 0; break;
    case CURLOPT_CAINFO: (void)va_arg(ap, const char *); break;
    case CURLOPT_LOW_SPEED_LIMIT:
    case CURLOPT_LOW_SPEED_TIME: (void)va_arg(ap, long); break;
    default: rc = CURLE_BAD_FUNCTION_ARGUMENT;
    }
    va_end(ap);
    return rc;
}

CURLcode curl_easy_getinfo(CURL *h, CURLINFO info, ...) {
    struct easy *e = h;
    va_list ap;
    va_start(ap, info);
    CURLcode rc = CURLE_OK;
    if (info == CURLINFO_RESPONSE_CODE) *va_arg(ap, long *) = e->response_code;
    else rc = CURLE_BAD_FUNCTION_ARGUMENT;
    va_end(ap);
    return rc;
}

const char *curl_easy_strerror(CURLcode c) {
    switch (c) {
    case CURLE_OK: return "No error";
    case CURLE_UNSUPPORTED_PROTOCOL: return "Unsupported protocol";
    case CURLE_URL_MALFORMAT: return "URL using bad/illegal format or missing URL";
    case CURLE_COULDNT_RESOLVE_HOST: return "Could not resolve hostname";
    case CURLE_COULDNT_CONNECT: return "Could not connect to server";
    case CURLE_HTTP_RETURNED_ERROR: return "HTTP response code said error";
    case CURLE_WRITE_ERROR: return "Failed writing received data";
    case CURLE_OPERATION_TIMEDOUT: return "Timeout was reached";
    case CURLE_ABORTED_BY_CALLBACK: return "Operation was aborted by an application callback";
    case CURLE_TOO_MANY_REDIRECTS: return "Number of redirects hit maximum amount";
    case CURLE_SEND_ERROR: return "Failed sending data to the peer";
    case CURLE_RECV_ERROR: return "Failure when receiving data from the peer";
    case CURLE_SSL_CONNECT_ERROR: return "SSL connect error";
    case CURLE_BAD_FUNCTION_ARGUMENT: return "A libcurl function was given a bad argument";
    default: return "Unknown error";
    }
}

/* ---- TLS without chain validation -------------------------------------- */

typedef struct {
    const br_x509_class *vtable;
    br_x509_decoder_context dec;
    int first;
} accept_any;

static void aa_start_chain(const br_x509_class **c, const char *name) { (void)name; ((accept_any *)c)->first = 1; }
static void aa_start_cert(const br_x509_class **c, uint32_t len) {
    (void)len;
    accept_any *a = (accept_any *)c;
    if (a->first) br_x509_decoder_init(&a->dec, NULL, NULL);
}
static void aa_append(const br_x509_class **c, const unsigned char *b, size_t n) {
    accept_any *a = (accept_any *)c;
    if (a->first) br_x509_decoder_push(&a->dec, b, n);
}
static void aa_end_cert(const br_x509_class **c) { ((accept_any *)c)->first = 0; }
static unsigned aa_end_chain(const br_x509_class **c) {
    return br_x509_decoder_last_error(&((accept_any *)c)->dec) == 0 ? 0 : BR_ERR_X509_NOT_TRUSTED;
}
static const br_x509_pkey *aa_pkey(const br_x509_class *const *c, unsigned *usages) {
    if (usages) *usages = BR_KEYTYPE_KEYX | BR_KEYTYPE_SIGN;
    return br_x509_decoder_get_pkey(&((accept_any *)c)->dec);
}
static const br_x509_class accept_any_class = {
    sizeof(accept_any), aa_start_chain, aa_start_cert, aa_append, aa_end_cert, aa_end_chain, aa_pkey
};

/* ---- one connection ----------------------------------------------------- */

struct conn {
    int tls;
    br_ssl_client_context sc;
    br_x509_minimal_context xc;
    accept_any aa;
    br_sslio_context io;
    unsigned char iobuf[BR_SSL_BUFSIZE_BIDI];
};

static int low_read(void *ctx, unsigned char *d, size_t n) {
    (void)ctx;
    long r = k1kc_net_recv(d, n);
    return r > 0 ? (int)r : -1;
}
static int low_write(void *ctx, const unsigned char *d, size_t n) {
    (void)ctx;
    long r = k1kc_net_send(d, n);
    return r > 0 ? (int)r : -1;
}

static long conn_send(struct conn *c, const void *d, size_t n) {
    if (!c->tls) return k1kc_net_send(d, n) < 0 ? -1 : (long)n;
    if (br_sslio_write_all(&c->io, d, n) < 0 || br_sslio_flush(&c->io) < 0) return -1;
    return (long)n;
}
/* count, 0 = closed, -1 error, -2 timeout */
static long conn_recv(struct conn *c, void *d, size_t n) {
    if (!c->tls) return k1kc_net_recv(d, n);
    int r = br_sslio_read(&c->io, d, n);
    if (r > 0) return r;
    /* A close_notify, or an abrupt end after the data, is the end of the body. */
    int err = br_ssl_engine_last_error(&c->sc.eng);
    return (err == BR_ERR_OK || err == BR_ERR_IO) ? 0 : -1;
}

/* ---- URL ---------------------------------------------------------------- */

struct url {
    int tls;
    char host[256];
    unsigned port;
    char path[2048];
};

static int parse_url(const char *u, struct url *out) {
    if (strncmp(u, "http://", 7) == 0) { out->tls = 0; out->port = 80; u += 7; }
    else if (strncmp(u, "https://", 8) == 0) { out->tls = 1; out->port = 443; u += 8; }
    else return CURLE_UNSUPPORTED_PROTOCOL;
    size_t i = 0;
    while (*u && *u != '/' && *u != ':' && *u != '?' && i < sizeof out->host - 1) out->host[i++] = *u++;
    out->host[i] = 0;
    if (i == 0) return CURLE_URL_MALFORMAT;
    if (*u == ':') {
        out->port = 0;
        u++;
        while (*u >= '0' && *u <= '9') out->port = out->port * 10 + (unsigned)(*u++ - '0');
        if (out->port == 0 || out->port > 65535) return CURLE_URL_MALFORMAT;
    }
    if (*u == '/' || *u == '?') {
        size_t n = strlen(u);
        if (n >= sizeof out->path - 1) return CURLE_URL_MALFORMAT;
        if (*u == '?') { out->path[0] = '/'; memcpy(out->path + 1, u, n + 1); }
        else memcpy(out->path, u, n + 1);
    } else {
        strcpy(out->path, "/");
    }
    return CURLE_OK;
}

static int dotted_quad(const char *s, uint32_t *ip) {
    unsigned v[4], n = 0, cur = 0, digits = 0;
    for (;; s++) {
        if (*s >= '0' && *s <= '9') {
            cur = cur * 10 + (unsigned)(*s - '0');
            if (++digits > 3 || cur > 255) return 0;
        } else if (*s == '.' || *s == 0) {
            if (!digits || n > 3) return 0;
            v[n++] = cur;
            cur = digits = 0;
            if (*s == 0) break;
        } else return 0;
    }
    if (n != 4) return 0;
    *ip = v[0] << 24 | v[1] << 16 | v[2] << 8 | v[3];
    return 1;
}

/* ---- the transfer ------------------------------------------------------- */

static int ci_prefix(const char *line, const char *name) {
    size_t n = strlen(name);
    for (size_t i = 0; i < n; i++) {
        char a = line[i], b = name[i];
        if (a >= 'A' && a <= 'Z') a = (char)(a + 32);
        if (a != b) return 0;
    }
    return 1;
}

/* One request on one connection. Fills `location` when the answer is a redirect. */
static CURLcode transfer(struct easy *e, const struct url *u, char *location, size_t location_max, int *redirect) {
    static struct conn c;
    uint32_t ip;
    *redirect = 0;
    if (!dotted_quad(u->host, &ip) && k1kc_net_resolve(u->host, strlen(u->host), &ip) != 0)
        return CURLE_COULDNT_RESOLVE_HOST;
    int rc = k1kc_net_connect(ip, (uint16_t)u->port);
    if (rc == 2) return CURLE_OPERATION_TIMEDOUT;
    if (rc != 0) return CURLE_COULDNT_CONNECT;
    uint64_t started = k1kc_uptime_ms();

    memset(&c, 0, sizeof c);
    c.tls = u->tls;
    if (c.tls) {
        unsigned char seed[32];
        br_ssl_client_init_full(&c.sc, &c.xc, NULL, 0);
        c.aa.vtable = &accept_any_class;
        br_ssl_engine_set_x509(&c.sc.eng, &c.aa.vtable);
        br_ssl_engine_set_buffer(&c.sc.eng, c.iobuf, sizeof c.iobuf, 1);
        k1kc_entropy(seed, sizeof seed);
        br_ssl_engine_inject_entropy(&c.sc.eng, seed, sizeof seed);
        br_ssl_client_reset(&c.sc, u->host, 0);
        br_sslio_init(&c.io, &c.sc.eng, low_read, NULL, low_write, NULL);
    }

    /* Request. */
    static char req[4096];
    size_t n = 0;
    const char *method = e->nobody ? "HEAD" : (e->post ? "POST" : "GET");
    n += (size_t)snprintf(req + n, sizeof req - n, "%s %s HTTP/1.0\r\nHost: %s\r\nUser-Agent: %s\r\nAccept: */*\r\nConnection: close\r\n",
                          method, u->path, u->host, e->user_agent ? e->user_agent : "k1k-curl");
    for (struct curl_slist *h = e->headers; h && n < sizeof req - 64; h = h->next)
        n += (size_t)snprintf(req + n, sizeof req - n, "%s\r\n", h->data);
    size_t post_len = e->post ? strlen(e->post) : 0;
    if (e->post) n += (size_t)snprintf(req + n, sizeof req - n, "Content-Length: %u\r\n", (unsigned)post_len);
    n += (size_t)snprintf(req + n, sizeof req - n, "\r\n");
    if (n >= sizeof req) { k1kc_net_close(); return CURLE_SEND_ERROR; }
    if (conn_send(&c, req, n) < 0 || (post_len && conn_send(&c, e->post, post_len) < 0)) {
        k1kc_net_close();
        return c.tls ? CURLE_SSL_CONNECT_ERROR : CURLE_SEND_ERROR;
    }

    /* Headers. */
    static char head[HDR_MAX];
    size_t have = 0, body_at = 0;
    for (;;) {
        if (have >= sizeof head - 1) { k1kc_net_close(); return CURLE_RECV_ERROR; }
        long r = conn_recv(&c, head + have, sizeof head - 1 - have);
        if (r == -2) { k1kc_net_close(); return CURLE_OPERATION_TIMEDOUT; }
        if (r <= 0) { k1kc_net_close(); return CURLE_RECV_ERROR; }
        have += (size_t)r;
        head[have] = 0;
        char *end = strstr(head, "\r\n\r\n");
        if (end) { body_at = (size_t)(end - head) + 4; *end = 0; break; }
    }
    e->response_code = 0;
    if (strncmp(head, "HTTP/", 5) == 0) {
        const char *sp = strchr(head, ' ');
        if (sp) e->response_code = strtol(sp + 1, NULL, 10);
    }
    location[0] = 0;
    for (char *line = strstr(head, "\r\n"); line; line = strstr(line + 2, "\r\n"))
        if (ci_prefix(line + 2, "location:")) {
            const char *v = line + 2 + 9;
            while (*v == ' ') v++;
            size_t i = 0;
            while (v[i] && v[i] != '\r' && i < location_max - 1) { location[i] = v[i]; i++; }
            location[i] = 0;
        }
    if (e->follow && location[0] && e->response_code >= 300 && e->response_code < 400) {
        *redirect = 1;
        k1kc_net_close();
        return CURLE_OK;
    }
    if (e->fail_on_error && e->response_code >= 400) {
        k1kc_net_close();
        return CURLE_HTTP_RETURNED_ERROR;
    }

    /* Body: what came with the headers, then the rest until the peer closes. */
    curl_off_t total = 0;
    CURLcode result = CURLE_OK;
    size_t chunk = have - body_at;
    char *data = head + body_at;
    for (;;) {
        if (chunk && !e->nobody) {
            if (e->write_fn) {
                if (e->write_fn(data, 1, chunk, e->write_data) != chunk) { result = CURLE_WRITE_ERROR; break; }
            }
            total += (curl_off_t)chunk;
            if (!e->no_progress && e->progress_fn && e->progress_fn(e->progress_data, 0, total, 0, 0) != 0) {
                result = CURLE_ABORTED_BY_CALLBACK;
                break;
            }
        }
        if (e->timeout_s && k1kc_uptime_ms() - started > (uint64_t)e->timeout_s * 1000) { result = CURLE_OPERATION_TIMEDOUT; break; }
        static char buf[8192];
        long r = conn_recv(&c, buf, sizeof buf);
        if (r == 0) break;
        if (r == -2) { result = CURLE_OPERATION_TIMEDOUT; break; }
        if (r < 0) { result = CURLE_RECV_ERROR; break; }
        data = buf;
        chunk = (size_t)r;
    }
    if (c.tls && result == CURLE_OK) br_sslio_close(&c.io);
    k1kc_net_close();
    return result;
}

CURLcode curl_easy_perform(CURL *h) {
    struct easy *e = h;
    if (!e || !e->url) return CURLE_URL_MALFORMAT;
    char current[2048];
    if (strlen(e->url) >= sizeof current) return CURLE_URL_MALFORMAT;
    strcpy(current, e->url);
    for (int hops = 0; hops <= MAX_REDIRECTS; hops++) {
        struct url u;
        CURLcode rc = (CURLcode)parse_url(current, &u);
        if (rc != CURLE_OK) return rc;
        char location[2048];
        int redirect;
        rc = transfer(e, &u, location, sizeof location, &redirect);
        if (rc != CURLE_OK || !redirect) return rc;
        if (strncmp(location, "http://", 7) == 0 || strncmp(location, "https://", 8) == 0) {
            strcpy(current, location);
        } else if (location[0] == '/') {
            snprintf(current, sizeof current, "%s://%s:%u%s", u.tls ? "https" : "http", u.host, u.port, location);
        } else {
            return CURLE_URL_MALFORMAT;
        }
    }
    return CURLE_TOO_MANY_REDIRECTS;
}
