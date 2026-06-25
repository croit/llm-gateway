// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH
//
// noiou — "no io_uring" launcher for Bun-compiled binaries under gVisor.
//
// Why this exists: excalirender is compiled with Bun. Bun's event loop probes
// io_uring (io_uring_setup) and pidfd_open. Under gVisor (runsc — our
// production OCI runtime) those syscalls are HALF-emulated: the setup call
// succeeds, so Bun commits to that fast path, but the ring/fd then never
// delivers the events Bun waits on. Bun blocks forever (an infinite
// epoll_pwait loop returning 0 events) and every render hits the sandbox
// wall-clock timeout. See oven-sh/bun#16063 and google/gvisor#11331.
//
// The fix: make those syscalls return a CLEAN ENOSYS. Bun treats "not
// supported" as "use the portable fallback" (epoll + SIGCHLD) — which works
// under gVisor. Verified locally: forcing io_uring_setup -> ENOSYS makes
// excalirender render normally under all sandbox hardening flags.
//
// We do this with a seccomp filter the process installs ON ITSELF, then exec
// the real program. This is deliberately the LEAST-privileged option: a
// seccomp filter can only ever REMOVE capability, never grant it, and it lives
// entirely inside the sandbox image — the gVisor isolation boundary is
// untouched and the OCI runtime is not weakened. Installing a filter
// unprivileged requires no_new_privs, which the sandbox already sets
// (--security-opt no-new-privileges); we also set it here so the wrapper works
// standalone (e.g. the build-time smoke test).
#include <stddef.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <linux/seccomp.h>
#include <linux/filter.h>
#include <linux/audit.h>

// x86_64 syscall numbers (the production sandbox is amd64).
#define SYS_io_uring_setup    425
#define SYS_io_uring_enter    426
#define SYS_io_uring_register 427
#define SYS_pidfd_open        434

int main(int argc, char **argv) {
    struct sock_filter filter[] = {
        // Only act on x86_64; on any other arch allow everything (never break
        // an unexpected arch — the filter is an optimization-killer, not a
        // security boundary).
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        // Match the io_uring / pidfd fast-path syscalls; everything else passes.
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_io_uring_setup,    4, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_io_uring_enter,    3, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_io_uring_register, 2, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_pidfd_open,        1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        // Return ENOSYS (38) for the matched syscalls so Bun takes its fallback.
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (38 & SECCOMP_RET_DATA)),
    };
    struct sock_fprog prog = {
        .len = (unsigned short)(sizeof(filter) / sizeof(filter[0])),
        .filter = filter,
    };

    if (argc < 2) {
        fprintf(stderr, "usage: noiou <program> [args...]\n");
        return 2;
    }
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) {
        perror("noiou: prctl(PR_SET_NO_NEW_PRIVS)");
        return 111;
    }
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog, 0, 0) != 0) {
        perror("noiou: prctl(PR_SET_SECCOMP)");
        return 112;
    }
    execvp(argv[1], &argv[1]);
    perror("noiou: execvp");
    return 127;
}
