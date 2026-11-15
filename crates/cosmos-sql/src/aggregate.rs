//! Aggregate functions over the values of one group.
//!
//! An accumulator takes the argument value of each row as the rows go by, so a group never keeps its rows:
//! `COUNT(1)` over a million documents holds one number.

use crate::ast::AggregateKind;
use crate::value::{cmp_total, Num, Val};
use std::cmp::Ordering;

pub(crate) struct Acc {
    kind: AggregateKind,
    /// Defined values seen (COUNT) or numbers added (SUM, AVG).
    count: i64,
    int_sum: Option<i64>,
    float_sum: f64,
    /// SUM / AVG met a value that is not a number: the aggregate is undefined.
    bad: bool,
    /// MIN / MAX: the best value so far.
    best: Option<Val>,
}

impl Acc {
    pub fn new(kind: AggregateKind) -> Acc {
        Acc { kind, count: 0, int_sum: Some(0), float_sum: 0.0, bad: false, best: None }
    }

    /// Adds one row's value (undefined values are skipped). Returns how many bytes the accumulator now
    /// holds in addition (MIN / MAX keep a value), for the memory budget of the query.
    pub fn push(&mut self, v: &Val) -> usize {
        if v.is_undefined() {
            return 0;
        }
        match self.kind {
            AggregateKind::Count => self.count += 1,
            AggregateKind::Sum | AggregateKind::Average => {
                if self.bad {
                    return 0;
                }
                match v {
                    Val::Num(n) => {
                        self.count += 1;
                        self.float_sum += n.f64();
                        self.int_sum = match (self.int_sum, n) {
                            (Some(s), Num::Int(i)) => s.checked_add(*i),
                            _ => None,
                        };
                    }
                    // A string, boolean, null, array or object makes the whole aggregate undefined.
                    _ => self.bad = true,
                }
            }
            AggregateKind::Min | AggregateKind::Max => {
                // an array or an object among the values makes MIN and MAX undefined (a real account does that)
                if matches!(v.forced(), Val::Arr(_) | Val::Obj(_)) {
                    self.bad = true;
                }
                if self.bad {
                    return 0;
                }
                let better = match &self.best {
                    None => true,
                    Some(b) => {
                        let o = cmp_total(v, b);
                        if self.kind == AggregateKind::Min {
                            o == Ordering::Less
                        } else {
                            o == Ordering::Greater
                        }
                    }
                };
                if better {
                    let kept = v.forced();
                    let bytes = kept.est_bytes(usize::MAX / 2);
                    self.best = Some(kept);
                    return bytes;
                }
            }
        }
        0
    }

    pub fn finish(&self) -> Val {
        match self.kind {
            AggregateKind::Count => Val::int(self.count),
            AggregateKind::Sum | AggregateKind::Average => {
                if self.bad {
                    return Val::Undefined;
                }
                if self.kind == AggregateKind::Sum {
                    // the sum of no values is 0 (Azure answers 0, not undefined); AVG of no values is undefined
                    match self.int_sum {
                        Some(s) => Val::int(s),
                        None => Val::num(self.float_sum),
                    }
                } else if self.count == 0 {
                    Val::Undefined
                } else {
                    // integers are added exactly and converted once: 5 + 9007199254740993 + -3 over 3 is
                    // 3002399751580332, as Azure writes it
                    let sum = match self.int_sum {
                        Some(s) => s as f64,
                        None => self.float_sum,
                    };
                    Val::num(sum / self.count as f64)
                }
            }
            AggregateKind::Min | AggregateKind::Max => {
                if self.bad {
                    Val::Undefined
                } else {
                    self.best.clone().unwrap_or(Val::Undefined)
                }
            }
        }
    }
}
