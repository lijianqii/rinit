/*
 * Test-only guest helper for the long soak.
 *
 * It is deliberately not part of the shipped image: the production image has
 * no Unix-socket client and no pty tool, which leaves notify, connection-driven
 * socket activation and TTY success untested from inside the guest.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static void fail(const char *what)
{
    fprintf(stderr, "rinit-soak-helper: %s: %s\n", what, strerror(errno));
    exit(1);
}

static socklen_t unix_addr(struct sockaddr_un *sa, const char *path)
{
    size_t len = strlen(path);

    memset(sa, 0, sizeof(*sa));
    sa->sun_family = AF_UNIX;
    if (path[0] == '@') {
        if (len > sizeof(sa->sun_path) - 1) {
            errno = ENAMETOOLONG;
            fail("socket path");
        }
        sa->sun_path[0] = '\0';
        memcpy(sa->sun_path + 1, path + 1, len - 1);
        return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + len);
    }
    if (len >= sizeof(sa->sun_path)) {
        errno = ENAMETOOLONG;
        fail("socket path");
    }
    memcpy(sa->sun_path, path, len + 1);
    return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + len + 1);
}

static void notify_ready(void)
{
    const char *path = getenv("NOTIFY_SOCKET");
    struct sockaddr_un sa;
    socklen_t len;
    int fd;

    if (path == NULL || path[0] == '\0') {
        fprintf(stderr, "rinit-soak-helper: NOTIFY_SOCKET is unset\n");
        exit(2);
    }
    fd = socket(AF_UNIX, SOCK_DGRAM, 0);
    if (fd < 0) {
        fail("socket");
    }
    len = unix_addr(&sa, path);
    if (sendto(fd, "READY=1\n", 8, 0, (struct sockaddr *)&sa, len) < 0) {
        fail("sendto");
    }
    close(fd);
}

static void socket_connect(const char *path)
{
    struct sockaddr_un sa;
    socklen_t len = unix_addr(&sa, path);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);

    if (fd < 0) {
        fail("socket");
    }
    if (connect(fd, (struct sockaddr *)&sa, len) < 0) {
        fail("connect");
    }
    if (write(fd, "x", 1) < 0) {
        fail("write");
    }
    close(fd);
}

static void oom_eat(void)
{
    const size_t total = 32U * 1024U * 1024U;
    volatile unsigned char *memory = malloc(total);
    size_t offset;

    if (memory == NULL) {
        fail("malloc");
    }
    for (offset = 0; offset < total; offset += 4096U) {
        memory[offset] = (unsigned char)(offset >> 12);
    }
    sleep(30);
}

static void pty_hold(const char *path)
{
    int master = posix_openpt(O_RDWR | O_NOCTTY);
    char *slave;

    if (master < 0) {
        fail("posix_openpt");
    }
    if (grantpt(master) < 0 || unlockpt(master) < 0) {
        fail("grantpt/unlockpt");
    }
    slave = ptsname(master);
    if (slave == NULL) {
        fail("ptsname");
    }
    unlink(path);
    if (symlink(slave, path) < 0) {
        fail("symlink");
    }
    for (;;) {
        pause();
    }
}

int main(int argc, char **argv)
{
    if (argc < 2) {
        fprintf(stderr,
                "usage: %s notify-ready [--hold] | notify-hold | oom-eat | "
                "socket-connect PATH | pty-hold PATH\n",
                argv[0]);
        return 2;
    }
    if (strcmp(argv[1], "notify-ready") == 0) {
        notify_ready();
        if (argc > 2 && strcmp(argv[2], "--hold") == 0) {
            sleep(30);
        }
        return 0;
    }
    if (strcmp(argv[1], "notify-hold") == 0) {
        sleep(30);
        return 0;
    }
    if (strcmp(argv[1], "oom-eat") == 0) {
        oom_eat();
        return 0;
    }
    if (strcmp(argv[1], "socket-connect") == 0) {
        if (argc != 3) {
            fprintf(stderr, "rinit-soak-helper: socket-connect needs PATH\n");
            return 2;
        }
        socket_connect(argv[2]);
        return 0;
    }
    if (strcmp(argv[1], "pty-hold") == 0) {
        if (argc != 3) {
            fprintf(stderr, "rinit-soak-helper: pty-hold needs PATH\n");
            return 2;
        }
        pty_hold(argv[2]);
        return 0;
    }
    fprintf(stderr, "rinit-soak-helper: unknown mode %s\n", argv[1]);
    return 2;
}
