//! sqlite 3.45's date/time functions — `strftime(FORMAT, TIMESTRING)`,
//! `date(X)`, `time(X)`, `datetime(X)` and `julianday(X)` — a line-for-line
//! port of `strftimeFunc`/`dateFunc`/`timeFunc`/`datetimeFunc`/`juliandayFunc`
//! over the subset of sqlite's time-string grammar that mpedb accepts. All five
//! share ONE parser and ONE set of refusals, exactly as sqlite shares `isDate`.
//!
//! # `'now'` — how a deterministic engine says it
//!
//! sqlite fixes `'now'` ONCE PER STATEMENT (`iCurrentTime`), so two
//! `date('now')` in one statement agree and the same statement run twice does
//! not. mpedb reproduces that WITHOUT giving this module a clock: the BINDER
//! recognises a LITERAL `'now'` in the time-value position and rewrites it into
//! a reserved parameter slot, which the facade fills once per `execute()` with
//! the statement-start instant rendered as `YYYY-MM-DD HH:MM:SS.SSS` UTC
//! ([`sqlite_now_string`]). So:
//!
//! * the compiled plan carries a PARAMETER REFERENCE, never a timestamp — plan
//!   bytes stay a deterministic function of the SQL, which is what makes the
//!   content-hashed, cross-process plan registry sound;
//! * the instant is runtime state, filled once per statement, so every `'now'`
//!   in one statement reads the same slot and therefore the same value;
//! * const-folding can never bake a clock, because a `Param` is not a constant
//!   (and the binder's fold gate says so explicitly);
//! * this module still sees only an ordinary ISO-8601 string, so `'now'` needs
//!   no special case here at all.
//!
//! A `'now'` that is NOT a bind-time literal — a column or parameter whose
//! VALUE happens to be the text `now` — stays REFUSED by name below: resolving
//! it would need a clock inside `eval`, which would drift within one statement.
//!
//! Time zone: `'now'` is UTC, exactly as in sqlite (only the `'localtime'`
//! modifier shifts it, and the whole modifier language is refused).
//!
//! # What is supported
//!
//! **Time strings** (ISO-8601, exactly as sqlite's `parseYyyyMmDd` /
//! `parseHhMmSs` / `parseTimezone` read them — fixed-width fields, no
//! abbreviation):
//!
//! * `YYYY-MM-DD`
//! * `YYYY-MM-DD HH:MM[:SS[.SSS…]]` (a `T` — or any run of spaces — separates
//!   the date and the time)
//! * `HH:MM[:SS[.SSS…]]` (the date defaults to 2000-01-01, sqlite's rule)
//! * any of the above carrying a `Z` / `z` / `+HH:MM` / `-HH:MM` suffix
//!
//! A leading `-` on the year (a BC date) is accepted, as sqlite accepts it.
//!
//! **Format specifiers** — every one sqlite 3.45 has:
//! `%d %e %f %F %H %I %j %J %k %l %m %M %p %P %R %s %S %T %u %w %W %Y %%`.
//!
//! # What is refused (a clean error, never a guessed value)
//!
//! sqlite answers `NULL` for a time string it cannot parse, so an unsupported
//! *form* must not be allowed to fall into that same `NULL`: the two would be
//! indistinguishable and mpedb would be silently returning a different answer
//! than sqlite for `'now'`, for a Julian-day number, or for a modifier. So
//! every input this module does not reproduce is an ERROR that names it:
//!
//! * modifiers (`strftime(f, t, '+1 day')`, `date(t, 'start of month')`) —
//!   sqlite's modifier language is a large surface (`±N days/months/years`,
//!   `start of day/month/year`, `weekday N`, `unixepoch`, `julianday`, `auto`,
//!   `localtime`, `utc`, `subsec`, `ceiling`, `floor`), `localtime`/`utc` are
//!   not even deterministic (they read the host time zone), and NOTHING in the
//!   Django or CPython surfaces mpedb targets emits one — so the whole language
//!   is refused BY NAME rather than partially implemented;
//! * a RUNTIME `'now'` (see above — the bind-time literal is supported),
//!   `'unixepoch'`-style numeric time values and the bare Julian-day number
//!   form (`strftime('%Y', 2455352.5)`);
//! * anything else that is not one of the ISO-8601 forms above, including the
//!   out-of-range component values sqlite itself rejects (`2010-13-01`);
//! * an unknown format specifier, and a trailing bare `%`.
//!
//! NULL propagates (a NULL format or a NULL time string yields NULL), exactly
//! as sqlite does.
//!
//! # The quirks that ARE reproduced
//!
//! sqlite keeps the parsed Y/M/D/h/m/s rather than round-tripping them through
//! the Julian day, *except* that `isDate()` invalidates the parsed Y/M/D when
//! `D > 28` (so `'2010-02-30'` normalises to `2010-03-02`) and `computeJD()`
//! invalidates both when a non-zero timezone was given. The h/m/s are never
//! renormalised on their own, which is why `'2010-01-01 24:00'` prints hour
//! `24`, and why `%S` of `…:56.9999` is `56` while `%f` of the same value is
//! `57.000`. All of that is modelled here rather than smoothed over.

use super::printf::sqlite_printf;
use crate::error::{Error, Result};
use crate::value::Value;

/// sqlite's `validJulianDay`: the millisecond Julian day must be non-negative
/// and no larger than `INT_464269060799999` — Julian day 5373484.5, i.e. the
/// end of the year 9999.
const MAX_JD_MS: i64 = 464_269_060_799_999;

/// sqlite's `DateTime`, restricted to the fields the supported grammar sets.
#[derive(Clone, Copy)]
struct Dt {
    ijd: i64,
    y: i32,
    mo: i32,
    d: i32,
    h: i32,
    mi: i32,
    s: f64,
    /// Timezone offset in minutes east of UTC.
    tz: i32,
    valid_jd: bool,
    valid_ymd: bool,
    valid_hms: bool,
    valid_tz: bool,
}

impl Dt {
    fn new() -> Dt {
        Dt {
            ijd: 0,
            y: 0,
            mo: 0,
            d: 0,
            h: 0,
            mi: 0,
            s: 0.0,
            tz: 0,
            valid_jd: false,
            valid_ymd: false,
            valid_hms: false,
            valid_tz: false,
        }
    }

    /// sqlite's `computeJD`. Returns false on the year-range guard sqlite sets
    /// `isError` for.
    fn compute_jd(&mut self) -> bool {
        if self.valid_jd {
            return true;
        }
        let (mut y, mut m, d) = if self.valid_ymd {
            (self.y, self.mo, self.d)
        } else {
            // No date given: sqlite assumes 2000-01-01 (and leaves validYMD
            // clear, so computeYMD later derives the date back from the JD).
            (2000, 1, 1)
        };
        if !(-4713..=9999).contains(&y) {
            return false;
        }
        if m <= 2 {
            y -= 1;
            m += 12;
        }
        // C integer division truncates toward zero, and so does Rust's — the
        // two agree for every year in range, negative ones included.
        let a = y / 100;
        let b = 2 - a + (a / 4);
        let x1 = 36525 * (y + 4716) / 100;
        let x2 = 306001 * (m + 1) / 10000;
        self.ijd = (((x1 + x2 + d + b) as f64 - 1524.5) * 86_400_000.0) as i64;
        self.valid_jd = true;
        if self.valid_hms {
            self.ijd += self.h as i64 * 3_600_000
                + self.mi as i64 * 60_000
                + (self.s * 1000.0 + 0.5) as i64;
            if self.valid_tz {
                self.ijd -= self.tz as i64 * 60_000;
                self.valid_ymd = false;
                self.valid_hms = false;
                self.valid_tz = false;
            }
        }
        true
    }

    /// sqlite's `computeYMD`.
    fn compute_ymd(&mut self) {
        if self.valid_ymd {
            return;
        }
        if !self.valid_jd {
            self.y = 2000;
            self.mo = 1;
            self.d = 1;
        } else if !(0..=MAX_JD_MS).contains(&self.ijd) {
            self.y = 0;
            self.mo = 0;
            self.d = 0;
        } else {
            let z = ((self.ijd + 43_200_000) / 86_400_000) as i32;
            let alpha = ((z as f64 + 32044.75) / 36524.25) as i32 - 52;
            let a = z + 1 + alpha - ((alpha + 100) / 4) + 25;
            let b = a + 1524;
            let c = ((b as f64 - 122.1) / 365.25) as i32;
            let d = (36525 * (c & 32767)) / 100;
            let e = ((b - d) as f64 / 30.6001) as i32;
            let x1 = (30.6001 * e as f64) as i32;
            self.d = b - d - x1;
            self.mo = if e < 14 { e - 1 } else { e - 13 };
            self.y = if self.mo > 2 { c - 4716 } else { c - 4715 };
        }
        self.valid_ymd = true;
    }

    /// sqlite's `computeHMS`.
    fn compute_hms(&mut self) {
        if self.valid_hms {
            return;
        }
        self.compute_jd();
        let day_ms = ((self.ijd + 43_200_000) % 86_400_000) as i32;
        self.s = (day_ms % 60_000) as f64 / 1000.0;
        let day_min = day_ms / 60_000;
        self.mi = day_min % 60;
        self.h = day_min / 60;
        self.valid_hms = true;
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// sqlite's `getDigits` for ONE field: exactly `n` ASCII digits, the value in
/// `min..=max`, and — when `next` is non-zero — that separator immediately
/// after. Returns the value and leaves `i` one PAST the separator position
/// (sqlite advances unconditionally; the callers re-anchor on a fixed width, so
/// the extra step is harmless).
fn get_digits(b: &[u8], i: &mut usize, n: usize, min: i32, max: i32, next: u8) -> Option<i32> {
    let mut val: i32 = 0;
    for _ in 0..n {
        let c = *b.get(*i)?;
        if !c.is_ascii_digit() {
            return None;
        }
        val = val * 10 + (c - b'0') as i32;
        *i += 1;
    }
    if val < min || val > max {
        return None;
    }
    if next != 0 && b.get(*i).copied() != Some(next) {
        return None;
    }
    *i += 1;
    Some(val)
}

/// sqlite's `parseTimezone`. `at` is the offset just past the seconds field.
/// Returns `Some(tz_minutes)` when the remainder of the string is a (possibly
/// empty) timezone, `None` when it is junk.
fn parse_timezone(b: &[u8], mut at: usize) -> Option<i32> {
    while at < b.len() && is_space(b[at]) {
        at += 1;
    }
    let tz = match b.get(at).copied() {
        Some(b'-') | Some(b'+') => {
            let sgn = if b[at] == b'-' { -1 } else { 1 };
            at += 1;
            let mut i = at;
            let n_hr = get_digits(b, &mut i, 2, 0, 14, b':')?;
            let n_mn = get_digits(b, &mut i, 2, 0, 59, 0)?;
            at += 5;
            sgn * (n_mn + n_hr * 60)
        }
        Some(b'Z') | Some(b'z') => {
            at += 1;
            0
        }
        None => return Some(0),
        Some(_) => return None,
    };
    while at < b.len() && is_space(b[at]) {
        at += 1;
    }
    if at != b.len() {
        return None;
    }
    Some(tz)
}

/// sqlite's `parseHhMmSs`, filling the h/m/s/tz fields of `p`.
fn parse_hh_mm_ss(b: &[u8], p: &mut Dt) -> bool {
    let mut i = 0usize;
    let h = match get_digits(b, &mut i, 2, 0, 24, b':') {
        Some(v) => v,
        None => return false,
    };
    let m = match get_digits(b, &mut i, 2, 0, 59, 0) {
        Some(v) => v,
        None => return false,
    };
    let mut at = 5usize; // sqlite: zDate += 5
    let mut s = 0i32;
    let mut ms = 0.0f64;
    if b.get(at).copied() == Some(b':') {
        at += 1;
        let mut j = at;
        match get_digits(b, &mut j, 2, 0, 59, 0) {
            Some(v) => s = v,
            None => return false,
        }
        at += 2;
        if b.get(at).copied() == Some(b'.') && b.get(at + 1).is_some_and(|c| c.is_ascii_digit()) {
            at += 1;
            let mut scale = 1.0f64;
            while b.get(at).is_some_and(|c| c.is_ascii_digit()) {
                ms = ms * 10.0 + (b[at] - b'0') as f64;
                scale *= 10.0;
                at += 1;
            }
            ms /= scale;
        }
    }
    let tz = match parse_timezone(b, at) {
        Some(tz) => tz,
        None => return false,
    };
    // A fractional part long enough to overflow the accumulator (roughly 309+
    // digits) makes `ms` and `scale` both infinite, so `ms/scale` is NaN. In C
    // the cast of that NaN to an integer is undefined and sqlite ends up with a
    // Julian day outside its valid range — a NULL. Here it would quietly cast
    // to 0 and produce an ANSWER, so it is rejected at the parse instead.
    if !ms.is_finite() {
        return false;
    }
    p.valid_jd = false;
    p.valid_hms = true;
    p.h = h;
    p.mi = m;
    p.s = s as f64 + ms;
    p.tz = tz;
    // sqlite: `p->validTZ = p->tz!=0` — a `Z` (or `+00:00`) sets NO offset, so
    // the parsed h/m/s survive untouched, which is why `'…12:34:56Z'` keeps its
    // sub-second spelling while `'…12:34:56+02:00'` is renormalised.
    p.valid_tz = tz != 0;
    true
}

/// sqlite's `parseYyyyMmDd`.
fn parse_yyyy_mm_dd(b: &[u8], p: &mut Dt) -> bool {
    let mut off = 0usize;
    let neg = b.first().copied() == Some(b'-');
    if neg {
        off = 1;
    }
    let mut i = off;
    let y = match get_digits(b, &mut i, 4, 0, 9999, b'-') {
        Some(v) => v,
        None => return false,
    };
    let m = match get_digits(b, &mut i, 2, 1, 12, b'-') {
        Some(v) => v,
        None => return false,
    };
    let d = match get_digits(b, &mut i, 2, 1, 31, 0) {
        Some(v) => v,
        None => return false,
    };
    let mut at = off + 10; // sqlite: zDate += 10
    while at < b.len() && (is_space(b[at]) || b[at] == b'T') {
        at += 1;
    }
    if parse_hh_mm_ss(&b[at..], p) {
        // got the time
    } else if at == b.len() {
        p.valid_hms = false;
        p.tz = 0;
        p.valid_tz = false;
    } else {
        return false;
    }
    p.valid_jd = false;
    p.valid_ymd = true;
    p.y = if neg { -y } else { y };
    p.mo = m;
    p.d = d;
    if p.valid_tz {
        p.compute_jd();
    }
    true
}

/// The refusal text for a time string mpedb will not interpret. Deliberately
/// an ERROR rather than sqlite's NULL: sqlite ANSWERS for a Julian-day number
/// and for a modifier, so returning NULL there would be a silently different
/// answer rather than a refusal.
///
/// **A RUNTIME `'now'` keeps a reason of its own.** The bind-time literal is
/// supported (see the module header: it is rewritten into the statement-instant
/// parameter). A `'now'` that only turns out to be `'now'` when a row is read
/// cannot take that path, and resolving it here would mean:
///
/// * a clock read INSIDE `eval`, which would drift WITHIN one statement —
///   `SELECT date(c) FROM big_table` would answer differently per row, where
///   sqlite fixes one `iCurrentTime` for the whole statement. A different wrong
///   answer, plus a syscall in the per-row hot path;
/// * a clock dependency in `mpedb-types`, which has none by design.
///
/// A CHECK body or an index expression is refused for the same reason one level
/// up (the binder has no statement-instant slot there): a CHECK containing
/// `'now'` would pass at INSERT and fail on any later re-validation (CHECKs are
/// stored as SOURCE and recompiled at attach), and the mirror's convergence
/// criterion — replay must reproduce the source EXACTLY — has no meaning for a
/// predicate whose answer depends on when it ran.
fn unsupported_time(z: &str) -> Error {
    let shown: String = z.chars().take(64).collect();
    let why = if z.trim().eq_ignore_ascii_case("now") {
        " (a 'now' that is not a bind-time LITERAL in the time-value position cannot be \
         resolved: the statement instant is bound once per execute, and reading a clock \
         per row would drift within one statement)"
    } else {
        ""
    };
    Error::TypeMismatch(format!(
        "unsupported time string {shown:?}; mpedb accepts only the ISO-8601 forms \
         'YYYY-MM-DD', 'YYYY-MM-DD[ T]HH:MM[:SS[.SSS]]' and 'HH:MM[:SS[.SSS]]', each with an \
         optional 'Z' or '+HH:MM'/'-HH:MM' suffix, plus the literal 'now'. sqlite's \
         Julian-day and unix-epoch number forms and its modifier language are refused \
         rather than guessed{why}"
    ))
}

/// The refusal for sqlite's modifier language, shared by all five functions so
/// the wording (and the list of what is refused) cannot drift.
fn unsupported_modifiers(name: &str, n: usize) -> Error {
    Error::TypeMismatch(format!(
        "{name}(): modifiers are not supported ({n} given); sqlite's modifier language \
         ('+1 day', '-3 months', 'start of month', 'start of day', 'weekday 0', \
         'unixepoch', 'julianday', 'auto', 'localtime', 'utc', 'subsec', 'ceiling', \
         'floor') is refused rather than partially implemented — 'localtime'/'utc' are \
         not even deterministic, and nothing in the surfaces mpedb targets emits one"
    ))
}

/// Unix microseconds → the ISO-8601 UTC time string `YYYY-MM-DD HH:MM:SS.SSS`
/// that the binder's statement-instant parameter carries for a literal `'now'`.
///
/// MILLISECOND precision on purpose: sqlite's own `'now'` is a millisecond
/// Julian day (`sqlite3OsCurrentTimeInt64`), and this string is immediately
/// re-parsed by [`parse_date_or_time`], whose `(s*1000.0 + 0.5) as i64` recovers
/// those milliseconds EXACTLY. Rendering more digits would only add rounding
/// noise sqlite does not have.
///
/// Out-of-range instants (a clock before year 1 or past 9999) are CLAMPED to the
/// representable ends rather than wrapping: the value is then rejected by the
/// parser as an out-of-range time string — an error, never a wrong answer.
pub fn sqlite_now_string(unix_micros: i64) -> String {
    // Julian-day milliseconds of the Unix epoch: 2440587.5 days.
    const UNIX_EPOCH_JD_MS: i64 = 210_866_760_000_000;
    let ms = unix_micros.div_euclid(1000);
    let ijd = ms.saturating_add(UNIX_EPOCH_JD_MS).clamp(0, MAX_JD_MS);
    let mut x = Dt::new();
    x.ijd = ijd;
    x.valid_jd = true;
    x.compute_ymd();
    x.compute_hms();
    let milli = (x.s * 1000.0 + 0.5) as i64 % 1000;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        x.y, x.mo, x.d, x.h, x.mi, x.s as i32, milli
    )
}

/// sqlite's `dateFunc`/`datetimeFunc` YEAR rendering — and it is NOT `%04d`.
///
/// The two functions write the four year digits BY HAND into a buffer whose
/// slot 0 is a `'-'`, then return the buffer from index `Y<0 ? 0 : 1`. So a BC
/// year keeps all four digits and gains a sign: `date('-0500-03-04')` is
/// `-0500-03-04`. `strftime('%Y', …)` of the same date is `-500`, because THAT
/// one really is `%04d` and C's width includes the sign.
///
/// Reproduced rather than smoothed over: the two spellings differ in sqlite, so
/// making them agree here would be a wrong answer for one of them. (The parser
/// caps `|Y|` at 4713, so the four digits never overflow.)
fn date_year(y: i32) -> String {
    let a = y.unsigned_abs() % 10_000;
    let sign = if y < 0 { "-" } else { "" };
    format!("{sign}{a:04}")
}

/// `date(X)` / `time(X)` / `datetime(X)` / `julianday(X)` — sqlite's
/// `dateFunc`/`timeFunc`/`datetimeFunc`/`juliandayFunc`.
///
/// All four share [`parse_date_or_time`] with [`sqlite_strftime`], so the time
/// grammar and its refusals can never drift between them. The OUTPUT is written
/// exactly as each sqlite function writes it — see [`date_year`] for the one
/// place where that is deliberately not the `strftime` spelling. `julianday` is
/// the one that returns a REAL.
pub(super) fn sqlite_date_family(f: super::ScalarFn, args: &[Value]) -> Result<Value> {
    use super::ScalarFn as F;
    let name = f.name();
    if args.len() > 1 {
        return Err(unsupported_modifiers(name, args.len() - 1));
    }
    let z = match &args[0] {
        Value::Text(s) => s.as_str(),
        other => {
            return Err(Error::TypeMismatch(format!(
                "{name}() time value must be text, got {} — sqlite would read a number as a \
                 Julian day, a form mpedb does not support",
                other.type_name()
            )))
        }
    };
    let mut x = parse_date_or_time(z)?;
    x.compute_jd();
    x.compute_ymd();
    x.compute_hms();
    Ok(match f {
        F::Date => Value::Text(format!("{}-{:02}-{:02}", date_year(x.y), x.mo, x.d)),
        F::Time => Value::Text(format!("{:02}:{:02}:{:02}", x.h, x.mi, x.s as i32)),
        F::DateTime => Value::Text(format!(
            "{}-{:02}-{:02} {:02}:{:02}:{:02}",
            date_year(x.y),
            x.mo,
            x.d,
            x.h,
            x.mi,
            x.s as i32
        )),
        // sqlite: `sqlite3_result_double(context, x.iJD/86400000.0)`.
        F::JulianDay => Value::Float(x.ijd as f64 / 86_400_000.0),
        _ => return Err(Error::Internal("not a date-family function".into())),
    })
}

/// sqlite's `isDate` for the one-argument (no-modifier) case.
fn parse_date_or_time(z: &str) -> Result<Dt> {
    let b = z.as_bytes();
    let mut p = Dt::new();
    if !parse_yyyy_mm_dd(b, &mut p) {
        let mut q = Dt::new();
        if !parse_hh_mm_ss(b, &mut q) {
            return Err(unsupported_time(z));
        }
        p = q;
    }
    if !p.compute_jd() || !(0..=MAX_JD_MS).contains(&p.ijd) {
        return Err(unsupported_time(z));
    }
    // sqlite: "make sure a YYYY-MM-DD is normalized" — 2023-02-31 → 2023-03-03.
    // Only Y/M/D is invalidated; the h/m/s keep their parsed spelling.
    if p.valid_ymd && p.d > 28 {
        p.valid_ymd = false;
    }
    Ok(p)
}

/// `%.16g` / `%06.3f` go through the sqlite printf port so their digits are
/// byte-identical to sqlite's own formatter.
fn printf1(fmt: &str, v: f64) -> Result<String> {
    match sqlite_printf(&[Value::Text(fmt.to_string()), Value::Float(v)])? {
        Value::Text(s) => Ok(s),
        _ => Err(Error::TypeMismatch("strftime(): internal format failure".into())),
    }
}

pub(super) fn sqlite_strftime(args: &[Value]) -> Result<Value> {
    if args.len() > 2 {
        return Err(unsupported_modifiers("strftime", args.len() - 2));
    }
    let fmt = match &args[0] {
        Value::Text(s) => s.as_str(),
        other => {
            return Err(Error::TypeMismatch(format!(
                "strftime() format must be text, got {}",
                other.type_name()
            )))
        }
    };
    let z = match &args[1] {
        Value::Text(s) => s.as_str(),
        other => {
            return Err(Error::TypeMismatch(format!(
                "strftime() time value must be text, got {} — sqlite would read a number as a \
                 Julian day, a form mpedb does not support",
                other.type_name()
            )))
        }
    };
    let mut x = parse_date_or_time(z)?;
    x.compute_jd();
    x.compute_ymd();
    x.compute_hms();

    let f = fmt.as_bytes();
    let mut out = String::with_capacity(fmt.len() + 16);
    let mut i = 0usize;
    while i < f.len() {
        if f[i] != b'%' {
            // Copy the byte run up to the next '%' verbatim (UTF-8 safe: a '%'
            // can never be a continuation byte).
            let start = i;
            while i < f.len() && f[i] != b'%' {
                i += 1;
            }
            out.push_str(&fmt[start..i]);
            continue;
        }
        i += 1;
        let cf = match f.get(i) {
            Some(c) => *c,
            None => {
                return Err(Error::TypeMismatch(
                    "strftime(): the format string ends in a bare '%'".into(),
                ))
            }
        };
        i += 1;
        match cf {
            b'd' => out.push_str(&format!("{:02}", x.d)),
            b'e' => out.push_str(&format!("{:2}", x.d)),
            b'f' => {
                let s = if x.s > 59.999 { 59.999 } else { x.s };
                out.push_str(&printf1("%06.3f", s)?);
            }
            b'F' => out.push_str(&format!("{:04}-{:02}-{:02}", x.y, x.mo, x.d)),
            b'H' => out.push_str(&format!("{:02}", x.h)),
            b'k' => out.push_str(&format!("{:2}", x.h)),
            b'I' | b'l' => {
                let mut h = x.h;
                if h > 12 {
                    h -= 12;
                }
                if h == 0 {
                    h = 12;
                }
                if cf == b'I' {
                    out.push_str(&format!("{h:02}"));
                } else {
                    out.push_str(&format!("{h:2}"));
                }
            }
            b'j' | b'W' => {
                // Days since Jan 1 of the same year, at the same time of day:
                // the h/m/s contribution cancels between the two Julian days.
                let mut y = x;
                y.valid_jd = false;
                y.mo = 1;
                y.d = 1;
                y.valid_ymd = true;
                if !y.compute_jd() {
                    return Err(unsupported_time(z));
                }
                let n_day = (x.ijd - y.ijd + 43_200_000) / 86_400_000;
                if cf == b'W' {
                    // 0 = Monday … 6 = Sunday.
                    let wd = ((x.ijd + 43_200_000) / 86_400_000).rem_euclid(7);
                    out.push_str(&format!("{:02}", (n_day + 7 - wd) / 7));
                } else {
                    out.push_str(&format!("{:03}", n_day + 1));
                }
            }
            b'J' => out.push_str(&printf1("%.16g", x.ijd as f64 / 86_400_000.0)?),
            b'm' => out.push_str(&format!("{:02}", x.mo)),
            b'M' => out.push_str(&format!("{:02}", x.mi)),
            b'p' => out.push_str(if x.h >= 12 { "PM" } else { "AM" }),
            b'P' => out.push_str(if x.h >= 12 { "pm" } else { "am" }),
            b'R' => out.push_str(&format!("{:02}:{:02}", x.h, x.mi)),
            b's' => out.push_str(&(x.ijd / 1000 - 21_086_676 * 10_000).to_string()),
            b'S' => out.push_str(&format!("{:02}", x.s as i32)),
            b'T' => out.push_str(&format!("{:02}:{:02}:{:02}", x.h, x.mi, x.s as i32)),
            b'u' | b'w' => {
                let c = ((x.ijd + 129_600_000) / 86_400_000).rem_euclid(7);
                let c = if c == 0 && cf == b'u' { 7 } else { c };
                out.push_str(&c.to_string());
            }
            b'Y' => out.push_str(&format!("{:04}", x.y)),
            b'%' => out.push('%'),
            other => {
                let shown = if other.is_ascii_graphic() {
                    format!("%{}", other as char)
                } else {
                    format!("%\\x{other:02x}")
                };
                return Err(Error::TypeMismatch(format!(
                    "strftime(): unsupported format specifier '{shown}'; mpedb supports \
                     %d %e %f %F %H %I %j %J %k %l %m %M %p %P %R %s %S %T %u %w %W %Y %%"
                )));
            }
        }
    }
    Ok(Value::Text(out))
}
