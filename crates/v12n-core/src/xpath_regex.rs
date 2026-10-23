// SPDX-License-Identifier: Apache-2.0
//! XPath regular expressions (XPath and XQuery Functions and Operators 3.1,
//! §5.6 Regular expression syntax) as PostgreSQL advanced regular
//! expressions with the same matches, for `REGEX` (SPARQL 1.1 §17.4.3.14).
//!
//! The translation is written for PostgreSQL's `~` under `COLLATE "C"`,
//! with no embedded options: `.` and negated brackets match a newline, `^`
//! and `$` match only at the ends of the string, and nothing depends on
//! the locale. Everything XPath means differently is spelled out:
//!
//! - `.` without the `s` flag matches anything but `#x0A` and `#x0D`.
//! - `^` and `$` with the `m` flag match at line boundaries as XPath 3.1
//!   defines them (not after a final newline, not at the end after one).
//! - The `i` flag expands every character to its case variants
//!   (`lower-case(c1) eq lower-case(c2)` or the same with `upper-case`),
//!   instead of PostgreSQL's locale-dependent case folding.
//! - `\s` is exactly `[ \t\n\r]`.
//! - Literal characters other than ASCII letters and digits are written as
//!   `\uXXXX` or `\UXXXXXXXX`, so nothing in them is special.
//!
//! Not supported: `\d`, `\w`, `\i`, `\c`, `\p{..}` and their complements,
//! whose Unicode classes PostgreSQL does not have independently of the
//! locale; back-references; character class subtraction; counts above
//! 255 (PostgreSQL's limit); and the `q` flag (XPath 3.0).

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

/// Why a pattern has no translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Untranslated {
    /// Not a valid XPath regular expression or flags: `fn:matches` raises
    /// an error (err:FORX0001, err:FORX0002), so `REGEX` is an error.
    Invalid,
    /// Valid, but not supported, for this reason.
    Unsupported(String),
}

/// PostgreSQL's limit on bounds of `{m,n}`.
const MAX_COUNT: u32 = 255;
/// Largest range expanded with its case variants under the `i` flag.
const MAX_CASE_RANGE: u32 = 0x3000;

/// The PostgreSQL pattern for `pattern` with `flags`.
pub(crate) fn translate(pattern: &str, flags: &str) -> Result<String, Untranslated> {
    let mut parser = Parser {
        chars: Vec::new(),
        pos: 0,
        dot_all: false,
        multi_line: false,
        case_insensitive: false,
        out: String::new(),
    };
    let mut extended = false;
    for flag in flags.chars() {
        match flag {
            's' => parser.dot_all = true,
            'm' => parser.multi_line = true,
            'i' => parser.case_insensitive = true,
            'x' => extended = true,
            'q' => return Err(unsupported("the q flag of REGEX")),
            _ => return Err(Untranslated::Invalid),
        }
    }
    parser.chars = if extended {
        without_whitespace(pattern)
    } else {
        pattern.chars().collect()
    };
    parser.reg_exp()?;
    if parser.pos < parser.chars.len() {
        // An unmatched `)`.
        return Err(Untranslated::Invalid);
    }
    Ok(parser.out)
}

/// The `x` flag: whitespace is removed, except inside character class
/// expressions.
fn without_whitespace(pattern: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut in_class = false;
    let mut escaped = false;
    for c in pattern.chars() {
        if !in_class && !escaped && matches!(c, '\t' | '\n' | '\r' | ' ') {
            continue;
        }
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '[' => in_class = true,
            ']' => in_class = false,
            _ => {}
        }
        out.push(c);
    }
    out
}

fn unsupported(reason: &str) -> Untranslated {
    Untranslated::Unsupported(reason.to_owned())
}

/// A member of a character class expression.
#[derive(Debug, Clone, Copy)]
enum Member {
    Char(char),
    Range(char, char),
    /// `\s`
    Space,
    /// `\S`, which a class cannot hold in PostgreSQL.
    NotSpace,
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    dot_all: bool,
    multi_line: bool,
    case_insensitive: bool,
    out: String,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        self.pos += 1;
        c
    }

    fn reg_exp(&mut self) -> Result<(), Untranslated> {
        self.branch()?;
        while self.peek() == Some('|') {
            self.pos += 1;
            self.out.push('|');
            self.branch()?;
        }
        Ok(())
    }

    fn branch(&mut self) -> Result<(), Untranslated> {
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            self.piece()?;
        }
        Ok(())
    }

    fn piece(&mut self) -> Result<(), Untranslated> {
        let quantifiable = self.atom()?;
        if matches!(self.peek(), Some('?' | '*' | '+' | '{')) {
            if !quantifiable {
                return Err(unsupported("a quantifier on ^ or $ in REGEX"));
            }
            self.quantifier()?;
        }
        Ok(())
    }

    fn quantifier(&mut self) -> Result<(), Untranslated> {
        match self.next() {
            Some(c @ ('?' | '*' | '+')) => self.out.push(c),
            Some('{') => {
                let min = self.count()?.ok_or(Untranslated::Invalid)?;
                let max = match self.next() {
                    Some('}') => Some(Some(min)),
                    Some(',') => {
                        let max = self.count()?;
                        if self.next() != Some('}') {
                            return Err(Untranslated::Invalid);
                        }
                        Some(max)
                    }
                    _ => None,
                };
                let max = max.ok_or(Untranslated::Invalid)?;
                if max.is_some_and(|max| max < min) {
                    return Err(Untranslated::Invalid);
                }
                if min > MAX_COUNT || max.is_some_and(|max| max > MAX_COUNT) {
                    return Err(unsupported("counts above 255 in REGEX"));
                }
                self.out.push_str(&match max {
                    Some(max) if max == min => format!("{{{min}}}"),
                    Some(max) => format!("{{{min},{max}}}"),
                    None => format!("{{{min},}}"),
                });
            }
            _ => return Err(Untranslated::Invalid),
        }
        // A reluctant quantifier matches the same strings; PostgreSQL has it.
        if self.peek() == Some('?') {
            self.pos += 1;
            self.out.push('?');
        }
        Ok(())
    }

    /// Decimal digits, `None` when there are none.
    fn count(&mut self) -> Result<Option<u32>, Untranslated> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if start == self.pos {
            return Ok(None);
        }
        let digits: String = self.chars[start..self.pos].iter().collect();
        // More than any allowed count, which is checked after.
        Ok(Some(digits.parse().unwrap_or(u32::MAX)))
    }

    /// Returns whether the atom can take a quantifier.
    fn atom(&mut self) -> Result<bool, Untranslated> {
        let c = self.next().ok_or(Untranslated::Invalid)?;
        match c {
            '(' => {
                if self.chars.get(self.pos..self.pos + 2) == Some(&['?', ':']) {
                    self.pos += 2;
                    self.out.push_str("(?:");
                } else {
                    self.out.push('(');
                }
                self.reg_exp()?;
                if self.next() != Some(')') {
                    return Err(Untranslated::Invalid);
                }
                self.out.push(')');
            }
            '[' => self.class()?,
            '.' if self.dot_all => self.out.push('.'),
            '.' => self.out.push_str("[^\\u000A\\u000D]"),
            '^' if self.multi_line => {
                // After a newline that is not the last character.
                self.out.push_str("(?:^|(?<=\\u000A)(?!$))");
                return Ok(false);
            }
            '^' => {
                self.out.push('^');
                return Ok(false);
            }
            '$' if self.multi_line => {
                // Before a newline, or at the end when no newline ends it.
                self.out.push_str("(?:(?=\\u000A)|(?<!\\u000A)$)");
                return Ok(false);
            }
            '$' => {
                self.out.push('$');
                return Ok(false);
            }
            '\\' => match self.escape()? {
                Member::Char(c) => self.literal(c),
                Member::Space => self.out.push_str("[\\u0020\\u0009\\u000A\\u000D]"),
                Member::NotSpace => self.out.push_str("[^\\u0020\\u0009\\u000A\\u000D]"),
                Member::Range(..) => unreachable!("an escape is one character or a class"),
            },
            // Metacharacters that must be escaped to stand for themselves.
            '?' | '*' | '+' | '{' | '}' | ')' | ']' | '|' => return Err(Untranslated::Invalid),
            c => self.literal(c),
        }
        Ok(true)
    }

    /// What the escape after a `\` stands for.
    fn escape(&mut self) -> Result<Member, Untranslated> {
        let c = self.next().ok_or(Untranslated::Invalid)?;
        Ok(Member::Char(match c {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '\\' | '|' | '.' | '?' | '*' | '+' | '(' | ')' | '{' | '}' | '-' | '[' | ']' | '^'
            | '$' => c,
            's' => return Ok(Member::Space),
            'S' => return Ok(Member::NotSpace),
            'd' | 'D' | 'w' | 'W' | 'i' | 'I' | 'c' | 'C' | 'p' | 'P' => {
                return Err(Untranslated::Unsupported(format!(
                    "\\{c} in REGEX, whose Unicode character class PostgreSQL does not have \
                     independently of the locale"
                )));
            }
            '1'..='9' => return Err(unsupported("back-references in REGEX")),
            _ => return Err(Untranslated::Invalid),
        }))
    }

    /// A character class expression, after its `[`.
    fn class(&mut self) -> Result<(), Untranslated> {
        let negated = self.peek() == Some('^');
        if negated {
            self.pos += 1;
        }
        let mut members = Vec::new();
        loop {
            let c = self.next().ok_or(Untranslated::Invalid)?;
            match c {
                ']' if !members.is_empty() => break,
                '-' if self.peek() == Some('[') => {
                    return Err(unsupported("character class subtraction in REGEX"));
                }
                '[' | ']' => return Err(Untranslated::Invalid),
                _ => {}
            }
            let first = match c {
                '\\' => self.escape()?,
                // `-` stands for itself first or last in a class.
                '-' if !members.is_empty() && self.peek() != Some(']') => {
                    return Err(Untranslated::Invalid);
                }
                c => Member::Char(c),
            };
            let low = match first {
                Member::Char(low) => low,
                Member::NotSpace => {
                    return Err(unsupported("\\S inside a character class in REGEX"));
                }
                _ => {
                    members.push(first);
                    continue;
                }
            };
            let is_range = self.peek() == Some('-') && self.chars.get(self.pos + 1) != Some(&']');
            if !is_range {
                members.push(first);
                continue;
            }
            self.pos += 1;
            let high = match self.next().ok_or(Untranslated::Invalid)? {
                '\\' => match self.escape()? {
                    Member::Char(c) => c,
                    _ => return Err(Untranslated::Invalid),
                },
                '[' | ']' => return Err(Untranslated::Invalid),
                c => c,
            };
            if high < low {
                return Err(Untranslated::Invalid);
            }
            members.push(Member::Range(low, high));
        }
        self.out.push('[');
        if negated {
            self.out.push('^');
        }
        let mut extra = BTreeSet::new();
        for member in &members {
            match *member {
                Member::Char(c) => {
                    self.out.push_str(&escaped(c));
                    if self.case_insensitive {
                        extra.extend(case_variants(c));
                    }
                }
                Member::Range(low, high) => {
                    self.out
                        .push_str(&format!("{}-{}", escaped(low), escaped(high)));
                    if self.case_insensitive {
                        if u32::from(high) - u32::from(low) > MAX_CASE_RANGE {
                            return Err(unsupported(
                                "large character ranges in case-insensitive REGEX",
                            ));
                        }
                        for c in low..=high {
                            extra.extend(case_variants(c));
                        }
                    }
                }
                Member::Space => self.out.push_str("\\u0020\\u0009\\u000A\\u000D"),
                Member::NotSpace => unreachable!("rejected while parsing"),
            }
        }
        let covered = |c: char| {
            members.iter().any(|m| match *m {
                Member::Char(m) => m == c,
                Member::Range(low, high) => (low..=high).contains(&c),
                Member::Space | Member::NotSpace => false,
            })
        };
        for c in extra.into_iter().filter(|&c| !covered(c)) {
            self.out.push_str(&escaped(c));
        }
        self.out.push(']');
        Ok(())
    }

    /// A character that stands for itself, with its case variants under
    /// the `i` flag.
    fn literal(&mut self, c: char) {
        let variants = if self.case_insensitive {
            case_variants(c)
        } else {
            BTreeSet::new()
        };
        if variants.len() <= 1 {
            self.out.push_str(&escaped(c));
            return;
        }
        self.out.push('[');
        for v in variants {
            self.out.push_str(&escaped(v));
        }
        self.out.push(']');
    }
}

/// `c` as itself when it is an ASCII letter or digit, else as a
/// character-entry escape, which is literal everywhere in a pattern.
fn escaped(c: char) -> String {
    if c.is_ascii_alphanumeric() {
        c.to_string()
    } else if u32::from(c) <= 0xFFFF {
        format!("\\u{:04X}", u32::from(c))
    } else {
        format!("\\U{:08X}", u32::from(c))
    }
}

/// `c` and the characters `c2` for which `lower-case(c) eq lower-case(c2)`
/// or `upper-case(c) eq upper-case(c2)` (XPath F&O 3.1 §5.6.2, flag `i`),
/// with Unicode's default full case mappings.
fn case_variants(c: char) -> BTreeSet<char> {
    let (by_lower, by_upper) = case_maps();
    let mut out = BTreeSet::from([c]);
    for (map, key) in [(by_lower, lower(c)), (by_upper, upper(c))] {
        if let Some(chars) = map.get(&key) {
            out.extend(chars);
        }
    }
    out
}

fn lower(c: char) -> String {
    c.to_lowercase().collect()
}

fn upper(c: char) -> String {
    c.to_uppercase().collect()
}

type CaseMap = HashMap<String, Vec<char>>;

/// Every character that case mappings change, by its lower-case and by its
/// upper-case string. A character they leave alone has no variants but
/// those that map onto it, which these maps find under its own string.
fn case_maps() -> &'static (CaseMap, CaseMap) {
    static MAPS: OnceLock<(CaseMap, CaseMap)> = OnceLock::new();
    MAPS.get_or_init(|| {
        let mut by_lower: CaseMap = HashMap::new();
        let mut by_upper: CaseMap = HashMap::new();
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            let (l, u) = (lower(c), upper(c));
            let unchanged = |s: &str| s.chars().eq(std::iter::once(c));
            if unchanged(&l) && unchanged(&u) {
                continue;
            }
            by_lower.entry(l).or_default().push(c);
            by_upper.entry(u).or_default().push(c);
        }
        (by_lower, by_upper)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(pattern: &str, flags: &str) -> String {
        translate(pattern, flags).unwrap_or_else(|e| panic!("{pattern:?} {flags:?}: {e:?}"))
    }

    fn unsupported_reason(pattern: &str, flags: &str) -> String {
        match translate(pattern, flags) {
            Err(Untranslated::Unsupported(reason)) => reason,
            other => panic!("{pattern:?} {flags:?}: {other:?}"),
        }
    }

    #[test]
    fn literals_are_escaped_unless_ascii_letters_or_digits() {
        assert_eq!(ok("ab1", ""), "ab1");
        assert_eq!(ok("a b-c,é", ""), "a\\u0020b\\u002Dc\\u002C\\u00E9");
        assert_eq!(
            ok("\\.\\-\\$\\n\\t", ""),
            "\\u002E\\u002D\\u0024\\u000A\\u0009"
        );
        assert_eq!(ok("\u{1F600}", ""), "\\U0001F600");
        assert_eq!(ok("", ""), "");
    }

    #[test]
    fn structure_is_kept() {
        assert_eq!(ok("a|(b|c)*", ""), "a|(b|c)*");
        assert_eq!(
            ok("(?:ab)+?x{2}y{2,}z{0,3}?", ""),
            "(?:ab)+?x{2}y{2,}z{0,3}?"
        );
        assert_eq!(ok("^a$", ""), "^a$");
        assert_eq!(ok("[^a-c.]", ""), "[^a-c\\u002E]");
        assert_eq!(ok("[-a]", ""), "[\\u002Da]");
        assert_eq!(ok("[a-]", ""), "[a\\u002D]");
        assert_eq!(ok("[\\]\\[]", ""), "[\\u005D\\u005B]");
    }

    #[test]
    fn dot_whitespace_and_anchors_follow_xpath() {
        assert_eq!(ok("a.b", ""), "a[^\\u000A\\u000D]b");
        assert_eq!(ok("a.b", "s"), "a.b");
        assert_eq!(
            ok("\\s\\S", ""),
            "[\\u0020\\u0009\\u000A\\u000D][^\\u0020\\u0009\\u000A\\u000D]"
        );
        assert_eq!(ok("[\\s,]", ""), "[\\u0020\\u0009\\u000A\\u000D\\u002C]");
        assert_eq!(
            ok("^a$", "m"),
            "(?:^|(?<=\\u000A)(?!$))a(?:(?=\\u000A)|(?<!\\u000A)$)"
        );
        // Whitespace goes, except inside a class.
        assert_eq!(ok(" a b [ c] ", "x"), "ab[\\u0020c]");
    }

    #[test]
    fn case_insensitive_matching_lists_the_case_variants() {
        assert_eq!(ok("aB1", "i"), "[Aa][Bb]1");
        // XPath F&O 3.1 §5.6.2: lower-case or upper-case equal. The dotless
        // i upper-cases to I; the Kelvin sign lower-cases to k; the dotted
        // capital I lower-cases to two characters and is no variant.
        assert_eq!(ok("i", "i"), "[Ii\\u0131]");
        assert_eq!(ok("k", "i"), "[Kk\\u212A]");
        assert_eq!(ok("[a-c]", "i"), "[a-cABC]");
        assert_eq!(ok("[^x]", "i"), "[^xX]");
        assert!(unsupported_reason("[\u{100}-\u{FFFF}]", "i").contains("large"));
    }

    #[test]
    fn unsupported_and_invalid_patterns() {
        for (pattern, flags) in [
            ("\\d", ""),
            ("\\w+", ""),
            ("\\p{Lu}", ""),
            ("(a)\\1", ""),
            ("[a-z-[aeiou]]", ""),
            ("a{256}", ""),
            ("[\\S]", ""),
            ("^*", ""),
            ("a", "q"),
        ] {
            unsupported_reason(pattern, flags);
        }
        for (pattern, flags) in [
            ("(", ""),
            (")", ""),
            ("*a", ""),
            ("a{2,1}", ""),
            ("a{", ""),
            ("[]", ""),
            ("[a", ""),
            ("a]", ""),
            ("a}", ""),
            ("\\", ""),
            ("\\z", ""),
            ("[a-c-e]", ""),
            ("[z-a]", ""),
            ("a", "g"),
        ] {
            assert_eq!(
                translate(pattern, flags),
                Err(Untranslated::Invalid),
                "{pattern:?}"
            );
        }
    }
}
