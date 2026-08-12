#!/usr/bin/env bash
# Derive the libc contract for the GUI phases: exactly which C library functions Qt
# calls, taken from a real Qt build rather than guessed.
#
# G5 is the phase that can become infinite, and the antidote in the roadmap is to
# measure instead of guessing. The measurement is this: every undefined symbol in
# Qt's own libraries that the C library (or the math library) is expected to
# provide. What comes out is a finite list — a few hundred names — and that list is
# the phase's contract. `scripts/libc-progress.sh` scores the implementation
# against it.
#
# Provenance matters more than convenience here, so the header of the generated file
# records which Qt and which libc the list came from. A list from someone's memory
# of what libc contains would look identical and be worthless.
#
# Note what this list is *not*: Qt here is the distribution's shared build, which
# pulls in things a static `-fno-exceptions -fno-rtti` build for a phone would not
# (dlopen for plugins, fork/exec, System V IPC, inotify). Those names stay in the
# list on purpose — deciding to refuse a symbol is a decision worth recording, and
# `docs/LIBC-CONTRACT.md` records it next to the reason. Silently dropping them
# would make the contract shorter and the plan less honest.
#
# Usage: scripts/libc-contract.sh [output-file]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${1:-$here/docs/libc-contract.txt}"

# The Qt modules a QML application actually links: Core, Gui, Qml, Quick. Widgets is
# deliberately absent — the target is QML, and QtWidgets would add a X11/dialog
# surface this system will never have.
libs=()
for module in Core Gui Qml Quick; do
    lib="$(ls /usr/lib/libQt6$module.so.6 /usr/lib64/libQt6$module.so.6 2>/dev/null | head -1 || true)"
    [ -n "$lib" ] && libs+=("$lib")
done
if [ "${#libs[@]}" -eq 0 ]; then
    echo "libc-contract: no Qt 6 libraries found (install qt6-base/qt6-declarative)" >&2
    exit 1
fi

libc="$(ls /usr/lib/libc.so.6 /lib/x86_64-linux-gnu/libc.so.6 2>/dev/null | head -1 || true)"
libm="$(ls /usr/lib/libm.so.6 /lib/x86_64-linux-gnu/libm.so.6 2>/dev/null | head -1 || true)"
[ -n "$libc" ] || { echo "libc-contract: no libc.so.6 to compare against" >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Undefined symbols across the Qt modules: everything they expect somebody else to
# provide. Versions (`@GLIBC_2.34`) are stripped — a version tag is a packaging
# detail, not a different function.
for lib in "${libs[@]}"; do
    nm -u -D "$lib"
done | awk '{print $2}' | sed 's/@.*//' | sort -u >"$tmp/undef"

# …intersected with what the C and math libraries actually define. Everything else
# is C++ runtime, glib, or another dependency, and belongs to other phases.
nm -D --defined-only "$libc" | awk '{print $3}' | sed 's/@.*//' | sort -u >"$tmp/libc"
comm -12 "$tmp/undef" "$tmp/libc" >"$tmp/want-libc"
if [ -n "$libm" ]; then
    nm -D --defined-only "$libm" | awk '{print $3}' | sed 's/@.*//' | sort -u >"$tmp/libm"
    comm -12 "$tmp/undef" "$tmp/libm" >"$tmp/want-libm"
else
    : >"$tmp/want-libm"
fi
# glibc puts some math in libc too, so a symbol can land in both lists. libm wins:
# it is the layer that owns it.
comm -23 "$tmp/want-libc" "$tmp/want-libm" >"$tmp/want-libc-only"
mv "$tmp/want-libc-only" "$tmp/want-libc"

qtver="$(basename "$(readlink -f "${libs[0]}")")"
libcver="$("$libc" 2>/dev/null | head -1 || echo "unknown libc")"

# Classify, then sort — in that order and into separate files. Sorting the header
# along with the body is an easy mistake to make with one pipeline, and it produces
# a file whose provenance comments are scattered through the data.
#
# Layer 1 is pure computation, 2 memory, 3 time, 4 files and I/O, 5 threads and TLS,
# 6 multiplexing, 7 process/system — the order the roadmap implements them in, and
# the order risk rises. Anything unmatched lands in 7 so it is visible rather than
# quietly absent.
{
    awk '{ print "libm", $0 }' "$tmp/want-libm"
    awk '{ print "libc", $0 }' "$tmp/want-libc"
} | awk '
    {
        origin = $1; sym = $2; layer = 7
        if (origin == "libm") layer = 1
        else if (sym ~ /^(str|mem|wcs|qsort|bsearch|isspace|__isoc23_|abs$|atoi|atol|strtol|strtod|snprintf|sprintf|vsnprintf|printf|puts|fputs|fputc|fgetc|fgets|perror|setlocale|nl_langinfo|__(printf|fprintf|vfprintf|snprintf|sprintf|strcpy|strcat|strncat|memcpy|memset|vsnprintf|realpath)_chk$)/) layer = 1
        if (sym ~ /^(malloc|free|calloc|realloc|aligned_alloc|mmap|mmap64|munmap|mremap|mprotect|madvise|getpagesize|brk|sbrk)$/) layer = 2
        if (sym ~ /^(clock_gettime|nanosleep|gettimeofday|time|mktime|localtime_r|gmtime_r|tzset|__tzname|tzname)$/) layer = 3
        if (sym ~ /^(open|open64|openat|close|read|write|lseek|lseek64|fstat64|stat64|lstat64|statx|statfs64|access|fopen64|fclose|fread|fwrite|fseeko64|ftello64|feof|fflush|fileno|opendir|readdir64|closedir|mkdir|mkdirat|rmdir|unlink|unlinkat|rename|renameat|renameat2|readlink|symlink|link|linkat|truncate64|ftruncate|ftruncate64|fcntl|flock|ioctl|isatty|getcwd|chdir|fchdir|chmod|fchmod|futimens|fdatasync|sendfile|copy_file_range|shm_open|stdout|stderr)$/) layer = 4
        if (sym ~ /^(pthread_|sem_|__libc_single_threaded|sched_yield|sched_get_priority|__sched_cpucount|sched_getaffinity)/) layer = 5
        if (sym ~ /^(ppoll|poll|select|eventfd|eventfd_read|eventfd_write|pipe2|dup3|close_range|inotify_)/) layer = 6
        printf "%d %s\n", layer, sym
    }
' | sort -k1,1n -k2,2 >"$tmp/body"

{
    echo "# The libc contract for the GUI phases — GENERATED by scripts/libc-contract.sh."
    echo "#"
    echo "# Every name below is a symbol Qt leaves undefined and a C library defines, so"
    echo "# it is a function this system has to provide (or deliberately refuse — see"
    echo "# docs/LIBC-CONTRACT.md) before Qt can link. Do not edit by hand; regenerate."
    echo "#"
    echo "# Qt:   $qtver, modules: $(printf '%s ' "${libs[@]##*/lib}" | sed 's/\.so\.6 / /g')"
    echo "# libc: $libcver"
    echo "# counts: $(wc -l <"$tmp/want-libc") from libc, $(wc -l <"$tmp/want-libm") from libm"
    echo "#"
    echo "# Two columns: the layer (see the roadmap's G5 list) and the symbol."
    echo
    cat "$tmp/body"
} >"$out"

echo "wrote $out"
awk '!/^#/ && NF == 2 { n[$1]++ } END { for (l = 1; l <= 7; l++) printf "  layer %d: %d\n", l, n[l] + 0 }' "$out"
awk '!/^#/ && NF == 2 { total++ } END { printf "  total:   %d\n", total }' "$out"
