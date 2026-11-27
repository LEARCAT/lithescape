#define _GNU_SOURCE
#include <sys/stat.h>
#include <fcntl.h>
#include <time.h>
#include <sys/time.h>

int utimensat(int dirfd, const char *pathname, const struct timespec times[2], int flags) {
    (void)dirfd; (void)pathname; (void)times; (void)flags;
    return 0;
}
int futimens(int fd, const struct timespec times[2]) {
    (void)fd; (void)times;
    return 0;
}
int utimes(const char *filename, const struct timeval times[2]) {
    (void)filename; (void)times;
    return 0;
}
int utime(const char *filename, const struct utimbuf *times) {
    (void)filename; (void)times;
    return 0;
}
