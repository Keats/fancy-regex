use regex_syntax::hir::{Class, ClassUnicode, ClassUnicodeRange, Hir, HirKind};

use crate::to_hir::{expr_to_hir, HirCtx};
use crate::{Expr, LookAround, RegexOptions};

/// A set of bytes, stored as a 256 bits bitmap
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct ByteSet([u64; 4]);

impl ByteSet {
    pub(crate) fn new() -> Self {
        Self([0; 4])
    }

    pub(crate) fn insert(&mut self, byte: u8) {
        self.0[(byte >> 6) as usize] |= 1u64 << (byte & 63);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0 == [0; 4]
    }

    pub(crate) fn contains(&self, byte: u8) -> bool {
        self.0[(byte >> 6) as usize] & (1u64 << (byte & 63)) != 0
    }

    pub(crate) fn union(&mut self, other: &Self) {
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            *a |= *b;
        }
    }

    /// Iterates over the bytes in the set, in ascending order
    pub(crate) fn iter(&self) -> impl Iterator<Item = u8> + '_ {
        set_bits(&self.0).map(|i| i as u8)
    }
}

/// Get an iterator for the bits set, starting from the lowest
fn bits_of(mut bits: u64) -> impl Iterator<Item = usize> {
    core::iter::from_fn(move || {
        (bits != 0).then(|| {
            let bit = bits.trailing_zeros() as usize;
            // clear the lowest set bit so the next call gets the following one
            bits &= bits - 1;
            bit
        })
    })
}

/// Get an iterator for the indices of the bits set, starting from the lowest
pub(crate) fn set_bits(words: &[u64]) -> impl Iterator<Item = usize> + '_ {
    words
        .iter()
        .enumerate()
        .flat_map(|(i, &word)| bits_of(word).map(move |bit| i * 64 + bit))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Start {
    /// Every match must start with a byte from the set
    Definite(ByteSet),
    /// The match can be empty but otherwise it starts with a byte from the set
    MaybeEmpty(ByteSet),
    /// Could be anything.
    Bail,
}

/// Same logic as fancy_start but on regex-syntax HIR instead.
fn regex_syntax_start(expr: &Hir) -> Start {
    match expr.kind() {
        // 0 len match
        HirKind::Empty | HirKind::Look(_) => Start::MaybeEmpty(ByteSet::new()),
        HirKind::Literal(l) => {
            if let Some(first) = l.0.first() {
                let mut set = ByteSet::new();
                set.insert(*first);
                Start::Definite(set)
            } else {
                Start::MaybeEmpty(ByteSet::new())
            }
        }
        HirKind::Class(class) => {
            let mut set = ByteSet::new();
            match class {
                Class::Unicode(cls) => {
                    for r in cls.ranges() {
                        if r.start().is_ascii() {
                            for i in (r.start() as u32)..=(r.end() as u32).min(0x7F) {
                                set.insert(i as u8);
                            }
                        }

                        if !r.end().is_ascii() {
                            // unicode, just insert lead bytes range
                            for i in 0xC2..=0xF4 {
                                set.insert(i as u8);
                            }
                        }
                    }
                }
                Class::Bytes(cls) => {
                    for r in cls.ranges() {
                        for i in r.start()..=r.end() {
                            set.insert(i);
                        }
                    }
                }
            }

            if set.is_empty() {
                Start::Bail
            } else {
                Start::Definite(set)
            }
        }
        HirKind::Capture(c) => regex_syntax_start(&c.sub),
        HirKind::Repetition(r) => {
            let child_start = regex_syntax_start(&r.sub);
            if r.min == 0 {
                // if we allow 0 reps then it's not definite
                match child_start {
                    Start::Definite(set) | Start::MaybeEmpty(set) => Start::MaybeEmpty(set),
                    Start::Bail => Start::Bail,
                }
            } else {
                child_start
            }
        }
        HirKind::Concat(c) => {
            let mut set = ByteSet::new();
            for expr in c {
                match regex_syntax_start(expr) {
                    Start::Definite(mut s) => {
                        // We only care about the first byte so as soon as we hit a definite match
                        // we don't need to continue processing the rest
                        s.union(&set);
                        return Start::Definite(s);
                    }
                    Start::MaybeEmpty(s) => {
                        set.union(&s);
                    }
                    Start::Bail => {
                        // If we even get one bail, we bail everything
                        return Start::Bail;
                    }
                }
            }
            Start::MaybeEmpty(set)
        }
        HirKind::Alternation(exprs) => {
            let mut total = None;
            for expr in exprs {
                let expr_start = regex_syntax_start(expr);
                total = Some(match (total, expr_start) {
                    (None, b) => b,
                    (Some(Start::Bail), _) | (Some(_), Start::Bail) => Start::Bail,
                    (Some(Start::Definite(mut a)), Start::Definite(b)) => {
                        a.union(&b);
                        Start::Definite(a)
                    }
                    (Some(Start::Definite(mut a)), Start::MaybeEmpty(b))
                    | (Some(Start::MaybeEmpty(mut a)), Start::Definite(b))
                    | (Some(Start::MaybeEmpty(mut a)), Start::MaybeEmpty(b)) => {
                        a.union(&b);
                        Start::MaybeEmpty(a)
                    }
                });
            }
            total.unwrap_or(Start::MaybeEmpty(ByteSet::new()))
        }
    }
}

#[inline]
fn first_utf8_byte(c: char) -> u8 {
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf).as_bytes()[0]
}

/// TODO: what to do with low selective things like `\S+` or anything negated like `[^a]` that can match pretty much any chars?
fn fancy_start(expr: &Expr, ctx: &mut HirCtx) -> Start {
    match expr {
        Expr::Empty | Expr::Assertion(_) | Expr::KeepOut | Expr::ContinueFromPreviousMatchEnd => {
            Start::MaybeEmpty(ByteSet::new())
        }
        Expr::Literal { val, casei } => {
            let Some(first) = val.chars().next() else {
                return Start::MaybeEmpty(ByteSet::new());
            };
            let mut set = ByteSet::new();
            if *casei {
                let mut class = ClassUnicode::new([ClassUnicodeRange::new(first, first)]);
                if class.try_case_fold_simple().is_err() {
                    return Start::Bail;
                }
                for range in class.ranges() {
                    for c in range.start()..=range.end() {
                        set.insert(first_utf8_byte(c));
                    }
                }
            } else {
                set.insert(first_utf8_byte(first));
            }
            Start::Definite(set)
        }
        Expr::Delegate { .. } => match expr_to_hir(expr, ctx) {
            Some(hir) => regex_syntax_start(&hir),
            None => Start::Bail,
        },

        Expr::Concat(exprs) => {
            let mut set = ByteSet::new();
            for expr in exprs {
                match fancy_start(expr, ctx) {
                    Start::Definite(mut s) => {
                        // We only care about the first byte so as soon as we hit a definite match
                        // we don't need to continue processing the rest
                        s.union(&set);
                        return Start::Definite(s);
                    }
                    Start::MaybeEmpty(s) => {
                        set.union(&s);
                    }
                    Start::Bail => {
                        // If we even get one bail, we bail everything
                        return Start::Bail;
                    }
                }
            }
            Start::MaybeEmpty(set)
        }

        Expr::Alt(exprs) => {
            let mut total = None;
            for expr in exprs {
                let expr_start = fancy_start(expr, ctx);
                total = Some(match (total, expr_start) {
                    (None, b) => b,
                    (Some(Start::Bail), _) | (Some(_), Start::Bail) => Start::Bail,
                    (Some(Start::Definite(mut a)), Start::Definite(b)) => {
                        a.union(&b);
                        Start::Definite(a)
                    }
                    (Some(Start::Definite(mut a)), Start::MaybeEmpty(b))
                    | (Some(Start::MaybeEmpty(mut a)), Start::Definite(b))
                    | (Some(Start::MaybeEmpty(mut a)), Start::MaybeEmpty(b)) => {
                        a.union(&b);
                        Start::MaybeEmpty(a)
                    }
                });
            }
            total.unwrap_or(Start::MaybeEmpty(ByteSet::new()))
        }

        Expr::Group(child) => fancy_start(child, ctx),
        Expr::AtomicGroup(child) => fancy_start(child, ctx),

        Expr::Repeat { child, lo, .. } => {
            let child_start = fancy_start(child, ctx);
            if *lo == 0 {
                // if we allow 0 reps then it's not definite
                match child_start {
                    Start::Definite(set) | Start::MaybeEmpty(set) => Start::MaybeEmpty(set),
                    Start::Bail => Start::Bail,
                }
            } else {
                child_start
            }
        }
        Expr::LookAround(expr, lookaround) => match lookaround {
            LookAround::LookAhead => match fancy_start(expr, ctx) {
                Start::Definite(set) => Start::Definite(set),
                _ => Start::MaybeEmpty(ByteSet::new()),
            },
            _ => Start::MaybeEmpty(ByteSet::new()),
        },
        _ => Start::Bail,
    }
}

pub(crate) fn byte_set_from_expr(expr: &Expr, options: &RegexOptions) -> Option<ByteSet> {
    let mut hir_ctx = HirCtx::from(options);
    match fancy_start(expr, &mut hir_ctx) {
        Start::Definite(set) if !set.is_empty() => Some(set),
        _ => None,
    }
}
