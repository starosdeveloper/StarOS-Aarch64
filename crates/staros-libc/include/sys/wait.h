/* sys/wait.h — waiting for children this system cannot have.
 *
 * `fork` refuses with `ENOSYS` and `exec` with `EACCES`, so a process here never
 * has a child to wait for. Every function below returns -1 with `errno` set to
 * `ECHILD`, which is precisely the answer POSIX defines for "there are none" — a
 * caller looping until `waitpid` reports no more children terminates immediately
 * and correctly, which is more than a refusal could manage.
 *
 * The status macros are real arithmetic on a value nothing here will produce. They
 * are here so that code handling both cases compiles, and because a program that
 * defines its own copies is a program with a second definition to get wrong.
 */
#ifndef _SYS_WAIT_H
#define _SYS_WAIT_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

#define WNOHANG   1
#define WUNTRACED 2
#define WCONTINUED 8

#define WEXITSTATUS(s) (((s) & 0xff00) >> 8)
#define WTERMSIG(s)    ((s) & 0x7f)
#define WSTOPSIG(s)    WEXITSTATUS(s)
#define WIFEXITED(s)   (WTERMSIG(s) == 0)
#define WIFSIGNALED(s) (((signed char)(((s) & 0x7f) + 1) >> 1) > 0)
#define WIFSTOPPED(s)  (((s) & 0xff) == 0x7f)
#define WIFCONTINUED(s) ((s) == 0xffff)

pid_t wait(int *status);
pid_t waitpid(pid_t pid, int *status, int options);

#ifdef __cplusplus
}
#endif

#endif /* sys/wait.h */
