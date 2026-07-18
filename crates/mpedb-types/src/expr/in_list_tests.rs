use super::*;

fn prog(instrs: Vec<Instr>, consts: Vec<Value>) -> ExprProgram {
    ExprProgram::new(instrs, consts).unwrap()
}

/// `InList` and `InParam` must agree on 3VL exactly — they share
/// `in_items_3vl` for that reason, and this pins that they cannot drift.
#[test]
fn in_list_and_in_param_give_identical_3vl() {
    let cases: Vec<(Value, Vec<Value>, Option<bool>)> = vec![
        (Value::Int(2), vec![Value::Int(1), Value::Int(2)], Some(true)),
        (Value::Int(3), vec![Value::Int(1), Value::Int(2)], Some(false)),
        // no match but a NULL is present -> unknown, NOT false
        (Value::Int(3), vec![Value::Int(1), Value::Null], None),
        // a match wins even alongside a NULL
        (Value::Int(1), vec![Value::Int(1), Value::Null], Some(true)),
        // NULL probe is never TRUE
        (Value::Null, vec![Value::Int(1)], None),
    ];
    for (probe, items, want) in cases {
        let via_list = prog(
            {
                let mut i = vec![Instr::PushConst(0)];
                for n in 0..items.len() {
                    i.push(Instr::PushConst(1 + n as u16));
                }
                i.push(Instr::InList(items.len() as u16));
                i
            },
            std::iter::once(probe.clone()).chain(items.clone()).collect(),
        )
        .eval(&[], &[])
        .unwrap();

        let via_param = prog(vec![Instr::PushConst(0), Instr::InParam(0)], vec![probe.clone()])
            .eval(&[], &[Value::List(items.clone())])
            .unwrap();

        let got = match &via_list {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            v => panic!("non-bool {v:?}"),
        };
        assert_eq!(got, want, "InList({probe:?}, {items:?})");
        assert_eq!(via_list, via_param, "the two IN forms disagree on {probe:?} in {items:?}");
    }
}

/// `x IN ()` — the empty set — is FALSE for every probe, NULL included
/// (SQL 3VL). A zero-arity InList pops the probe and pushes that FALSE, so
/// the verifier ACCEPTS the program and eval yields Bool(false), never the
/// probe left posing as a bool.
#[test]
fn zero_arity_in_list_is_empty_set_false() {
    for probe in [Value::Int(5), Value::Null] {
        let got = prog(vec![Instr::PushConst(0), Instr::InList(0)], vec![probe.clone()])
            .eval(&[], &[])
            .unwrap();
        assert_eq!(got, Value::Bool(false), "{probe:?} IN () must be FALSE");
    }
}

#[test]
fn in_list_underflow_is_corrupt() {
    // claims 3 elements but only 2 values are pushed
    let r = ExprProgram::new(
        vec![Instr::PushCol(0), Instr::PushCol(1), Instr::InList(3)],
        vec![],
    );
    assert!(matches!(r, Err(Error::Corrupt(_))), "got {r:?}");
}

#[test]
fn in_list_round_trips_through_the_codec() {
    let p = prog(
        vec![
            Instr::PushCol(0),
            Instr::PushConst(0),
            Instr::PushConst(1),
            Instr::InList(2),
        ],
        vec![Value::Int(1), Value::Int(2)],
    );
    let mut bytes = Vec::new();
    p.encode_into(&mut bytes);
    let mut pos = 0;
    assert_eq!(ExprProgram::decode(&bytes, &mut pos).unwrap(), p);
    assert_eq!(pos, bytes.len());
    // truncation at every offset must be Corrupt, never a panic (repo rule)
    for n in 0..bytes.len() {
        let mut pos = 0;
        assert!(
            ExprProgram::decode(&bytes[..n], &mut pos).is_err(),
            "truncation at {n} decoded"
        );
    }
}
