//! Hand-rolled `--flag value` / `--flag=value` / `--switch` parsing for the
//! subcommands that take options. Unknown flags are usage errors.

use std::collections::{HashMap, HashSet};

use crate::util::{usage, Failure};

pub struct Parsed {
    pub positional: Vec<String>,
    values: HashMap<String, String>,
    switches: HashSet<String>,
    /// What the caller *declared*, kept so `value`/`has` can catch a flag being
    /// asked for from the wrong list. See [`Parsed::value`].
    declared_values: HashSet<String>,
    declared_switches: HashSet<String>,
}

pub fn parse(
    args: &[String],
    value_flags: &[&str],
    switch_flags: &[&str],
) -> Result<Parsed, Failure> {
    let mut p = Parsed {
        positional: Vec::new(),
        values: HashMap::new(),
        switches: HashSet::new(),
        declared_values: value_flags.iter().map(|s| s.to_string()).collect(),
        declared_switches: switch_flags.iter().map(|s| s.to_string()).collect(),
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let Some(name) = arg.strip_prefix("--") else {
            p.positional.push(arg.clone());
            continue;
        };
        if let Some((n, v)) = name.split_once('=') {
            if value_flags.contains(&n) {
                p.values.insert(n.to_owned(), v.to_owned());
            } else {
                return usage(format!("unknown flag --{n}"));
            }
        } else if value_flags.contains(&name) {
            match it.next() {
                Some(v) => {
                    p.values.insert(name.to_owned(), v.clone());
                }
                None => return usage(format!("--{name} needs a value")),
            }
        } else if switch_flags.contains(&name) {
            p.switches.insert(name.to_owned());
        } else {
            return usage(format!("unknown flag --{name}"));
        }
    }
    Ok(p)
}

impl Parsed {
    /// Was `--name` (a valueless switch) given?
    ///
    /// Panics if `name` was not declared as a switch — same reason as
    /// [`Parsed::value`].
    pub fn has(&self, name: &str) -> bool {
        debug_assert!(
            self.declared_switches.contains(name),
            "--{name} is read with has() but was not declared a switch flag{}",
            if self.declared_values.contains(name) {
                " (it is declared a VALUE flag — use value()/require())"
            } else {
                ""
            }
        );
        self.switches.contains(name)
    }

    /// The value of `--name <v>`, or None if absent.
    ///
    /// **Panics (debug) if `name` was not declared a value flag.** The two
    /// `&[&str]` lists [`parse`] takes are adjacent and same-typed, so putting a
    /// value flag in the switch list is an easy and *silent* mistake: `--size_mb
    /// 64` then parses as a switch, `64` lands in `positional`, and `value()`
    /// returns None — so the command runs with the default and says nothing.
    /// This shipped in three commands (mirror import/roundtrip, crash) and made
    /// `--durability wal` quietly import with durability=none, which is the kind
    /// of bug that only surfaces as data loss. The assert turns the whole class
    /// into a first-run panic.
    pub fn value(&self, name: &str) -> Option<&str> {
        debug_assert!(
            self.declared_values.contains(name),
            "--{name} is read with value() but was not declared a value flag{}",
            if self.declared_switches.contains(name) {
                " (it is declared a SWITCH — move it to the value_flags list)"
            } else {
                ""
            }
        );
        self.values.get(name).map(String::as_str)
    }

    pub fn require(&self, name: &str) -> Result<&str, Failure> {
        self.value(name)
            .ok_or_else(|| Failure::Usage(format!("missing required --{name}")))
    }

    pub fn require_u64(&self, name: &str) -> Result<u64, Failure> {
        self.require(name)?
            .parse()
            .map_err(|_| Failure::Usage(format!("--{name} must be an unsigned integer")))
    }

    pub fn u64_or(&self, name: &str, default: u64) -> Result<u64, Failure> {
        match self.value(name) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| Failure::Usage(format!("--{name} must be an unsigned integer"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn value_and_switch_flags_parse() {
        let p = parse(&argv(&["--dir", "/tmp", "--fast", "--n=3"]), &["dir", "n"], &["fast"])
            .unwrap();
        assert_eq!(p.value("dir"), Some("/tmp"));
        assert_eq!(p.value("n"), Some("3"));
        assert!(p.has("fast"));
    }

    /// The bug this guard exists for: a value flag declared in the switch list
    /// used to parse silently — `--size_mb` became a switch, `64` fell into
    /// `positional`, `value()` returned None, and the command ran with the
    /// default while reporting success. `--durability wal` importing with
    /// durability=none is the worst instance. Now it panics on first use.
    #[test]
    #[cfg_attr(not(debug_assertions), ignore = "guards are debug_assert!, compiled out in release")]
    #[should_panic(expected = "move it to the value_flags list")]
    fn reading_a_switch_as_a_value_panics() {
        let p = parse(&argv(&["--size_mb", "64"]), &[], &["size_mb"]).unwrap();
        let _ = p.value("size_mb");
    }

    #[test]
    #[cfg_attr(not(debug_assertions), ignore = "guards are debug_assert!, compiled out in release")]
    #[should_panic(expected = "use value()/require()")]
    fn reading_a_value_as_a_switch_panics() {
        let p = parse(&argv(&["--dir", "/tmp"]), &["dir"], &[]).unwrap();
        let _ = p.has("dir");
    }

    #[test]
    #[cfg_attr(not(debug_assertions), ignore = "guards are debug_assert!, compiled out in release")]
    #[should_panic(expected = "was not declared")]
    fn reading_an_undeclared_flag_panics() {
        let p = parse(&argv(&[]), &["dir"], &[]).unwrap();
        let _ = p.value("typo");
    }

    #[test]
    fn unknown_flag_is_a_usage_error() {
        assert!(parse(&argv(&["--nope"]), &["dir"], &[]).is_err());
        assert!(parse(&argv(&["--dir"]), &["dir"], &[]).is_err(), "--dir needs a value");
    }
}
