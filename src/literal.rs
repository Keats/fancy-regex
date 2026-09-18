//! Required-literal analysis and the prefilter built from it.
//!
//! A pattern routed through the backtracking VM is tried at successive start positions with
//! no knowledge of what the text must contain. Oniguruma avoids that with its "exact string"
//! optimization: a substring every match must contain, searched for before the matcher runs.
//! This module does the same for fancy patterns. The literals are extracted from the parsed
//! expression, and an unanchored search first checks that one of them occurs in the haystack
//! from the search start; when none does, the search returns no match without running the VM.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use regex_automata::util::prefilter::Prefilter;
use regex_automata::{MatchKind, Span};
use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};

use crate::optimize::optimize;
use crate::{Expr, LookAround, RegexOptions, Result};

/// A literal that must appear in any match, with whether it matches case-insensitively.
pub(crate) type RequiredLiteral = (String, bool);

/// Upper bound on the needles handed to the prefilter after case expansion. Beyond this the
/// multi-substring search stops being cheap relative to the VM it protects.
const MAX_NEEDLES: usize = 64;

/// Literals of which at least one must appear in any match of `expr`, or `None` when no such
/// set can be established. The analysis is conservative: every construct it does not
/// understand contributes nothing, so the result is only ever weaker than the truth.
pub(crate) fn required_literals(expr: &Expr) -> Option<Vec<RequiredLiteral>> {
    match expr {
        Expr::Literal { val, casei } => Some(vec![(val.clone(), *casei)]),
        Expr::Group(inner) => required_literals(inner),
        Expr::AtomicGroup(inner) => required_literals(inner),
        // A positive lookahead must match at this position, so its literal is in the text
        Expr::LookAround(inner, LookAround::LookAhead) => required_literals(inner),
        Expr::Repeat { child, lo, .. } if *lo >= 1 => required_literals(child),
        // Every branch must contribute, otherwise nothing is required
        Expr::Alt(branches) => {
            let mut out = Vec::new();
            for branch in branches {
                out.extend(required_literals(branch)?);
            }
            Some(out)
        }
        // Runs of adjacent literals form one literal. Among all candidates keep the most
        // selective set: the one whose shortest literal is longest.
        Expr::Concat(children) => {
            let mut candidates: Vec<Vec<RequiredLiteral>> = Vec::new();
            let mut run: Option<RequiredLiteral> = None;
            for child in children {
                if let Expr::Literal { val, casei } = child {
                    match &mut run {
                        Some((s, c)) if *c == *casei => s.push_str(val),
                        _ => {
                            candidates.extend(run.take().map(|r| vec![r]));
                            run = Some((val.clone(), *casei));
                        }
                    }
                    continue;
                }
                candidates.extend(run.take().map(|r| vec![r]));
                candidates.extend(required_literals(child));
            }
            candidates.extend(run.take().map(|r| vec![r]));
            candidates
                .into_iter()
                .max_by_key(|set| set.iter().map(|(s, _)| s.len()).min().unwrap_or(0))
        }
        _ => None,
    }
}

/// The required literals of `pattern` under `options`, without compiling it. Analyzes the
/// same rewritten tree `Regex::new_options` compiles.
pub(crate) fn required_literals_of(
    pattern: &str,
    options: &RegexOptions,
) -> Result<Option<Vec<RequiredLiteral>>> {
    let mut tree = Expr::parse_tree_with_flags(pattern, options.compute_flags())?;
    optimize(&mut tree);
    Ok(required_literals(&tree.expr))
}

/// Checks that a haystack contains one of the required literals of a pattern.
#[derive(Clone, Debug)]
pub(crate) struct LiteralPrefilter {
    inner: Prefilter,
}

impl LiteralPrefilter {
    /// Builds the prefilter for `expr`, or `None` when the pattern has no required literals or
    /// they cannot be turned into a small set of byte needles.
    pub(crate) fn from_expr(expr: &Expr) -> Option<Self> {
        let literals = reduce(required_literals(expr)?);
        // Any-of semantics: every literal must be searchable, and they share the budget
        let budget = (MAX_NEEDLES / literals.len()).max(1);
        let mut needles: Vec<Vec<u8>> = Vec::new();
        for (literal, casei) in &literals {
            if *casei {
                needles.extend(casei_needles(literal, budget)?);
            } else {
                needles.push(literal.as_bytes().to_vec());
            }
        }
        needles.sort();
        needles.dedup();
        Prefilter::new(MatchKind::LeftmostFirst, &needles).map(|inner| Self { inner })
    }

    /// Whether a match starting at or after `start` is possible at all
    pub(crate) fn may_match(&self, haystack: &[u8], start: usize) -> bool {
        let end = haystack.len();
        if start >= end {
            return false;
        }
        self.inner.find(haystack, Span { start, end }).is_some()
    }
}

/// Drops literals that contain another literal of the set: text containing `sqlFragment`
/// contains `sql`, so `sql` alone covers both. Comparison is lowercase when either side is
/// case-insensitive, which only ever drops a literal the shorter one still covers.
fn reduce(mut literals: Vec<RequiredLiteral>) -> Vec<RequiredLiteral> {
    literals.sort_by_key(|(s, _)| s.len());
    literals.dedup();
    let mut kept: Vec<RequiredLiteral> = Vec::with_capacity(literals.len());
    for (literal, casei) in literals {
        let covered = kept.iter().any(|(short, short_casei)| {
            if *short_casei {
                literal.to_lowercase().contains(&short.to_lowercase())
            } else if casei {
                // a case-sensitive `short` only covers `literal` if it appears as is
                literal.contains(short.as_str()) && !short.chars().any(char::is_alphabetic)
            } else {
                literal.contains(short.as_str())
            }
        });
        if !covered {
            kept.push((literal, casei));
        }
    }
    kept
}

/// The byte needles for a case-insensitive literal: every combination of the simple case
/// folds of its characters. When the full literal needs more than `budget` needles, a prefix
/// of it is used instead, since a substring of a required literal is required too. `None`
/// when even one character does not fit, or when case folding data is unavailable.
fn casei_needles(literal: &str, budget: usize) -> Option<Vec<Vec<u8>>> {
    let mut variants: Vec<Vec<u8>> = vec![Vec::new()];
    for c in literal.chars() {
        let folds = case_folds(c)?;
        if variants.len() * folds.len() > budget {
            break;
        }
        let mut next = Vec::with_capacity(variants.len() * folds.len());
        for v in &variants {
            for f in &folds {
                let mut n = v.clone();
                let mut buf = [0u8; 4];
                n.extend_from_slice(f.encode_utf8(&mut buf).as_bytes());
                next.push(n);
            }
        }
        variants = next;
    }
    if variants[0].is_empty() {
        return None;
    }
    Some(variants)
}

/// The characters equivalent to `c` under Unicode simple case folding, `c` included.
fn case_folds(c: char) -> Option<Vec<char>> {
    let mut class = ClassUnicode::new([ClassUnicodeRange::new(c, c)]);
    class.try_case_fold_simple().ok()?;
    Some(
        class
            .iter()
            .flat_map(|range| (range.start()..=range.end()).collect::<Vec<char>>())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn lits(pattern: &str) -> Option<Vec<(String, bool)>> {
        let tree = Expr::parse_tree(pattern).unwrap();
        required_literals(&tree.expr)
    }

    fn lit(s: &str) -> (String, bool) {
        (s.to_string(), false)
    }

    #[test]
    fn picks_the_most_selective_literal_of_a_concat() {
        assert_eq!(lits(r"(\s*((?:|inline-)css))(`)"), Some(vec![lit("css")]));
        assert_eq!(lits(r"(@let)\s+(\w+)"), Some(vec![lit("@let")]));
        assert_eq!(lits(r"\bfoo\d+barbaz"), Some(vec![lit("barbaz")]));
    }

    #[test]
    fn alternation_needs_every_branch() {
        assert_eq!(
            lits(r"\b(html|template)\b"),
            Some(vec![lit("html"), lit("template")])
        );
        // an empty branch makes the alternation contribute nothing, the concat falls back
        assert_eq!(lits(r"(?:|inline-)css"), Some(vec![lit("css")]));
        assert_eq!(lits(r"(a|[bc])"), None);
    }

    #[test]
    fn optional_and_lookbehind_do_not_count_but_positive_lookahead_does() {
        assert_eq!(lits(r"(sql)?"), None);
        assert_eq!(lits(r"(?<=x)\w+"), None);
        assert_eq!(lits(r"(?=.*sql)\w+"), Some(vec![lit("sql")]));
        assert_eq!(lits(r"(?!sql)\w+"), None);
        assert_eq!(lits(r"(?>sql)\d"), Some(vec![lit("sql")]));
    }

    #[test]
    fn case_insensitive_literals_are_flagged() {
        assert_eq!(lits(r"(?i)SQL"), Some(vec![("SQL".to_string(), true)]));
        assert_eq!(
            lits(r"\b((?i)sql|sqlFragment(?-i))\s*(?=`)"),
            Some(vec![
                ("sql".to_string(), true),
                ("sqlFragment".to_string(), true)
            ])
        );
    }

    #[test]
    fn escapes_merge_into_one_literal() {
        assert_eq!(lits(r"(\$\{)"), Some(vec![lit("${")]));
        assert_eq!(lits(r"\{\{"), Some(vec![lit("{{")]));
    }

    fn prefilter(pattern: &str) -> Option<LiteralPrefilter> {
        let tree = Expr::parse_tree(pattern).unwrap();
        LiteralPrefilter::from_expr(&tree.expr)
    }

    #[test]
    fn prefilter_checks_the_haystack_from_the_start_position() {
        let pf = prefilter(r"(?=.*sql)\w+").unwrap();
        assert!(pf.may_match(b"select sql from x", 0));
        assert!(!pf.may_match(b"select sql from x", 8));
        assert!(!pf.may_match(b"nothing here", 0));
        assert!(!pf.may_match(b"sql", 3));
    }

    #[test]
    fn prefilter_expands_ascii_case() {
        let pf = prefilter(r"(?i)sql(?=`)").unwrap();
        assert!(pf.may_match(b"x SQL y", 0));
        assert!(pf.may_match(b"x sQl y", 0));
        assert!(!pf.may_match(b"x sq y", 0));
    }

    #[test]
    fn prefilter_folds_unicode_case() {
        // k also matches the Kelvin sign, s the long s, under simple case folding
        let pf = prefilter(r"(?<=x)(?i)k").unwrap();
        assert!(pf.may_match("x\u{212A}".as_bytes(), 0));
        let pf = prefilter(r"(?<=x)(?i)s").unwrap();
        assert!(pf.may_match("x\u{17F}".as_bytes(), 0));
        let pf = prefilter(r"(?<=x)(?i)привет").unwrap();
        assert!(pf.may_match("xПРИВЕТ".as_bytes(), 0));
        assert!(!pf.may_match("xпока".as_bytes(), 0));
    }

    #[test]
    fn long_case_insensitive_literals_fall_back_to_a_prefix() {
        // 20 letters would need a million needles; a prefix within budget is used instead
        let needles = casei_needles("abcdefghijklmnopqrst", 64).unwrap();
        assert!(needles.len() <= 64);
        assert!(needles.iter().all(|n| n.len() >= 5));
        // the prefilter still gates on that prefix
        let pf = prefilter(r"(?i)abcdefghijklmnopqrst(?=x)").unwrap();
        assert!(pf.may_match(b"ABCDEFGHIJKLMNOPQRSTx", 0));
        assert!(!pf.may_match(b"zzzzzzzzzzzzzzzzzzzzz", 0));
    }

    #[test]
    fn prefilter_gives_up_without_a_required_literal() {
        assert!(prefilter(r"(a|[bc])(?=\d)").is_none());
        assert!(prefilter(r"(\w)\1").is_none());
    }

    #[test]
    fn reduce_drops_literals_covered_by_shorter_ones() {
        let lits = vec![
            ("sqlFragment".to_string(), true),
            ("sql".to_string(), true),
            ("${".to_string(), false),
        ];
        assert_eq!(
            reduce(lits),
            vec![("${".to_string(), false), ("sql".to_string(), true)]
        );
        // a case-sensitive letter literal does not cover a case-insensitive superstring
        let lits = vec![("ab".to_string(), false), ("xaby".to_string(), true)];
        assert_eq!(reduce(lits).len(), 2);
    }

    #[test]
    fn case_folds_include_the_character_itself() {
        let mut folds = case_folds('k').unwrap();
        folds.sort();
        assert_eq!(folds, vec!['K', 'k', '\u{212A}']);
        assert_eq!(case_folds('1').unwrap(), vec!['1']);
    }
}
