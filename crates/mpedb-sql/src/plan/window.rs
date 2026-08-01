//! Window frames and functions: [`Frame`] + its legality rules, [`WindowFunc`].

use super::*;

/// An explicit window frame (format 36): a unit (`ROWS`/`RANGE`/`GROUPS`) plus a
/// start and end boundary. The offsets are constants baked into the plan bytes,
/// so one content-hashed plan reproduces the same frame in every process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub mode: FrameMode,
    pub start: FrameBound,
    pub end: FrameBound,
    /// Frame EXCLUSION (format 66) — SQL:2003, and what Django's ORM emits for
    /// `frame=ValueRange(..., exclusion=...)`. Punches a hole in an otherwise
    /// contiguous frame, so it is the one frame feature the sliding host-
    /// aggregate path cannot answer incrementally.
    pub exclude: FrameExclude,
}

/// Which rows a frame drops around the CURRENT row. Measured against sqlite
/// 3.45.1 before implementing — the peer group is the current row's ORDER BY
/// ties WITHIN the partition, and with no ORDER BY the whole partition is one
/// peer group (so `Group` empties the frame and `Ties` leaves exactly the
/// current row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameExclude {
    /// The default: nothing is dropped.
    #[default]
    NoOthers,
    /// Just the current row.
    CurrentRow,
    /// The current row AND its peers.
    Group,
    /// The current row's peers, but NOT the current row — which is why this is
    /// a filter over the frame rather than a narrowing of its bounds: the kept
    /// row stays in its window-order position.
    Ties,
}

/// Frame unit. `Rows` counts physical rows; `Range` compares ORDER BY values
/// (peer semantics for the supported UNBOUNDED/CURRENT ROW bounds); `Groups`
/// counts peer groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMode {
    Rows,
    Range,
    Groups,
}

/// A frame boundary. `Preceding`/`Following` carry a constant non-negative
/// offset (rows for `Rows`, peer-groups for `Groups`; refused for `Range`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBound {
    UnboundedPreceding,
    /// The offset, which may be a PARAMETER — one value for the whole
    /// execution, the same reasoning [`WinInt`] carries for `lag`/`lead`.
    /// `ROWS BETWEEN ? PRECEDING AND CURRENT ROW` is what an ORM writes for a
    /// runtime window size, and baking it in would have made every distinct
    /// size a distinct plan hash.
    Preceding(WinInt),
    CurrentRow,
    Following(WinInt),
    UnboundedFollowing,
}

impl Frame {
    /// Wire tag for a boundary as a FRAME START (`None` ⇒ illegal as a start,
    /// i.e. `UNBOUNDED FOLLOWING`). Also the ordinal used to reject an end that
    /// precedes the start — matching sqlite, which treats every `N PRECEDING`
    /// alike (rank 1) and every `N FOLLOWING` alike (rank 3) regardless of `N`.
    fn start_rank(b: FrameBound) -> Option<u8> {
        match b {
            FrameBound::UnboundedPreceding => Some(0),
            FrameBound::Preceding(_) => Some(1),
            FrameBound::CurrentRow => Some(2),
            FrameBound::Following(_) => Some(3),
            FrameBound::UnboundedFollowing => None,
        }
    }

    /// Ordinal of a boundary as a FRAME END (`None` ⇒ illegal as an end, i.e.
    /// `UNBOUNDED PRECEDING`).
    fn end_rank(b: FrameBound) -> Option<u8> {
        match b {
            FrameBound::UnboundedPreceding => None,
            FrameBound::Preceding(_) => Some(1),
            FrameBound::CurrentRow => Some(2),
            FrameBound::Following(_) => Some(3),
            FrameBound::UnboundedFollowing => Some(4),
        }
    }

    /// Whether this frame yields the same result regardless of the (arbitrary)
    /// row order within a partition — the condition for allowing it with NO
    /// window ORDER BY. `Range`/`Groups` collapse to a single peer group without
    /// an ORDER BY, so every such frame is whole-partition-or-empty; a physical
    /// `Rows` frame is order-dependent unless it spans the whole partition.
    fn order_independent(&self) -> bool {
        match self.mode {
            FrameMode::Range | FrameMode::Groups => true,
            FrameMode::Rows => matches!(
                (self.start, self.end),
                (FrameBound::UnboundedPreceding, FrameBound::UnboundedFollowing)
            ),
        }
    }

    /// Structural legality of the frame for `func`, given whether the window has
    /// an ORDER BY. Returns a human message on failure; the planner maps it to a
    /// `bind_err`, decode/validate to `Corrupt`, so the same rules gate both the
    /// prepare path and a hostile blob. The rules are sqlite's, verified against
    /// 3.45:
    ///  - a frame is meaningful only on aggregate / `first_value` / `last_value`
    ///    / `nth_value` windows (elsewhere sqlite silently ignores it — refused
    ///    here so a frame never quietly changes nothing);
    ///  - the start cannot be `UNBOUNDED FOLLOWING`, the end cannot be
    ///    `UNBOUNDED PRECEDING`, and the end cannot precede the start;
    ///  - `RANGE` with a `PRECEDING`/`FOLLOWING` offset needs EXACTLY ONE
    ///    `ORDER BY` expression, since the bound is that expression's value
    ///    ± the offset (sqlite refuses zero or several for the same reason,
    ///    with the message reproduced here);
    ///  - an order-dependent frame needs an ORDER BY.
    pub(crate) fn check(
        &self,
        func: WindowFunc,
        n_order_by: usize,
    ) -> std::result::Result<(), String> {
        let has_order_by = n_order_by > 0;
        if !matches!(
            func,
            WindowFunc::Agg(_)
                | WindowFunc::Host
                | WindowFunc::FirstValue
                | WindowFunc::LastValue
                | WindowFunc::NthValue(_)
        ) {
            return Err(
                "an explicit frame is only supported on aggregate and \
                 first_value/last_value/nth_value window functions"
                    .into(),
            );
        }
        let Some(sr) = Self::start_rank(self.start) else {
            return Err("a window frame cannot START at UNBOUNDED FOLLOWING".into());
        };
        let Some(er) = Self::end_rank(self.end) else {
            return Err("a window frame cannot END at UNBOUNDED PRECEDING".into());
        };
        if sr > er {
            return Err("unsupported frame specification: the end boundary precedes the start".into());
        }
        if matches!(self.mode, FrameMode::Range)
            && (matches!(self.start, FrameBound::Preceding(_) | FrameBound::Following(_))
                || matches!(self.end, FrameBound::Preceding(_) | FrameBound::Following(_)))
            && n_order_by != 1
        {
            // sqlite's own wording — the bound is `<the one ORDER BY value> ±
            // offset`, so zero keys leaves nothing to offset FROM and several
            // leaves no single value to offset.
            return Err(
                "RANGE with offset PRECEDING/FOLLOWING requires one ORDER BY expression".into(),
            );
        }
        if !has_order_by && !self.order_independent() {
            return Err(
                "an explicit ROWS frame with a bounded edge requires an ORDER BY in the OVER clause \
                 (without one the row order, and so the frame, is undefined)"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Which window function a [`WindowSpec`] computes. A closed enum with wire tags
/// (like [`AggFn`]/`ScalarFn`): ranking is SQL-only, and the aggregate half
/// reuses [`AggFn`] verbatim so the NULL/overflow/type rules never fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunc {
    /// Distinct sequential 1..n within each partition; ties broken by input
    /// (gather) order.
    RowNumber,
    /// Ties share a rank, the next rank SKIPS (1,1,3).
    Rank,
    /// Ties share a rank, no gaps (1,1,2).
    DenseRank,
    /// An aggregate over the default frame — cumulative (`RANGE … CURRENT ROW`)
    /// when the window has ORDER BY, else the whole partition.
    Agg(AggFn),
    /// `lag(expr, offset, …)` — the value `offset` rows BEFORE the current row in
    /// the partition (window order); out of range ⇒ the spec's `default` (or
    /// NULL). Frame-independent. The offset is a literal or a PARAMETER
    /// ([`WinInt`]); a per-ROW expression is refused.
    Lag(WinInt),
    /// `lead(expr, offset, …)` — the value `offset` rows AFTER the current row.
    Lead(WinInt),
    /// `first_value(expr)` — the first row of the frame, i.e. (default frame)
    /// the partition's first row: constant across the partition.
    FirstValue,
    /// `last_value(expr)` — the last row of the frame: the current row's
    /// peer-group end (default RANGE frame with ORDER BY), or the partition's
    /// last row (no ORDER BY).
    LastValue,
    /// `nth_value(expr, n)` — the n-th row (1-based, `i64`) of the frame, or NULL
    /// if the frame has fewer than n rows. `n` is a CONSTANT ≥ 1 (validated).
    NthValue(WinInt),
    /// `ntile(n)` — the partition's rows distributed into `n` buckets as equally
    /// as possible (bucket number 1..n). sqlite's rule: the first `rows % n`
    /// buckets get `ceil(rows/n)` rows, the rest `floor`. `n` is a CONSTANT ≥ 1
    /// (validated); requires ORDER BY (the planner refuses it otherwise). Result
    /// is `Int64`, never NULL. Takes no per-row value.
    Ntile(WinInt),
    /// `percent_rank()` — `(rank - 1) / (rows_in_partition - 1)`, or 0.0 for a
    /// one-row partition. Uses `rank()` semantics (ties share). `Float64`, never
    /// NULL, no argument.
    PercentRank,
    /// `cume_dist()` — `(rows whose ORDER BY value is ≤ the current row's, peers
    /// included) / rows_in_partition`. `Float64`, never NULL, no argument.
    CumeDist,
    /// A HOST-registered window aggregate — sqlite's `create_window_function`
    /// (`xStep`/`xFinal` PLUS `xValue`/`xInverse`). The NAME rides in
    /// [`WindowSpec::host`] rather than here, so this enum stays `Copy` and the
    /// twelve built-in shapes encode byte-for-byte as they always did. Result
    /// type is `Any`: the callback decides per row.
    Host,
}

impl WindowFunc {
    /// Wire tag. `Agg` is tag 4 followed by the [`AggFn`] tag byte;
    /// `Lag`/`Lead`/`NthValue`/`Ntile` are their tag followed by an i64
    /// (offset / n / bucket count).
    pub(crate) fn tag(self) -> u8 {
        match self {
            WindowFunc::RowNumber => 1,
            WindowFunc::Rank => 2,
            WindowFunc::DenseRank => 3,
            WindowFunc::Agg(_) => 4,
            WindowFunc::Lag(_) => 5,
            WindowFunc::Lead(_) => 6,
            WindowFunc::FirstValue => 7,
            WindowFunc::LastValue => 8,
            WindowFunc::NthValue(_) => 9,
            WindowFunc::Ntile(_) => 10,
            WindowFunc::PercentRank => 11,
            WindowFunc::CumeDist => 12,
            WindowFunc::Host => 13,
        }
    }
}
