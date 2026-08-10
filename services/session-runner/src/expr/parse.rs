//! The Pratt parser. Binding powers implement the pinned precedence ladder
//! (postfix → unary → `* / %` → `+ -` → comparisons → `&&` → `||` → `?:`),
//! with the comparison level explicitly non-chaining.

use super::ast::{BinOp, Expr, UnOp};
use super::check;
use super::lex::{Tok, Token};
use super::{ExprError, Span};

/// Parse a token stream (as produced by [`super::lex::lex`], ending in
/// [`Tok::Eof`]) into an AST.
pub fn parse(toks: Vec<Token>) -> Result<Expr, ExprError> {
    let mut p = Parser { toks, pos: 0 };
    let expr = p.expr_bp(0, 0)?;
    let t = p.peek();
    if !matches!(t.tok, Tok::Eof) {
        return Err(ExprError::parse(
            format!("expected end of expression, found {}", t.tok.describe()),
            t.span,
        ));
    }
    Ok(expr)
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

/// Recursion guard: parsing recurses per nesting level (parentheses,
/// unary runs, argument lists, indexes, ternary branches), so unbounded
/// input depth would overflow the stack. No legitimate workflow
/// expression comes anywhere near this depth.
const MAX_DEPTH: u32 = 64;

const BP_TERNARY: (u8, u8) = (2, 1);
const BP_OR: (u8, u8) = (3, 4);
const BP_AND: (u8, u8) = (5, 6);
const BP_CMP: (u8, u8) = (7, 8);
const BP_ADD: (u8, u8) = (9, 10);
const BP_MUL: (u8, u8) = (11, 12);
const BP_UNARY: u8 = 13;
const BP_POSTFIX: u8 = 15;

const fn infix_op(tok: &Tok) -> Option<(BinOp, (u8, u8))> {
    Some(match tok {
        Tok::OrOr => (BinOp::Or, BP_OR),
        Tok::AndAnd => (BinOp::And, BP_AND),
        Tok::EqEq => (BinOp::Eq, BP_CMP),
        Tok::BangEq => (BinOp::Ne, BP_CMP),
        Tok::Lt => (BinOp::Lt, BP_CMP),
        Tok::Le => (BinOp::Le, BP_CMP),
        Tok::Gt => (BinOp::Gt, BP_CMP),
        Tok::Ge => (BinOp::Ge, BP_CMP),
        Tok::Plus => (BinOp::Add, BP_ADD),
        Tok::Minus => (BinOp::Sub, BP_ADD),
        Tok::Star => (BinOp::Mul, BP_MUL),
        Tok::Slash => (BinOp::Div, BP_MUL),
        Tok::Percent => (BinOp::Rem, BP_MUL),
        _ => return None,
    })
}

impl Parser {
    /// The current token. The stream always ends with `Eof`, which this
    /// returns forever once reached.
    fn peek(&self) -> Token {
        let idx = self.pos.min(self.toks.len().saturating_sub(1));
        self.toks.get(idx).cloned().unwrap_or(Token {
            tok: Tok::Eof,
            span: Span::new(0, 0),
        })
    }

    fn bump(&mut self) -> Token {
        let t = self.peek();
        if self.pos.saturating_add(1) < self.toks.len() {
            self.pos = self.pos.saturating_add(1);
        }
        t
    }

    fn expr_bp(&mut self, min_bp: u8, depth: u32) -> Result<Expr, ExprError> {
        if depth > MAX_DEPTH {
            return Err(ExprError::parse(
                format!("expression is nested too deeply (limit: {MAX_DEPTH} levels)"),
                self.peek().span,
            ));
        }
        let mut lhs = self.primary(depth)?;

        loop {
            let t = self.peek();
            match &t.tok {
                // ---- postfix: member access, indexing, calls ----
                Tok::Dot if BP_POSTFIX >= min_bp => {
                    self.bump();
                    let f = self.bump();
                    let field = match f.tok {
                        Tok::Ident(name) => name,
                        Tok::Null | Tok::True | Tok::False => {
                            return Err(ExprError::parse(
                                format!(
                                    "{} is a reserved word and cannot be a field name — use ['…'] indexing",
                                    f.tok.describe()
                                ),
                                f.span,
                            ));
                        }
                        other => {
                            return Err(ExprError::parse(
                                format!(
                                    "expected a field name after `.`, found {}",
                                    other.describe()
                                ),
                                f.span,
                            ));
                        }
                    };
                    let span = Span::new(lhs.span().start, f.span.end);
                    lhs = Expr::Member {
                        obj: Box::new(lhs),
                        field,
                        span,
                    };
                }
                Tok::LBracket if BP_POSTFIX >= min_bp => {
                    self.bump();
                    let idx = self.expr_bp(0, depth.saturating_add(1))?;
                    let close = self.bump();
                    if !matches!(close.tok, Tok::RBracket) {
                        return Err(ExprError::parse(
                            format!(
                                "expected `]` to close the index opened at offset {}, found {}",
                                t.span.start,
                                close.tok.describe()
                            ),
                            close.span,
                        ));
                    }
                    let span = Span::new(lhs.span().start, close.span.end);
                    lhs = Expr::Index {
                        obj: Box::new(lhs),
                        idx: Box::new(idx),
                        span,
                    };
                }
                Tok::LParen if BP_POSTFIX >= min_bp => match &lhs {
                    Expr::Ident(name, name_span) => {
                        let func = name.clone();
                        let func_span = *name_span;
                        self.bump();
                        let (args, end) = self.call_args(t.span.start, depth.saturating_add(1))?;
                        lhs = Expr::Call {
                            func,
                            func_span,
                            args,
                            span: Span::new(func_span.start, end),
                        };
                    }
                    Expr::Member { .. } => {
                        return Err(ExprError::parse(
                            format!(
                                "method calls are not supported — only the built-in functions can be called ({})",
                                check::function_names()
                            ),
                            t.span,
                        ));
                    }
                    _ => {
                        return Err(ExprError::parse(
                            "this cannot be called — only the built-in functions can be called",
                            t.span,
                        ));
                    }
                },

                // ---- ternary ----
                Tok::Question if BP_TERNARY.0 >= min_bp => {
                    self.bump();
                    let then = self.expr_bp(0, depth.saturating_add(1))?;
                    let colon = self.bump();
                    if !matches!(colon.tok, Tok::Colon) {
                        return Err(ExprError::parse(
                            format!(
                                "`?` needs a matching `:` (condition ? then : else) — found {}",
                                colon.tok.describe()
                            ),
                            colon.span,
                        ));
                    }
                    let els = self.expr_bp(BP_TERNARY.1, depth.saturating_add(1))?;
                    let span = Span::new(lhs.span().start, els.span().end);
                    lhs = Expr::Cond {
                        cond: Box::new(lhs),
                        then: Box::new(then),
                        els: Box::new(els),
                        span,
                    };
                }

                // ---- binary operators ----
                other => {
                    let Some((op, (l_bp, r_bp))) = infix_op(other) else {
                        break;
                    };
                    if l_bp < min_bp {
                        break;
                    }
                    self.bump();
                    let rhs = self.expr_bp(r_bp, depth.saturating_add(1))?;
                    let span = Span::new(lhs.span().start, rhs.span().end);
                    lhs = Expr::Binary {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                        span,
                    };
                    // Comparison operators do not chain: the grouping of
                    // `a == b < c` differs between CEL and JavaScript, so
                    // the format refuses the ambiguity outright.
                    if op.is_comparison() {
                        if let Some((next_op, _)) = infix_op(&self.peek().tok) {
                            if next_op.is_comparison() {
                                return Err(ExprError::parse(
                                    format!(
                                        "comparison operators cannot be chained — write (a {} b) && (b {} c), or parenthesize explicitly",
                                        op.sym(),
                                        next_op.sym()
                                    ),
                                    self.peek().span,
                                ));
                            }
                        }
                    }
                }
            }
        }
        Ok(lhs)
    }

    fn call_args(&mut self, open_at: usize, depth: u32) -> Result<(Vec<Expr>, usize), ExprError> {
        let mut args = Vec::new();
        if matches!(self.peek().tok, Tok::RParen) {
            let close = self.bump();
            return Ok((args, close.span.end));
        }
        loop {
            args.push(self.expr_bp(0, depth)?);
            let t = self.bump();
            match t.tok {
                Tok::Comma => {
                    if matches!(self.peek().tok, Tok::RParen) {
                        return Err(ExprError::parse("trailing comma in argument list", t.span));
                    }
                }
                Tok::RParen => return Ok((args, t.span.end)),
                other => {
                    return Err(ExprError::parse(
                        format!(
                            "expected `,` or `)` in the argument list opened at offset {open_at}, found {}",
                            other.describe()
                        ),
                        t.span,
                    ));
                }
            }
        }
    }

    fn primary(&mut self, depth: u32) -> Result<Expr, ExprError> {
        let t = self.bump();
        Ok(match t.tok {
            Tok::Num(n) => Expr::Num(n, t.span),
            Tok::Str(s) => Expr::Str(s, t.span),
            Tok::Null => Expr::Null(t.span),
            Tok::True => Expr::Bool(true, t.span),
            Tok::False => Expr::Bool(false, t.span),
            Tok::Ident(name) => Expr::Ident(name, t.span),
            Tok::LParen => {
                let inner = self.expr_bp(0, depth.saturating_add(1))?;
                let close = self.bump();
                if !matches!(close.tok, Tok::RParen) {
                    return Err(ExprError::parse(
                        format!(
                            "expected `)` to close the group opened at offset {}, found {}",
                            t.span.start,
                            close.tok.describe()
                        ),
                        close.span,
                    ));
                }
                inner
            }
            Tok::Minus => {
                let rhs = self.expr_bp(BP_UNARY, depth.saturating_add(1))?;
                let span = Span::new(t.span.start, rhs.span().end);
                Expr::unary(UnOp::Neg, rhs, span)
            }
            Tok::Bang => {
                let rhs = self.expr_bp(BP_UNARY, depth.saturating_add(1))?;
                let span = Span::new(t.span.start, rhs.span().end);
                Expr::unary(UnOp::Not, rhs, span)
            }
            Tok::Plus => {
                return Err(ExprError::parse(
                    "unary `+` is not supported — remove it",
                    t.span,
                ));
            }
            Tok::LBracket => {
                return Err(ExprError::parse(
                    "array literals are not supported — arrays only come from tool results",
                    t.span,
                ));
            }
            other => {
                return Err(ExprError::parse(
                    format!("expected an expression, found {}", other.describe()),
                    t.span,
                ));
            }
        })
    }
}
