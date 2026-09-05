use crate::lazy::prefilter::Prefilter;
use crate::lazy::regex::LazyRegex;
use crate::regexset::match_pattern_at_input_position;
use crate::{BytesMode, Input, RegexInput, RegexSet, RegexSetMatch, RegexSetOptions};
use std::sync::Arc;

#[derive(Debug)]
struct Remainder {
    set: Arc<RegexSet>,
    indices: Vec<usize>,
}

impl Remainder {
    pub fn find_input<'t, S: Input + ?Sized>(
        &self,
        input: RegexInput<'t, S>,
    ) -> crate::Result<Option<RegexSetMatch<'t, S>>> {
        if let Some(mut matches) = self.set.find_input(input)? {
            if let Some(mut m) = matches.next().transpose()? {
                // We want the idx at the rule level, not just the regset
                m.pattern_index = self.indices[m.pattern_index];
                Ok(Some(m))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }
}

/// TODO
#[derive(Debug)]
pub struct Finder {
    regexes: Vec<Arc<LazyRegex>>,
    prefilter: Prefilter,
    remainder: Option<Remainder>,
}

impl Finder {
    /// TODO
    pub fn new(regexes: Vec<Arc<LazyRegex>>) -> crate::Result<Self> {
        if regexes.iter().any(|r| r.bytes_mode() != BytesMode::Unicode) {
            return Err(crate::Error::CompileError(Box::new(
                crate::CompileError::UnexpectedGeneralError(
                    "Finder only supports `BytesMode::Unicode`, the LazyRegexes need to be constructed with that mode.".to_string(),
                ),
            )));
        }

        let sets: Vec<_> = regexes.iter().map(|r| r.start_bytes().cloned()).collect();
        let (prefilter, remaining) = match Prefilter::from_byte_sets(&sets) {
            Some(c) => c,
            None => (Prefilter::default(), (0..regexes.len()).collect()),
        };

        let remainder = if remaining.is_empty() {
            None
        } else {
            let compiled = remaining
                .iter()
                .map(|&i| regexes[i].regex())
                .collect::<Result<Vec<_>, crate::Error>>()?;

            Some(Remainder {
                set: Arc::new(RegexSet::from_regexes(
                    compiled,
                    RegexSetOptions::default(),
                )?),
                indices: remaining,
            })
        };

        Ok(Self {
            regexes,
            prefilter,
            remainder,
        })
    }

    /// TODO
    pub fn find_input<'t, S: Input + ?Sized>(
        &self,
        input: RegexInput<'t, S>,
    ) -> crate::Result<Option<RegexSetMatch<'t, S>>> {
        if self.regexes.is_empty() {
            return Ok(None);
        }

        let set_hit = if let Some(x) = &self.remainder {
            let hit = x.find_input(input.clone())?;
            if x.indices.len() == self.regexes.len() {
                return Ok(hit);
            }
            hit
        } else {
            None
        };

        let haystack = input.haystack();
        let bytes = haystack.as_bytes();
        let end = input.get_range().end;
        let mut walk_hit = None;
        let mut search_start = input.effective_start();
        let anchored = input.is_anchored();

        'walk: while search_start < end {
            // Regset first so we can get a starting pos to stop the walk early
            if let Some(hit) = set_hit.as_ref() {
                if search_start > hit.start() {
                    break;
                }
            }

            if self.prefilter.may_match_at(bytes[search_start]) {
                for idx in self.prefilter.candidates(bytes[search_start]) {
                    let regex = self.regexes[idx].regex_ref()?;
                    if let Some(m) =
                        match_pattern_at_input_position(regex, idx, &input, search_start)?
                    {
                        walk_hit = Some(m);
                        break 'walk;
                    }
                }
            }

            if anchored {
                break;
            }
            search_start = haystack.advance_position(search_start);
        }

        match (set_hit, walk_hit) {
            (None, None) => Ok(None),
            (Some(m), None) => Ok(Some(m)),
            (None, Some(m)) => Ok(Some(m)),
            (Some(set_m), Some(walk_m)) => {
                // Earliest win, otherwise by the idx in the list of patterns
                if set_m.start() < walk_m.start()
                    || (set_m.start() == walk_m.start() && set_m.pattern() < walk_m.pattern())
                {
                    Ok(Some(set_m))
                } else {
                    Ok(Some(walk_m))
                }
            }
        }
    }
}
