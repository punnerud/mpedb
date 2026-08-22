/*
** mpedb_capi_mprintf / mpedb_capi_snprintf and their va_list forms.
**
** These live in C because they are C-variadic, and defining a C-variadic
** function is still unstable in Rust (`error[E0658]: C-variadic functions are
** unstable`, checked on 1.96). Only the variadic entry points are here; they
** allocate with malloc so that sqlite3_free — which is libc free in the shim —
** releases them.
**
** sqlite's printf is NOT the C one. It adds four conversions, and consumers
** rely on them:
**
**   %q   string, single quotes doubled     (goes inside a '...' literal)
**   %Q   as %q but wrapped in '...';       a NULL argument renders as NULL
**   %w   string, double quotes doubled     (goes inside a "..." identifier)
**   %z   as %s, then sqlite3_free()s the argument
**
** and %s with NULL renders empty, not "(null)".
**
** Everything else is delegated to the platform snprintf one conversion at a
** time. It has to be one at a time: a va_list is indeterminate after being
** passed to vsnprintf, so the format cannot be split into runs and handed over
** wholesale. Each specifier is copied out, terminated, and applied to the one
** argument it consumes.
*/

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

extern void sqlite3_free(void *);

/* The crate links with ThinLTO, and LTO internalizes any symbol the linker's
** preserve list does not name — that list is built from the Rust #[no_mangle]
** exports, which these four are not. Without the attribute they end up local
** ('t' in nm): linked in, but invisible to every consumer. build.rs also keeps
** this translation unit out of LTO, for the same reason from the other side. */
#define MPEDB_API __attribute__((visibility("default"), used))

typedef struct Buf {
    char  *p;
    size_t n;    /* bytes used, excluding the terminator */
    size_t cap;
    int    oom;
} Buf;

static void buf_need(Buf *b, size_t extra) {
    if (b->oom) return;
    if (b->n + extra + 1 <= b->cap) return;
    size_t cap = b->cap ? b->cap : 128;
    while (cap < b->n + extra + 1) {
        if (cap > (size_t)1 << 40) { b->oom = 1; return; }
        cap *= 2;
    }
    char *q = (char *)realloc(b->p, cap);
    if (!q) { b->oom = 1; return; }
    b->p = q;
    b->cap = cap;
}

static void buf_add(Buf *b, const char *s, size_t n) {
    if (!n) return;
    buf_need(b, n);
    if (b->oom) return;
    memcpy(b->p + b->n, s, n);
    b->n += n;
    b->p[b->n] = 0;
}

static void buf_c(Buf *b, char c) { buf_add(b, &c, 1); }

/* %q / %Q / %w: copy with `dq` doubled, optionally wrapped in `dq`. */
static void buf_quoted(Buf *b, const char *s, char dq, int wrap) {
    if (!s) {
        /* A NULL argument. %Q renders the bare SQL keyword, so the result is
        ** still a valid expression; %q and %w render the parenthesized form,
        ** which is deliberately NOT valid SQL — it shows up in the output
        ** instead of silently vanishing. Measured against sqlite 3.31.1. */
        buf_add(b, wrap ? "NULL" : "(NULL)", wrap ? 4 : 6);
        return;
    }
    if (wrap) buf_c(b, dq);
    for (const char *c = s; *c; c++) {
        buf_c(b, *c);
        if (*c == dq) buf_c(b, dq);
    }
    if (wrap) buf_c(b, dq);
}

/* One ordinary conversion, applied to the single argument it consumes.
**
** The argument is pulled ONCE into a union, because va_arg cannot be replayed:
** if the rendering does not fit the stack buffer we have to render again at
** the exact width snprintf reported, and that second call needs the value, not
** the va_list. */
static void buf_one(Buf *b, const char *spec, int kind, va_list *ap) {
    union { int i; long l; long long ll; size_t z; double d; long double ld; void *p; } v;
    switch (kind) {
        case 'd': v.i  = va_arg(*ap, int);         break;
        case 'l': v.l  = va_arg(*ap, long);        break;
        case 'L': v.ll = va_arg(*ap, long long);   break;
        case 'z': v.z  = va_arg(*ap, size_t);      break;
        case 'f': v.d  = va_arg(*ap, double);      break;
        case 'D': v.ld = va_arg(*ap, long double); break;
        case 'p': v.p  = va_arg(*ap, void *);      break;
        default:  return;
    }

    char small[256];
    char *dst = small;
    size_t cap = sizeof small;
    char *heap = 0;

    for (int pass = 0; pass < 2; pass++) {
        int n;
        switch (kind) {
            case 'd': n = snprintf(dst, cap, spec, v.i);  break;
            case 'l': n = snprintf(dst, cap, spec, v.l);  break;
            case 'L': n = snprintf(dst, cap, spec, v.ll); break;
            case 'z': n = snprintf(dst, cap, spec, v.z);  break;
            case 'f': n = snprintf(dst, cap, spec, v.d);  break;
            case 'D': n = snprintf(dst, cap, spec, v.ld); break;
            default:  n = snprintf(dst, cap, spec, v.p);  break;
        }
        if (n < 0) { free(heap); return; }
        if ((size_t)n < cap) {
            buf_add(b, dst, (size_t)n);
            free(heap);
            return;
        }
        if (pass) { free(heap); return; }      /* should not happen twice */
        cap = (size_t)n + 1;                   /* snprintf told us the exact size */
        heap = (char *)malloc(cap);
        if (!heap) { b->oom = 1; return; }
        dst = heap;
    }
    free(heap);
}

MPEDB_API char *mpedb_capi_vmprintf(const char *fmt, va_list ap_in) {
    Buf b = {0, 0, 0, 0};
    va_list ap;
    va_copy(ap, ap_in);
    if (!fmt) { va_end(ap); return 0; }

    for (const char *f = fmt; *f; ) {
        if (*f != '%') {
            const char *start = f;
            while (*f && *f != '%') f++;
            buf_add(&b, start, (size_t)(f - start));
            continue;
        }
        const char *spec_start = f;
        f++;                                   /* past '%' */
        if (*f == '%') { buf_c(&b, '%'); f++; continue; }

        /* flags, width, precision — including the '*' forms, which each
        ** consume an int argument before the conversion does. */
        int star_w = 0, star_p = 0;
        while (*f && strchr("-+ #0'", *f)) f++;
        if (*f == '*') { star_w = 1; f++; } else while (*f >= '0' && *f <= '9') f++;
        if (*f == '.') {
            f++;
            if (*f == '*') { star_p = 1; f++; } else while (*f >= '0' && *f <= '9') f++;
        }
        /* length modifier */
        int len_l = 0, len_ll = 0, len_z = 0, len_ld = 0;
        if (*f == 'h') { f++; if (*f == 'h') f++; }
        else if (*f == 'l') { f++; len_l = 1; if (*f == 'l') { f++; len_l = 0; len_ll = 1; } }
        /* 'z' is ambiguous: sqlite's own conversion (string, then free) and
        ** C's size_t length modifier. It is read as the modifier only when an
        ** integer conversion follows; standing alone it is sqlite's %z. Read
        ** wrong, "%z" swallows the z and formats whatever character came next.
        **
        ** This is a DELIBERATE, NAMED divergence from sqlite, and the only one
        ** in this file. sqlite has no size_t modifier: it reads the z of "%zu"
        ** as its own conversion, takes the argument as a char*, and — measured
        ** against 3.31.1 — segfaults on `mprintf("%zu", (size_t)12345)` while
        ** this renders 12345. Nothing can call "%zu" against sqlite and work,
        ** so no real consumer depends on the crash; every DEFINED format
        ** behaves identically. Erring toward not dereferencing an integer as a
        ** pointer is worth one documented difference in undefined use. */
        else if (*f == 'z' && f[1] && strchr("diouxX", f[1])) { f++; len_z = 1; }
        else if (*f == 'j') { f++; len_ll = 1; }
        else if (*f == 't') { f++; len_z = 1; }
        else if (*f == 'L') { f++; len_ld = 1; }

        char conv = *f;
        if (!conv) { buf_add(&b, spec_start, (size_t)(f - spec_start)); break; }
        f++;

        int wv = 0, pv = 0;
        if (star_w) wv = va_arg(ap, int);
        if (star_p) pv = va_arg(ap, int);

        /* sqlite's own four, none of which the platform snprintf knows. */
        if (conv == 'q' || conv == 'Q' || conv == 'w') {
            const char *s = va_arg(ap, const char *);
            /* Only %Q wraps. %q and %w double their quote and nothing more. */
            buf_quoted(&b, s, conv == 'w' ? '"' : '\'', conv == 'Q');
            continue;
        }
        if (conv == 'z') {
            char *s = va_arg(ap, char *);
            if (s) { buf_add(&b, s, strlen(s)); sqlite3_free(s); }
            continue;
        }
        if (conv == 's') {
            const char *s = va_arg(ap, const char *);
            if (!s) continue;                  /* sqlite: NULL renders empty */
            size_t n = strlen(s);
            if (star_p && pv >= 0 && (size_t)pv < n) n = (size_t)pv;
            /* A width, if any, still has to pad. Rebuild the spec for it. */
            if (star_w || (size_t)(f - spec_start) > 2) {
                char spec[64], tmp[64];
                size_t sl = (size_t)(f - spec_start);
                if (sl >= sizeof spec) sl = sizeof spec - 1;
                memcpy(spec, spec_start, sl); spec[sl] = 0;
                if (star_w || star_p) {
                    /* Collapse the '*' forms into literal numbers. */
                    snprintf(tmp, sizeof tmp, "%%%s%d.%ds",
                             strchr(spec, '-') ? "-" : "",
                             star_w ? wv : 0, (int)n);
                    memcpy(spec, tmp, strlen(tmp) + 1);
                }
                char out[512];
                int r = snprintf(out, sizeof out, spec, s);
                if (r >= 0 && (size_t)r < sizeof out) { buf_add(&b, out, (size_t)r); continue; }
            }
            buf_add(&b, s, n);
            continue;
        }
        if (conv == 'c') {
            int c = va_arg(ap, int);
            buf_c(&b, (char)c);
            continue;
        }
        if (conv == 'n') { (void)va_arg(ap, int *); continue; }  /* sqlite ignores %n */

        /* Everything else goes to the platform snprintf, one argument wide. */
        {
            char spec[64];
            size_t sl = (size_t)(f - spec_start);
            if (sl >= sizeof spec) sl = sizeof spec - 1;
            memcpy(spec, spec_start, sl);
            spec[sl] = 0;
            if (star_w || star_p) {
                /* Replace the '*'s with the values already pulled, so the
                ** delegated call takes exactly one argument. */
                char rebuilt[64];
                int k = 0;
                rebuilt[k++] = '%';
                for (size_t i = 1; i < sl && k < (int)sizeof rebuilt - 12; i++) {
                    if (spec[i] == '*') {
                        k += snprintf(rebuilt + k, sizeof rebuilt - k, "%d",
                                      (i && spec[i - 1] == '.') ? pv : wv);
                    } else {
                        rebuilt[k++] = spec[i];
                    }
                }
                rebuilt[k] = 0;
                memcpy(spec, rebuilt, (size_t)k + 1);
            }
            int kind;
            if (strchr("eEfFgGaA", conv))      kind = len_ld ? 'D' : 'f';
            else if (conv == 'p')              kind = 'p';
            else if (len_ll)                   kind = 'L';
            else if (len_z)                    kind = 'z';
            else if (len_l)                    kind = 'l';
            else                               kind = 'd';
            buf_one(&b, spec, kind, &ap);
        }
    }
    va_end(ap);

    if (b.oom) { free(b.p); return 0; }
    if (!b.p) {                                 /* empty result is still a string */
        b.p = (char *)malloc(1);
        if (b.p) b.p[0] = 0;
    }
    return b.p;
}

MPEDB_API char *mpedb_capi_mprintf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    char *r = mpedb_capi_vmprintf(fmt, ap);
    va_end(ap);
    return r;
}

/* Note the argument order: sqlite puts the SIZE first and returns the buffer,
** where C's snprintf puts the buffer first and returns a length. */
MPEDB_API char *mpedb_capi_vsnprintf(int n, char *buf, const char *fmt, va_list ap) {
    if (n <= 0 || !buf) return buf;
    char *r = mpedb_capi_vmprintf(fmt, ap);
    if (!r) { buf[0] = 0; return buf; }
    size_t len = strlen(r);
    if (len > (size_t)n - 1) len = (size_t)n - 1;
    memcpy(buf, r, len);
    buf[len] = 0;
    free(r);
    return buf;
}

MPEDB_API char *mpedb_capi_snprintf(int n, char *buf, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    char *r = mpedb_capi_vsnprintf(n, buf, fmt, ap);
    va_end(ap);
    return r;
}
