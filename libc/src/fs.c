/* Writable files on the FAT16 volume, POSIX-shaped: mkdir (sys/stat.h),
   unlink (unistd.h) and rename (stdio.h) sit on the k1kc_* calls from
   k1k.h, translating the fsproto status into errno. Creating a file with
   its content is one K1K-specific call — k1kc_create — because the file
   service has no open-for-write: the whole payload travels in one request. */
#include <errno.h>
#include <string.h>
#include <sys/stat.h>
#include <k1k.h>

static int fs_errno(int status) {
    switch (status) {
    case 1: return ENOENT;   /* no such file */
    case 2: return EISDIR;   /* is a directory */
    case 3: return ENOTDIR;  /* not a directory */
    case 6: return EINVAL;   /* invalid name or request */
    case 7: return ENOSPC;   /* the disk (or the buffer) is full */
    case 10: return EEXIST;  /* the name already exists */
    case -1: return EACCES;  /* no fs capability */
    default: return EIO;
    }
}

int mkdir(const char *path, mode_t mode) {
    (void)mode;
    int r = k1kc_mkdir(path, strlen(path));
    if (r == 0) return 0;
    errno = fs_errno(r);
    return -1;
}

int unlink(const char *path) {
    int r = k1kc_unlink(path, strlen(path));
    if (r == 0) return 0;
    errno = fs_errno(r);
    return -1;
}

int rename(const char *old_path, const char *new_path) {
    int r = k1kc_rename(old_path, strlen(old_path), new_path, strlen(new_path));
    if (r == 0) return 0;
    errno = fs_errno(r);
    return -1;
}
