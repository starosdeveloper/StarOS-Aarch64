/* ctype.h — character classification for the C locale, which is the only one.
 *
 * These are the pure-computation half of the contract's layer 1: no table, no
 * locale, ASCII only. A byte above 0x7f is not a letter here, and that is the C
 * locale's answer rather than an approximation of one.
 */
#ifndef _STAROS_CTYPE_H
#define _STAROS_CTYPE_H 1

#ifdef __cplusplus
extern "C" {
#endif

int isalnum(int c);
int isalpha(int c);
int isblank(int c);
int iscntrl(int c);
int isdigit(int c);
int isgraph(int c);
int islower(int c);
int isprint(int c);
int ispunct(int c);
int isspace(int c);
int isupper(int c);
int isxdigit(int c);
int tolower(int c);
int toupper(int c);

/* glibc's classification table, which libstdc++ reaches past this header to use.
 *
 * `std::ctype<char>` is not built on `isalpha`. libstdc++'s `ctype_base` for a
 * glibc target is a set of *mask bits* and a table lookup — `__ctype_b_loc()`
 * returns a pointer to a pointer to 384 shorts, indexed from -128 so that a signed
 * `char` can be used directly. Every `std::locale`, every `std::isalpha`, and
 * QV4's lexer through `<cwctype>` go through it.
 *
 * So the table is real here and has the same shape. The negative half is zeroed:
 * this is the C locale, where a byte above 127 classifies as nothing, and the
 * indices below zero are the same bytes seen as signed. Getting the offset wrong
 * would make every high byte a letter, which is the sort of thing that turns up as
 * a parser accepting rubbish months later.
 */
#define _ISbit(bit) ((bit) < 8 ? ((1 << (bit)) << 8) : ((1 << (bit)) >> 8))

enum {
    _ISupper  = _ISbit(0),
    _ISlower  = _ISbit(1),
    _ISalpha  = _ISbit(2),
    _ISdigit  = _ISbit(3),
    _ISxdigit = _ISbit(4),
    _ISspace  = _ISbit(5),
    _ISprint  = _ISbit(6),
    _ISgraph  = _ISbit(7),
    _ISblank  = _ISbit(8),
    _IScntrl  = _ISbit(9),
    _ISpunct  = _ISbit(10),
    _ISalnum  = _ISbit(11)
};

/* Indexed from -128 to 255. The pointer is to the entry for zero, so `table[-1]`
 * is a valid read — which is why the array behind it is 384 entries and not 256. */
const unsigned short **__ctype_b_loc(void);
const int **__ctype_tolower_loc(void);
const int **__ctype_toupper_loc(void);

#ifdef __cplusplus
}
#endif

#endif /* ctype.h */
