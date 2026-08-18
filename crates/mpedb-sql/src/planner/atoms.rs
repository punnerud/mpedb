//! Conjunct/atom utilities for access-path extraction (moved verbatim from
//! `planner/mod.rs`).

use super::*;

// ---- access-path extraction -------------------------------------------------

/// A `col <op> atom` conjunct usable for key extraction.
#[derive(Clone)]
pub(super) enum Atom {
    Param(u16),
    Const(Value),
}

impl Atom {
    pub(super) fn to_key_part(&self, consts: &mut Vec<Value>) -> Result<KeyPart> {
        Ok(match self {
            Atom::Param(i) => KeyPart::Param(*i),
            Atom::Const(v) => KeyPart::Const(push_plan_const(consts, v.clone())?),
        })
    }
}

pub(super) fn as_atom(e: &BExpr) -> Option<Atom> {
    match e {
        BExpr::Param(i) => Some(Atom::Param(*i)),
        // NULL never matches a key (PK/unique probes are on non-null values);
        // leave such conjuncts in the residual filter.
        BExpr::Const(v) if !v.is_null() => Some(Atom::Const(v.clone())),
        _ => None,
    }
}

/// `col <cmp> atom` (either operand order; op flipped when reversed).
/// Also matches [`BExpr::ClassCmp`] (inequality with free params) so a
/// `pk >= $1` bound still becomes a PkRange after the float-param compare fix.
pub(super) fn as_col_cmp(e: &BExpr) -> Option<(u16, BinOp, Atom)> {
    let flipped = |op: BinOp| match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    };
    let (op, l, r) = match e {
        BExpr::Binary(op, l, r) => (*op, l.as_ref(), r.as_ref()),
        BExpr::ClassCmp(op, l, r, _, _) => (*op, l.as_ref(), r.as_ref()),
        _ => return None,
    };
    match (l, r) {
        (BExpr::Col(c), rhs) => as_atom(rhs).map(|a| (*c, op, a)),
        (lhs, BExpr::Col(c)) => as_atom(lhs).map(|a| (*c, flipped(op), a)),
        _ => None,
    }
}

/// AND a conjunct list back together, preserving order. `None` for an empty
/// list — the callers all mean "no predicate" by that.
pub(super) fn and_all(conjuncts: Vec<BExpr>) -> Option<BExpr> {
    conjuncts.into_iter().reduce(and)
}

/// The highest column slot an expression reads, or `None` for a column-free
/// expression (consts/params only). What the #65 pushdown places conjuncts
/// by: left-deep prefixes share slot numbering, so a conjunct is evaluable
/// at exactly the steps whose accumulated width exceeds this.
pub(super) fn max_col(e: &BExpr) -> Option<u16> {
    let mut m: Option<u16> = None;
    let mut stack = vec![e];
    while let Some(e) = stack.pop() {
        match e {
            BExpr::Col(c) => m = Some(m.map_or(*c, |p| p.max(*c))),
            BExpr::Unary(_, a)
            | BExpr::Like(a, _, _, _)
            | BExpr::Glob(a, _)
            | BExpr::Regexp(a, _)
            | BExpr::Cast(a, _)
            | BExpr::CastPg(a, _)
            | BExpr::InParam(a, _)
            | BExpr::InParamColl(a, _, _) => stack.push(a),
            BExpr::Binary(_, a, b)
            | BExpr::IsDistinct(a, b, _)
            | BExpr::CollateCmp(_, a, b, _)
            | BExpr::RegexpDyn(a, b)
            | BExpr::LikeDyn(a, b, _, _)
            | BExpr::GlobDyn(a, b)
            | BExpr::ClassCmp(_, a, b, _, _) => {
                stack.push(a);
                stack.push(b);
            }
            BExpr::InList(a, list) | BExpr::InListColl(a, list, _) => {
                stack.push(a);
                stack.extend(list.iter());
            }
            BExpr::Case(arms, else_) => {
                for (c, r) in arms {
                    stack.push(c);
                    stack.push(r);
                }
                if let Some(e) = else_ {
                    stack.push(e);
                }
            }
            BExpr::ConcatN(args)
            | BExpr::Coalesce(args)
            | BExpr::Call(_, args)
            | BExpr::CallColl(_, args, _)
            | BExpr::HostCall { args, .. }
            | BExpr::SpellCall { args, .. } => {
                stack.extend(args.iter())
            }
            BExpr::Const(_) | BExpr::Param(_) => {}
        }
    }
    m
}

pub(super) fn split_and(e: BExpr, out: &mut Vec<BExpr>) {
    match e {
        BExpr::Binary(BinOp::And, l, r) => {
            split_and(*l, out);
            split_and(*r, out);
        }
        other => out.push(other),
    }
}


/// Re-AND the unconsumed conjuncts, preserving statement order.
pub(super) fn rebuild_residual(conjuncts: Vec<BExpr>, consumed: &[bool]) -> Option<BExpr> {
    let mut rest = conjuncts
        .into_iter()
        .zip(consumed)
        .filter_map(|(c, &used)| if used { None } else { Some(c) });
    let first = rest.next()?;
    Some(rest.fold(first, |acc, c| {
        BExpr::Binary(BinOp::And, Box::new(acc), Box::new(c))
    }))
}

