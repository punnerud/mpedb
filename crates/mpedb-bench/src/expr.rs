//! Where does a residual filter's time actually go?
//!
//! `benchmarks/olap.md` records the gap this exists to explain: on a 2M-row
//! fact table, `scan-sum` costs mpedb 30.9 ms and `scan-filter-sum` 120.0 ms.
//! Adding the filter costs 89 ms, against SQLite's 25 and DuckDB's 1.6 — and
//! mpedb is otherwise within 1–2× of SQLite, so that ratio is the outlier
//! worth understanding before anyone optimises anything.
//!
//! Two answers were on the table: copy-and-patch JIT (malisper's 5 µs
//! stencils, 11–18× over an interpreter) and vectorised kernels (DuckDB's).
//! Both accelerate a loop; neither helps if the loop's cost is not where it is
//! assumed to be. So measure first.
//!
//! **The decomposition.** Three arms run the SAME predicate with the SAME
//! opcode sequence, varying only what a stack slot holds:
//!
//! | arm | stack slot | models |
//! |---|---|---|
//! | `value-stack` | `Value` (32 B, `needs_drop`) | the engine today |
//! | `f64-stack` | `f64` (8 B, no drop) | type specialisation |
//! | `closure` | nothing — operands in registers | the JIT/hand-written ceiling |
//!
//! Dispatch is IDENTICAL in the first two: the same match over the same
//! opcodes, in the same order. So `value − f64` is what boxing costs and
//! specialisation would recover, and `f64 − closure` is what remains for a
//! JIT or a vectoriser to attack. That second number is the one that decides
//! whether copy-and-patch is worth its per-architecture stencils.
//!
//! Arms are interleaved inside one loop, as `extents.rs` does, so host drift
//! moves all three together rather than whichever ran first.

use mpedb_types::{ExprProgram, Value};
use std::time::Instant;

/// `lon >= a AND lon <= b AND ts >= c AND ts <= d` — the residual `q=omraade`
/// leaves after the index takes `lat`, and the shape `scan-filter-sum` uses.
/// Eleven opcodes: four pushes of a column, four of a constant, four
/// comparisons, three ANDs.
fn predicate_program() -> ExprProgram {
    use mpedb_types::Instr::*;
    ExprProgram::new(
        vec![
            PushCol(0), PushConst(0), Ge,
            PushCol(0), PushConst(1), Le,
            And,
            PushCol(1), PushConst(2), Ge,
            PushCol(1), PushConst(3), Le,
            And,
            And,
        ],
        vec![
            Value::Float(10.80),
            Value::Float(10.95),
            Value::Float(1.6e9),
            Value::Float(1.7e9),
        ],
    )
    .expect("the program is well-formed")
}

/// The same opcodes over an `f64` stack. Deliberately a copy of the engine's
/// shape rather than something cleverer: if this were written better than the
/// real interpreter in any way OTHER than the slot type, the difference would
/// measure the rewrite instead of the boxing.
#[derive(Clone, Copy)]
enum FOp {
    PushCol(u16),
    PushConst(u16),
    Ge,
    Le,
    And,
}

fn f64_eval(ops: &[FOp], consts: &[f64], cols: &[f64], stack: &mut Vec<f64>) -> bool {
    stack.clear();
    for op in ops {
        match *op {
            FOp::PushCol(i) => stack.push(cols[i as usize]),
            FOp::PushConst(i) => stack.push(consts[i as usize]),
            FOp::Ge => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(if a >= b { 1.0 } else { 0.0 });
            }
            FOp::Le => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(if a <= b { 1.0 } else { 0.0 });
            }
            FOp::And => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(if a != 0.0 && b != 0.0 { 1.0 } else { 0.0 });
            }
        }
    }
    stack.pop().unwrap() != 0.0
}

fn f64_program() -> (Vec<FOp>, Vec<f64>) {
    use FOp::*;
    (
        vec![
            PushCol(0), PushConst(0), Ge,
            PushCol(0), PushConst(1), Le,
            And,
            PushCol(1), PushConst(2), Ge,
            PushCol(1), PushConst(3), Le,
            And,
            And,
        ],
        vec![10.80, 10.95, 1.6e9, 1.7e9],
    )
}

/// Rows shaped like `spor`: a longitude around Oslo and a timestamp, half of
/// them inside the predicate so neither branch direction is free.
fn rows(n: usize) -> (Vec<Vec<Value>>, Vec<[f64; 2]>) {
    let mut v = Vec::with_capacity(n);
    let mut f = Vec::with_capacity(n);
    for i in 0..n {
        let lon = 10.70 + (i % 400) as f64 * 0.001;
        let ts = 1.55e9 + (i % 500) as f64 * 1e6;
        v.push(vec![Value::Float(lon), Value::Float(ts)]);
        f.push([lon, ts]);
    }
    (v, f)
}

/// The bench measured against itself: the same arm over programs of growing
/// length. Time must be linear in opcodes, and the intercept is the per-call
/// cost that belongs to neither dispatch nor boxing. A bench whose slope is
/// flat is not measuring what it claims to.
fn scaling_control(vrows: &[Vec<Value>], reps: usize) {
    use mpedb_types::Instr::*;
    println!("\n  Kontroll: skalerer benken med programlengde?");
    println!("  {:<22} {:>8} {:>12}", "opkoder", "ns/rad", "ns/opkode");
    let mut prev: Option<(usize, f64)> = None;
    // 3, 7, 11, 15 opcodes — one comparison added each time.
    for terms in 1..=4usize {
        let mut instrs = Vec::new();
        for t in 0..terms {
            instrs.push(PushCol((t % 2) as u16));
            instrs.push(PushConst((t % 4) as u16));
            instrs.push(Ge);
            if t > 0 {
                instrs.push(And);
            }
        }
        let n_ops = instrs.len();
        let prog = ExprProgram::new(
            instrs,
            vec![
                Value::Float(10.80),
                Value::Float(10.95),
                Value::Float(1.6e9),
                Value::Float(1.7e9),
            ],
        )
        .expect("well-formed");
        let mut stack: Vec<Value> = Vec::with_capacity(prog.max_stack());
        let mut best = f64::MAX;
        for _ in 0..reps {
            let t0 = Instant::now();
            let mut kept = 0usize;
            for r in vrows {
                if prog.eval_filter(&mut stack, r, &[]).unwrap_or(false) {
                    kept += 1;
                }
            }
            std::hint::black_box(kept);
            best = best.min(t0.elapsed().as_secs_f64());
        }
        let ns = best / vrows.len() as f64 * 1e9;
        let slope = match prev {
            Some((po, pn)) => format!("{:.1}", (ns - pn) / (n_ops - po) as f64),
            None => "—".to_string(),
        };
        println!("  {:<22} {:>8.1} {:>12}", n_ops, ns, slope);
        prev = Some((n_ops, ns));
    }
}

/// The 11.3 ns per opcode, taken apart.
///
/// Knowing that boxing costs ten nanoseconds an opcode does not say WHICH part
/// of boxing: the 32-byte move, the `clone` call, the drop glue, or `sql_cmp`'s
/// walk over the type cross-product. Each is a different fix, so each is timed
/// on its own here rather than argued about.
fn opcode_parts(reps: usize, n: usize) {
    let a = Value::Float(10.83);
    let b = Value::Float(10.90);
    let text = Value::Text("4a9f01".to_string());

    let mut best = [f64::MAX; 7];
    for _ in 0..reps {
        // 1. clone a numeric Value — what PushCol/PushConst do
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(a.clone());
        }
        best[0] = best[0].min(t.elapsed().as_secs_f64());

        // 2. clone a TEXT Value — the same opcode when the column is text
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(text.clone());
        }
        best[1] = best[1].min(t.elapsed().as_secs_f64());

        // 3. sql_cmp on two numerics
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(a.sql_cmp(&b).unwrap());
        }
        best[2] = best[2].min(t.elapsed().as_secs_f64());

        // 4. a Vec<Value> push+pop round trip — the move and the drop glue
        let mut v: Vec<Value> = Vec::with_capacity(4);
        let t = Instant::now();
        for _ in 0..n {
            v.push(a.clone());
            std::hint::black_box(v.pop());
        }
        best[3] = best[3].min(t.elapsed().as_secs_f64());

        // 5b. is the cost the VARIANT or the enum machinery? If Null clones
        // as slowly as Float, nothing about the payload explains it.
        let nul = Value::Null;
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(nul.clone());
        }
        best[5] = best[5].min(t.elapsed().as_secs_f64());

        // 5c. a plain 32-byte struct with no drop and no enum — the floor for
        // moving the same number of bytes.
        // The field is never read on purpose — the point is the MOVE, and
        // reading it would measure a load as well.
        #[derive(Clone, Copy)]
        struct Bytes32(#[allow(dead_code)] [u64; 4]);
        let raw = Bytes32([1, 2, 3, 4]);
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(raw);
        }
        best[6] = best[6].min(t.elapsed().as_secs_f64());

        // 5. the same round trip with f64 — the floor for a stack operation
        let mut f: Vec<f64> = Vec::with_capacity(4);
        let t = Instant::now();
        for _ in 0..n {
            f.push(10.83);
            std::hint::black_box(f.pop());
        }
        best[4] = best[4].min(t.elapsed().as_secs_f64());
    }
    let ns = |i: usize| best[i] / n as f64 * 1e9;
    println!("\n  Hvor gaar de ~11 ns per opkode?");
    println!("  {:<34} {:>8}", "del", "ns");
    for (i, name) in [
        "clone Value::Float",
        "clone Value::Text (heap)",
        "sql_cmp(Float, Float)",
        "Vec<Value> push+pop",
        "Vec<f64> push+pop",
        "clone Value::Null",
        "kopier 32 byte (ingen enum)",
    ]
    .iter()
    .enumerate()
    {
        println!("  {:<34} {:>8.2}", name, ns(i));
    }
}

pub fn run(rows_n: usize, reps: usize) {
    let prog = predicate_program();
    let (fops, fconsts) = f64_program();
    let (vrows, frows) = rows(rows_n);

    let mut vstack: Vec<Value> = Vec::with_capacity(prog.max_stack());
    let mut fstack: Vec<f64> = Vec::with_capacity(16);

    let (mut t_value, mut t_f64, mut t_closure) = (f64::MAX, f64::MAX, f64::MAX);
    // Kept and summed so nothing can be optimised away, and so a wrong arm
    // shows up as a different count rather than as a suspiciously fast one.
    let (mut k_value, mut k_f64, mut k_closure) = (0usize, 0usize, 0usize);

    for _ in 0..reps {
        // --- the engine as it is ---
        let t = Instant::now();
        let mut kept = 0usize;
        for r in &vrows {
            if prog.eval_filter(&mut vstack, r, &[]).unwrap_or(false) {
                kept += 1;
            }
        }
        t_value = t_value.min(t.elapsed().as_secs_f64());
        k_value = kept;

        // --- same opcodes, 8-byte slots ---
        let t = Instant::now();
        let mut kept = 0usize;
        for r in &frows {
            if f64_eval(&fops, &fconsts, r, &mut fstack) {
                kept += 1;
            }
        }
        t_f64 = t_f64.min(t.elapsed().as_secs_f64());
        k_f64 = kept;

        // --- no interpreter at all ---
        let t = Instant::now();
        let mut kept = 0usize;
        for r in &frows {
            if r[0] >= 10.80 && r[0] <= 10.95 && r[1] >= 1.6e9 && r[1] <= 1.7e9 {
                kept += 1;
            }
        }
        t_closure = t_closure.min(t.elapsed().as_secs_f64());
        k_closure = kept;
    }

    assert_eq!(k_value, k_f64, "the arms must agree on which rows pass");
    assert_eq!(k_value, k_closure, "the arms must agree on which rows pass");

    let per = |t: f64| t / rows_n as f64 * 1e9;
    let (pv, pf, pc) = (per(t_value), per(t_f64), per(t_closure));
    println!("\nUttrykksevaluering — {rows_n} rader, {reps} runder, beste av hver");
    println!("  {} rader passerte filteret\n", k_value);
    println!("  {:<26} {:>9} {:>10}", "arm", "ns/rad", "mot topp");
    println!("  {:<26} {:>9.1} {:>9.1}x", "value-stack (motoren)", pv, pv / pc);
    println!("  {:<26} {:>9.1} {:>9.1}x", "f64-stack (typet)", pf, pf / pc);
    println!("  {:<26} {:>9.1} {:>9.1}x", "closure (taket)", pc, 1.0);
    println!();
    let boxing = pv - pf;
    let interp = pf - pc;
    let total = pv - pc;
    println!("  innpakning i Value   {:>7.1} ns/rad  ({:.0} % av avstanden til taket)",
             boxing, 100.0 * boxing / total);
    println!("  tolkens rest         {:>7.1} ns/rad  ({:.0} %)",
             interp, 100.0 * interp / total);
    println!();
    scaling_control(&vrows, reps);
    opcode_parts(reps, 2_000_000);
    println!();
    println!("  Den andre linja er alt en JIT eller en vektoriserer kan angripe.");
    println!("  Er den liten, er copy-and-patch avlyst uansett hva den koster.");
}
