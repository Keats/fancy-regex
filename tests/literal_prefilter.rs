//! End-to-end behavior of the required-literal prefilter on VM-routed patterns.

use fancy_regex::{Regex, RegexInput, RegexOptionsBuilder};

/// A pattern that needs the backtracking VM (lookahead) and requires the literal `sql`.
const FANCY_SQL: &str = r"(?=[^:]*sql)\w+";

#[test]
fn skips_haystacks_without_the_literal_and_matches_those_with_it() {
    let re = Regex::new(FANCY_SQL).unwrap();
    assert!(re.find("select name from users").unwrap().is_none());
    let m = re.find("select sql from users").unwrap().unwrap();
    assert_eq!(m.as_str(), "select");
    assert!(re.is_match("sql").unwrap());
}

#[test]
fn respects_the_search_start_position() {
    let re = Regex::new(FANCY_SQL).unwrap();
    // `sql` sits before the start position, so no match can contain it
    assert!(re.find_from_pos("sql then words", 4).unwrap().is_none());
    assert!(re.find_from_pos("words then sql", 4).unwrap().is_some());
}

#[test]
fn case_insensitive_literals_match_any_case() {
    let re = Regex::new(r"(?=.*(?i)sql)\w+").unwrap();
    assert!(re.is_match("SELECT SQL").unwrap());
    assert!(re.is_match("select SqL").unwrap());
    assert!(!re.is_match("select nothing").unwrap());
}

#[test]
fn anchored_searches_are_unaffected() {
    let re = Regex::new(FANCY_SQL).unwrap();
    let text = "select sql";
    let anchored = RegexInput::new(text).from_pos(0).anchored(true);
    assert_eq!(re.find_input(anchored).unwrap().unwrap().as_str(), "select");
    let anchored_late = RegexInput::new(text).from_pos(7).anchored(true);
    assert_eq!(
        re.find_input(anchored_late).unwrap().unwrap().as_str(),
        "sql"
    );
}

#[test]
fn captures_take_the_same_shortcut() {
    let re = Regex::new(r"(\w+)(?=.*sql)").unwrap();
    assert!(re.captures("no literal here").unwrap().is_none());
    let caps = re.captures("a b sql").unwrap().unwrap();
    assert_eq!(caps.get(1).unwrap().as_str(), "a");
}

#[test]
fn results_are_identical_with_the_prefilter_off() {
    let mut builder = RegexOptionsBuilder::new();
    builder.build_literal_prefilter(false);
    let plain = builder.build(FANCY_SQL.to_string()).unwrap();
    let with = Regex::new(FANCY_SQL).unwrap();
    for text in ["", "sql", "select sql from x", "select from x", "x sql sql"] {
        let a = plain.find(text).unwrap().map(|m| (m.start(), m.end()));
        let b = with.find(text).unwrap().map(|m| (m.start(), m.end()));
        assert_eq!(a, b, "text: {text:?}");
    }
}

#[test]
fn patterns_without_a_required_literal_still_work() {
    // backreference: VM-routed, no literal to extract
    let re = Regex::new(r"(\w)\1").unwrap();
    assert_eq!(re.find("abccd").unwrap().unwrap().as_str(), "cc");
    // alternation with a class branch
    let re = Regex::new(r"(?=\d)(a|[bc])?\d").unwrap();
    assert!(re.is_match("7").unwrap());
}

#[test]
fn bytes_haystacks_are_searched_too() {
    let re = Regex::new(FANCY_SQL).unwrap();
    assert!(re.find(b"select from users".as_slice()).unwrap().is_none());
    assert!(re.find(b"select sql".as_slice()).unwrap().is_some());
}

#[test]
fn builder_exposes_the_literals() {
    let builder = RegexOptionsBuilder::new();
    assert_eq!(
        builder.required_literals(FANCY_SQL).unwrap(),
        Some(vec![("sql".to_string(), false)])
    );
    assert_eq!(
        builder
            .required_literals(r"(?i)(\s*((?:|inline-)css))(`)")
            .unwrap(),
        Some(vec![("css".to_string(), true)])
    );
    assert!(builder.required_literals(r"(\w)\1").unwrap().is_none());
    assert!(builder.required_literals(r"(").is_err());
}
