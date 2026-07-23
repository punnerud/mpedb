//! Exact sequencing — the `(subset, last)` mode of the MPEE kernel
//! (design/DESIGN-MPEE-GENERAL.md §9.2, stage M4).
//!
//! The join-order solver's subset DP deliberately has NO last-element state:
//! a join step's cost depends only on the placed SET, which is a stronger
//! property than routing has. Sequencing with ADDITIVE PAIRWISE costs — order
//! N stops to minimize Σ cost(prev, next) — needs the weaker classic state,
//! `(visited subset, last stop)`: Held-Karp. Exact, and exactly the ground
//! truth the routing benchmark scores heuristics against; the price is
//! O(2^n · n²) time and O(2^n · n) memory, which is why `n` is capped where
//! the tables stop fitting in tens of megabytes. Beyond the cap this
//! DECLINES — a heuristic's regime must be entered knowingly, not by a
//! silent fallback that stops being exact.
//!
//! Costs are exact `i64`s, not magnitude buckets: nothing here enters plan
//! bytes or a content hash, so the stability law that quantizes the join
//! solver's inputs does not apply. What DOES carry over is the purchase
//! discipline: the solver buys each matrix cell exactly once through the
//! oracle (`cells_bought` says how many), so a caller with a remote or
//! compressed matrix pays N·(N−1) reads and not one per DP transition.

/// Largest solvable instance: 2^(n−1) · n table entries at 8 bytes stays
/// under ~20 MB. Chosen for memory, not time (n = 18 solves in well under a
/// second); raising it is a memory decision, not an algorithmic one.
pub const MAX_SEQUENCE_N: u16 = 18;

/// An exact sequencing answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequenced {
    /// Visit order, starting at node 0. For a roundtrip the implicit final
    /// leg back to 0 is INCLUDED in `cost` but not repeated in `order`.
    pub order: Vec<u16>,
    pub cost: i64,
    /// Distinct matrix cells read through the oracle — the streaming-N×N
    /// number the benchmark reports (exactness costs the full N·(N−1)).
    pub cells_bought: u64,
}

/// Solve the exact minimum-cost sequence over nodes `0..n`, starting at 0 and
/// visiting every node once; `roundtrip` adds the closing leg back to 0
/// (brooom's default shape — a vehicle with `end = start`). `None` when
/// `n` exceeds [`MAX_SEQUENCE_N`] (the decline, never a silent heuristic) or
/// `n == 0`.
pub fn solve_sequence(n: u16, cost: &dyn Fn(u16, u16) -> i64, roundtrip: bool) -> Option<Sequenced> {
    if n == 0 || n > MAX_SEQUENCE_N {
        return None;
    }
    if n == 1 {
        return Some(Sequenced { order: vec![0], cost: 0, cells_bought: 0 });
    }
    let m = (n - 1) as usize; // nodes 1..n, bit i = node i+1
    let full = 1usize << m;

    // Buy every cell once, up front: the DP re-reads pairs freely afterwards.
    let nn = n as usize;
    let mut c = vec![0i64; nn * nn];
    let mut cells = 0u64;
    for i in 0..nn {
        for j in 0..nn {
            if i != j {
                c[i * nn + j] = cost(i as u16, j as u16);
                cells += 1;
            }
        }
    }

    // dp[mask][last] = cheapest path 0 → … → (last+1) visiting exactly the
    // nodes of `mask`; parent[mask][last] = previous node index, for the
    // reconstruction.
    const INF: i64 = i64::MAX / 4;
    let mut dp = vec![INF; full * m];
    let mut parent = vec![u8::MAX; full * m];
    for last in 0..m {
        dp[(1 << last) * m + last] = c[last + 1];
    }
    for mask in 1..full {
        for last in 0..m {
            if mask & (1 << last) == 0 {
                continue;
            }
            let cur = dp[mask * m + last];
            if cur >= INF {
                continue;
            }
            let rest = !mask & (full - 1);
            let mut nxt = rest;
            while nxt != 0 {
                let b = nxt.trailing_zeros() as usize;
                nxt &= nxt - 1;
                let nm = mask | (1 << b);
                let cand = cur + c[(last + 1) * nn + (b + 1)];
                if cand < dp[nm * m + b] {
                    dp[nm * m + b] = cand;
                    parent[nm * m + b] = last as u8;
                }
            }
        }
    }

    // Best terminal (plus the closing leg for a roundtrip).
    let mut best = (INF, 0usize);
    for last in 0..m {
        let mut v = dp[(full - 1) * m + last];
        if roundtrip {
            v = v.saturating_add(c[(last + 1) * nn]);
        }
        if v < best.0 {
            best = (v, last);
        }
    }

    // Reconstruct: walk parents from the terminal.
    let mut order = Vec::with_capacity(nn);
    let mut mask = full - 1;
    let mut last = best.1;
    while parent[mask * m + last] != u8::MAX {
        order.push((last + 1) as u16);
        let p = parent[mask * m + last] as usize;
        mask &= !(1 << last);
        last = p;
    }
    order.push((last + 1) as u16);
    order.push(0);
    order.reverse();
    Some(Sequenced { order, cost: best.0, cells_bought: cells })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute force over every permutation — the oracle for n small enough to
    /// enumerate. Held-Karp must match it exactly, open and closed.
    fn brute(n: u16, cost: &dyn Fn(u16, u16) -> i64, roundtrip: bool) -> i64 {
        fn perms(rest: &mut Vec<u16>, cur: &mut Vec<u16>, best: &mut i64, c: &dyn Fn(u16, u16) -> i64, rt: bool) {
            if rest.is_empty() {
                let mut t = 0i64;
                for w in cur.windows(2) {
                    t += c(w[0], w[1]);
                }
                if rt {
                    t += c(*cur.last().unwrap(), 0);
                }
                *best = (*best).min(t);
                return;
            }
            for i in 0..rest.len() {
                let x = rest.remove(i);
                cur.push(x);
                perms(rest, cur, best, c, rt);
                cur.pop();
                rest.insert(i, x);
            }
        }
        let mut rest: Vec<u16> = (1..n).collect();
        let mut cur = vec![0u16];
        let mut best = i64::MAX;
        perms(&mut rest, &mut cur, &mut best, cost, roundtrip);
        best
    }

    #[test]
    fn held_karp_matches_brute_force() {
        // Deterministic xorshift costs, asymmetric on purpose (real road
        // matrices are), for every n the brute force can afford.
        let mut x = 0x5EED_1234u64;
        let mut rng = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for n in 2..=8u16 {
            let nn = n as usize;
            let m: Vec<i64> = (0..nn * nn).map(|_| (rng() % 1000) as i64).collect();
            let cost = move |i: u16, j: u16| m[i as usize * nn + j as usize];
            for rt in [false, true] {
                let got = solve_sequence(n, &cost, rt).expect("within cap");
                assert_eq!(got.cost, brute(n, &cost, rt), "n={n} rt={rt}");
                // The order must COST what the solver claims.
                let mut t = 0i64;
                for w in got.order.windows(2) {
                    t += cost(w[0], w[1]);
                }
                if rt {
                    t += cost(*got.order.last().unwrap(), 0);
                }
                assert_eq!(t, got.cost, "claimed cost must be the order's cost");
                assert_eq!(got.cells_bought, (nn * nn - nn) as u64);
                // Every node exactly once, starting at 0.
                let mut seen: Vec<u16> = got.order.clone();
                seen.sort_unstable();
                assert_eq!(seen, (0..n).collect::<Vec<_>>());
                assert_eq!(got.order[0], 0);
            }
        }
    }

    #[test]
    fn the_cap_declines_instead_of_guessing() {
        assert!(solve_sequence(MAX_SEQUENCE_N + 1, &|_, _| 1, true).is_none());
        assert!(solve_sequence(0, &|_, _| 1, true).is_none());
    }
}
