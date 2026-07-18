/*
** A minimal, real external consumer of the mpedb-capi shim: it includes the
** shim's sqlite3.h and links libmpedb_sqlite3, then opens an in-memory
** database, creates a table, inserts rows (prepared + bound), and reads them
** back — proving a plain C libsqlite3 caller runs against mpedb. Prints the
** rows and exits non-zero on any mismatch.
*/
#include "sqlite3.h"
#include <stdio.h>
#include <string.h>

#define CHECK(cond, msg)                                                       \
  do {                                                                         \
    if (!(cond)) {                                                             \
      fprintf(stderr, "FAIL: %s\n", msg);                                      \
      return 1;                                                                \
    }                                                                          \
  } while (0)

int main(void) {
  sqlite3 *db = 0;
  int rc = sqlite3_open(":memory:", &db);
  CHECK(rc == SQLITE_OK && db != 0, "open :memory:");

  printf("libversion: %s\n", sqlite3_libversion());

  rc = sqlite3_exec(db,
                    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", 0,
                    0, 0);
  CHECK(rc == SQLITE_OK, "create table");

  /* Prepared INSERT with bound parameters. */
  sqlite3_stmt *ins = 0;
  rc = sqlite3_prepare_v2(db, "INSERT INTO users (id, name) VALUES (?, ?)", -1,
                          &ins, 0);
  CHECK(rc == SQLITE_OK, "prepare insert");
  CHECK(sqlite3_bind_parameter_count(ins) == 2, "param count");

  const char *names[3] = {"ada", "grace", "linus"};
  for (int i = 0; i < 3; i++) {
    sqlite3_reset(ins);
    sqlite3_clear_bindings(ins);
    sqlite3_bind_int(ins, 1, i + 1);
    sqlite3_bind_text(ins, 2, names[i], -1, SQLITE_TRANSIENT);
    rc = sqlite3_step(ins);
    CHECK(rc == SQLITE_DONE, "step insert");
  }
  sqlite3_finalize(ins);

  /* SELECT the rows back. */
  sqlite3_stmt *sel = 0;
  rc = sqlite3_prepare_v2(db, "SELECT id, name FROM users ORDER BY id", -1, &sel,
                          0);
  CHECK(rc == SQLITE_OK, "prepare select");
  CHECK(sqlite3_column_count(sel) == 2, "column count");
  CHECK(strcmp(sqlite3_column_name(sel, 0), "id") == 0, "column name id");
  CHECK(strcmp(sqlite3_column_name(sel, 1), "name") == 0, "column name name");

  int seen = 0;
  while ((rc = sqlite3_step(sel)) == SQLITE_ROW) {
    int id = sqlite3_column_int(sel, 0);
    const unsigned char *nm = sqlite3_column_text(sel, 1);
    printf("row: id=%d name=%s\n", id, nm ? (const char *)nm : "(null)");
    CHECK(id == seen + 1, "row id order");
    CHECK(strcmp((const char *)nm, names[seen]) == 0, "row name");
    seen++;
  }
  CHECK(rc == SQLITE_DONE, "select done");
  CHECK(seen == 3, "row count");
  sqlite3_finalize(sel);

  /* A constraint error must surface as SQLITE_CONSTRAINT. */
  char *err = 0;
  rc = sqlite3_exec(db, "INSERT INTO users (id, name) VALUES (1, 'dup')", 0, 0,
                    &err);
  CHECK(rc == SQLITE_CONSTRAINT, "duplicate PK -> SQLITE_CONSTRAINT");
  if (err) sqlite3_free(err);

  sqlite3_close(db);
  printf("OK\n");
  return 0;
}
