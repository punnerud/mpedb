/*
** sqlite3.h — a hand-written subset of the sqlite3 C-API, declaring exactly the
** symbols exported by the mpedb-capi shim (libmpedb_sqlite3). It is ABI/name
** compatible with the upstream sqlite3.h for these functions, so a consumer
** that includes this header and links libmpedb_sqlite3 talks to mpedb. It is
** NOT the full sqlite3.h — only the core ~30 functions are covered.
*/
#ifndef MPEDB_SQLITE3_H
#define MPEDB_SQLITE3_H
#ifdef __cplusplus
extern "C" {
#endif

#include <stddef.h>

typedef struct sqlite3 sqlite3;
typedef struct sqlite3_stmt sqlite3_stmt;
typedef long long int sqlite3_int64;
typedef unsigned long long int sqlite3_uint64;
typedef void (*sqlite3_destructor_type)(void *);

#define SQLITE_STATIC      ((sqlite3_destructor_type)0)
#define SQLITE_TRANSIENT   ((sqlite3_destructor_type)-1)

/* Primary result codes. */
#define SQLITE_OK           0
#define SQLITE_ERROR        1
#define SQLITE_INTERNAL     2
#define SQLITE_PERM         3
#define SQLITE_ABORT        4
#define SQLITE_BUSY         5
#define SQLITE_LOCKED       6
#define SQLITE_NOMEM        7
#define SQLITE_READONLY     8
#define SQLITE_INTERRUPT    9
#define SQLITE_IOERR       10
#define SQLITE_CORRUPT     11
#define SQLITE_NOTFOUND    12
#define SQLITE_FULL        13
#define SQLITE_CANTOPEN    14
#define SQLITE_PROTOCOL    15
#define SQLITE_EMPTY       16
#define SQLITE_SCHEMA      17
#define SQLITE_TOOBIG      18
#define SQLITE_CONSTRAINT  19
#define SQLITE_MISMATCH    20
#define SQLITE_MISUSE      21
#define SQLITE_RANGE       25
#define SQLITE_NOTADB      26
#define SQLITE_ROW        100
#define SQLITE_DONE       101

/* Extended constraint codes. */
#define SQLITE_CONSTRAINT_CHECK       (SQLITE_CONSTRAINT | (1<<8))
#define SQLITE_CONSTRAINT_NOTNULL     (SQLITE_CONSTRAINT | (5<<8))
#define SQLITE_CONSTRAINT_PRIMARYKEY  (SQLITE_CONSTRAINT | (6<<8))
#define SQLITE_CONSTRAINT_UNIQUE      (SQLITE_CONSTRAINT | (8<<8))

/* Fundamental datatypes. */
#define SQLITE_INTEGER  1
#define SQLITE_FLOAT    2
#define SQLITE_TEXT     3
#define SQLITE_BLOB     4
#define SQLITE_NULL     5

/* open_v2 flags. */
#define SQLITE_OPEN_READONLY   0x00000001
#define SQLITE_OPEN_READWRITE  0x00000002
#define SQLITE_OPEN_CREATE     0x00000004
#define SQLITE_OPEN_URI        0x00000040
#define SQLITE_OPEN_MEMORY     0x00000080

/* open / close. */
int sqlite3_open(const char *filename, sqlite3 **ppDb);
int sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs);
int sqlite3_close(sqlite3 *db);
int sqlite3_close_v2(sqlite3 *db);
int sqlite3_busy_timeout(sqlite3 *db, int ms);
int sqlite3_extended_result_codes(sqlite3 *db, int onoff);
int sqlite3_get_autocommit(sqlite3 *db);

/* prepare / step / finalize / exec. */
int sqlite3_prepare_v2(sqlite3 *db, const char *zSql, int nByte,
                       sqlite3_stmt **ppStmt, const char **pzTail);
int sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte,
                    sqlite3_stmt **ppStmt, const char **pzTail);
int sqlite3_step(sqlite3_stmt *pStmt);
int sqlite3_reset(sqlite3_stmt *pStmt);
int sqlite3_finalize(sqlite3_stmt *pStmt);
int sqlite3_exec(sqlite3 *db, const char *sql,
                 int (*callback)(void *, int, char **, char **),
                 void *arg, char **errmsg);

/* bind (1-based). */
int sqlite3_bind_int(sqlite3_stmt *pStmt, int i, int v);
int sqlite3_bind_int64(sqlite3_stmt *pStmt, int i, sqlite3_int64 v);
int sqlite3_bind_double(sqlite3_stmt *pStmt, int i, double v);
int sqlite3_bind_text(sqlite3_stmt *pStmt, int i, const char *v, int n, void (*d)(void *));
int sqlite3_bind_blob(sqlite3_stmt *pStmt, int i, const void *v, int n, void (*d)(void *));
int sqlite3_bind_null(sqlite3_stmt *pStmt, int i);
int sqlite3_bind_parameter_count(sqlite3_stmt *pStmt);
int sqlite3_bind_parameter_index(sqlite3_stmt *pStmt, const char *name);
int sqlite3_clear_bindings(sqlite3_stmt *pStmt);

/* column read (0-based). */
int sqlite3_column_count(sqlite3_stmt *pStmt);
int sqlite3_data_count(sqlite3_stmt *pStmt);
int sqlite3_column_type(sqlite3_stmt *pStmt, int iCol);
int sqlite3_column_int(sqlite3_stmt *pStmt, int iCol);
sqlite3_int64 sqlite3_column_int64(sqlite3_stmt *pStmt, int iCol);
double sqlite3_column_double(sqlite3_stmt *pStmt, int iCol);
const unsigned char *sqlite3_column_text(sqlite3_stmt *pStmt, int iCol);
const void *sqlite3_column_blob(sqlite3_stmt *pStmt, int iCol);
int sqlite3_column_bytes(sqlite3_stmt *pStmt, int iCol);
const char *sqlite3_column_name(sqlite3_stmt *pStmt, int iCol);
const char *sqlite3_column_decltype(sqlite3_stmt *pStmt, int iCol);

/* status / misc. */
const char *sqlite3_errmsg(sqlite3 *db);
int sqlite3_errcode(sqlite3 *db);
int sqlite3_extended_errcode(sqlite3 *db);
int sqlite3_changes(sqlite3 *db);
int sqlite3_total_changes(sqlite3 *db);
sqlite3_int64 sqlite3_last_insert_rowid(sqlite3 *db);
const char *sqlite3_libversion(void);
int sqlite3_libversion_number(void);
const char *sqlite3_sourceid(void);
void *sqlite3_malloc(int n);
void *sqlite3_malloc64(sqlite3_uint64 n);
void sqlite3_free(void *p);

#ifdef __cplusplus
}
#endif
#endif /* MPEDB_SQLITE3_H */
