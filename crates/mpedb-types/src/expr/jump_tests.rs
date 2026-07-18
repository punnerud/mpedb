use super::*;

/// A backward jump would let a per-row expression loop forever. `eval` runs
/// inside the engine with no fuel counter, so a crafted or corrupt plan
/// could hang a reader. The verifier must refuse it, not the evaluator.
#[test]
fn backward_jump_is_refused_so_programs_terminate() {
    let r = ExprProgram::new(
        vec![Instr::PushConst(0), Instr::Jump(0)],
        vec![Value::Bool(true)],
    );
    assert!(
        matches!(&r, Err(Error::Corrupt(m)) if m.contains("terminate")),
        "got {r:?}"
    );
    // self-jump is backward too (t <= i)
    let r = ExprProgram::new(vec![Instr::PushConst(0), Instr::Jump(1)], vec![Value::Null]);
    assert!(matches!(r, Err(Error::Corrupt(_))), "got {r:?}");
}

/// If two paths reach an instruction at different depths, the stack means
/// different things depending on the row's data — and `max_stack` stops
/// being a bound.
#[test]
fn disagreeing_stack_depth_at_a_merge_is_corrupt() {
    // path A leaves 2 values at index 4, path B leaves 1
    let r = ExprProgram::new(
        vec![
            Instr::PushConst(0),        // 0: cond
            Instr::JumpIfNotTrue(4),    // 1: -> 4 with depth 0
            Instr::PushConst(1),        // 2: depth 1
            Instr::PushConst(1),        // 3: depth 2  (falls through to 4)
            Instr::PushConst(1),        // 4: merge point — disagreement
        ],
        vec![Value::Bool(true), Value::Int(1)],
    );
    assert!(
        matches!(&r, Err(Error::Corrupt(m)) if m.contains("disagrees")),
        "got {r:?}"
    );
}

#[test]
fn jump_past_the_end_and_unreachable_code_are_corrupt() {
    let r = ExprProgram::new(
        vec![Instr::PushConst(0), Instr::Jump(99)],
        vec![Value::Int(1)],
    );
    assert!(matches!(&r, Err(Error::Corrupt(m)) if m.contains("past end")), "got {r:?}");

    // instruction 2 can never be reached: nothing falls into it or jumps to it
    let r = ExprProgram::new(
        vec![Instr::PushConst(0), Instr::Jump(3), Instr::Neg, Instr::PushConst(0)],
        vec![Value::Int(1)],
    );
    assert!(matches!(&r, Err(Error::Corrupt(m)) if m.contains("unreachable")), "got {r:?}");
}

/// The shape the CASE codegen actually emits, evaluated both ways.
#[test]
fn case_shaped_program_evaluates_both_branches() {
    // CASE WHEN col0 THEN 10 ELSE 20 END
    let p = ExprProgram::new(
        vec![
            Instr::PushCol(0),       // 0
            Instr::JumpIfNotTrue(4), // 1 -> else
            Instr::PushConst(0),     // 2: 10
            Instr::Jump(5),          // 3 -> end
            Instr::PushConst(1),     // 4: 20
        ],
        vec![Value::Int(10), Value::Int(20)],
    )
    .unwrap();
    assert_eq!(p.eval(&[Value::Bool(true)], &[]).unwrap(), Value::Int(10));
    assert_eq!(p.eval(&[Value::Bool(false)], &[]).unwrap(), Value::Int(20));
    // NULL must take the ELSE arm, not the THEN arm
    assert_eq!(p.eval(&[Value::Null], &[]).unwrap(), Value::Int(20));

    // and it must survive the codec, truncation included
    let mut bytes = Vec::new();
    p.encode_into(&mut bytes);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&bytes, &mut pos).unwrap(), p);
    for n in 0..bytes.len() {
        let mut pos = 0;
        assert!(ExprProgram::decode(&bytes[..n], &mut pos).is_err(), "truncation at {n}");
    }
}
