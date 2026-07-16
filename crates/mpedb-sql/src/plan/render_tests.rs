use super::*;
use mpedb_types::{ScalarFn, Value};

fn r(instrs: Vec<Instr>, consts: Vec<Value>) -> String {
    let p = ExprProgram::new(instrs, consts).unwrap();
    render_program(&p, &|c| format!("c{c}"))
}

/// EXPLAIN and SELECT column names both render the COMPILED program, so an
/// instruction the renderer does not know does not merely look odd — the old
/// catch-all treated every unknown op as a binary operator and popped TWO
/// operands, corrupting the stack for everything after it. `lower(name)`
/// came out as `? ? name`.
#[test]
fn every_instruction_renders_without_eating_the_stack() {
    assert_eq!(
        r(vec![Instr::PushCol(0), Instr::Call(ScalarFn::Lower, 1)], vec![]),
        "lower(c0)"
    );
    assert_eq!(
        r(
            vec![
                Instr::PushCol(0),
                Instr::PushConst(0),
                Instr::PushConst(1),
                Instr::Call(ScalarFn::Substr, 3)
            ],
            vec![Value::Int(1), Value::Int(2)]
        ),
        "substr(c0, 1, 2)"
    );
    assert_eq!(
        r(
            vec![
                Instr::PushCol(0),
                Instr::PushConst(0),
                Instr::PushConst(1),
                Instr::InList(2)
            ],
            vec![Value::Int(1), Value::Int(2)]
        ),
        "c0 IN (1, 2)"
    );
    assert_eq!(
        r(vec![Instr::PushCol(0), Instr::InParam(0)], vec![]),
        "c0 IN ($1)"
    );
}

/// A program with jumps cannot be rendered by walking the stack — that is
/// decompilation. The first attempt tried, and rendered
/// `coalesce(name, 'd')` as `'d'`: the last arm's constant, presented as the
/// whole expression. EXPLAIN exists to tell you what will run, so a
/// confident wrong answer there is worse than no answer.
#[test]
fn control_flow_renders_as_an_honest_marker_not_a_plausible_lie() {
    // coalesce(c0, 'd') exactly as the binder emits it:
    //   0 PushCol          depth 1
    //   1 JumpIfNotNull(4) peeks -> jumps to the END with the value still on
    //   2 Pop              the NULL is discarded
    //   3 PushConst('d')   depth 1 again, so both paths agree at 4
    // Writing JumpIfNotNull(3) instead was rejected outright by the
    // verifier ("stack depth disagrees at instruction 3") — which is the
    // depth analysis earning its keep on a hand-written program.
    let out = r(
        vec![
            Instr::PushCol(0),
            Instr::JumpIfNotNull(4),
            Instr::Pop,
            Instr::PushConst(0),
        ],
        vec![Value::Text("d".into())],
    );
    assert_eq!(out, "<conditional>");
    // The old renderer produced exactly `'d'` here — the last arm's constant,
    // presented as the whole expression.
    assert!(
        !out.contains("'d'"),
        "must not present one arm as the whole expression, got {out}"
    );
}
