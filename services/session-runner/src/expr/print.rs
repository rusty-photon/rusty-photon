//! Precedence-aware pretty-printer (test-only). Renders an AST back to
//! source with minimal parentheses; the proptest round-trip property
//! asserts `parse(print(ast))` reproduces the same tree.

use super::ast::{BinOp, Expr, UnOp};

/// Precedence level for parenthesization decisions (loosest = 1).
fn prec(e: &Expr) -> u8 {
    match e {
        Expr::Cond { .. } => 1,
        Expr::Binary { op, .. } => match op {
            BinOp::Or => 2,
            BinOp::And => 3,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 4,
            BinOp::Add | BinOp::Sub => 5,
            BinOp::Mul | BinOp::Div | BinOp::Rem => 6,
        },
        Expr::Unary { .. } => 7,
        _ => 8,
    }
}

fn wrap_if(cond: bool, s: String) -> String {
    if cond {
        format!("({s})")
    } else {
        s
    }
}

/// A postfix operand (`x.f`, `x[i]`) must be primary-tight; a rendering
/// that starts with `-` (a folded negative literal) would rebind the
/// unary minus outside the postfix, so it gets parentheses too.
fn postfix_obj(obj: &Expr) -> String {
    let s = print(obj);
    wrap_if(prec(obj) < 8 || s.starts_with('-'), s)
}

pub fn print(e: &Expr) -> String {
    match e {
        Expr::Null(_) => "null".into(),
        Expr::Bool(b, _) => b.to_string(),
        Expr::Num(n, _) => format!("{n:?}"),
        Expr::Str(s, _) => quote(s),
        Expr::Ident(name, _) => name.clone(),
        Expr::Member { obj, field, .. } => format!("{}.{field}", postfix_obj(obj)),
        Expr::Index { obj, idx, .. } => format!("{}[{}]", postfix_obj(obj), print(idx)),
        Expr::Call { func, args, .. } => {
            let args = args.iter().map(print).collect::<Vec<_>>().join(", ");
            format!("{func}({args})")
        }
        Expr::Unary { op, rhs, .. } => {
            let inner = wrap_if(prec(rhs) < 7, print(rhs));
            match op {
                UnOp::Not => format!("!{inner}"),
                // `--x` is a lex error, so a `-`-leading operand needs parens.
                UnOp::Neg => {
                    if inner.starts_with('-') {
                        format!("-({inner})")
                    } else {
                        format!("-{inner}")
                    }
                }
            }
        }
        Expr::Binary { op, lhs, rhs, .. } => {
            let p = prec(e);
            // Left-associative: the right operand needs parens at equal
            // precedence. The comparison level is non-chaining, so a
            // comparison operand of a comparison needs parens on both sides.
            let l_wraps = if op.is_comparison() {
                prec(lhs) <= p
            } else {
                prec(lhs) < p
            };
            let r_wraps = prec(rhs) <= p;
            format!(
                "{} {} {}",
                wrap_if(l_wraps, print(lhs)),
                op.sym(),
                wrap_if(r_wraps, print(rhs))
            )
        }
        Expr::Cond {
            cond, then, els, ..
        } => {
            // `?:` is right-associative: only a conditional in condition
            // position re-groups without parens.
            let c = wrap_if(matches!(cond.as_ref(), Expr::Cond { .. }), print(cond));
            format!("{c} ? {} : {}", print(then), print(els))
        }
    }
}

/// Single-quoted string literal using exactly the pinned escape set.
fn quote(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}
