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
//!
//! `REPLACE` (SPARQL 1.1 §17.4.3.15) also depends on which match is
//! chosen where several start at one position. XPath takes the first
//! alternative and the greedy or reluctant choice of each quantifier in
//! turn (F&O 3.1 §5.6.4), PostgreSQL the longest match of the whole
//! pattern. [`translate_replace`] accepts only patterns where both choose
//! the same match: no alternatives, reluctant quantifiers or quantified
//! groups, and every repeated character set followed by characters it
//! cannot match. Each repetition then takes as many characters as it can,
//! whichever engine runs it.

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
    Ok(parse(pattern, flags)?.out)
}

/// A pattern and a replacement string for PostgreSQL's `regexp_replace`
/// with the flag `g`, which replace what `fn:replace` replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Replacement {
    pub pattern: String,
    pub replacement: String,
}

/// The `regexp_replace` arguments for `fn:replace(input, pattern,
/// replacement, flags)` (F&O 3.1 §5.6.4).
pub(crate) fn translate_replace(
    pattern: &str,
    flags: &str,
    replacement: &str,
) -> Result<Replacement, Untranslated> {
    let parser = parse(pattern, flags)?;
    // err:FORX0003: the pattern matches the empty string.
    if parser.nullable {
        return Err(Untranslated::Invalid);
    }
    let replacement = replacement_string(replacement, parser.groups)?;
    if parser.ambiguous {
        return Err(unsupported(AMBIGUOUS));
    }
    for (i, item) in parser.items.iter().enumerate() {
        if let Item::Chars { set, min, max } = item
            && Some(*min) != *max
            && set.intersects(&follow(&parser.items[i + 1..], parser.multi_line))
        {
            return Err(unsupported(AMBIGUOUS));
        }
    }
    Ok(Replacement {
        pattern: parser.out,
        replacement,
    })
}

const AMBIGUOUS: &str = "REPLACE with a pattern where XPath and PostgreSQL can choose different \
                         matches: alternatives, reluctant quantifiers, quantified groups, or a \
                         repeated character that what follows can also start with";

/// The characters that can come right after a piece followed by `rest`
/// in a match, with nothing after the end of the string.
fn follow(rest: &[Item], multi_line: bool) -> Chars {
    let mut out = Chars::default();
    for item in rest {
        match item {
            Item::Chars { set, min, .. } => {
                out = out.union(set);
                if *min > 0 {
                    break;
                }
            }
            Item::Start => {}
            // Only the end of the string, or with `m` a newline, follows.
            Item::End => {
                if multi_line {
                    out = out.union(&Chars::of(&['\n']));
                }
                break;
            }
        }
    }
    out
}

/// The replacement string of `fn:replace` as one for `regexp_replace`:
/// `$N` is group `N` (`$0` the whole match), `\$` and `\\` are `$` and
/// `\`, and the rest stands for itself. With `groups` groups, a reference
/// to a group that does not exist is the empty string for `N` up to 9;
/// above, its last digit is a character after the reference without it.
/// err:FORX0004 for a `$` without a digit or another `\`.
fn replacement_string(replacement: &str, groups: usize) -> Result<String, Untranslated> {
    fn reference(digits: &str, groups: usize) -> Result<String, Untranslated> {
        let n: usize = digits.parse().unwrap_or(usize::MAX);
        if n == 0 {
            Ok("\\&".to_owned())
        } else if n <= groups {
            if n > 9 {
                return Err(unsupported(
                    "REPLACE with a reference to a group after the ninth, which PostgreSQL's \
                     replacement strings cannot name",
                ));
            }
            Ok(format!("\\{n}"))
        } else if n <= 9 {
            Ok(String::new())
        } else {
            let (head, last) = digits.split_at(digits.len() - 1);
            Ok(reference(head, groups)? + last)
        }
    }
    let chars: Vec<char> = replacement.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            '\\' => {
                match chars.get(i) {
                    Some('\\') => out.push_str("\\\\"),
                    Some('$') => out.push('$'),
                    _ => return Err(Untranslated::Invalid),
                }
                i += 1;
            }
            '$' => {
                let digits: String = chars[i..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if digits.is_empty() {
                    return Err(Untranslated::Invalid);
                }
                out.push_str(&reference(&digits, groups)?);
                i += digits.len();
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// The parser after reading `pattern` with `flags`.
fn parse(pattern: &str, flags: &str) -> Result<Parser, Untranslated> {
    let mut parser = Parser {
        chars: Vec::new(),
        pos: 0,
        dot_all: false,
        multi_line: false,
        case_insensitive: false,
        out: String::new(),
        groups: 0,
        nullable: false,
        ambiguous: false,
        items: Vec::new(),
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
    parser.nullable = parser.reg_exp()?;
    if parser.pos < parser.chars.len() {
        // An unmatched `)`.
        return Err(Untranslated::Invalid);
    }
    Ok(parser)
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
    /// Capturing groups, which keep their numbers in `out`.
    groups: usize,
    /// Whether the pattern matches the empty string.
    nullable: bool,
    /// Whether it has an alternative, a reluctant quantifier or a
    /// quantified group.
    ambiguous: bool,
    /// The pieces in the order written, those inside groups included.
    items: Vec<Item>,
}

/// A piece of a pattern, for the choice of matches by `REPLACE`.
#[derive(Debug, Clone)]
enum Item {
    /// A character of `set`, from `min` to `max` (no bound for `None`)
    /// times.
    Chars {
        set: Chars,
        min: u32,
        max: Option<u32>,
    },
    /// `^`.
    Start,
    /// `$`.
    End,
}

/// A set of characters, as sorted, disjoint, non-adjacent ranges of code
/// points.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Chars(Vec<(u32, u32)>);

impl Chars {
    fn of(chars: &[char]) -> Self {
        Self::ranges(chars.iter().map(|&c| (u32::from(c), u32::from(c))))
    }

    fn ranges(ranges: impl IntoIterator<Item = (u32, u32)>) -> Self {
        let mut ranges: Vec<(u32, u32)> = ranges.into_iter().collect();
        ranges.sort_unstable();
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for (low, high) in ranges {
            match out.last_mut() {
                Some(last) if low <= last.1.saturating_add(1) => last.1 = last.1.max(high),
                _ => out.push((low, high)),
            }
        }
        Self(out)
    }

    fn union(&self, other: &Self) -> Self {
        Self::ranges(self.0.iter().chain(&other.0).copied())
    }

    fn complement(&self) -> Self {
        let mut out = Vec::new();
        let mut next = 0;
        for &(low, high) in &self.0 {
            if low > next {
                out.push((next, low - 1));
            }
            next = high + 1;
        }
        if next <= 0x10FFFF {
            out.push((next, 0x10FFFF));
        }
        Self(out)
    }

    fn intersects(&self, other: &Self) -> bool {
        self.0
            .iter()
            .any(|&(a, b)| other.0.iter().any(|&(c, d)| a <= d && c <= b))
    }
}

/// What an atom is.
struct Atom {
    /// Whether it can take a quantifier.
    quantifiable: bool,
    nullable: bool,
    /// For a character, `.`, an escape or a class: the characters it
    /// matches.
    chars: Option<Chars>,
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

    /// Returns whether the expression matches the empty string.
    fn reg_exp(&mut self) -> Result<bool, Untranslated> {
        let mut nullable = self.branch()?;
        while self.peek() == Some('|') {
            self.pos += 1;
            self.out.push('|');
            self.ambiguous = true;
            nullable |= self.branch()?;
        }
        Ok(nullable)
    }

    fn branch(&mut self) -> Result<bool, Untranslated> {
        let mut nullable = true;
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            nullable &= self.piece()?;
        }
        Ok(nullable)
    }

    fn piece(&mut self) -> Result<bool, Untranslated> {
        let atom = self.atom()?;
        if !matches!(self.peek(), Some('?' | '*' | '+' | '{')) {
            return Ok(atom.nullable);
        }
        if !atom.quantifiable {
            return Err(unsupported("a quantifier on ^ or $ in REGEX"));
        }
        let (min, max) = self.quantifier()?;
        match atom.chars {
            Some(set) => {
                let last = self.items.last_mut().expect("the atom's item");
                *last = Item::Chars { set, min, max };
            }
            None => self.ambiguous = true,
        }
        Ok(atom.nullable || min == 0)
    }

    /// Returns the least and the most repetitions, `None` for no bound.
    fn quantifier(&mut self) -> Result<(u32, Option<u32>), Untranslated> {
        let bounds = match self.next() {
            Some(c @ ('?' | '*' | '+')) => {
                self.out.push(c);
                match c {
                    '?' => (0, Some(1)),
                    '*' => (0, None),
                    _ => (1, None),
                }
            }
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
                (min, max)
            }
            _ => return Err(Untranslated::Invalid),
        };
        // A reluctant quantifier matches the same strings; PostgreSQL has it.
        if self.peek() == Some('?') {
            self.pos += 1;
            self.out.push('?');
            self.ambiguous = true;
        }
        Ok(bounds)
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

    fn atom(&mut self) -> Result<Atom, Untranslated> {
        let anchor = |this: &mut Self, item: Item| {
            this.items.push(item);
            Ok(Atom {
                quantifiable: false,
                nullable: true,
                chars: None,
            })
        };
        let c = self.next().ok_or(Untranslated::Invalid)?;
        let chars = match c {
            '(' => {
                if self.chars.get(self.pos..self.pos + 2) == Some(&['?', ':']) {
                    self.pos += 2;
                    self.out.push_str("(?:");
                } else {
                    self.out.push('(');
                    self.groups += 1;
                }
                let nullable = self.reg_exp()?;
                if self.next() != Some(')') {
                    return Err(Untranslated::Invalid);
                }
                self.out.push(')');
                return Ok(Atom {
                    quantifiable: true,
                    nullable,
                    chars: None,
                });
            }
            '[' => self.class()?,
            '.' if self.dot_all => {
                self.out.push('.');
                Chars::default().complement()
            }
            '.' => {
                self.out.push_str("[^\\u000A\\u000D]");
                Chars::of(&['\n', '\r']).complement()
            }
            '^' if self.multi_line => {
                // After a newline that is not the last character.
                self.out.push_str("(?:^|(?<=\\u000A)(?!$))");
                return anchor(self, Item::Start);
            }
            '^' => {
                self.out.push('^');
                return anchor(self, Item::Start);
            }
            '$' if self.multi_line => {
                // Before a newline, or at the end when no newline ends it.
                self.out.push_str("(?:(?=\\u000A)|(?<!\\u000A)$)");
                return anchor(self, Item::End);
            }
            '$' => {
                self.out.push('$');
                return anchor(self, Item::End);
            }
            '\\' => match self.escape()? {
                Member::Char(c) => self.literal(c),
                Member::Space => {
                    self.out.push_str("[\\u0020\\u0009\\u000A\\u000D]");
                    space()
                }
                Member::NotSpace => {
                    self.out.push_str("[^\\u0020\\u0009\\u000A\\u000D]");
                    space().complement()
                }
                Member::Range(..) => unreachable!("an escape is one character or a class"),
            },
            // Metacharacters that must be escaped to stand for themselves.
            '?' | '*' | '+' | '{' | '}' | ')' | ']' | '|' => return Err(Untranslated::Invalid),
            c => self.literal(c),
        };
        self.items.push(Item::Chars {
            set: chars.clone(),
            min: 1,
            max: Some(1),
        });
        Ok(Atom {
            quantifiable: true,
            nullable: false,
            chars: Some(chars),
        })
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

    /// A character class expression, after its `[`; returns the
    /// characters it matches.
    fn class(&mut self) -> Result<Chars, Untranslated> {
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
        let mut set = Chars::ranges(members.iter().flat_map(|m| match *m {
            Member::Char(c) => vec![(u32::from(c), u32::from(c))],
            Member::Range(low, high) => vec![(u32::from(low), u32::from(high))],
            Member::Space => space().0,
            Member::NotSpace => unreachable!("rejected while parsing"),
        }));
        set = set.union(&Chars::of(&extra.iter().copied().collect::<Vec<_>>()));
        for c in extra.into_iter().filter(|&c| !covered(c)) {
            self.out.push_str(&escaped(c));
        }
        self.out.push(']');
        Ok(if negated { set.complement() } else { set })
    }

    /// A character that stands for itself, with its case variants under
    /// the `i` flag; returns the characters it matches.
    fn literal(&mut self, c: char) -> Chars {
        let variants = if self.case_insensitive {
            case_variants(c)
        } else {
            BTreeSet::new()
        };
        if variants.len() <= 1 {
            self.out.push_str(&escaped(c));
            return Chars::of(&[c]);
        }
        self.out.push('[');
        for &v in &variants {
            self.out.push_str(&escaped(v));
        }
        self.out.push(']');
        Chars::of(&variants.into_iter().collect::<Vec<_>>())
    }
}

/// The characters of `\s`.
fn space() -> Chars {
    Chars::of(&[' ', '\t', '\n', '\r'])
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

    fn replace(pattern: &str, flags: &str, replacement: &str) -> Result<(String, String), Untranslated> {
        translate_replace(pattern, flags, replacement).map(|r| (r.pattern, r.replacement))
    }

    #[test]
    fn replacement_strings_name_groups_as_postgresql_does() {
        let groups = |replacement: &str| replace("(a)(b)c", "", replacement).map(|r| r.1);
        assert_eq!(groups("[$1-$2]"), Ok("[\\1-\\2]".to_owned()));
        assert_eq!(groups("$0"), Ok("\\&".to_owned()));
        // F&O 3.1 §5.6.4: a group that does not exist is empty up to 9;
        // above, the last digit is a character of its own.
        assert_eq!(groups("$3$9"), Ok(String::new()));
        assert_eq!(groups("$12"), Ok("\\12".to_owned()));
        assert_eq!(groups("$100"), Ok("\\100".to_owned()));
        assert_eq!(groups("$01"), Ok("\\1".to_owned()));
        assert_eq!(groups("\\$1 \\\\ & \\1"), Err(Untranslated::Invalid));
        assert_eq!(groups("\\$1 \\\\ &"), Ok("$1 \\\\ &".to_owned()));
        // err:FORX0004.
        assert_eq!(groups("$"), Err(Untranslated::Invalid));
        assert_eq!(groups("a$b"), Err(Untranslated::Invalid));
        assert_eq!(groups("\\a"), Err(Untranslated::Invalid));
        assert_eq!(groups("\\"), Err(Untranslated::Invalid));
        let many = replace("(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)", "", "$9");
        assert_eq!(many.map(|r| r.1), Ok("\\9".to_owned()));
        assert!(matches!(
            replace("(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)", "", "$10"),
            Err(Untranslated::Unsupported(reason)) if reason.contains("ninth")
        ));
        // Non-capturing groups keep the numbers of the others.
        assert_eq!(
            replace("(?:a)(b)", "", "$1").map(|r| r.1),
            Ok("\\1".to_owned())
        );
    }

    #[test]
    fn replace_rejects_patterns_that_match_the_empty_string() {
        // err:FORX0003, whether or not the pattern has other matches.
        for pattern in ["", "a*", "a?", "(a|)", "^", "$", "a{0,2}", "(?:a*)b?", "x*|y"] {
            assert_eq!(replace(pattern, "", "z"), Err(Untranslated::Invalid), "{pattern:?}");
        }
        assert_eq!(replace("a+", "", "z"), Ok(("a+".to_owned(), "z".to_owned())));
        assert_eq!(replace("^a", "m", "z").map(|r| r.1), Ok("z".to_owned()));
    }

    #[test]
    fn replace_takes_patterns_whose_matches_both_engines_choose_alike() {
        // Each repetition stops where nothing it matches can follow.
        for (pattern, flags) in [
            ("b", ""),
            ("a+b", ""),
            ("\\s+", ""),
            ("[^,]+,", ""),
            ("x[0-9]{1,3}y", ""),
            ("(\\s+)(-)", ""),
            ("a{2}a", ""),
            ("^\\s+", ""),
            ("a+$", ""),
            ("[a-z]+\\.", "i"),
            ("x.*", ""),
            ("[^\\n]+$", "m"),
            ("a.b", ""),
        ] {
            assert!(replace(pattern, flags, "").is_ok(), "{pattern:?} {flags:?}");
        }
        for (pattern, flags) in [
            // XPath takes the first alternative, PostgreSQL the longest.
            ("a|ab", ""),
            ("^\\s+|\\s+$", ""),
            ("a+?", ""),
            ("(ab)+", ""),
            ("(a)?b", ""),
            // a* followed by an a: XPath backtracks one, as PostgreSQL's
            // longest match does here, but not in general.
            ("a*ab", ""),
            ("[a-z]+[0-9a-z]", ""),
            ("a+b?a", ""),
            ("x.*y", ""),
            ("a+A", "i"),
            ("[a\\n]+$", "m"),
            (".+\\n", "s"),
        ] {
            assert!(
                matches!(
                    replace(pattern, flags, ""),
                    Err(Untranslated::Unsupported(reason)) if reason.contains("REPLACE")
                ),
                "{pattern:?} {flags:?}"
            );
        }
        // Not supported in REGEX either.
        assert!(matches!(replace("\\d+", "", ""), Err(Untranslated::Unsupported(_))));
        assert_eq!(replace("(", "", ""), Err(Untranslated::Invalid));
        assert_eq!(replace("a", "g", ""), Err(Untranslated::Invalid));
    }

    #[test]
    fn character_sets() {
        let set = Chars::of(&['c', 'a', 'b', 'x']);
        assert_eq!(set, Chars(vec![(97, 99), (120, 120)]));
        assert_eq!(
            set.complement(),
            Chars(vec![(0, 96), (100, 119), (121, 0x10FFFF)])
        );
        assert!(set.intersects(&Chars::of(&['x'])));
        assert!(!set.intersects(&Chars::of(&['d', 'w'])));
        assert_eq!(Chars::default().complement(), Chars(vec![(0, 0x10FFFF)]));
        assert_eq!(Chars::default().complement().complement(), Chars::default());
    }
}
