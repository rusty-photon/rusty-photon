//! The lexer. Implements the grammar pins exactly (JSON-syntax unsigned
//! number literals, the fixed string-escape set, ASCII identifiers) and
//! gives every out-of-language construct a targeted diagnostic.

use super::{ExprError, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Null,
    True,
    False,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    BangEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Bang,
    Question,
    Colon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Comma,
    Eof,
}

impl Tok {
    pub(crate) fn describe(&self) -> String {
        let sym = match self {
            Self::Num(n) => return format!("number `{n}`"),
            Self::Str(_) => return "string literal".into(),
            Self::Ident(s) => return format!("identifier `{s}`"),
            Self::Null => return "`null`".into(),
            Self::True => return "`true`".into(),
            Self::False => return "`false`".into(),
            Self::Eof => return "end of expression".into(),
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Star => "*",
            Self::Slash => "/",
            Self::Percent => "%",
            Self::EqEq => "==",
            Self::BangEq => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::AndAnd => "&&",
            Self::OrOr => "||",
            Self::Bang => "!",
            Self::Question => "?",
            Self::Colon => ":",
            Self::LParen => "(",
            Self::RParen => ")",
            Self::LBracket => "[",
            Self::RBracket => "]",
            Self::Dot => ".",
            Self::Comma => ",",
        };
        format!("`{sym}`")
    }
}

#[derive(Clone, Debug)]
pub struct Token {
    pub(crate) tok: Tok,
    pub(crate) span: Span,
}

struct Lexer<'a> {
    src: &'a str,
    chars: Vec<(usize, char)>,
    i: usize,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).map(|&(_, c)| c)
    }

    fn peek2(&self) -> Option<char> {
        self.chars.get(self.i.saturating_add(1)).map(|&(_, c)| c)
    }

    /// Byte offset of the next character (or end of input).
    fn pos(&self) -> usize {
        self.chars.get(self.i).map_or(self.src.len(), |&(b, _)| b)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i = self.i.saturating_add(1);
        }
        c
    }

    /// Consume the current character and return `tok` for it.
    fn single(&mut self, tok: Tok) -> Tok {
        self.bump();
        tok
    }
}

/// Tokenize `src`. The returned vector always ends with a [`Tok::Eof`]
/// token whose span sits at the end of the input.
pub fn lex(src: &str) -> Result<Vec<Token>, ExprError> {
    let mut lx = Lexer {
        src,
        chars: src.char_indices().collect(),
        i: 0,
    };
    let mut toks = Vec::new();
    loop {
        while matches!(lx.peek(), Some(' ' | '\t' | '\r' | '\n')) {
            lx.bump();
        }
        let start = lx.pos();
        let Some(c) = lx.peek() else {
            toks.push(Token {
                tok: Tok::Eof,
                span: Span::new(start, start),
            });
            return Ok(toks);
        };
        let tok = match c {
            '0'..='9' => {
                lx.bump();
                lex_number(&mut lx, c, start)?
            }
            '\'' | '"' => {
                lx.bump();
                lex_string(&mut lx, c, start)?
            }
            c if c.is_ascii_alphabetic() || c == '_' => lex_ident(&mut lx, start)?,
            '+' | '-' | '*' => arith_op(&mut lx, c, start)?,
            '/' => slash_op(&mut lx, start)?,
            '=' | '!' => eq_op(&mut lx, c, start)?,
            '<' | '>' => cmp_op(&mut lx, c, start)?,
            '&' | '|' => logic_op(&mut lx, c, start)?,
            '?' => question_op(&mut lx, start)?,
            '.' => dot_op(&mut lx, start)?,
            '%' => lx.single(Tok::Percent),
            ':' => lx.single(Tok::Colon),
            '(' => lx.single(Tok::LParen),
            ')' => lx.single(Tok::RParen),
            '[' => lx.single(Tok::LBracket),
            ']' => lx.single(Tok::RBracket),
            ',' => lx.single(Tok::Comma),
            other => return Err(rejected_char(&lx, other, start)),
        };
        let end = lx.pos();
        toks.push(Token {
            tok,
            span: Span::new(start, end),
        });
    }
}

/// `+`, `-`, `*`: single-character arithmetic operators whose doubled
/// forms are rejected with a teaching message.
fn arith_op(lx: &mut Lexer, c: char, start: usize) -> Result<Tok, ExprError> {
    lx.bump();
    if lx.peek() == Some(c) {
        let msg = match c {
            '+' => "`++` is not allowed — expressions cannot mutate values",
            '-' => "`--` is not allowed — for double negation write `-(-x)`, with parentheses",
            _ => "`**` is not supported — there is no exponentiation operator",
        };
        return Err(err_at(lx, start, msg));
    }
    Ok(match c {
        '+' => Tok::Plus,
        '-' => Tok::Minus,
        _ => Tok::Star,
    })
}

/// `/` alone is division; `//` and `/*` open comments, which are rejected.
fn slash_op(lx: &mut Lexer, start: usize) -> Result<Tok, ExprError> {
    lx.bump();
    match lx.peek() {
        Some('/' | '*') => Err(err_at(
            lx,
            start,
            "comments are not allowed in workflow expressions",
        )),
        _ => Ok(Tok::Slash),
    }
}

/// `=` and `!`: `==` / `!=` are the equality operators; `===`, `!==`,
/// `=>` and bare `=` are rejected with pointers to the supported forms.
fn eq_op(lx: &mut Lexer, c: char, start: usize) -> Result<Tok, ExprError> {
    lx.bump();
    if lx.peek() == Some('=') {
        if lx.peek2() == Some('=') {
            let msg = if c == '=' {
                "`===` is not an operator here — use `==` (all equality is strict)"
            } else {
                "`!==` is not an operator here — use `!=` (all equality is strict)"
            };
            return Err(err_at(lx, start, msg));
        }
        lx.bump();
        return Ok(if c == '=' { Tok::EqEq } else { Tok::BangEq });
    }
    if c == '!' {
        return Ok(Tok::Bang);
    }
    if lx.peek() == Some('>') {
        return Err(err_at(
            lx,
            start,
            "arrow functions are not supported — expressions cannot define functions",
        ));
    }
    Err(err_at(
        lx,
        start,
        "`=` is not an operator — did you mean `==`? (expressions cannot assign; use a `set` instruction)",
    ))
}

/// `<` and `>`, bare or with `=`; the doubled shift forms are rejected.
fn cmp_op(lx: &mut Lexer, c: char, start: usize) -> Result<Tok, ExprError> {
    lx.bump();
    if lx.peek() == Some('=') {
        lx.bump();
        return Ok(if c == '<' { Tok::Le } else { Tok::Ge });
    }
    if lx.peek() == Some(c) {
        return Err(err_at(lx, start, "bitwise shifts are not supported"));
    }
    Ok(if c == '<' { Tok::Lt } else { Tok::Gt })
}

/// `&&` and `||`; the lone forms are rejected.
fn logic_op(lx: &mut Lexer, c: char, start: usize) -> Result<Tok, ExprError> {
    lx.bump();
    if lx.peek() == Some(c) {
        lx.bump();
        return Ok(if c == '&' { Tok::AndAnd } else { Tok::OrOr });
    }
    let msg = if c == '&' {
        "single `&` is not an operator — use `&&` for logical and"
    } else {
        "single `|` is not an operator — use `||` for logical or"
    };
    Err(err_at(lx, start, msg))
}

/// `?` opens a ternary; `??` and `?.` are rejected.
fn question_op(lx: &mut Lexer, start: usize) -> Result<Tok, ExprError> {
    lx.bump();
    match lx.peek() {
        Some('?') => Err(err_at(
            lx,
            start,
            "`??` is not supported — test missing values explicitly with has(...) or != null",
        )),
        Some('.') => Err(err_at(
            lx,
            start,
            "`?.` is not supported — guard with has(...) instead",
        )),
        _ => Ok(Tok::Question),
    }
}

/// `.` is member access; a leading-dot number literal is rejected.
fn dot_op(lx: &mut Lexer, start: usize) -> Result<Tok, ExprError> {
    lx.bump();
    if matches!(lx.peek(), Some('0'..='9')) {
        return Err(err_at(
            lx,
            start,
            "number literals need a leading digit — write 0.5, not .5",
        ));
    }
    Ok(Tok::Dot)
}

/// The rejection message for a character that starts no token.
fn rejected_char(lx: &Lexer, c: char, start: usize) -> ExprError {
    let msg = match c {
        '`' => {
            "template strings are not supported — use ' or \" quotes (and string interpolation is not available)"
                .to_string()
        }
        ';' => "`;` is not allowed — a workflow expression is a single expression".to_string(),
        '{' | '}' => {
            "object literals are not supported — objects only come from tool results".to_string()
        }
        '~' | '^' => format!("`{c}` (bitwise) is not supported"),
        other => format!(
            "unexpected character `{other}` — identifiers are ASCII letters, digits and `_`"
        ),
    };
    err_at(lx, start, msg)
}

fn err_at(lx: &Lexer, start: usize, msg: impl Into<String>) -> ExprError {
    // Span the whole character at `start`: a fixed `start + 1` would split
    // a multi-byte UTF-8 character, and such a span is not a valid slice
    // boundary in the source (`&src[start..end]` would panic downstream).
    let end = lx
        .src
        .get(start..)
        .and_then(|rest| rest.chars().next())
        .map_or(start, |c| start.saturating_add(c.len_utf8()));
    ExprError::parse(msg, Span::new(start, end))
}

fn eat_digits(lx: &mut Lexer, start: usize) -> Result<bool, ExprError> {
    let mut any = false;
    loop {
        match lx.peek() {
            Some('0'..='9') => {
                lx.bump();
                any = true;
            }
            Some('_') => {
                return Err(err_at(
                    lx,
                    start,
                    "digit separators (`_`) are not supported in number literals",
                ));
            }
            _ => return Ok(any),
        }
    }
}

/// Lex a number literal. The caller has already consumed `first` (a digit)
/// starting at byte `start`.
fn lex_number(lx: &mut Lexer, first: char, start: usize) -> Result<Tok, ExprError> {
    if first == '0' {
        match lx.peek() {
            Some('0'..='9') => {
                return Err(err_at(
                    lx,
                    start,
                    "number literals cannot have leading zeros",
                ));
            }
            Some('x' | 'X') => {
                return Err(err_at(
                    lx,
                    start,
                    "hexadecimal literals are not supported — use decimal",
                ));
            }
            Some('b' | 'B') => {
                return Err(err_at(
                    lx,
                    start,
                    "binary literals are not supported — use decimal",
                ));
            }
            Some('o' | 'O') => {
                return Err(err_at(
                    lx,
                    start,
                    "octal literals are not supported — use decimal",
                ));
            }
            _ => {}
        }
    }
    eat_digits(lx, start)?;
    if lx.peek() == Some('.') {
        // Only a fraction if digits follow; `result.items[0].name` never
        // gets here because that `.` follows an identifier or `]`, not a
        // digit run.
        lx.bump();
        if !eat_digits(lx, start)? {
            return Err(err_at(
                lx,
                start,
                "number literals need digits after the decimal point — write 5.0, not 5.",
            ));
        }
    }
    if matches!(lx.peek(), Some('e' | 'E')) {
        lx.bump();
        if matches!(lx.peek(), Some('+' | '-')) {
            lx.bump();
        }
        if !eat_digits(lx, start)? {
            return Err(err_at(lx, start, "exponent needs digits (e.g. 1e3)"));
        }
    }
    if let Some(c) = lx.peek() {
        if c.is_ascii_alphanumeric() || c == '_' {
            return Err(err_at(
                lx,
                lx.pos(),
                format!("unexpected character `{c}` after number literal"),
            ));
        }
    }
    let end = lx.pos();
    let text = lx.src.get(start..end).unwrap_or_default();
    let value: f64 = text
        .parse()
        .map_err(|_| err_at(lx, start, format!("invalid number literal `{text}`")))?;
    if !value.is_finite() {
        return Err(ExprError::parse(
            format!("number literal `{text}` overflows the f64 range"),
            Span::new(start, end),
        ));
    }
    Ok(Tok::Num(value))
}

/// Lex a string literal. The caller has already consumed the opening
/// `quote` starting at byte `start`.
fn lex_string(lx: &mut Lexer, quote: char, start: usize) -> Result<Tok, ExprError> {
    let mut out = String::new();
    loop {
        let at = lx.pos();
        match lx.bump() {
            None => {
                return Err(ExprError::parse(
                    format!("unterminated string literal (opened at offset {start})"),
                    Span::new(start, lx.src.len()),
                ));
            }
            Some('\n') => {
                return Err(ExprError::parse(
                    "strings cannot contain raw newlines — use \\n",
                    Span::new(at, at.saturating_add(1)),
                ));
            }
            Some(c) if c == quote => return Ok(Tok::Str(out)),
            Some('\\') => match lx.bump() {
                Some('\\') => out.push('\\'),
                Some('\'') => out.push('\''),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let mut code = 0u32;
                    for _ in 0..4 {
                        let h = lx.bump().and_then(|c| c.to_digit(16)).ok_or_else(|| {
                            ExprError::parse(
                                "invalid \\u escape — expected exactly 4 hex digits (\\u00e9)",
                                Span::new(at, lx.pos()),
                            )
                        })?;
                        // Four hex digits: `code` peaks at 0xFFFF, five
                        // orders below the `u32` edge; saturating is the
                        // total spelling.
                        code = code.saturating_mul(16).saturating_add(h);
                    }
                    let ch = char::from_u32(code).ok_or_else(|| {
                        ExprError::parse(
                            "invalid \\u escape — surrogate code points are not valid characters",
                            Span::new(at, lx.pos()),
                        )
                    })?;
                    out.push(ch);
                }
                Some(other) => {
                    return Err(ExprError::parse(
                        format!("unknown escape `\\{other}` (supported: \\\\ \\' \\\" \\n \\r \\t \\uXXXX)"),
                        Span::new(at, lx.pos()),
                    ));
                }
                None => {
                    return Err(ExprError::parse(
                        format!("unterminated string literal (opened at offset {start})"),
                        Span::new(start, lx.src.len()),
                    ));
                }
            },
            Some(c) => out.push(c),
        }
    }
}

/// Lex an identifier or keyword starting at byte `start`.
fn lex_ident(lx: &mut Lexer, start: usize) -> Result<Tok, ExprError> {
    let mut name = String::new();
    while let Some(c) = lx.peek() {
        if c.is_ascii_alphanumeric() || c == '_' {
            name.push(c);
            lx.bump();
        } else {
            break;
        }
    }
    // CEL-style prefixed string literals (b'…', r'…', rb'…').
    if matches!(lx.peek(), Some('\'' | '"'))
        && name.chars().all(|c| matches!(c, 'b' | 'r' | 'B' | 'R'))
    {
        return Err(err_at(
            lx,
            start,
            format!("`{name}'…'` byte/raw string literals are not supported — plain ' or \" strings only"),
        ));
    }
    Ok(match name.as_str() {
        "null" => Tok::Null,
        "true" => Tok::True,
        "false" => Tok::False,
        _ => Tok::Ident(name),
    })
}
