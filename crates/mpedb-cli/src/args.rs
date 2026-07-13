//! Hand-rolled `--flag value` / `--flag=value` / `--switch` parsing for the
//! subcommands that take options. Unknown flags are usage errors.

use std::collections::{HashMap, HashSet};

use crate::util::{usage, Failure};

pub struct Parsed {
    pub positional: Vec<String>,
    values: HashMap<String, String>,
    switches: HashSet<String>,
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
    pub fn has(&self, name: &str) -> bool {
        self.switches.contains(name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
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
