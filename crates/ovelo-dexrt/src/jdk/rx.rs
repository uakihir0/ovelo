//! Pike-VM regular expressions: linear-time matching with capture
//! groups, character classes, lazy quantifiers and alternation — the
//! subset `java.util.regex`/`kotlin.text.Regex` APIs actually exercise.

#[derive(Clone, Debug)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Set(u8), // b'd' | b'D' | b'w' | b'W' | b's' | b'S'
}

impl ClassItem {
    fn test(&self, c: char) -> bool {
        match self {
            ClassItem::Char(x) => *x == c,
            ClassItem::Range(a, b) => (*a..=*b).contains(&c),
            ClassItem::Set(k) => match k {
                b'd' => c.is_ascii_digit(),
                b'D' => !c.is_ascii_digit(),
                b'w' => c.is_alphanumeric() || c == '_',
                b'W' => !(c.is_alphanumeric() || c == '_'),
                b's' => c.is_whitespace(),
                b'S' => !c.is_whitespace(),
                _ => false,
            },
        }
    }
}

#[derive(Debug)]
enum Ast {
    Empty,
    Char(char),
    Any,
    Class { neg: bool, items: Vec<ClassItem> },
    Group(usize, Box<Ast>),
    Cat(Vec<Ast>),
    Alt(Vec<Ast>),
    Rep(Box<Ast>, u32, Option<u32>, bool),
    Bol,
    Eol,
    WordB(bool),
}

#[derive(Debug)]
enum Inst {
    Char(char),
    Any,
    Class { neg: bool, items: Vec<ClassItem> },
    Bol,
    Eol,
    WordB(bool),
    Save(usize),
    Jmp(usize),
    Split(usize, usize),
    Match,
}

struct Parser<'a> {
    p: &'a [char],
    i: usize,
    groups: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.p.get(self.i).copied()
    }
    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn parse_alt(&mut self) -> Result<Ast, String> {
        let mut arms = vec![self.parse_seq()?];
        while self.eat('|') {
            arms.push(self.parse_seq()?);
        }
        Ok(if arms.len() == 1 {
            arms.pop().unwrap()
        } else {
            Ast::Alt(arms)
        })
    }

    fn parse_seq(&mut self) -> Result<Ast, String> {
        let mut v = Vec::new();
        while let Some(c) = self.peek() {
            if c == ')' || c == '|' {
                break;
            }
            v.push(self.parse_rep()?);
        }
        Ok(match v.len() {
            0 => Ast::Empty,
            1 => v.pop().unwrap(),
            _ => Ast::Cat(v),
        })
    }

    fn parse_rep(&mut self) -> Result<Ast, String> {
        let mut a = self.parse_atom()?;
        loop {
            let (min, max) = match self.peek() {
                Some('*') => {
                    self.i += 1;
                    (0, None)
                }
                Some('+') => {
                    self.i += 1;
                    (1, None)
                }
                Some('?') => {
                    self.i += 1;
                    (0, Some(1))
                }
                Some('{') => {
                    let save = self.i;
                    match self.parse_bounds() {
                        Some(b) => b,
                        None => {
                            self.i = save;
                            break;
                        }
                    }
                }
                _ => break,
            };
            let lazy = self.eat('?');
            let _pos = self.eat('+'); // possessive — treat as greedy
            a = Ast::Rep(Box::new(a), min, max, lazy);
        }
        Ok(a)
    }

    fn parse_bounds(&mut self) -> Option<(u32, Option<u32>)> {
        // `{n}` `{n,}` `{n,m}` — caller has peeked '{'
        if self.next() != Some('{') {
            return None;
        }
        let mut lo = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                lo.push(c);
                self.i += 1;
            } else {
                break;
            }
        }
        if lo.is_empty() {
            return None;
        }
        let lo: u32 = lo.parse().ok()?;
        if self.eat('}') {
            return Some((lo, Some(lo)));
        }
        if !self.eat(',') {
            return None;
        }
        let mut hi = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                hi.push(c);
                self.i += 1;
            } else {
                break;
            }
        }
        if !self.eat('}') {
            return None;
        }
        Some((lo, hi.parse().ok().map(Some).unwrap_or(None)))
    }

    fn parse_atom(&mut self) -> Result<Ast, String> {
        match self.next() {
            Some('(') => {
                if self.eat('?') {
                    if self.eat(':') {
                        let inner = self.parse_alt()?;
                        if !self.eat(')') {
                            return Err("unclosed (?:)".into());
                        }
                        Ok(inner)
                    } else {
                        Err("unsupported (? construct".into())
                    }
                } else {
                    self.groups += 1;
                    let g = self.groups;
                    let inner = self.parse_alt()?;
                    if !self.eat(')') {
                        return Err("unclosed group".into());
                    }
                    Ok(Ast::Group(g, Box::new(inner)))
                }
            }
            Some('[') => self.parse_class(),
            Some('.') => Ok(Ast::Any),
            Some('^') => Ok(Ast::Bol),
            Some('$') => Ok(Ast::Eol),
            Some('\\') => self.parse_escape(),
            Some(c) => Ok(Ast::Char(c)),
            None => Err("unexpected end".into()),
        }
    }

    fn parse_escape(&mut self) -> Result<Ast, String> {
        match self.next() {
            Some('d') | Some('D') | Some('w') | Some('W') | Some('s') | Some('S') => {
                let k = self.p[self.i - 1] as u8;
                Ok(Ast::Class {
                    neg: false,
                    items: vec![ClassItem::Set(k)],
                })
            }
            Some('b') => Ok(Ast::WordB(true)),
            Some('B') => Ok(Ast::WordB(false)),
            Some('A') => Ok(Ast::Bol),
            Some('z') | Some('Z') => Ok(Ast::Eol),
            Some('n') => Ok(Ast::Char('\n')),
            Some('r') => Ok(Ast::Char('\r')),
            Some('t') => Ok(Ast::Char('\t')),
            Some('f') => Ok(Ast::Char('\x0C')),
            Some('v') => Ok(Ast::Char('\x0B')),
            Some('0') => Ok(Ast::Char('\0')),
            Some('u') => {
                let mut v = 0u32;
                for _ in 0..4 {
                    match self.next().and_then(|c| c.to_digit(16)) {
                        Some(d) => v = v * 16 + d,
                        None => return Err("bad \\u escape".into()),
                    }
                }
                char::from_u32(v).map(Ast::Char).ok_or_else(|| "bad \\u".into())
            }
            Some('x') => {
                let mut v = 0u32;
                let mut n = 0;
                if self.eat('{') {
                    while let Some(c) = self.peek() {
                        if c == '}' {
                            break;
                        }
                        if let Some(d) = c.to_digit(16) {
                            v = v.wrapping_mul(16).wrapping_add(d);
                            n += 1;
                        }
                        self.i += 1;
                    }
                    self.eat('}');
                } else {
                    for _ in 0..2 {
                        match self.peek().and_then(|c| c.to_digit(16)) {
                            Some(d) => {
                                v = v * 16 + d;
                                n += 1;
                                self.i += 1;
                            }
                            None => break,
                        }
                    }
                }
                if n == 0 {
                    return Err("bad \\x escape".into());
                }
                char::from_u32(v).map(Ast::Char).ok_or_else(|| "bad \\x".into())
            }
            Some('Q') => {
                // \Q...\E literal run
                let mut v = Vec::new();
                loop {
                    match self.next() {
                        None => break,
                        Some('\\') if self.peek() == Some('E') => {
                            self.i += 1;
                            break;
                        }
                        Some(c) => v.push(Ast::Char(c)),
                    }
                }
                Ok(match v.len() {
                    0 => Ast::Empty,
                    1 => v.pop().unwrap(),
                    _ => Ast::Cat(v),
                })
            }
            Some('E') => Ok(Ast::Empty),
            Some(c) => Ok(Ast::Char(c)), // \. \* \\ \/ \- etc, plus letters we don't know
            None => Err("trailing \\".into()),
        }
    }

    fn parse_class(&mut self) -> Result<Ast, String> {
        let neg = self.eat('^');
        let mut items = Vec::new();
        // A `]` right after `[` or `[^` is a literal `]`.
        if self.peek() == Some(']') {
            items.push(ClassItem::Char(']'));
            self.i += 1;
        }
        loop {
            let Some(mut lo) = self.next() else {
                return Err("unclosed class".into());
            };
            if lo == ']' {
                break;
            }
            if lo == '\\' {
                match self.next() {
                    Some('d') | Some('D') | Some('w') | Some('W') | Some('s') | Some('S') => {
                        let k = self.p[self.i - 1] as u8;
                        items.push(ClassItem::Set(k));
                        continue;
                    }
                    Some('n') => lo = '\n',
                    Some('r') => lo = '\r',
                    Some('t') => lo = '\t',
                    Some('f') => lo = '\x0C',
                    Some('u') => {
                        let mut v = 0u32;
                        for _ in 0..4 {
                            match self.next().and_then(|c| c.to_digit(16)) {
                                Some(d) => v = v * 16 + d,
                                None => return Err("bad \\u".into()),
                            }
                        }
                        lo = char::from_u32(v).unwrap_or('\u{FFFD}');
                    }
                    Some(c) => lo = c,
                    None => return Err("unclosed class".into()),
                }
            }
            // range?
            if self.peek() == Some('-')
                && self.p.get(self.i + 1).copied().is_some()
                && self.p[self.i + 1] != ']'
            {
                self.i += 1;
                let mut hi = self.next().unwrap();
                if hi == '\\' {
                    hi = self.next().unwrap_or(hi);
                }
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Char(lo));
            }
        }
        Ok(Ast::Class { neg, items })
    }
}

fn emit(a: &Ast, out: &mut Vec<Inst>) {
    match a {
        Ast::Empty => {}
        Ast::Char(c) => out.push(Inst::Char(*c)),
        Ast::Any => out.push(Inst::Any),
        Ast::Class { neg, items } => out.push(Inst::Class {
            neg: *neg,
            items: items.clone(),
        }),
        Ast::Bol => out.push(Inst::Bol),
        Ast::Eol => out.push(Inst::Eol),
        Ast::WordB(w) => out.push(Inst::WordB(*w)),
        Ast::Group(i, inner) => {
            out.push(Inst::Save(2 * i));
            emit(inner, out);
            out.push(Inst::Save(2 * i + 1));
        }
        Ast::Cat(v) => {
            for a in v {
                emit(a, out);
            }
        }
        Ast::Alt(arms) => {
            let mut jmp_holes = Vec::new();
            for (i, arm) in arms.iter().enumerate() {
                if i + 1 < arms.len() {
                    let split = out.len();
                    out.push(Inst::Split(0, 0));
                    let body = out.len();
                    emit(arm, out);
                    let j = out.len();
                    out.push(Inst::Jmp(0));
                    jmp_holes.push(j);
                    let next = out.len();
                    out[split] = Inst::Split(body, next);
                } else {
                    emit(arm, out);
                }
            }
            let end = out.len();
            for j in jmp_holes {
                out[j] = Inst::Jmp(end);
            }
        }
        Ast::Rep(inner, min, max, lazy) => {
            // `min` mandatory copies, then `max-min` optionals (or a star).
            for _ in 0..*min {
                emit(inner, out);
            }
            match max {
                None => {
                    // e* — lazy swaps split order
                    let split = out.len();
                    out.push(Inst::Split(0, 0));
                    let body = out.len();
                    emit(inner, out);
                    out.push(Inst::Jmp(split));
                    let next = out.len();
                    out[split] = if *lazy {
                        Inst::Split(next, body)
                    } else {
                        Inst::Split(body, next)
                    };
                }
                Some(hi) => {
                    let opt = hi.saturating_sub(*min);
                    if opt > 0 && *lazy {
                        // Nested optional blocks, evaluated lazily:
                        // (?:(?:e(?:e)?)?) unrolled with deferred splits.
                        let mut holes = Vec::new();
                        for _ in 0..opt {
                            let s = out.len();
                            out.push(Inst::Split(0, 0));
                            holes.push(s);
                            let body = out.len();
                            out[s] = Inst::Split(body, usize::MAX); // patch skip after
                            emit(inner, out);
                        }
                        let end = out.len();
                        for s in holes {
                            if let Inst::Split(body, _) = out[s] {
                                out[s] = Inst::Split(end, body);
                            }
                        }
                    } else {
                        let mut holes = Vec::new();
                        for _ in 0..opt {
                            let s = out.len();
                            out.push(Inst::Split(0, 0));
                            holes.push(s);
                            emit(inner, out);
                        }
                        let end = out.len();
                        for s in holes {
                            out[s] = Inst::Split(s + 1, end);
                        }
                    }
                }
            }
        }
    }
}

type Saves = std::rc::Rc<Vec<Option<usize>>>;

/// A compiled pattern: `find`, `looking_at` and `matches` all return the
/// group's `(start, end)` char offsets, index `2g`/`2g+1` per group.
pub(crate) struct Rx {
    insts: Vec<Inst>,
    nslots: usize,
}

impl Rx {
    pub(crate) fn compile(pat: &str) -> Result<Rx, String> {
        let chars: Vec<char> = pat.chars().collect();
        let mut p = Parser {
            p: &chars,
            i: 0,
            groups: 0,
        };
        let ast = p.parse_alt()?;
        if p.i < chars.len() {
            return Err(format!("trailing pattern at {}", p.i));
        }
        let mut insts = vec![Inst::Save(0)];
        emit(&ast, &mut insts);
        insts.push(Inst::Save(1));
        insts.push(Inst::Match);
        Ok(Rx {
            insts,
            nslots: 2 * (p.groups + 1),
        })
    }

    pub(crate) fn ngroups(&self) -> usize {
        self.nslots / 2 - 1
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &self,
        list: &mut Vec<(usize, Saves)>,
        seen: &mut std::collections::HashSet<usize>,
        pc: usize,
        sp: usize,
        saves: &Saves,
        lo: usize,
        hi: usize,
        text: &[char],
    ) {
        if !seen.insert(pc) {
            return;
        }
        match &self.insts[pc] {
            Inst::Jmp(x) => self.add(list, seen, *x, sp, saves, lo, hi, text),
            Inst::Split(x, y) => {
                self.add(list, seen, *x, sp, saves, lo, hi, text);
                self.add(list, seen, *y, sp, saves, lo, hi, text);
            }
            Inst::Save(i) => {
                let mut s = (*saves.clone()).clone();
                s[*i] = Some(sp);
                self.add(list, seen, pc + 1, sp, &std::rc::Rc::new(s), lo, hi, text);
            }
            Inst::Bol => {
                if sp == lo {
                    self.add(list, seen, pc + 1, sp, saves, lo, hi, text);
                }
            }
            Inst::Eol => {
                if sp == hi {
                    self.add(list, seen, pc + 1, sp, saves, lo, hi, text);
                }
            }
            Inst::WordB(want) => {
                let w = |c: char| c.is_alphanumeric() || c == '_';
                let a = sp > 0 && w(text[sp - 1]);
                let b = sp < text.len() && w(text[sp]);
                if (a != b) == *want {
                    self.add(list, seen, pc + 1, sp, saves, lo, hi, text);
                }
            }
            _ => list.push((pc, saves.clone())),
        }
    }

    /// Leftmost-first search for a match starting at or after `from`
    /// (inside region `lo..hi`). `anchored` pins the start to `from`;
    /// `full` additionally requires the match to end at `hi`.
    fn run(
        &self,
        text: &[char],
        lo: usize,
        from: usize,
        hi: usize,
        anchored: bool,
        full: bool,
    ) -> Option<Vec<Option<usize>>> {
        let hi = hi.min(text.len());
        if from > hi {
            return None;
        }
        let init: Saves = std::rc::Rc::new(vec![None; self.nslots]);
        let mut clist: Vec<(usize, Saves)> = Vec::new();
        let mut nlist: Vec<(usize, Saves)> = Vec::new();
        let mut sp = from;
        let mut seeded = false;
        let mut matched: Option<Saves> = None;
        while sp <= hi {
            if !seeded && matched.is_none() {
                // New lowest-priority thread starting at sp. Anchored
                // searches seed once; find() seeds each position.
                let mut seen = std::collections::HashSet::new();
                self.add(&mut clist, &mut seen, 0, sp, &init, lo, hi, text);
                seeded = anchored;
            }
            let mut seen_n = std::collections::HashSet::new();
            for (pc, saves) in clist.drain(..) {
                let ok = match &self.insts[pc] {
                    Inst::Match => {
                        if !full || sp == hi {
                            // Cut lower-priority threads: higher ones
                            // already moved to nlist and may produce a
                            // better (greedy-longer) match later.
                            matched = Some(saves);
                            break;
                        }
                        false
                    }
                    Inst::Char(c) => sp < hi && text[sp] == *c,
                    Inst::Any => sp < hi && text[sp] != '\n',
                    Inst::Class { neg, items } => {
                        sp < hi && items.iter().any(|i| i.test(text[sp])) != *neg
                    }
                    _ => false,
                };
                if ok {
                    self.add(&mut nlist, &mut seen_n, pc + 1, sp + 1, &saves, lo, hi, text);
                }
            }
            std::mem::swap(&mut clist, &mut nlist);
            nlist.clear();
            if clist.is_empty() && (matched.is_some() || anchored || sp >= hi) {
                return matched.map(|s| (*s).clone());
            }
            if matched.is_some() {
                seeded = true; // survivors outrank any later start
            }
            sp += 1;
        }
        matched.map(|s| (*s).clone())
    }

    /// `Matcher.find()`: leftmost match at or after `from` within `lo..hi`.
    pub(crate) fn find(
        &self,
        text: &[char],
        lo: usize,
        from: usize,
        hi: usize,
    ) -> Option<Vec<Option<usize>>> {
        self.run(text, lo, from, hi, false, false)
    }

    /// `Matcher.lookingAt()`: match anchored at `from`, free end.
    pub(crate) fn looking_at(
        &self,
        text: &[char],
        lo: usize,
        from: usize,
        hi: usize,
    ) -> Option<Vec<Option<usize>>> {
        self.run(text, lo, from, hi, true, false)
    }

    /// `Matcher.matches()`: entire `lo..hi` region must match.
    pub(crate) fn matches(&self, text: &[char], lo: usize, hi: usize) -> Option<Vec<Option<usize>>> {
        self.run(text, lo, lo, hi, true, true)
    }
}

/// `String.split(regex, limit)` / `Pattern.split` — Java semantics:
/// `limit` > 0 caps the result length (the tail lands in the last
/// element), `limit` = 0 drops trailing empty strings, `limit` < 0
/// keeps everything. A zero-width match adjacent to the end of the
/// previous match is skipped (Java 8 rule); a zero-width match at 0
/// still yields a leading empty string.
pub(crate) fn regex_split(pat: &str, text: &str, limit: i32) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let Ok(rx) = Rx::compile(pat) else {
        return vec![text.to_string()];
    };
    let mut out: Vec<String> = Vec::new();
    let mut last = 0usize;
    let mut from = 0usize;
    while from <= n && (limit <= 0 || (out.len() as i32) < limit - 1) {
        let Some(m) = rx.find(&chars, 0, from, n) else {
            break;
        };
        let s = m.first().copied().flatten().unwrap_or(0);
        let e = m.get(1).copied().flatten().unwrap_or(s);
        if e == s && s == last && !out.is_empty() {
            from = s + 1;
            continue;
        }
        out.push(chars[last..s].iter().collect());
        last = e;
        from = if e == s { e + 1 } else { e };
    }
    out.push(chars[last..].iter().collect());
    if limit == 0 {
        while out.last().is_some_and(|p| p.is_empty()) {
            out.pop();
        }
    }
    out
}

/// `Pattern.matches(pat, text)` — full-input match.
pub(crate) fn full_match(pat: &str, text: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    match Rx::compile(pat) {
        Ok(rx) => rx.matches(&t, 0, t.len()).is_some(),
        Err(_) => pat == text,
    }
}

#[cfg(test)]
mod tests {
    use super::Rx;
    fn t(s: &str) -> Vec<char> {
        s.chars().collect()
    }
    #[test]
    fn okhttp_media_type() {
        let ts = Rx::compile("([a-zA-Z0-9-!#$%&'*+.^_`{|}~]+)/([a-zA-Z0-9-!#$%&'*+.^_`{|}~]+)").unwrap();
        let s = t("application/json; charset=utf-8");
        let m = ts.looking_at(&s, 0, 0, s.len()).expect("TYPE_SUBTYPE match");
        assert_eq!(m[0], Some(0));
        assert_eq!(m[1], Some(16));
        assert_eq!(m[2], Some(0));
        assert_eq!(m[3], Some(11));
        let pa = Rx::compile(";\\s*(?:([a-zA-Z0-9-!#$%&'*+.^_`{|}~]+)=(?:([a-zA-Z0-9-!#$%&'*+.^_`{|}~]+)|\"([^\"]*)\"))?").unwrap();
        let m = pa.looking_at(&s, 16, 16, s.len()).expect("PARAMETER match");
        assert_eq!(m[0], Some(16));
        assert_eq!(m[1], Some(31));
    }
    #[test]
    fn lazy_group() {
        let rx = Rx::compile(r#"window\["ytInitialData"\]\s*=\s*(\{.*?\});"#).unwrap();
        let s = t(r#"<script>window["ytInitialData"] = {"a":{"b":1}};var x;</script>"#);
        let m = rx.find(&s, 0, 0, s.len()).expect("find");
        assert_eq!(m[0], Some(8));
    }
    #[test]
    fn split_semantics() {
        use super::regex_split;
        // Dot-path navigation (NewPipe Parser.getValue).
        assert_eq!(
            regex_split("\\.", "a.b.c", 0),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // limit > 0 caps the array and keeps the tail in the last part.
        assert_eq!(
            regex_split("\\.", "a.b.c", 2),
            vec!["a".to_string(), "b.c".to_string()]
        );
        // limit = 0 drops trailing empty strings.
        assert_eq!(regex_split(",", "a,b,,", 0), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            regex_split(",", "a,b,,", -1),
            vec!["a".to_string(), "b".to_string(), "".to_string(), "".to_string()]
        );
        // Zero-width pattern still terminates (Java yields a leading
        // empty element for "" matching at position 0).
        assert_eq!(
            regex_split("", "ab", 0),
            vec!["".to_string(), "a".to_string(), "b".to_string()]
        );
    }
    #[test]
    fn basics() {
        let rx = Rx::compile("a.c*").unwrap();
        assert!(rx.matches(&t("axcc"), 0, 4).is_some());
        assert!(rx.matches(&t("axccd"), 0, 5).is_none());
        let rx = Rx::compile("(ab|cd)+e?").unwrap();
        assert!(rx.matches(&t("abcdab"), 0, 6).is_some());
        let rx = Rx::compile("\\d{2,4}").unwrap();
        assert!(rx.matches(&t("123"), 0, 3).is_some());
        assert!(rx.matches(&t("12345"), 0, 5).is_none());
    }
}

