use crate::error::{Error, Result};
use std::cmp::Ordering;
use std::fmt;

/// Column types. Rigid by default and by design: unlike sqlite, a column only
/// ever stores its declared type (or NULL where permitted), and writes with the
/// wrong type are rejected — that is the dev/prod parity this project exists for.
///
/// [`ColumnType::Any`] opts a SINGLE column out of that, sqlite-affinity style.
/// It is per column on purpose: "rigid schema" is the product, and making it a
/// database-wide switch would turn a property you can rely on into one you have
/// to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ColumnType {
    Int64 = 1,
    Float64 = 2,
    Bool = 3,
    Text = 4,
    Blob = 5,
    /// Microseconds since the Unix epoch, UTC.
    Timestamp = 6,
    /// Any scalar, decided per VALUE rather than per column (sqlite affinity).
    ///
    /// The discriminant lives in the row's FIXED section, not the varlen body —
    /// `row::fixed_width(Any) == 9`, a tag byte plus the eight the other types
    /// use. That is not an arbitrary layout choice: prefixing the body with a tag
    /// would make it `[tag] ++ bytes`, which does not exist as one contiguous
    /// slice to borrow, and both `btree::Payload::Parts` (#42) and the streaming
    /// insert (#43) borrow varlen bodies straight out of the caller's `Value`.
    /// Keeping the tag in the fixed slot leaves those untouched, and a rigid
    /// column pays nothing for a feature it does not use.
    Any = 7,
    /// Days since the Unix epoch, UTC. PostgreSQL's `date`.
    ///
    /// Its own type rather than a [`ColumnType::Timestamp`] at midnight,
    /// because the two are DIFFERENT PostgreSQL types and a client that asks
    /// the catalog gets one answer for a column: reporting `timestamp` for a
    /// `DATE` column is a wrong answer, and reflection reads exactly that.
    Date = 8,
    /// Microseconds since midnight. PostgreSQL's `time without time zone`.
    Time = 9,
    /// An EXACT decimal, stored as its canonical text.
    ///
    /// Text storage is not a shortcut: `pgtype.rs` already classifies
    /// `numeric` as `ViaText` — "lossless as long as the far side is the same
    /// type" — and a float cannot hold `1e-14` and `1.00` and tell them from
    /// each other. ORDERING goes through the numeric key encoding, so it is by
    /// VALUE: `1.0` and `1.00` store distinctly and key identically, which is
    /// PostgreSQL's own equality rule for `numeric`, not a compromise.
    Numeric = 10,
}

impl ColumnType {
    pub fn from_tag(tag: u8) -> Option<ColumnType> {
        Some(match tag {
            1 => ColumnType::Int64,
            2 => ColumnType::Float64,
            3 => ColumnType::Bool,
            4 => ColumnType::Text,
            5 => ColumnType::Blob,
            6 => ColumnType::Timestamp,
            7 => ColumnType::Any,
            8 => ColumnType::Date,
            9 => ColumnType::Time,
            10 => ColumnType::Numeric,
            _ => return None,
        })
    }

    pub fn parse(name: &str) -> Option<ColumnType> {
        Some(match name {
            "int64" | "int" | "integer" => ColumnType::Int64,
            "float64" | "float" | "real" | "double" => ColumnType::Float64,
            "bool" | "boolean" => ColumnType::Bool,
            "text" | "string" => ColumnType::Text,
            "blob" | "bytes" => ColumnType::Blob,
            "timestamp" => ColumnType::Timestamp,
            "any" => ColumnType::Any,
            "date" => ColumnType::Date,
            "time" => ColumnType::Time,
            "numeric" | "decimal" => ColumnType::Numeric,
            _ => return None,
        })
    }

    /// A **declared SQL type name** (whatever a `CREATE TABLE` wrote) → the
    /// rigid column type it becomes.
    ///
    /// [`ColumnType::parse`] covers mpedb's own config vocabulary — the RIGID
    /// side of the product, unchanged. This covers a name arriving through
    /// **`CREATE TABLE`**, where sqlite's affinity rule is the authority:
    /// `varchar(100)`, `bigint`, `datetime`, `integer unsigned`, `double
    /// precision`, `decimal(10,2)`, `blob`, `timestamp`.
    ///
    /// The rule is sqlite's affinity ([`Affinity::from_type_name`], the SAME
    /// algorithm `CAST(x AS …)` uses, #83) with exactly two names carved out —
    /// see below. One vocabulary, one table:
    ///
    ///    | affinity | example declarations                            | column type |
    ///    |----------|-------------------------------------------------|-------------|
    ///    | INTEGER  | `bigint`, `smallint`, `int(8)`, `integer unsigned` | `Int64`  |
    ///    | REAL     | `double precision`, `float`, `real`             | `Float64`   |
    ///    | TEXT     | `varchar(100)`, `char(1)`, `clob`, `text`       | `Text`      |
    ///    | BLOB     | `blob`, `longblob`, `cblob`                      | **`Any`**   |
    ///    | NUMERIC  | `decimal(10,2)`, `datetime`, `date`, `timestamp`, `numeric`, `varbinary` | `Any` |
    ///
    /// **NUMERIC and BLOB both become [`ColumnType::Any`]**, for the same
    /// reason and with opposite conversion rules (which is why the affinity
    /// must be carried alongside — [`ColumnType::declared`]): neither is a
    /// single storage class. A NUMERIC column holds an integer, a real OR a
    /// string depending on the value; a BLOB-affinity column converts NOTHING
    /// and holds whatever class was handed to it. `UPDATE t SET b='aaaa'` on a
    /// `b blob` column stores the four bytes as **TEXT** in sqlite —
    /// `typeof(b)` really is `'text'` — so a rigid `Blob` column is not a
    /// narrower sqlite, it is a different one (task #113).
    ///
    /// INTEGER/REAL/TEXT stay RIGID, which is narrower than sqlite: the
    /// column's affinity is APPLIED on the way in (`'12'` → `12` in an
    /// `int` column, `55` → `'55'` in a `varchar`, exactly sqlite), and a value
    /// the conversion cannot land inside the rigid type is REFUSED where sqlite
    /// would have kept the original class. That is a clean refusal, never a
    /// different answer. See [`crate::schema::ColumnDef::converts_on_store`].
    ///
    /// The two carve-outs, both names sqlite's rule sends to the default
    /// NUMERIC and mpedb spells for a type sqlite cannot express:
    /// * `bool`/`boolean` → [`ColumnType::Bool`]. Every consumer that writes
    ///   one writes `sqlite3_bind_int` 0/1, which the int/bool bridge converts
    ///   exactly, so the rigid type receives the values it is handed.
    /// * `any` → [`ColumnType::Any`] with BLOB affinity: mpedb's own escape
    ///   hatch has always meant *verbatim*, and NUMERIC would convert.
    ///
    /// `timestamp` used to be a third carve-out and is NOT one any more: SQL has
    /// no timestamp literal and the C API has no `sqlite3_bind_timestamp`, so a
    /// rigid `Timestamp` column created by `CREATE TABLE` was a column no SQL
    /// value could be written to — every consumer (CPython's `PARSE_DECLTYPES`
    /// above all) sends an ISO *string*, which sqlite stores. A config-declared
    /// `type = "timestamp"` is untouched: it goes through [`ColumnType::parse`].
    ///
    /// An EMPTY name is `Any`, not the `Numeric` this rule returns for a `CAST`:
    /// a column with NO declared type is sqlite's no-affinity column, which
    /// converts nothing — exactly `Any` + BLOB affinity.
    ///
    /// **This function alone loses the NUMERIC/no-conversion distinction** —
    /// `decimal(10,2)`, `blob` and no type at all all land on `Any`, and sqlite
    /// treats them OPPOSITELY at store time. Use [`ColumnType::declared`], which
    /// returns the [`Affinity`] alongside.
    pub fn from_declared(name: &str) -> ColumnType {
        ColumnType::declared(name).0
    }

    /// A **declared SQL type name** → the pair a column definition needs: the
    /// rigid storage type ([`ColumnType::from_declared`]) AND the sqlite
    /// [`Affinity`] that decides what happens to a value on its way IN.
    ///
    /// The two are genuinely independent, and collapsing them is a wrong
    /// answer, not a rounding error. `ColumnType::Any` is the storage side of
    /// TWO sqlite affinities that behave OPPOSITELY:
    ///
    /// | declared            | affinity  | `'1.50'` stores as | mpedb column   |
    /// |---------------------|-----------|--------------------|----------------|
    /// | `decimal(10,2)`, `numeric`, `date`, `datetime` | NUMERIC | `1.5` real | `Any` + `Numeric` |
    /// | *(nothing at all)*  | BLOB      | `'1.50'` text      | `Any` + `Blob` |
    ///
    /// so `Any` alone cannot say which one a column is. The affinity says.
    ///
    /// The rule:
    /// 1. an EMPTY name is sqlite's no-affinity column → (`Any`, `Blob`);
    /// 2. the two carve-outs named in [`ColumnType::from_declared`] —
    ///    `bool`/`boolean` and `any`;
    /// 3. otherwise sqlite's affinity of the name, mapped to the storage type
    ///    that can hold what that affinity produces.
    ///
    /// For the three RIGID affinities (INTEGER/REAL/TEXT) the affinity recorded
    /// is the real sqlite one, and it IS applied on the way in — the conversion
    /// runs first and the rigid type then refuses whatever it could not land
    /// inside. `Affinity::implied_by` agrees with sqlite's rule on all three by
    /// construction, so the field never contradicts `ty`.
    pub fn declared(name: &str) -> (ColumnType, Affinity) {
        let lower = name.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return (ColumnType::Any, Affinity::Blob);
        }
        // The two carve-outs (see `from_declared`): a name whose sqlite
        // affinity is the default NUMERIC and which mpedb spells for a type
        // sqlite has no way to express.
        match lower.as_str() {
            "bool" | "boolean" => return (ColumnType::Bool, Affinity::implied_by(ColumnType::Bool)),
            "any" => return (ColumnType::Any, Affinity::Blob),
            _ => {}
        }
        match Affinity::declared(&lower) {
            Affinity::Integer => (ColumnType::Int64, Affinity::Integer),
            Affinity::Real => (ColumnType::Float64, Affinity::Real),
            Affinity::Text => (ColumnType::Text, Affinity::Text),
            // sqlite's BLOB (historically NONE) affinity converts nothing and
            // holds every class — the typeless column, which is `Any`.
            Affinity::Blob => (ColumnType::Any, Affinity::Blob),
            Affinity::Numeric => (ColumnType::Any, Affinity::Numeric),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ColumnType::Int64 => "int64",
            ColumnType::Float64 => "float64",
            ColumnType::Bool => "bool",
            ColumnType::Text => "text",
            ColumnType::Blob => "blob",
            ColumnType::Timestamp => "timestamp",
            ColumnType::Any => "any",
            ColumnType::Date => "date",
            ColumnType::Time => "time",
            ColumnType::Numeric => "numeric",
        }
    }

    /// The canonical SQL type name reported as a column's `decltype` (the
    /// libsqlite3 `sqlite3_column_decltype` / Python `cursor.description[*][1]`).
    /// `None` for [`ColumnType::Any`] — a typeless column has no declared type,
    /// exactly as sqlite reports `NULL` for one. mpedb stores the rigid type, not
    /// the original DDL text, so this is the canonical spelling of that type.
    pub fn decltype_name(self) -> Option<&'static str> {
        Some(match self {
            ColumnType::Int64 => "INTEGER",
            ColumnType::Float64 => "REAL",
            ColumnType::Bool => "BOOLEAN",
            ColumnType::Text => "TEXT",
            ColumnType::Blob => "BLOB",
            ColumnType::Timestamp => "TIMESTAMP",
            ColumnType::Date => "DATE",
            ColumnType::Time => "TIME",
            ColumnType::Numeric => "NUMERIC",
            ColumnType::Any => return None,
        })
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// sqlite's five type *affinities* — the target of `CAST(x AS <type>)`.
///
/// Unlike a [`ColumnType`], an affinity is not a storage type: it is a
/// conversion *rule*. sqlite accepts ANY type name in a cast and folds it to
/// one of these five by a substring scan of the name (`from_type_name`), then
/// converts the value permissively (leading-numeric-prefix parses, truncation,
/// text rendering) rather than rejecting. mpedb matches that behaviour so the
/// sqllogictest corpus' `CAST(.. AS SIGNED/DECIMAL/VARCHAR/…)` no longer errors
/// on an unknown target name.
///
/// The mapping onto mpedb's typed [`Value`]s (applied by `cast_value`):
/// `Integer`→`Int`, `Real`→`Float`, `Text`→`Text`, `Blob`→`Blob`, and
/// `Numeric`→`Int` when the result is integral (else `Float`) — exactly sqlite's
/// `NUMERIC` affinity, whose runtime type is decided per value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Affinity {
    Integer = 1,
    Real = 2,
    Text = 3,
    Blob = 4,
    Numeric = 5,
}

impl Affinity {
    pub fn from_tag(tag: u8) -> Option<Affinity> {
        Some(match tag {
            1 => Affinity::Integer,
            2 => Affinity::Real,
            3 => Affinity::Text,
            4 => Affinity::Blob,
            5 => Affinity::Numeric,
            _ => return None,
        })
    }

    /// The SQL type name → affinity rule, a faithful port of sqlite's
    /// `sqlite3AffinityType`: scan the name left-to-right keeping a rolling
    /// 32-bit window; the default is `Numeric`. `INT` (once seen, anywhere)
    /// wins and terminates; `CHAR`/`CLOB`/`TEXT` force `Text`; `BLOB` forces
    /// `Blob` only while still `Numeric`/`Real`; `REAL`/`FLOA`/`DOUB` force
    /// `Real` only while still `Numeric`. An empty name stays `Numeric` (as in
    /// sqlite — NOT `Blob`). Multi-word names (`DOUBLE PRECISION`,
    /// `UNSIGNED BIG INT`) scan as their joined text.
    pub fn from_type_name(name: &str) -> Affinity {
        let mut aff = Affinity::Numeric;
        let mut h: u32 = 0;
        const CHAR: u32 = u32::from_be_bytes(*b"char");
        const CLOB: u32 = u32::from_be_bytes(*b"clob");
        const TEXT: u32 = u32::from_be_bytes(*b"text");
        const BLOB: u32 = u32::from_be_bytes(*b"blob");
        const REAL: u32 = u32::from_be_bytes(*b"real");
        const FLOA: u32 = u32::from_be_bytes(*b"floa");
        const DOUB: u32 = u32::from_be_bytes(*b"doub");
        const INT: u32 = 0x0069_6e74; // "int" in the low 24 bits
        for &b in name.as_bytes() {
            h = (h << 8).wrapping_add(b.to_ascii_lowercase() as u32);
            if h == CHAR || h == CLOB || h == TEXT {
                aff = Affinity::Text;
            } else if h == BLOB && matches!(aff, Affinity::Numeric | Affinity::Real) {
                aff = Affinity::Blob;
            } else if (h == REAL || h == FLOA || h == DOUB) && aff == Affinity::Numeric {
                aff = Affinity::Real;
            } else if (h & 0x00ff_ffff) == INT {
                return Affinity::Integer;
            }
        }
        aff
    }

    /// The affinity a mpedb [`ColumnType`] already enforces on its own.
    ///
    /// A rigid column refuses a value of the wrong class instead of converting
    /// it, so its affinity is descriptive: it is whatever class the column can
    /// actually hold. `Any` maps to `Blob` — sqlite's BLOB (historically
    /// "NONE") affinity, the one that converts nothing — because that is what
    /// `Any` does by default; the NUMERIC-affinity `Any` column is the one case
    /// this function cannot produce, and it comes from the DECLARED NAME via
    /// [`ColumnType::declared`], never from the storage type.
    pub fn implied_by(ty: ColumnType) -> Affinity {
        match ty {
            // `Date`/`Time` are i64 scalars like `Timestamp`, so a value that
            // reaches them is an integer.
            ColumnType::Int64
            | ColumnType::Bool
            | ColumnType::Timestamp
            | ColumnType::Date
            | ColumnType::Time => Affinity::Integer,
            ColumnType::Float64 => Affinity::Real,
            // `Numeric` stores canonical TEXT, and its affinity has to say so:
            // this is the affinity a value is COERCED to on the way in, and
            // coercing a decimal to a float is the exactness loss the type
            // exists to prevent.
            ColumnType::Text | ColumnType::Numeric => Affinity::Text,
            ColumnType::Blob | ColumnType::Any => Affinity::Blob,
        }
    }

    /// The affinity of a **column's declared type name** — `from_type_name`
    /// plus the one case it does not cover.
    ///
    /// An EMPTY name is a column with NO declared type, which sqlite gives BLOB
    /// (historically "NONE") affinity: it converts nothing. That is NOT what
    /// [`Affinity::from_type_name`] returns for an empty string — a `CAST` to
    /// an empty type name is NUMERIC — and the two are exact opposites at store
    /// time, so the distinction has to be made here rather than at each caller.
    pub fn declared(name: &str) -> Affinity {
        if name.trim().is_empty() {
            Affinity::Blob
        } else {
            Affinity::from_type_name(name)
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Affinity::Integer => "INTEGER",
            Affinity::Real => "REAL",
            Affinity::Text => "TEXT",
            Affinity::Blob => "BLOB",
            Affinity::Numeric => "NUMERIC",
        }
    }
}

impl fmt::Display for Affinity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A single SQL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Blob(Vec<u8>),
    /// Microseconds since the Unix epoch, UTC.
    Timestamp(i64),
    /// Days since the Unix epoch, UTC — see [`ColumnType::Date`].
    Date(i64),
    /// Microseconds since midnight — see [`ColumnType::Time`].
    Time(i64),
    /// An exact decimal in canonical text form — see [`ColumnType::Numeric`].
    Numeric(String),
    /// **A session-context list — a parameter value only, never a stored one**
    /// (design/DESIGN-MULTIDB.md §2.6). It exists so `col IN (current_setting('k'))`
    /// can bind a variable-length membership set to ONE reserved slot: the
    /// arity lives in the data, not the plan bytes, so the plan hash stays
    /// context-independent and one plan still serves every session (§4.1).
    ///
    /// There is deliberately no `ColumnType::List`: a list has no column to be
    /// stored in, no key encoding, and no ordering. Every path that would need
    /// one rejects it — `column_type()` returns `None`-like behaviour via
    /// `fits`, `sql_cmp` refuses it, and the row/key codecs error rather than
    /// inventing a representation. The ONLY thing it supports is membership.
    List(Vec<Value>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The column type this value stores into, or `None` for NULL.
    pub fn column_type(&self) -> Option<ColumnType> {
        Some(match self {
            Value::Null => return None,
            Value::Int(_) => ColumnType::Int64,
            Value::Float(_) => ColumnType::Float64,
            Value::Bool(_) => ColumnType::Bool,
            Value::Text(_) => ColumnType::Text,
            Value::Blob(_) => ColumnType::Blob,
            Value::Timestamp(_) => ColumnType::Timestamp,
            Value::Date(_) => ColumnType::Date,
            Value::Time(_) => ColumnType::Time,
            Value::Numeric(_) => ColumnType::Numeric,
            // A list is not storable, so it has no column type. `fits` uses this
            // to reject it from every column, which is what we want: the only
            // legal home for a List is a context param slot.
            Value::List(_) => return None,
        })
    }

    /// Whether this value may be stored in a column of type `ty`
    /// (NULL is accepted here; nullability is checked separately).
    /// Whether this value may be stored in a `ty` column.
    ///
    /// NULL fits anything (nullability is checked separately), and `Any` accepts
    /// anything — that is what it is for. Everything else must match exactly:
    /// mpedb does not convert, because a conversion that succeeds locally and
    /// fails in production is the whole problem this project is aimed at.
    pub fn fits(&self, ty: ColumnType) -> bool {
        if ty == ColumnType::Any {
            // ...except a context list, which is param-only (DESIGN-MULTIDB
            // §2.6) and has no encoding in any column, loose or not.
            return !matches!(self, Value::List(_));
        }
        match self.column_type() {
            None => true,
            Some(t) => t == ty,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self.column_type() {
            None => "null",
            Some(t) => t.name(),
        }
    }

    /// SQL comparison. Returns `None` if either side is NULL (three-valued
    /// logic); errors on cross-type comparison (the binder inserts explicit
    /// coercions, so a runtime mix is a bug or a corrupt plan blob).
    ///
    /// Text and blob compare bytewise (binary collation). Floats use IEEE
    /// total order with -0.0 == 0.0 and all NaNs equal, matching the key
    /// encoding in [`crate::keycode`].
    pub fn sql_cmp(&self, other: &Value) -> Result<Option<Ordering>> {
        use Value::*;
        Ok(Some(match (self, other) {
            (Null, _) | (_, Null) => return Ok(None),
            (Int(a), Int(b)) => a.cmp(b),
            (Float(a), Float(b)) => float_total_cmp(*a, *b),
            // An INTEGER against a REAL — the one CROSS-class comparison mpedb
            // answers, because it is the one that cannot depend on affinity.
            // Affinity never turns a number into something other than a number,
            // so `1000 < 1.5` has sqlite's answer no matter what column either
            // side came from; number-against-TEXT does depend on it, and stays
            // a refusal until comparison affinity lands. This case is ordinary
            // now rather than exotic: a NUMERIC-affinity column stores '1000' as
            // an integer and '1.50' as a real, so ORDER BY, MAX and DISTINCT
            // over one meet both classes in the same column.
            (Int(a), Float(b)) => int_float_cmp(*a, *b),
            (Float(a), Int(b)) => int_float_cmp(*b, *a).reverse(),
            (Bool(a), Bool(b)) => a.cmp(b),
            (Text(a), Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Blob(a), Blob(b)) => a.cmp(b),
            (Timestamp(a), Timestamp(b)) => a.cmp(b),
            (Date(a), Date(b)) => a.cmp(b),
            (Time(a), Time(b)) => a.cmp(b),
            // By VALUE, never by spelling: `1.0` and `1.00` are one number,
            // which is PostgreSQL's own rule for the type.
            (Numeric(a), Numeric(b)) => numeric_cmp(a, b),
            // Lists have no ordering and comparing one is always a bug in the
            // caller, not a NULL: say so rather than silently yielding NULL,
            // which in a policy predicate would read as "row not visible" and
            // hide the mistake.
            (List(_), _) | (_, List(_)) => {
                return Err(Error::TypeMismatch(
                    "a context list supports only `IN` membership, not comparison".into(),
                ))
            }
            (a, b) => {
                return Err(Error::TypeMismatch(format!(
                    "cannot compare {} with {}",
                    a.type_name(),
                    b.type_name()
                )))
            }
        }))
    }

    /// sqlite's **storage-class total order**, for contexts that compare stored
    /// values AGAINST EACH OTHER: `ORDER BY`, `MIN`/`MAX`, `GROUP BY`,
    /// `DISTINCT`, window ordering.
    ///
    /// `NULL < numbers < TEXT < BLOB`, integers and reals interleaved
    /// numerically, text bytewise (or by `coll`), blobs bytewise. NULL is
    /// returned as `None` so each caller keeps its own NULLS FIRST/LAST rule.
    ///
    /// Distinct from [`Value::sql_cmp`], which REFUSES a cross-class pair, and
    /// the split is the point. A `WHERE` comparison puts a stored value against
    /// an operand whose class sqlite would have converted first (comparison
    /// affinity: `price < '40.0'` compares 40.0, not the string), so answering
    /// it by class order alone would be a WRONG ANSWER — `sql_cmp` keeps
    /// refusing until that conversion exists. A sort compares stored values
    /// only: affinity was already applied on the way IN, nothing is left to
    /// convert, and the class order IS sqlite's answer.
    ///
    /// Without this, an `any` column holding both `'abc'` and `10` sorted
    /// arbitrarily: `sql_cmp` refused the pair and every sort comparator turned
    /// the refusal into `Equal`, which is not an order at all.
    pub fn sort_cmp(&self, other: &Value, coll: Collation) -> Option<Ordering> {
        use Value::*;
        if self.is_null() || other.is_null() {
            return None;
        }
        match (self, other) {
            (Text(a), Text(b)) => Some(coll.compare_str(a, b)),
            _ => match self.sql_cmp_collated(other, coll) {
                Ok(o) => o,
                // A cross-class pair `sql_cmp` refuses: order it by sqlite's
                // storage classes. mpedb's own Bool/Timestamp have no sqlite
                // class, so a pair involving one stays incomparable (`None`,
                // read by every caller as "peers") rather than being given an
                // invented rank.
                Err(_) => match (class_rank(self), class_rank(other)) {
                    (Some(x), Some(y)) => Some(x.cmp(&y)),
                    _ => None,
                },
            },
        }
    }

    /// SQL comparison under an explicit collating sequence (task: COLLATE).
    ///
    /// Collation affects TEXT–TEXT comparison ONLY (sqlite's rule): every other
    /// type — and any NULL — falls straight through to [`Value::sql_cmp`], so a
    /// numeric or blob comparison is never perturbed by a stray `COLLATE`. For
    /// two texts the bytes are ordered by `coll`. [`Collation::Binary`] is
    /// byte-identical to `sql_cmp`, so a Binary-tagged comparison and an
    /// untagged one can never disagree.
    pub fn sql_cmp_collated(&self, other: &Value, coll: Collation) -> Result<Option<Ordering>> {
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => Ok(Some(coll.compare_str(a, b))),
            _ => self.sql_cmp(other),
        }
    }
}

/// A collating sequence: how two TEXT values are ordered for comparison and
/// sorting. mpedb ships sqlite's three built-ins and nothing else; the tag is
/// carried in plan bytes (comparison [`Instr`](crate::Instr)s and ORDER BY
/// keys), so it is a closed enum with a stable wire tag like
/// [`ColumnType`]/`ScalarFn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Collation {
    /// Compare by memcmp of the raw UTF-8 bytes — mpedb's native order and the
    /// keycode order. The default when no `COLLATE` is in force.
    #[default]
    Binary = 0,
    /// Case-insensitive, but ONLY for the 26 ASCII letters (sqlite does NOT
    /// casefold Unicode): each byte in `A'..='Z'` is folded to lowercase before
    /// comparison, everything else compared as-is.
    NoCase = 1,
    /// Like [`Collation::Binary`] but trailing ASCII spaces (`0x20`) are ignored
    /// on both sides: `'abc'` == `'abc   '`.
    Rtrim = 2,
}

impl Collation {
    /// Decode a wire tag; `None` (→ `Corrupt`) for an unknown byte.
    pub fn from_tag(t: u8) -> Option<Collation> {
        Some(match t {
            0 => Collation::Binary,
            1 => Collation::NoCase,
            2 => Collation::Rtrim,
            _ => return None,
        })
    }

    /// The SQL name, as written after `COLLATE` and rendered by EXPLAIN.
    pub fn name(self) -> &'static str {
        match self {
            Collation::Binary => "BINARY",
            Collation::NoCase => "NOCASE",
            Collation::Rtrim => "RTRIM",
        }
    }

    /// Resolve a collation name (case-insensitive), or `None` if unknown.
    pub fn parse(name: &str) -> Option<Collation> {
        if name.eq_ignore_ascii_case("BINARY") {
            Some(Collation::Binary)
        } else if name.eq_ignore_ascii_case("NOCASE") {
            Some(Collation::NoCase)
        } else if name.eq_ignore_ascii_case("RTRIM") {
            Some(Collation::Rtrim)
        } else {
            None
        }
    }

    /// Order two strings under this collation. `Binary` is exactly
    /// `a.as_bytes().cmp(b.as_bytes())`.
    pub fn compare_str(self, a: &str, b: &str) -> Ordering {
        match self {
            Collation::Binary => a.as_bytes().cmp(b.as_bytes()),
            Collation::NoCase => nocase_cmp(a.as_bytes(), b.as_bytes()),
            Collation::Rtrim => a
                .trim_end_matches(' ')
                .as_bytes()
                .cmp(b.trim_end_matches(' ').as_bytes()),
        }
    }

    /// The CANONICAL fold of a text value under this collation: two strings are
    /// equal under the collation iff their folds are byte-identical. This is the
    /// equality half of [`compare_str`] made into a normal form, so a
    /// collation-aware GROUP BY / DISTINCT key can be built by folding then
    /// encoding bytewise (`keycode::encode_key_collated`).
    ///
    /// `Binary` is the identity. `NoCase` ASCII-lowercases (matching sqlite: only
    /// A–Z fold, never Unicode); the fold is byte-length-preserving, so
    /// folded-equal implies `nocase_cmp`-equal (which tie-breaks on length).
    /// `Rtrim` drops trailing ASCII spaces.
    pub fn fold_key(self, s: &str) -> std::borrow::Cow<'_, str> {
        use std::borrow::Cow;
        match self {
            Collation::Binary => Cow::Borrowed(s),
            Collation::NoCase => Cow::Owned(s.to_ascii_lowercase()),
            Collation::Rtrim => Cow::Borrowed(s.trim_end_matches(' ')),
        }
    }
}

/// The collating sequence an `ORDER BY` key is compared under: one of the three
/// BUILT-IN [`Collation`]s, or a HOST collation registered by the connection
/// through `sqlite3_create_collation` (design/DESIGN-UDF.md stage 3).
///
/// It is a separate type from [`Collation`], and deliberately reaches only ONE
/// place in the plan — the `ORDER BY` key list. That is the honest scope, and
/// the type system is what enforces it:
///
/// * A host collation is a **comparator**, not an encoding. Every other user of
///   `Collation` in this crate turns a value into KEY BYTES — the B+tree key
///   encoding, the GROUP BY / DISTINCT fold ([`Collation::fold_key`]), a
///   column's declared collation in the schema. A callback cannot produce a
///   sort key, so a host collation cannot appear there, and because those APIs
///   still take `Collation`, one cannot be constructed for them at all.
/// * An index built under BINARY cannot answer a host-collated range, so the
///   planner's ORDER-BY-satisfied-by-scan-order elision requires
///   `Native(Binary)` and a host collation blocks it by construction.
///
/// What that leaves is exactly what sqlite's own `create_collation` tests ask
/// for: `ORDER BY <expr> COLLATE <name>` sorted through the callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderColl {
    Native(Collation),
    /// A host collation, carried BY NAME (not by registry index): the plan is
    /// self-describing, so two connections cannot disagree about which callback
    /// slot 3 is. Such a plan never enters the shared registry — the same
    /// one-connection-only rule host functions and aggregates already have.
    Host(String),
}

impl Default for OrderColl {
    fn default() -> Self {
        OrderColl::Native(Collation::Binary)
    }
}

impl OrderColl {
    /// The host collation's name, or `None` for a built-in.
    pub fn host(&self) -> Option<&str> {
        match self {
            OrderColl::Native(_) => None,
            OrderColl::Host(n) => Some(n.as_str()),
        }
    }

    /// The built-in this key sorts under, or `None` for a host collation.
    pub fn native(&self) -> Option<Collation> {
        match self {
            OrderColl::Native(c) => Some(*c),
            OrderColl::Host(_) => None,
        }
    }

    /// The name as written after `COLLATE`, for EXPLAIN and error messages.
    pub fn name(&self) -> &str {
        match self {
            OrderColl::Native(c) => c.name(),
            OrderColl::Host(n) => n.as_str(),
        }
    }
}

/// Resolve a HOST collation's comparator at execution time, mirroring
/// [`HostFns`](crate::HostFns) for scalars and [`HostAggs`](crate::HostAggs) for
/// aggregates. `None` is threaded wherever no host collation can be in scope, so
/// the mechanism stays inert for every plan that names none.
pub trait HostColls {
    /// Is a collating sequence registered under `name`? Checked ONCE, before a
    /// sort starts, so the comparator itself cannot fail mid-`sort_by` (where
    /// there is nowhere to report an error and "peers" would silently return
    /// rows in the wrong order). An unregistered name is sqlite's
    /// "no such collation sequence: <name>".
    fn has(&self, name: &str) -> bool;

    /// Order two TEXT values under `name`. Text only: sqlite applies a
    /// collating sequence to a text-vs-text comparison and settles every other
    /// pair by storage class first, which is what [`Value::sort_cmp`] already
    /// does. Only ever called for a name [`has`](Self::has) accepted.
    fn compare(&self, name: &str, a: &str, b: &str) -> Ordering;
}

/// sqlite NOCASE: fold each ASCII uppercase byte to lowercase and compare the
/// folded byte streams, breaking a tie on length. Bytes outside `A'..='Z'`
/// (including all non-ASCII UTF-8 continuation bytes) are compared unchanged —
/// which is exactly why NOCASE does not casefold Unicode.
fn nocase_cmp(a: &[u8], b: &[u8]) -> Ordering {
    #[inline]
    fn fold(x: u8) -> u8 {
        if x.is_ascii_uppercase() {
            x + 32
        } else {
            x
        }
    }
    let n = a.len().min(b.len());
    for i in 0..n {
        let c = fold(a[i]).cmp(&fold(b[i]));
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// Total order over f64 matching the memcmp key encoding: -0.0 and 0.0 are
/// equal, all NaNs are equal and sort above +inf.
/// sqlite's storage-class rank: NULL (not ranked here) < numbers < text < blob.
/// `None` for mpedb's own `Bool`/`Timestamp` and for a context `List`, none of
/// which is a sqlite storage class to rank against one.
fn class_rank(v: &Value) -> Option<u8> {
    Some(match v {
        Value::Int(_) | Value::Float(_) => 1,
        Value::Text(_) => 2,
        Value::Blob(_) => 3,
        Value::Null
        // A NUMERIC does NOT rank with the numbers, tempting as that is. This
        // function is only ever reached for a pair `sql_cmp` refused, and a
        // rank makes every such pair COMPARE — so ranking it 1 would make
        // `Numeric("2")` and `Int(1)` equal, silently, because the rank is all
        // that would be looked at. It is mpedb-native like Bool/Timestamp: no
        // sqlite class to rank against, so a mixed pair stays peers until an
        // exact decimal↔binary comparison exists to answer it properly.
        | Value::Numeric(_)
        | Value::Bool(_)
        | Value::Timestamp(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::List(_) => return None,
    })
}

/// An `i64` against an `f64`, EXACTLY — a port of sqlite's
/// `sqlite3IntFloatCompare`.
///
/// Casting either side to the other's type is wrong at the edges: `i as f64`
/// rounds every magnitude past 2^53, so `9007199254740993 < 9007199254740992.0`
/// would compare equal, and `r as i64` truncates. So: reject the out-of-range
/// reals first, compare against the truncated integer, and only then break a
/// tie by widening the integer — which is exact whenever the truncation was.
///
/// NaN sorts ABOVE every integer. That is where [`float_total_cmp`] already
/// puts it (the canonicalized NaN image is the largest), and a total order
/// matters more here than agreeing with sqlite's `i > NaN`: sqlite cannot store
/// a NaN at all — it turns into NULL on the way in — so no differential can
/// observe the difference, while an order that is not total would break every
/// sort that meets one.
fn int_float_cmp(i: i64, r: f64) -> Ordering {
    if r.is_nan() {
        return Ordering::Less;
    }
    if r < -9223372036854775808.0 {
        return Ordering::Greater;
    }
    if r >= 9223372036854775808.0 {
        return Ordering::Less;
    }
    let y = r as i64; // in range by the guards above; truncates toward zero
    match i.cmp(&y) {
        Ordering::Equal => {}
        other => return other,
    }
    // Equal after truncation: the fractional part decides. `i as f64` is exact
    // here because `i == r.trunc() as i64` and that round-tripped.
    (i as f64).partial_cmp(&r).unwrap_or(Ordering::Equal)
}

pub fn float_total_cmp(a: f64, b: f64) -> Ordering {
    normalize_float_bits(a).cmp(&normalize_float_bits(b))
}

/// `n` as an `f64` when the conversion is EXACT, else `None`.
///
/// The obvious `(n as f64) as i64 == n` check is WRONG: the `f64 -> i64` cast
/// saturates in Rust, so `i64::MAX` — whose nearest `f64` is 2^63, one past the
/// range — would round-trip to `i64::MAX` and read as exact. Going out through
/// `i128`, which holds every `f64` in this neighbourhood without saturating,
/// gives the honest answer.
///
/// This is the losslessness test for the int↔float parameter bridge (#74):
/// sqlite compares an integer against a real EXACTLY
/// (`sqlite3IntFloatCompare`), so rounding an operand before the comparison
/// could flip a `>` on a large key.
pub fn exact_int_as_float(n: i64) -> Option<f64> {
    let f = n as f64;
    (f as i128 == n as i128).then_some(f)
}

/// `f` as an `i64` when the conversion is EXACT — finite, integral, and inside
/// the i64 range — else `None`. NaN and the infinities fail the `fract` test.
///
/// This is sqlite's `applyNumericAffinity` rule: an INTEGER-affinity column
/// stores a real as an integer only when the round trip loses nothing.
pub fn exact_float_as_int(f: f64) -> Option<i64> {
    if !f.is_finite() || f.fract() != 0.0 {
        return None;
    }
    let i = f as i128;
    (i >= i64::MIN as i128 && i <= i64::MAX as i128).then_some(i as i64)
}

/// The canonical text of a decimal: the spelling two EQUAL numerics share.
///
/// PostgreSQL's `numeric` compares by VALUE, so `1.0` and `1.00` are one
/// value and must group as one and compare as equal. mpedb stores the text a
/// client wrote, so equality needs a spelling that depends only on the value:
/// no leading `+`, no insignificant zeros on either end, no `-0`, and `.`
/// dropped when nothing follows it.
///
/// Deliberately textual and total: anything this cannot recognise as a decimal
/// is returned trimmed and unchanged, so a value that arrived from somewhere
/// unexpected keys as itself rather than as a silently different number.
pub fn canonical_numeric(s: &str) -> String {
    let t = s.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    // Exponent forms are left alone: normalising `1e3` to `1000` is a decision
    // about how many digits a value HAS, and getting it wrong here would move
    // rows between groups.
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return t.to_string();
    }
    let (int_part, frac_part) = match digits.split_once('.') {
        Some((i, f)) => (i, f),
        None => (digits, ""),
    };
    let int_trimmed = int_part.trim_start_matches('0');
    let frac_trimmed = frac_part.trim_end_matches('0');
    let int_final = if int_trimmed.is_empty() { "0" } else { int_trimmed };
    let mut out = String::with_capacity(t.len());
    // `-0` and `-0.000` are zero, and zero has no sign.
    if neg && !(int_final == "0" && frac_trimmed.is_empty()) {
        out.push('-');
    }
    out.push_str(int_final);
    if !frac_trimmed.is_empty() {
        out.push('.');
        out.push_str(frac_trimmed);
    }
    out
}

/// The ELEMENTS of a value used as an array by a set-returning function.
///
/// Two shapes are accepted, and the second is the reason this function exists:
///
/// * a [`Value::List`] — an array that a query PRODUCED (`array_agg`), and
/// * PostgreSQL's EXTERNAL TEXT form — `{1,2,3}` for an array literal and
///   space-separated for an `int2vector`/`oidvector`. mpedb's catalog stores
///   `pg_index.indkey` in that form because the catalog is materialized into a
///   real table and a list has no row encoding, and `unnest(indkey)` is how
///   every ORM reads a composite index's column list.
///
/// `None` means "not an array", which every caller turns into a NAMED refusal.
/// A text that is not a vector literal is refused rather than treated as a
/// one-element set: PostgreSQL cannot resolve `unnest('hello')` either, and
/// silently answering one row would be a wrong answer, not a kindness.
///
/// NULL is the EMPTY set (`Some(vec![])`), matching `unnest(NULL)`.
pub fn array_elements(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::List(items) => Some(items.clone()),
        Value::Null => Some(Vec::new()),
        Value::Text(s) => {
            let t = s.trim();
            // `{}` is the empty array; `{a,b}` is braced-and-comma-separated;
            // anything else must be the space-separated vector form.
            let (body, comma) = match t.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
                Some(inner) => (inner.trim(), true),
                None => (t, false),
            };
            if body.is_empty() {
                return Some(Vec::new());
            }
            let parts: Vec<&str> = if comma {
                body.split(',').map(str::trim).collect()
            } else {
                body.split_whitespace().collect()
            };
            // Every element must be an integer. That is what the two vector
            // types hold, and it is the check that keeps an ordinary sentence
            // from parsing as a one-element array.
            let mut out = Vec::with_capacity(parts.len());
            for p in parts {
                out.push(Value::Int(p.parse::<i64>().ok()?));
            }
            Some(out)
        }
        _ => None,
    }
}

/// PostgreSQL's own limits on a `numeric`: at most this many digits before the
/// decimal point, and this many after it. A literal outside them is refused by
/// name rather than truncated — and they double as the bound that keeps
/// [`parse_numeric`]'s exponent expansion from allocating without limit.
pub const NUMERIC_MAX_WEIGHT: i64 = 131_072;
pub const NUMERIC_MAX_SCALE: i64 = 16_383;

/// Parse a decimal literal into the text a [`Value::Numeric`] stores, or `None`
/// when it is not one.
///
/// Accepts what PostgreSQL's `numeric` input accepts: an optional sign, digits
/// with an optional decimal point, an optional `e`-exponent, and the three
/// special names (`NaN`, `Infinity`, `-Infinity`, case-insensitively, plus
/// `inf`). An exponent is EXPANDED rather than kept — `1e3` stores as `1000`.
///
/// **The SCALE is kept.** `1.00` stores as `1.00`, not as `1`, because
/// PostgreSQL's `numeric` carries its scale into its output and a client that
/// wrote `Decimal("1.00")` gets `Decimal("1.00")` back. What is normalised is
/// only what carries no information: a leading `+`, leading zeros on the
/// integer part, and the sign of zero.
///
/// That makes this a DIFFERENT function from [`canonical_numeric`], and the
/// split is the design: this one is the STORED form (scale preserved), that one
/// is the KEY and COMPARISON form (scale erased, so `1.0` and `1.00` are one
/// group and one UNIQUE key — PostgreSQL's own equality for the type). Storage
/// is exact; ordering is by value.
///
/// It is also the strict counterpart to `canonical_numeric`, which is total by
/// design and hands back anything it does not recognise unchanged. Use this one
/// wherever a bad literal must become an ERROR (a `CAST`, a bound parameter, an
/// import); use that one wherever a stored value is merely being re-keyed.
pub fn parse_numeric(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    // The specials, by name. `NaN` has no sign in PostgreSQL (`-NaN` is `NaN`).
    if body.eq_ignore_ascii_case("nan") {
        return Some("NaN".to_string());
    }
    if body.eq_ignore_ascii_case("infinity") || body.eq_ignore_ascii_case("inf") {
        return Some(if neg { "-Infinity".into() } else { "Infinity".into() });
    }

    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => {
            let e = &body[i + 1..];
            let e = if let Some(r) = e.strip_prefix('+') { r } else { e };
            // A lone sign, an empty exponent or a non-numeric one is not a
            // number; a HUGE one is caught by the weight check below.
            let neg_e = e.starts_with('-');
            let digits = if neg_e { &e[1..] } else { e };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // Saturating: an exponent that does not fit an i64 cannot fit the
            // weight limit either, so it lands on the same refusal.
            let mag: i64 = digits.parse().unwrap_or(i64::MAX);
            (&body[..i], if neg_e { -mag } else { mag })
        }
        None => (body, 0),
    };

    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    // At least one digit, and nothing but digits — `.`, `1.2.3` and `1_000`
    // are all not numbers.
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().chain(frac_part.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }

    // Shift the point by the exponent: the digits are `int_part ++ frac_part`
    // and the point sits `int_part.len() + exp` places into them.
    let digits: String = format!("{int_part}{frac_part}");
    // Saturating: `1e9999999999999999999` parses its exponent as i64::MAX and
    // must land on the weight refusal below, not on an arithmetic overflow.
    let point = (int_part.len() as i64).saturating_add(exp);
    if point > NUMERIC_MAX_WEIGHT || (digits.len() as i64).saturating_sub(point) > NUMERIC_MAX_SCALE
    {
        return None;
    }
    let plain = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        let (a, b) = digits.split_at(point as usize);
        format!("{a}.{b}")
    };
    // Leading zeros on the integer part carry nothing; trailing zeros on the
    // fraction carry the SCALE, so they stay.
    let (ip, fp) = plain.split_once('.').unwrap_or((plain.as_str(), ""));
    let ip = ip.trim_start_matches('0');
    let ip = if ip.is_empty() { "0" } else { ip };
    let mut out = String::with_capacity(plain.len() + 1);
    // Zero has no sign, whatever its scale: PostgreSQL prints `-0.00` as
    // `0.00`.
    if neg && !(ip == "0" && fp.bytes().all(|b| b == b'0')) {
        out.push('-');
    }
    out.push_str(ip);
    if !fp.is_empty() {
        out.push('.');
        out.push_str(fp);
    }
    Some(out)
}

/// An exact decimal as `(mantissa, scale)` — the value is `mantissa / 10^scale`.
///
/// `None` for a special (`NaN`, `±Infinity`), for anything that is not a plain
/// decimal, and for a value needing more than an i128's 38 digits. Every
/// caller turns that into a NAMED refusal: silently widening to f64 here would
/// throw away exactly the digits this type exists to keep.
pub(crate) fn decimal_parts(s: &str) -> Option<(i128, u32)> {
    let t = s.trim();
    if numeric_special_rank(t).is_some() {
        return None;
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (ip, fp) = body.split_once('.').unwrap_or((body, ""));
    if (ip.is_empty() && fp.is_empty())
        || !ip.bytes().chain(fp.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let m: i128 = format!("{ip}{fp}").parse().ok()?;
    Some((if neg { -m } else { m }, fp.len() as u32))
}

/// The inverse of [`decimal_parts`]: `mantissa / 10^scale` as canonical text.
pub(crate) fn decimal_text(m: i128, scale: u32) -> String {
    if scale == 0 {
        return m.to_string();
    }
    let neg = m < 0;
    // The magnitude as digits, padded so there is something before the point.
    let digits = m.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = if digits.len() <= scale {
        format!("{}{}", "0".repeat(scale + 1 - digits.len()), digits)
    } else {
        digits
    };
    let (ip, fp) = padded.split_at(padded.len() - scale);
    let out = format!("{ip}.{fp}");
    // Zero has no sign, whatever its scale.
    if neg && m != 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Rank of a special: `-Infinity` below every finite value, `Infinity` above,
/// `NaN` above that — PostgreSQL's `numeric` ordering, where NaN is the
/// largest value rather than incomparable.
fn numeric_special_rank(s: &str) -> Option<u8> {
    match s {
        "-Infinity" => Some(0),
        "Infinity" => Some(3),
        "NaN" => Some(4),
        _ => None,
    }
}

/// Compare two decimals BY VALUE, in plain (non-exponent) form.
///
/// Total and allocation-free: insignificant zeros on either end are ignored
/// rather than assumed absent, so `1.0` and `1.00` compare equal even though
/// neither is the canonical spelling. Nothing here trusts its caller to have
/// canonicalised, because a wrong comparison is a wrong answer and the check
/// costs two `trim`s.
pub fn numeric_cmp(a: &str, b: &str) -> Ordering {
    match (numeric_special_rank(a), numeric_special_rank(b)) {
        (Some(x), Some(y)) => return x.cmp(&y),
        // A finite value ranks between -Infinity (0) and Infinity (3).
        (Some(x), None) => return x.cmp(&2),
        (None, Some(y)) => return 2.cmp(&y),
        (None, None) => {}
    }
    // A leading `+` is a spelling, not a sign — `canonical_numeric` drops it,
    // and this must agree or `+5` would compare above `5`.
    let sign = |s: &str| -> (bool, usize) {
        match s.as_bytes().first() {
            Some(b'-') => (true, 1),
            Some(b'+') => (false, 1),
            _ => (false, 0),
        }
    };
    let ((an, ao), (bn, bo)) = (sign(a), sign(b));
    let (ab, bb) = (&a[ao..], &b[bo..]);
    let (ai, af) = split_significant(ab);
    let (bi, bf) = split_significant(bb);
    // Zero has no sign, so `-0` and `0` must not take the sign branch below.
    let (an, bn) = (an && !(ai.is_empty() && af.is_empty()), bn && !(bi.is_empty() && bf.is_empty()));
    if an != bn {
        return if an { Ordering::Less } else { Ordering::Greater };
    }
    // More integer digits IS a bigger number once the leading zeros are gone,
    // and equal counts compare digit by digit. The fraction then compares
    // lexicographically: with trailing zeros gone, the shorter side runs out
    // where the longer still has a nonzero digit to come.
    let mag = ai.len().cmp(&bi.len()).then_with(|| ai.cmp(bi)).then_with(|| af.cmp(bf));
    if an {
        mag.reverse()
    } else {
        mag
    }
}

/// An unsigned decimal's significant digits: `(integer part, fraction)` with
/// the insignificant zeros trimmed off each end.
fn split_significant(s: &str) -> (&str, &str) {
    let (i, f) = s.split_once('.').unwrap_or((s, ""));
    (i.trim_start_matches('0'), f.trim_end_matches('0'))
}

// ---------------------------------------------------------------------------
// Calendar: the ONE date/time text codec
// ---------------------------------------------------------------------------
//
// It lives here, not in the wire crate, because three callers need it — the
// PostgreSQL wire format, the store-time coercion, and `CAST` — and three
// copies of a calendar is three chances to disagree about a leap year.

/// Days from 1970-01-01 for a proleptic-Gregorian date, or `None` when
/// `(y, m, d)` is not one. Howard Hinnant's `days_from_civil`, which is exact
/// over the whole i64 range and needs no table.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// The inverse: `(year, month, day)` for a day count since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        // Proleptic Gregorian, so the rule holds for every year including the
        // ones before the calendar existed — which is what `date` means.
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// `YYYY-MM-DD` → days since the Unix epoch, or `None`.
///
/// Strict on purpose, and this strictness IS the feature: sqlite's CAST rule
/// takes the leading number, so `'2020-01-02'` became `2020`. Nothing here
/// takes a prefix — either the whole string is a date or it is not one.
pub fn parse_date_text(s: &str) -> Option<i64> {
    let t = s.trim();
    let neg = t.starts_with('-');
    let body = if neg { &t[1..] } else { t };
    let mut p = body.split('-');
    let y: i64 = int_field(p.next()?)?;
    let m: i64 = int_field(p.next()?)?;
    let d: i64 = int_field(p.next()?)?;
    if p.next().is_some() {
        return None;
    }
    days_from_civil(if neg { -y } else { y }, m, d)
}

/// `HH:MM[:SS[.ffffff]]` → microseconds since midnight, or `None`. A zone
/// suffix means `timetz`, which mpedb has no type for, so it is refused rather
/// than silently dropped.
pub fn parse_time_text(s: &str) -> Option<i64> {
    let t = s.trim();
    let (hms, frac) = t.split_once('.').unwrap_or((t, ""));
    let mut p = hms.split(':');
    let h: i64 = int_field(p.next()?)?;
    let mi: i64 = int_field(p.next()?)?;
    let sec: i64 = match p.next() {
        Some(x) => int_field(x)?,
        None => 0,
    };
    if p.next().is_some() {
        return None;
    }
    // 24:00:00 is PostgreSQL's end-of-day, and a leap second is 60.
    if h > 24 || mi > 59 || sec > 60 || (h == 24 && (mi != 0 || sec != 0)) {
        return None;
    }
    let mut us = 0i64;
    if !frac.is_empty() {
        if !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut f = frac.to_string();
        f.truncate(6);
        while f.len() < 6 {
            f.push('0');
        }
        us = f.parse().ok()?;
    }
    Some((h * 3600 + mi * 60 + sec) * 1_000_000 + us)
}

/// One all-digits field. `parse::<i64>()` alone would accept `+3` and `-3`,
/// which inside a date would make `2020-+1-02` a date.
fn int_field(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `YYYY-MM-DD[ HH:MM[:SS[.ffffff]]][zone]` → microseconds since the Unix
/// epoch, or `None`. A missing zone reads as UTC, which is what mpedb's
/// `Timestamp` means by construction.
pub fn parse_timestamp_text(s: &str) -> Option<i64> {
    let t = s.trim();
    let neg_year = t.starts_with('-');
    let body = if neg_year { &t[1..] } else { t };
    let (date, rest) = body.split_once(['T', ' ']).unwrap_or((body, ""));
    let mut dp = date.split('-');
    let y: i64 = int_field(dp.next()?)?;
    let mo: i64 = int_field(dp.next()?)?;
    let d: i64 = int_field(dp.next()?)?;
    if dp.next().is_some() {
        return None;
    }
    let days = days_from_civil(if neg_year { -y } else { y }, mo, d)?;

    // Split the zone off before the time, or `+01` would be read as a field.
    let (time, offset_secs) = split_zone(rest)?;
    let us_of_day = if time.is_empty() { 0 } else { parse_time_text(time)? };
    let secs = days
        .checked_mul(86_400)?
        .checked_add(us_of_day.div_euclid(1_000_000))?
        .checked_sub(offset_secs)?;
    secs.checked_mul(1_000_000)?.checked_add(us_of_day.rem_euclid(1_000_000))
}

/// A trailing time zone, as `(time part, offset in seconds EAST of UTC)`.
fn split_zone(rest: &str) -> Option<(&str, i64)> {
    if rest.is_empty() {
        return Some(("", 0));
    }
    if let Some(t) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return Some((t, 0));
    }
    // Look for a sign that is not the first character, so a negative year
    // (already peeled off above) cannot be mistaken for a zone.
    let bytes = rest.as_bytes();
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'+' || bytes[i] == b'-' {
            let (t, z) = rest.split_at(i);
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let z = &z[1..];
            let (zh, zm) = match z.split_once(':') {
                Some((a, b)) => (a, b),
                None if z.len() == 4 => (&z[..2], &z[2..]),
                None => (z, "0"),
            };
            return Some((t, sign * (int_field(zh)? * 3600 + int_field(zm)? * 60)));
        }
    }
    Some((rest, 0))
}

/// A `Timestamp` as PostgreSQL's `timestamptz` text output: UTC, with the
/// `+00` zone mpedb's timestamps always carry.
pub fn timestamp_text(us: i64) -> String {
    let (secs, frac) = (us.div_euclid(1_000_000), us.rem_euclid(1_000_000));
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    let base = format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    );
    if frac == 0 {
        return format!("{base}+00");
    }
    let mut f = format!("{frac:06}");
    while f.ends_with('0') {
        f.pop();
    }
    format!("{base}.{f}+00")
}

/// A `Date` as PostgreSQL's `DateStyle = ISO` prints it.
pub fn date_text(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A `Time` as PostgreSQL prints it: `HH:MM:SS[.ffffff]`, trailing zeros of
/// the fraction trimmed.
pub fn time_text(us: i64) -> String {
    let (secs, frac) = (us.div_euclid(1_000_000), us.rem_euclid(1_000_000));
    let base = format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60);
    if frac == 0 {
        return base;
    }
    let mut f = format!("{frac:06}");
    while f.ends_with('0') {
        f.pop();
    }
    format!("{base}.{f}")
}

/// Order-preserving u64 image of an f64: flips the sign bit for positives and
/// all bits for negatives, after canonicalizing -0.0 and NaN.
pub fn normalize_float_bits(v: f64) -> u64 {
    let v = if v == 0.0 { 0.0 } else { v }; // -0.0 -> 0.0
    let bits = if v.is_nan() { f64::NAN.to_bits() } else { v.to_bits() };
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{v:?}"),
            Value::Bool(v) => f.write_str(if *v { "true" } else { "false" }),
            Value::List(items) => {
                f.write_str("(")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
            Value::Text(v) => write!(f, "'{}'", v.replace('\'', "''")),
            Value::Blob(v) => {
                f.write_str("x'")?;
                for b in v {
                    write!(f, "{b:02x}")?;
                }
                f.write_str("'")
            }
            Value::Timestamp(v) => write!(f, "timestamp({v})"),
            Value::Date(v) => write!(f, "date({v})"),
            Value::Time(v) => write!(f, "time({v})"),
            // Unquoted: a NUMERIC is a NUMBER whose exactness happens to be
            // carried as text, and quoting it would render it as a string.
            Value::Numeric(v) => f.write_str(v),
        }
    }
}

/// Deterministic (non-ordered) serialization of a value, used inside plan
/// blobs and schema canonicalization. Length-prefixed, bounds-checked decode.
pub fn write_value(buf: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => buf.push(0),
        Value::Int(x) => {
            buf.push(1);
            buf.extend_from_slice(&x.to_le_bytes());
        }
        Value::Float(x) => {
            buf.push(2);
            buf.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        Value::Bool(x) => {
            buf.push(3);
            buf.push(*x as u8);
        }
        Value::Text(s) => {
            buf.push(4);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Blob(b) => {
            buf.push(5);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Timestamp(x) => {
            buf.push(6);
            buf.extend_from_slice(&x.to_le_bytes());
        }
        // 7 is the context list; the three below continue from 8, matching
        // their `ColumnType` discriminants so the two tag spaces stay readable
        // side by side.
        Value::Date(x) => {
            buf.push(8);
            buf.extend_from_slice(&x.to_le_bytes());
        }
        Value::Time(x) => {
            buf.push(9);
            buf.extend_from_slice(&x.to_le_bytes());
        }
        Value::Numeric(x) => {
            buf.push(10);
            buf.extend_from_slice(&(x.len() as u32).to_le_bytes());
            buf.extend_from_slice(x.as_bytes());
        }
        // A context list DOES have to serialize: the intent ring encodes params
        // with this function (ring_exec::encode_params) and context values are
        // params, so without this `col IN (current_setting(..))` would work
        // alone and break the moment a second writer contended. Nested lists are
        // impossible by construction (Session::set_list takes scalars), but the
        // encoding is recursive anyway so a decoder can never be surprised.
        Value::List(items) => {
            buf.push(7);
            buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for it in items {
                write_value(buf, it);
            }
        }
    }
}

/// Decode a value written by [`write_value`], advancing `*pos`. All reads are
/// bounds-checked so corrupt/hostile input yields `Error::Corrupt`, never a
/// panic or out-of-bounds access.
pub fn read_value(buf: &[u8], pos: &mut usize) -> Result<Value> {
    fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
        let end = pos
            .checked_add(n)
            .filter(|&e| e <= buf.len())
            .ok_or_else(|| Error::Corrupt("truncated value".into()))?;
        let s = &buf[*pos..end];
        *pos = end;
        Ok(s)
    }
    let tag = take(buf, pos, 1)?[0];
    Ok(match tag {
        0 => Value::Null,
        1 => Value::Int(i64::from_le_bytes(take(buf, pos, 8)?.try_into().unwrap())),
        2 => Value::Float(f64::from_bits(u64::from_le_bytes(
            take(buf, pos, 8)?.try_into().unwrap(),
        ))),
        3 => Value::Bool(match take(buf, pos, 1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(Error::Corrupt("invalid bool".into())),
        }),
        4 => {
            let len = u32::from_le_bytes(take(buf, pos, 4)?.try_into().unwrap()) as usize;
            let bytes = take(buf, pos, len)?;
            Value::Text(
                std::str::from_utf8(bytes)
                    .map_err(|_| Error::Corrupt("invalid utf-8 in text value".into()))?
                    .to_owned(),
            )
        }
        5 => {
            let len = u32::from_le_bytes(take(buf, pos, 4)?.try_into().unwrap()) as usize;
            Value::Blob(take(buf, pos, len)?.to_vec())
        }
        6 => Value::Timestamp(i64::from_le_bytes(take(buf, pos, 8)?.try_into().unwrap())),
        7 => {
            let n = u32::from_le_bytes(take(buf, pos, 4)?.try_into().unwrap()) as usize;
            // A hostile length must not pre-allocate: each element is decoded
            // (and bounds-checked) before the next, so a lie about `n` runs out
            // of buffer instead of out of memory.
            let mut items = Vec::new();
            for _ in 0..n {
                let v = read_value(buf, pos)?;
                // Reject nesting on the way IN, so nothing downstream ever has
                // to reason about a list of lists.
                if matches!(v, Value::List(_)) {
                    return Err(Error::Corrupt("nested context list".into()));
                }
                items.push(v);
            }
            Value::List(items)
        }
        8 => Value::Date(i64::from_le_bytes(take(buf, pos, 8)?.try_into().unwrap())),
        9 => Value::Time(i64::from_le_bytes(take(buf, pos, 8)?.try_into().unwrap())),
        10 => {
            let len = u32::from_le_bytes(take(buf, pos, 4)?.try_into().unwrap()) as usize;
            let bytes = take(buf, pos, len)?;
            Value::Numeric(
                std::str::from_utf8(bytes)
                    .map_err(|_| Error::Corrupt("invalid utf-8 in numeric value".into()))?
                    .to_owned(),
            )
        }
        _ => return Err(Error::Corrupt(format!("invalid value tag {tag}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_variants() {
        let values = vec![
            Value::Null,
            Value::Int(-42),
            Value::Int(i64::MIN),
            Value::Float(3.75),
            Value::Float(f64::NEG_INFINITY),
            Value::Bool(true),
            Value::Text("hløl \0 zero".into()),
            Value::Blob(vec![0, 255, 0, 1]),
            Value::Timestamp(1_720_000_000_000_000),
        ];
        let mut buf = Vec::new();
        for v in &values {
            write_value(&mut buf, v);
        }
        let mut pos = 0;
        for v in &values {
            assert_eq!(&read_value(&buf, &mut pos).unwrap(), v);
        }
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn truncated_input_is_error_not_panic() {
        let mut buf = Vec::new();
        write_value(&mut buf, &Value::Text("hello".into()));
        for cut in 0..buf.len() {
            assert!(read_value(&buf[..cut], &mut 0).is_err());
        }
    }

    /// The declared-name → column-type table, spelled out. Every row here is
    /// cross-checked against the real `sqlite3` binary's affinity in
    /// `crates/mpedb/tests/django_parse_gaps.rs`.
    #[test]
    fn declared_type_names_map_by_affinity() {
        for (decl, want) in [
            // mpedb's own vocabulary, where sqlite's affinity rule agrees.
            ("int64", ColumnType::Int64),
            ("INTEGER", ColumnType::Int64),
            ("real", ColumnType::Float64),
            ("text", ColumnType::Text),
            // The two carve-outs.
            ("bool", ColumnType::Bool),
            ("BOOLEAN", ColumnType::Bool),
            ("any", ColumnType::Any),
            // …and the names where sqlite's rule now wins (task #113): a
            // BLOB-affinity column converts nothing and a `timestamp` is
            // NUMERIC, so both are the per-value column.
            ("timestamp", ColumnType::Any),
            ("blob", ColumnType::Any),
            ("bytes", ColumnType::Any),
            ("string", ColumnType::Any),
            // INTEGER affinity (step 2).
            ("bigint", ColumnType::Int64),
            ("smallint", ColumnType::Int64),
            ("tinyint", ColumnType::Int64),
            ("int(8)", ColumnType::Int64),
            ("integer unsigned", ColumnType::Int64),
            ("unsigned big int", ColumnType::Int64),
            ("int2", ColumnType::Int64),
            // `INT` wins wherever it appears, even in the second word — this is
            // sqlite's rule, and `floating point` really is INTEGER there.
            ("floating point", ColumnType::Int64),
            // REAL affinity.
            ("double precision", ColumnType::Float64),
            ("float", ColumnType::Float64),
            // TEXT affinity.
            ("varchar(100)", ColumnType::Text),
            ("VARCHAR", ColumnType::Text),
            ("char(1)", ColumnType::Text),
            ("nchar(55)", ColumnType::Text),
            ("clob", ColumnType::Text),
            ("native character(70)", ColumnType::Text),
            // BLOB affinity — only a name literally containing `blob`; the
            // typeless column, which converts nothing.
            ("longblob", ColumnType::Any),
            ("cblob", ColumnType::Any),
            // NUMERIC affinity → the per-value column. (`varbinary` really is
            // NUMERIC in sqlite: the BLOB rule is a `blob` substring, nothing
            // else.)
            ("varbinary", ColumnType::Any),
            ("decimal(10,2)", ColumnType::Any),
            ("numeric", ColumnType::Any),
            ("date", ColumnType::Any),
            ("datetime", ColumnType::Any),
            ("nosuchtype", ColumnType::Any),
            // No declared type at all is sqlite's no-affinity column.
            ("", ColumnType::Any),
            ("   ", ColumnType::Any),
        ] {
            assert_eq!(ColumnType::from_declared(decl), want, "declared `{decl}`");
        }
    }

    /// The DDL path and the CONFIG path are deliberately different vocabularies
    /// (task #113). `parse` is the rigid product; `from_declared` is sqlite.
    /// They agree everywhere sqlite's affinity rule agrees, and the exact set
    /// where they differ is pinned here so a future widening cannot soften a
    /// config-declared column by accident.
    #[test]
    fn from_declared_differs_from_parse_only_on_the_named_sqlite_affinity_words() {
        for name in [
            "int64", "int", "integer", "float64", "float", "real", "double", "bool", "boolean",
            "text", "any",
        ] {
            assert_eq!(
                ColumnType::from_declared(name),
                ColumnType::parse(name).unwrap(),
                "`{name}` must mean the same thing in a config and in a CREATE TABLE"
            );
        }
        // sqlite's rule wins for these: BLOB affinity converts nothing and is
        // per-value; `timestamp`/`string`/`bytes` are NUMERIC affinity.
        for name in ["blob", "bytes", "timestamp", "string"] {
            assert_eq!(ColumnType::from_declared(name), ColumnType::Any, "`{name}`");
            assert_ne!(ColumnType::parse(name).unwrap(), ColumnType::Any, "`{name}`");
        }
    }

    #[test]
    fn float_order_semantics() {
        assert_eq!(float_total_cmp(0.0, -0.0), Ordering::Equal);
        assert_eq!(float_total_cmp(f64::NAN, f64::NAN), Ordering::Equal);
        assert_eq!(float_total_cmp(f64::INFINITY, f64::NAN), Ordering::Less);
        assert_eq!(float_total_cmp(-1.0, 1.0), Ordering::Less);
        assert_eq!(
            float_total_cmp(f64::NEG_INFINITY, f64::MIN),
            Ordering::Less
        );
    }
}

#[cfg(test)]
mod affinity_tests {
    use super::*;
    use crate::expr::store_affinity;

    /// The declared-name → (type, affinity) pair, which is what a column
    /// definition needs and what one `ColumnType` alone cannot say.
    #[test]
    fn declared_splits_numeric_from_typeless() {
        for n in ["numeric", "decimal(10,2)", "decimal", "datetime", "date", "nosuchtype"] {
            assert_eq!(ColumnType::declared(n), (ColumnType::Any, Affinity::Numeric), "{n}");
        }
        // A column with NO declared type is the OPPOSITE behaviour under the
        // same storage type.
        assert_eq!(ColumnType::declared(""), (ColumnType::Any, Affinity::Blob));
        assert_eq!(ColumnType::declared("  "), (ColumnType::Any, Affinity::Blob));
        // The carve-outs, plus a name sqlite's rule already agrees with.
        assert_eq!(ColumnType::declared("any"), (ColumnType::Any, Affinity::Blob));
        assert_eq!(ColumnType::declared("bool"), (ColumnType::Bool, Affinity::Integer));
        assert_eq!(ColumnType::declared("int64"), (ColumnType::Int64, Affinity::Integer));
        // `timestamp` is NUMERIC affinity like `datetime` (#113) — SQL has no
        // timestamp literal, so a rigid one here was unwritable.
        assert_eq!(
            ColumnType::declared("timestamp"),
            (ColumnType::Any, Affinity::Numeric)
        );
        // BLOB affinity is the THIRD behaviour under `Any`: converts nothing,
        // and unlike the no-type column it is a name a consumer wrote.
        for n in ["blob", "longblob", "cblob"] {
            assert_eq!(ColumnType::declared(n), (ColumnType::Any, Affinity::Blob), "{n}");
        }
        // The rigid three: the affinity is sqlite's, and it agrees with the one
        // the type implies.
        for (n, t) in [
            ("bigint", ColumnType::Int64),
            ("double precision", ColumnType::Float64),
            ("varchar(100)", ColumnType::Text),
        ] {
            let (ty, aff) = ColumnType::declared(n);
            assert_eq!((ty, aff), (t, Affinity::implied_by(t)), "{n}");
            assert_eq!(aff, Affinity::from_type_name(n), "{n}");
        }
    }

    /// Store-time affinity is NOT `CAST`. Both are sqlite's, and they disagree —
    /// pinned so the two can never be merged into one function with a flag.
    #[test]
    fn store_affinity_is_not_cast() {
        use crate::expr::{ExprProgram, Instr};
        let cast = |v: Value, aff: Affinity| {
            ExprProgram::new(vec![Instr::PushParam(0), Instr::Cast(aff)], vec![])
                .unwrap()
                .eval(&[], &[v])
                .unwrap()
        };
        // A numeric PREFIX is a number to CAST and not a number at all to
        // affinity, which leaves the text alone.
        assert_eq!(cast(Value::Text("12abc".into()), Affinity::Numeric), Value::Int(12));
        assert_eq!(
            store_affinity(Affinity::Numeric, Value::Text("12abc".into())),
            Value::Text("12abc".into())
        );
        // CAST stops at sqlite's 2^51 `RealSameAsInt` bound; store-time affinity
        // uses the full i64 round trip, so this one really does become an int.
        assert_eq!(cast(Value::Text("1e18".into()), Affinity::Numeric), Value::Float(1e18));
        assert_eq!(
            store_affinity(Affinity::Numeric, Value::Text("1e18".into())),
            Value::Int(1_000_000_000_000_000_000)
        );
        // A blob is parsed by CAST and never by affinity.
        assert_eq!(cast(Value::Blob(b"12".to_vec()), Affinity::Numeric), Value::Int(12));
        assert_eq!(
            store_affinity(Affinity::Numeric, Value::Blob(b"12".to_vec())),
            Value::Blob(b"12".to_vec())
        );
    }

    /// BLOB affinity — the typeless column — converts nothing, ever.
    #[test]
    fn blob_affinity_is_verbatim() {
        for v in [
            Value::Text("1.50".into()),
            Value::Text("abc".into()),
            Value::Int(3),
            Value::Float(1.0),
            Value::Blob(b"7".to_vec()),
            Value::Null,
        ] {
            assert_eq!(store_affinity(Affinity::Blob, v.clone()), v);
        }
    }

    /// The two affinities only the sqlite-overlay path can produce today.
    #[test]
    fn real_and_text_affinity_follow_sqlite() {
        // REAL: the NUMERIC rule, then widen an integer result.
        assert_eq!(store_affinity(Affinity::Real, Value::Text("0012".into())), Value::Float(12.0));
        assert_eq!(store_affinity(Affinity::Real, Value::Int(1)), Value::Float(1.0));
        assert_eq!(
            store_affinity(Affinity::Real, Value::Text("abc".into())),
            Value::Text("abc".into())
        );
        assert_eq!(
            store_affinity(Affinity::Real, Value::Blob(b"1".to_vec())),
            Value::Blob(b"1".to_vec())
        );
        // TEXT: numbers render, everything else is left alone.
        assert_eq!(store_affinity(Affinity::Text, Value::Int(1)), Value::Text("1".into()));
        assert_eq!(store_affinity(Affinity::Text, Value::Float(1.5)), Value::Text("1.5".into()));
        assert_eq!(
            store_affinity(Affinity::Text, Value::Blob(b"a".to_vec())),
            Value::Blob(b"a".to_vec())
        );
        assert_eq!(store_affinity(Affinity::Text, Value::Null), Value::Null);
        // INTEGER is NUMERIC at store time (sqlite's own documented equality).
        for v in [Value::Text("1.50".into()), Value::Text("abc".into()), Value::Float(1.0)] {
            assert_eq!(
                store_affinity(Affinity::Integer, v.clone()),
                store_affinity(Affinity::Numeric, v)
            );
        }
    }

    /// INTEGER against REAL is exact — no cast to a common type, which would
    /// lose every magnitude past 2^53.
    #[test]
    fn int_against_real_is_exact() {
        let cmp = |a: Value, b: Value| a.sql_cmp(&b).unwrap().unwrap();
        assert_eq!(cmp(Value::Int(1000), Value::Float(1.5)), Ordering::Greater);
        assert_eq!(cmp(Value::Float(1.5), Value::Int(1000)), Ordering::Less);
        assert_eq!(cmp(Value::Int(1), Value::Float(1.0)), Ordering::Equal);
        assert_eq!(cmp(Value::Int(1), Value::Float(1.5)), Ordering::Less);
        assert_eq!(cmp(Value::Int(2), Value::Float(1.5)), Ordering::Greater);
        // Past 2^53: `i as f64` would round these together.
        assert_eq!(
            cmp(Value::Int(9007199254740993), Value::Float(9007199254740992.0)),
            Ordering::Greater
        );
        assert_eq!(
            cmp(Value::Float(9007199254740992.0), Value::Int(9007199254740993)),
            Ordering::Less
        );
        // Reals outside i64 range: no truncation, no UB.
        assert_eq!(cmp(Value::Int(i64::MAX), Value::Float(1e300)), Ordering::Less);
        assert_eq!(cmp(Value::Int(i64::MIN), Value::Float(-1e300)), Ordering::Greater);
        assert_eq!(cmp(Value::Int(0), Value::Float(f64::INFINITY)), Ordering::Less);
        assert_eq!(cmp(Value::Int(0), Value::Float(f64::NEG_INFINITY)), Ordering::Greater);
        // NaN sorts above every integer, agreeing with `float_total_cmp` so the
        // order stays total.
        assert_eq!(cmp(Value::Int(i64::MAX), Value::Float(f64::NAN)), Ordering::Less);
        assert_eq!(
            float_total_cmp(f64::NAN, f64::INFINITY),
            Ordering::Greater,
            "the two must agree on where NaN sits"
        );
        // A number against TEXT is still a clean REFUSAL: its answer depends on
        // comparison affinity, which is not implemented yet.
        assert!(Value::Int(1).sql_cmp(&Value::Text("1".into())).is_err());
    }
}

#[cfg(test)]
mod numeric_tests {
    use super::*;

    #[test]
    fn parses_what_postgresql_parses() {
        for (input, want) in [
            ("0", "0"),
            ("-0", "0"),
            ("+0.000", "0.000"),
            ("1", "1"),
            ("+1", "1"),
            // The SCALE survives — this is what a `Decimal("1.00")` round trip
            // rests on, and what `canonical_numeric` deliberately erases for
            // the key.
            ("1.0", "1.0"),
            ("1.00", "1.00"),
            ("0001.5000", "1.5000"),
            ("-1.50", "-1.50"),
            (" 42 ", "42"),
            // Exponents are EXPANDED: one value, one spelling.
            ("1e3", "1000"),
            ("1E3", "1000"),
            ("1e+3", "1000"),
            ("1.5e-3", "0.0015"),
            ("-2.5e2", "-250"),
            ("1.50e2", "150"),
            ("1e-5", "0.00001"),
            (".5", "0.5"),
            ("5.", "5"),
            // The specials, by name.
            ("nan", "NaN"),
            ("NaN", "NaN"),
            ("-nan", "NaN"),
            ("infinity", "Infinity"),
            ("-Infinity", "-Infinity"),
            ("inf", "Infinity"),
        ] {
            assert_eq!(parse_numeric(input).as_deref(), Some(want), "{input}");
        }
    }

    #[test]
    fn refuses_what_is_not_a_number() {
        for input in [
            "", " ", "abc", "1.2.3", "1_000", "--1", "1e", "1e+", "1ee3", "0x10", "1 2", ".",
            "1e999999999999999999999", "1e-99999999999999999999",
        ] {
            assert_eq!(parse_numeric(input), None, "{input} should not parse");
        }
    }

    /// A `numeric` compares by VALUE — the property `1.0 = 1.00` rests on.
    #[test]
    fn compares_by_value_not_by_spelling() {
        use Ordering::*;
        for (a, b, want) in [
            ("1.0", "1.00", Equal),
            ("1", "1.000", Equal),
            ("0", "-0", Equal),
            ("+5", "5", Equal),
            ("9", "10", Less),
            ("0.5", "0.51", Less),
            ("-0.5", "-0.51", Greater),
            ("-1", "1", Less),
            ("-100", "-9", Less),
            ("0.00001", "0.0001", Less),
            // PostgreSQL puts NaN above everything, unlike a float NaN.
            ("NaN", "Infinity", Greater),
            ("Infinity", "999999999999999999999", Greater),
            ("-Infinity", "-999999999999999999999", Less),
            ("NaN", "NaN", Equal),
        ] {
            assert_eq!(numeric_cmp(a, b), want, "{a} vs {b}");
            assert_eq!(numeric_cmp(b, a), want.reverse(), "{b} vs {a}");
        }
    }

    /// The stored form keeps the scale; the key form does not. Two functions,
    /// two jobs — a single "canonical" would have to give up one of them.
    #[test]
    fn the_stored_form_and_the_key_form_differ_on_purpose() {
        for (input, stored, key) in [
            ("1.00", "1.00", "1"),
            ("1.50", "1.50", "1.5"),
            ("0.000", "0.000", "0"),
            ("100", "100", "100"),
        ] {
            assert_eq!(parse_numeric(input).as_deref(), Some(stored), "stored {input}");
            assert_eq!(canonical_numeric(stored), key, "key {input}");
        }
        // …and the two spellings still compare and group as ONE value.
        assert_eq!(numeric_cmp("1.00", "1.0"), Ordering::Equal);
    }

    /// `sql_cmp` routes a numeric pair to the value comparison, so a `WHERE`
    /// on a numeric column is not a spelling test.
    #[test]
    fn sql_cmp_answers_a_numeric_pair() {
        let a = Value::Numeric("1.0".into());
        let b = Value::Numeric("1.00".into());
        assert_eq!(a.sql_cmp(&b).unwrap(), Some(Ordering::Equal));
        let c = Value::Numeric("10".into());
        let d = Value::Numeric("9".into());
        assert_eq!(c.sql_cmp(&d).unwrap(), Some(Ordering::Greater));
    }

    /// A numeric against an INTEGER stays a refusal rather than a rank-only
    /// "equal". Ranking it with the numbers made `Numeric("2")` and `Int(1)`
    /// compare equal, which is the wrong answer this split exists to avoid.
    #[test]
    fn a_numeric_against_an_integer_is_not_silently_equal() {
        let n = Value::Numeric("2".into());
        let i = Value::Int(1);
        assert!(n.sql_cmp(&i).is_err());
        assert_eq!(n.sort_cmp(&i, Collation::Binary), None);
    }
}

#[cfg(test)]
mod calendar_tests {
    use super::*;

    #[test]
    fn dates_round_trip_across_the_whole_range() {
        for (text, days) in [
            ("1970-01-01", 0),
            ("1970-01-02", 1),
            ("1969-12-31", -1),
            ("2020-01-01", 18262),
            ("2020-01-02", 18263),
            ("2020-02-29", 18321), // a leap day
            ("2000-02-29", 11016), // the 400-year rule
            ("0001-01-01", -719162),
            ("9999-12-31", 2932896),
        ] {
            assert_eq!(parse_date_text(text), Some(days), "parse {text}");
            assert_eq!(date_text(days), text, "render {days}");
        }
    }

    /// The bug this whole type surface exists to close: sqlite's CAST rule
    /// takes the leading number, so `'2020-01-02'` became `2020`. Nothing here
    /// may accept a prefix.
    #[test]
    fn a_date_is_all_or_nothing() {
        for bad in [
            "2020",
            "2020-01",
            "2020-01-02 03:04:05",
            "2020-01-02T03:04",
            "2020-13-01",
            "2020-00-01",
            "2020-01-00",
            "2020-01-32",
            "2021-02-29", // not a leap year
            "1900-02-29", // the 100-year rule
            "2020-+1-02",
            "abc",
            "",
            "2020-01-02x",
        ] {
            assert_eq!(parse_date_text(bad), None, "{bad} should not parse");
        }
    }

    #[test]
    fn times_round_trip_and_refuse_a_zone() {
        for (text, us) in [
            ("00:00:00", 0),
            ("03:04:05", 11_045_000_000),
            ("23:59:59.999999", 86_399_999_999),
            ("03:04:05.0015", 11_045_001_500),
        ] {
            assert_eq!(parse_time_text(text), Some(us), "parse {text}");
            assert_eq!(time_text(us), text, "render {us}");
        }
        // Legal input that renders back with the seconds PostgreSQL prints.
        assert_eq!(parse_time_text("03:04"), Some(11_040_000_000));
        // 24:00:00 is PostgreSQL's end-of-day; 24:00:01 is not a time.
        assert_eq!(parse_time_text("24:00:00"), Some(86_400_000_000));
        for bad in ["3", "25:00:00", "24:00:01", "03:04:05+02", "03:04:05Z", "0a:00:00", ""] {
            assert_eq!(parse_time_text(bad), None, "{bad} should not parse");
        }
    }
}

#[cfg(test)]
mod timestamp_text_tests {
    use super::*;

    #[test]
    fn timestamps_round_trip() {
        for (text, us) in [
            ("1970-01-01 00:00:00+00", 0i64),
            ("2023-11-14 22:13:20+00", 1_700_000_000_000_000),
            ("1969-12-31 23:59:59+00", -1_000_000),
            ("2023-11-14 22:13:20.0015+00", 1_700_000_000_001_500),
        ] {
            assert_eq!(parse_timestamp_text(text), Some(us), "parse {text}");
            assert_eq!(timestamp_text(us), text, "render {us}");
        }
    }

    #[test]
    fn zones_and_separators_are_understood() {
        let base = parse_timestamp_text("2020-01-02 03:04:05").unwrap();
        assert_eq!(parse_timestamp_text("2020-01-02T03:04:05"), Some(base));
        assert_eq!(parse_timestamp_text("2020-01-02 03:04:05Z"), Some(base));
        // +01 means the instant is an hour EARLIER in UTC.
        assert_eq!(parse_timestamp_text("2020-01-02 04:04:05+01"), Some(base));
        assert_eq!(parse_timestamp_text("2020-01-02 02:04:05-01"), Some(base));
        assert_eq!(parse_timestamp_text("2020-01-02 04:34:05+01:30"), Some(base));
        // A bare date is midnight.
        assert_eq!(
            parse_timestamp_text("2020-01-02"),
            Some(days_from_civil(2020, 1, 2).unwrap() * 86_400 * 1_000_000)
        );
    }

    /// Same rule as `parse_date_text`: all or nothing, never a prefix.
    #[test]
    fn a_timestamp_is_all_or_nothing() {
        for bad in ["2020", "2020-01", "2020-13-01 00:00:00", "2021-02-29 00:00:00", "abc", ""] {
            assert_eq!(parse_timestamp_text(bad), None, "{bad} should not parse");
        }
    }
}
