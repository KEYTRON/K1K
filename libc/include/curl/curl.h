/* A small, source-compatible subset of libcurl's easy interface for K1K: what
   WARP (and programs like it) use to fetch URLs over HTTP and HTTPS. It is not
   libcurl: HTTP/1.0 requests, no chunked bodies, no cookies, no proxies, one
   transfer at a time. Option and error numbers match libcurl's. */
#ifndef K1K_CURL_H
#define K1K_CURL_H
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef void CURL;
typedef long long curl_off_t;
typedef int CURLcode;
typedef int CURLoption;
typedef int CURLINFO;

struct curl_slist {
    char *data;
    struct curl_slist *next;
};

#define CURL_GLOBAL_DEFAULT 3L

enum {
    CURLE_OK = 0,
    CURLE_UNSUPPORTED_PROTOCOL = 1,
    CURLE_URL_MALFORMAT = 3,
    CURLE_COULDNT_RESOLVE_HOST = 6,
    CURLE_COULDNT_CONNECT = 7,
    CURLE_HTTP_RETURNED_ERROR = 22,
    CURLE_WRITE_ERROR = 23,
    CURLE_OPERATION_TIMEDOUT = 28,
    CURLE_ABORTED_BY_CALLBACK = 42,
    CURLE_TOO_MANY_REDIRECTS = 47,
    CURLE_SEND_ERROR = 55,
    CURLE_RECV_ERROR = 56,
    CURLE_SSL_CONNECT_ERROR = 35,
    CURLE_BAD_FUNCTION_ARGUMENT = 43
};

enum {
    CURLOPT_WRITEDATA = 10001,
    CURLOPT_URL = 10002,
    CURLOPT_POSTFIELDS = 10015,
    CURLOPT_USERAGENT = 10018,
    CURLOPT_LOW_SPEED_LIMIT = 19,
    CURLOPT_LOW_SPEED_TIME = 20,
    CURLOPT_HTTPHEADER = 10023,
    CURLOPT_WRITEFUNCTION = 20011,
    CURLOPT_TIMEOUT = 13,
    CURLOPT_NOPROGRESS = 43,
    CURLOPT_NOBODY = 44,
    CURLOPT_FAILONERROR = 45,
    CURLOPT_FOLLOWLOCATION = 52,
    CURLOPT_CAINFO = 10065,
    CURLOPT_XFERINFODATA = 10057,
    CURLOPT_CONNECTTIMEOUT = 78,
    CURLOPT_XFERINFOFUNCTION = 20219
};

#define CURLINFO_RESPONSE_CODE 0x200002

typedef size_t (*curl_write_callback)(char *ptr, size_t size, size_t nmemb, void *userdata);
typedef int (*curl_xferinfo_callback)(void *clientp, curl_off_t dltotal, curl_off_t dlnow,
                                      curl_off_t ultotal, curl_off_t ulnow);

CURLcode curl_global_init(long flags);
void curl_global_cleanup(void);
CURL *curl_easy_init(void);
void curl_easy_cleanup(CURL *h);
CURLcode curl_easy_setopt(CURL *h, CURLoption option, ...);
CURLcode curl_easy_perform(CURL *h);
CURLcode curl_easy_getinfo(CURL *h, CURLINFO info, ...);
const char *curl_easy_strerror(CURLcode code);
struct curl_slist *curl_slist_append(struct curl_slist *list, const char *s);
void curl_slist_free_all(struct curl_slist *list);

#ifdef __cplusplus
}
#endif
#endif
