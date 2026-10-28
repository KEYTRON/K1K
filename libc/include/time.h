#ifndef _TIME_H
#define _TIME_H
typedef long clock_t;
#define CLOCKS_PER_SEC 1000
clock_t clock(void);   /* milliseconds since boot */
#endif
