#ifndef _SYS_STAT_H
#define _SYS_STAT_H
/* The FAT16 volume has no modes; mkdir takes one for POSIX shape and ignores it. */
typedef unsigned mode_t;
int mkdir(const char *path, mode_t mode);
#endif
