/* stdio.h — the console, as `crates/staros-libc` implements it.
 *
 * There is no file *stream* layer: `FILE` exists, `stdout` and `stderr` point at
 * the debug console, and `fopen` is absent rather than stubbed. A program that
 * needs a stream over a file will get one when something needs it, and until then
 * the missing symbol at link time is the honest answer.
 */
#ifndef _STAROS_STDIO_H
#define _STAROS_STDIO_H 1

#include <stdarg.h>
#include <stddef.h>
/* For `off_t`, which `fseeko` and the large-file names take. C does not put it in
 * <stdio.h>'s own set of types, but POSIX puts these functions here, so the header
 * that declares them has to reach for it. */
#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct _IO_FILE FILE;

extern FILE *stdout;
extern FILE *stderr;

int printf(const char *format, ...);
int fprintf(FILE *stream, const char *format, ...);
int snprintf(char *buf, size_t size, const char *format, ...);
int sprintf(char *buf, const char *format, ...);
int puts(const char *s);
int fputs(const char *s, FILE *stream);
int fputc(int c, FILE *stream);
int putchar(int c);
size_t fwrite(const void *ptr, size_t size, size_t count, FILE *stream);
int fflush(FILE *stream);
void perror(const char *message);

/* The rest of C's <stdio.h>, declared and not implemented, because <cstdio>
 * imports all of it into namespace std — see the note in <wchar.h>. A program that
 * calls `fopen` here gets an undefined symbol, which is the truth: there is no
 * stream layer over files yet. */
typedef struct {
    long __pos;
    int __state;
} fpos_t;

FILE *fopen(const char *path, const char *mode);
FILE *freopen(const char *path, const char *mode, FILE *stream);
/* The two functions joining streams to descriptors, both implemented in
 * `crates/staros-libc/src/stream.rs` and neither declared until Qt asked.
 * `qfsfileengine_unix.cpp` calls `fileno` at seven places — it is how QFile reaches
 * `fstat`, `lseek` and `ftruncate` on a `FILE *` it was handed. */
FILE *fdopen(int fd, const char *mode);
int fileno(FILE *stream);
FILE *tmpfile(void);
char *tmpnam(char *s);
int fclose(FILE *stream);
int feof(FILE *stream);
int ferror(FILE *stream);
void clearerr(FILE *stream);
int fgetc(FILE *stream);
char *fgets(char *s, int n, FILE *stream);
int getc(FILE *stream);
int getchar(void);
int putc(int c, FILE *stream);
int ungetc(int c, FILE *stream);
size_t fread(void *ptr, size_t size, size_t count, FILE *stream);
int fseek(FILE *stream, long offset, int whence);
long ftell(FILE *stream);
int fgetpos(FILE *stream, fpos_t *position);
int fsetpos(FILE *stream, const fpos_t *position);

/* `fseeko` and `ftello`, which differ from `fseek` and `ftell` in taking and
 * returning `off_t` rather than `long`. On this target the two are the same 64-bit
 * type, so the difference is nominal here and real everywhere the code might be
 * read — declaring them faithfully is what keeps that true. */
int fseeko(FILE *stream, off_t offset, int whence);
off_t ftello(FILE *stream);

/* The large-file names. `fopen64`, `fseeko64` and `ftello64` are real symbols in
 * `crates/staros-libc/src/stream.rs`, so they are declared. `fgetpos64` and
 * `fsetpos64` are not — `fpos_t` is one type here, there is nothing for a second
 * pair of functions to do differently, and a macro cannot drift out of step with the
 * functions it names. Qt reaches `::ftello64` in `qfile.cpp` line 1051. */
FILE *fopen64(const char *path, const char *mode);
int fseeko64(FILE *stream, off_t offset, int whence);
off_t ftello64(FILE *stream);
#define fgetpos64 fgetpos
#define fsetpos64 fsetpos
void rewind(FILE *stream);
void setbuf(FILE *stream, char *buffer);
int setvbuf(FILE *stream, char *buffer, int mode, size_t size);
int fscanf(FILE *stream, const char *format, ...);
int scanf(const char *format, ...);
int sscanf(const char *s, const char *format, ...);
int vfprintf(FILE *stream, const char *format, va_list args);
int vprintf(const char *format, va_list args);
int vsprintf(char *s, const char *format, va_list args);
int vsnprintf(char *s, size_t n, const char *format, va_list args);
int vfscanf(FILE *stream, const char *format, va_list args);
int vscanf(const char *format, va_list args);
int vsscanf(const char *s, const char *format, va_list args);
int remove(const char *path);
int rename(const char *from, const char *to);
extern FILE *stdin;

#define EOF (-1)
#define BUFSIZ 1024
#define FILENAME_MAX 128
#define FOPEN_MAX 8
#define TMP_MAX 1
#define L_tmpnam 16
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#define _IOFBF 0
#define _IOLBF 1
#define _IONBF 2

#ifdef __cplusplus
}
#endif

#endif /* stdio.h */
