//! The evaluator. Strict, coercion-free semantics over JSON values, per
//! the evaluation pins in `docs/services/session-runner.md` § Expressions:
//!
//! - Path traversal is **total**: member/index access through `null`, a
//!   missing key, an out-of-range or non-integer index, or a value of the
//!   wrong shape yields `null` — it never raises. `has(path)` is true iff
//!   the path resolves to a non-null value. The loudness comes when the
//!   `null` reaches an operator or function.
//! - Arithmetic (`+ - * / %`, unary `-`) and ordered comparisons
//!   (`< <= > >=`) take numbers only. Division and remainder by zero
//!   raise. Any arithmetic result outside the finite f64 range raises at
//!   the producing operation, so every number in the system is finite.
//! - `&&` / `||` / `!` and the `?:` condition take booleans only — there
//!   is no truthiness. `&&` / `||` short-circuit left to right; `?:`
//!   evaluates only the taken branch (this is what makes
//!   `has(x) && session.x > 0` a sound guard).
//! - `==` / `!=` accept any two values: deep structural equality, numbers
//!   by numeric value (a JSON integer `5` equals the literal `5`),
//!   cross-type comparison is `false`, never an error.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::ast::{BinOp, Expr, UnOp};
use super::num::ExactInt;
use super::{ExprError, Span};

/// Everything an evaluation reads: the five namespaces and the engine
/// clock (`seconds_until()` is measured against `now` — the one
/// sanctioned impurity, injected so evaluation stays deterministic).
///
/// An absent namespace resolves to `null`, so e.g. `error.message`
/// outside a `catch` scope is `null`, not an error.
#[derive(Clone, Copy, Debug)]
pub struct EvalContext<'a> {
    pub params: Option<&'a Value>,
    pub session: Option<&'a Value>,
    pub result: Option<&'a Value>,
    pub event: Option<&'a Value>,
    pub error: Option<&'a Value>,
    pub now: DateTime<Utc>,
}

impl<'a> EvalContext<'a> {
    /// A context with all namespaces absent.
    #[must_use]
    pub const fn new(now: DateTime<Utc>) -> Self {
        Self {
            params: None,
            session: None,
            result: None,
            event: None,
            error: None,
            now,
        }
    }

    fn namespace(&self, name: &str) -> Option<&'a Value> {
        match name {
            "params" => self.params,
            "session" => self.session,
            "result" => self.result,
            "event" => self.event,
            "error" => self.error,
            _ => None,
        }
    }
}

pub fn eval(expr: &Expr, ctx: &EvalContext<'_>) -> Result<Value, ExprError> {
    match expr {
        Expr::Null(_) => Ok(Value::Null),
        Expr::Bool(b, _) => Ok(Value::Bool(*b)),
        Expr::Num(n, span) => num_value(*n, *span),
        Expr::Str(s, _) => Ok(Value::String(s.clone())),
        Expr::Ident(name, _) => Ok(ctx.namespace(name).cloned().unwrap_or(Value::Null)),
        Expr::Member { obj, field, .. } => {
            let o = eval(obj, ctx)?;
            Ok(match o {
                Value::Object(mut m) => m.remove(field.as_str()).unwrap_or(Value::Null),
                _ => Value::Null,
            })
        }
        Expr::Index { obj, idx, .. } => {
            let o = eval(obj, ctx)?;
            let i = eval(idx, ctx)?;
            Ok(index_value(o, &i))
        }
        Expr::Unary { op, rhs, span } => match op {
            UnOp::Neg => {
                let n = want_num(eval(rhs, ctx)?, rhs.span(), "unary `-`")?;
                num_value(-n, *span)
            }
            UnOp::Not => {
                let b = want_bool(eval(rhs, ctx)?, rhs.span(), "`!`")?;
                Ok(Value::Bool(!b))
            }
        },
        Expr::Binary { op, lhs, rhs, span } => eval_binary(*op, lhs, rhs, *span, ctx),
        Expr::Cond {
            cond, then, els, ..
        } => {
            let c = want_bool(eval(cond, ctx)?, cond.span(), "the `?:` condition")?;
            if c {
                eval(then, ctx)
            } else {
                eval(els, ctx)
            }
        }
        Expr::Call {
            func,
            args,
            span,
            func_span,
        } => eval_call(func, args, *span, *func_span, ctx),
    }
}

fn eval_binary(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    span: Span,
    ctx: &EvalContext<'_>,
) -> Result<Value, ExprError> {
    match op {
        // Short-circuiting boolean operators.
        BinOp::And => {
            let l = want_bool(eval(lhs, ctx)?, lhs.span(), "`&&`")?;
            if !l {
                return Ok(Value::Bool(false));
            }
            let r = want_bool(eval(rhs, ctx)?, rhs.span(), "`&&`")?;
            Ok(Value::Bool(r))
        }
        BinOp::Or => {
            let l = want_bool(eval(lhs, ctx)?, lhs.span(), "`||`")?;
            if l {
                return Ok(Value::Bool(true));
            }
            let r = want_bool(eval(rhs, ctx)?, rhs.span(), "`||`")?;
            Ok(Value::Bool(r))
        }
        // Equality: any two values, deep, never an error.
        BinOp::Eq => {
            let l = eval(lhs, ctx)?;
            let r = eval(rhs, ctx)?;
            Ok(Value::Bool(value_eq(&l, &r)))
        }
        BinOp::Ne => {
            let l = eval(lhs, ctx)?;
            let r = eval(rhs, ctx)?;
            Ok(Value::Bool(!value_eq(&l, &r)))
        }
        // Ordered comparisons: numbers only.
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let l = want_cmp_num(eval(lhs, ctx)?, lhs.span(), op)?;
            let r = want_cmp_num(eval(rhs, ctx)?, rhs.span(), op)?;
            let b = match op {
                BinOp::Lt => l < r,
                BinOp::Le => l <= r,
                BinOp::Gt => l > r,
                _ => l >= r,
            };
            Ok(Value::Bool(b))
        }
        // Arithmetic: numbers only; zero divisors and non-finite results
        // raise at this operation.
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
            let what = format!("`{}`", op.sym());
            let l = want_num(eval(lhs, ctx)?, lhs.span(), &what)?;
            let r = want_num(eval(rhs, ctx)?, rhs.span(), &what)?;
            let out = match op {
                BinOp::Add => l + r,
                BinOp::Sub => l - r,
                BinOp::Mul => l * r,
                BinOp::Div => {
                    if r == 0.0 {
                        return Err(ExprError::eval("division by zero", span));
                    }
                    l / r
                }
                _ => {
                    if r == 0.0 {
                        return Err(ExprError::eval("remainder (`%`) by zero", span));
                    }
                    l % r
                }
            };
            num_value(out, span)
        }
    }
}

fn eval_call(
    func: &str,
    args: &[Expr],
    span: Span,
    func_span: Span,
    ctx: &EvalContext<'_>,
) -> Result<Value, ExprError> {
    match (func, args) {
        ("abs", [x]) => num_value(want_arg_num(x, ctx, "abs")?.abs(), span),
        ("floor", [x]) => num_value(want_arg_num(x, ctx, "floor")?.floor(), span),
        ("ceil", [x]) => num_value(want_arg_num(x, ctx, "ceil")?.ceil(), span),
        // Rounds half away from zero: round(2.5) == 3, round(-2.5) == -3.
        ("round", [x]) => num_value(want_arg_num(x, ctx, "round")?.round(), span),
        ("min", args) if args.len() >= 2 => {
            let mut out = f64::INFINITY;
            for a in args {
                out = out.min(want_arg_num(a, ctx, "min")?);
            }
            num_value(out, span)
        }
        ("max", args) if args.len() >= 2 => {
            let mut out = f64::NEG_INFINITY;
            for a in args {
                out = out.max(want_arg_num(a, ctx, "max")?);
            }
            num_value(out, span)
        }
        ("clamp", [x, lo, hi]) => {
            let x = want_arg_num(x, ctx, "clamp")?;
            let lo = want_arg_num(lo, ctx, "clamp")?;
            let hi = want_arg_num(hi, ctx, "clamp")?;
            if lo > hi {
                return Err(ExprError::eval(
                    format!("clamp() needs lo <= hi, got lo={lo} > hi={hi}"),
                    span,
                ));
            }
            num_value(x.clamp(lo, hi), span)
        }
        ("seconds", [s]) => {
            let s = want_arg_str(s, ctx, "seconds", "a humantime string like '1m30s'")?;
            let d = humantime::parse_duration(&s).map_err(|e| {
                ExprError::eval(
                    format!("seconds() could not parse `{s}` as a duration: {e}"),
                    span,
                )
            })?;
            num_value(d.as_secs_f64(), span)
        }
        ("humantime", [n]) => {
            let n = want_arg_num(n, ctx, "humantime")?;
            let d = std::time::Duration::try_from_secs_f64(n).map_err(|_| {
                ExprError::eval(
                    format!("humantime() needs a non-negative number of seconds in range, got {n}"),
                    span,
                )
            })?;
            Ok(Value::String(humantime::format_duration(d).to_string()))
        }
        ("has", [path]) => {
            // Traversal is total, so evaluating the path cannot raise on a
            // miss; `has` is simply "resolves to non-null".
            let v = eval(path, ctx)?;
            Ok(Value::Bool(!matches!(v, Value::Null)))
        }
        ("seconds_until", [s]) => {
            let s = want_arg_str(s, ctx, "seconds_until", "an RFC 3339 timestamp string")?;
            let t = DateTime::parse_from_rfc3339(&s).map_err(|e| {
                ExprError::eval(
                    format!("seconds_until() could not parse `{s}` as an RFC 3339 timestamp: {e}"),
                    span,
                )
            })?;
            let delta = t.with_timezone(&Utc).signed_duration_since(ctx.now);
            num_value(delta.as_seconds_f64(), span)
        }
        // Unreachable after static checking (unknown names and arities are
        // rejected at parse time); kept as an error so `eval` is total.
        _ => Err(ExprError::eval(
            format!(
                "unknown function or arity: `{func}` with {} argument(s)",
                args.len()
            ),
            func_span,
        )),
    }
}

/// Index into a value. Total: any miss or shape mismatch is `null`.
fn index_value(obj: Value, idx: &Value) -> Value {
    match (obj, idx) {
        (Value::Object(mut m), Value::String(k)) => m.remove(k.as_str()).unwrap_or(Value::Null),
        (Value::Array(mut a), Value::Number(n)) => {
            let exact = n
                .as_f64()
                .and_then(ExactInt::try_from_f64)
                .and_then(ExactInt::to_usize);
            let Some(i) = exact else {
                return Value::Null;
            };
            if i < a.len() {
                a.swap_remove(i)
            } else {
                Value::Null
            }
        }
        _ => Value::Null,
    }
}

/// Deep structural equality. Numbers compare by numeric value regardless
/// of their JSON representation (integer vs float); cross-type comparison
/// is `false`.
fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(p), Some(q)) => p == q,
            _ => false,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(v, w)| value_eq(v, w))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| value_eq(v, w)))
        }
        _ => a == b,
    }
}

const fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Wrap a finite f64 as a JSON number; a non-finite value is an
/// arithmetic overflow at `span`.
fn num_value(n: f64, span: Span) -> Result<Value, ExprError> {
    serde_json::Number::from_f64(n)
        .map(Value::Number)
        .ok_or_else(|| {
            ExprError::eval(
                "arithmetic overflow — the result is outside the finite f64 range",
                span,
            )
        })
}

fn want_num(v: Value, span: Span, what: &str) -> Result<f64, ExprError> {
    match v {
        Value::Number(n) => n.as_f64().ok_or_else(|| {
            ExprError::eval(format!("{what} needs a number in the f64 range"), span)
        }),
        Value::Null => Err(ExprError::eval(
            format!("{what} needs a number, got null — guard with has(...) or != null"),
            span,
        )),
        other => Err(ExprError::eval(
            format!("{what} needs a number, got {}", type_name(&other)),
            span,
        )),
    }
}

fn want_cmp_num(v: Value, span: Span, op: BinOp) -> Result<f64, ExprError> {
    if let Value::String(_) = v {
        return Err(ExprError::eval(
            format!(
                "`{}` compares numbers only — string ordering is not defined (use == or != for strings)",
                op.sym()
            ),
            span,
        ));
    }
    want_num(v, span, &format!("`{}`", op.sym()))
}

fn want_bool(v: Value, span: Span, what: &str) -> Result<bool, ExprError> {
    match v {
        Value::Bool(b) => Ok(b),
        Value::Null => Err(ExprError::eval(
            format!("{what} needs a boolean, got null — guard with has(...) or != null"),
            span,
        )),
        other => Err(ExprError::eval(
            format!(
                "{what} needs a boolean, got {} — there is no truthiness; compare explicitly",
                type_name(&other)
            ),
            span,
        )),
    }
}

fn want_arg_num(arg: &Expr, ctx: &EvalContext<'_>, func: &str) -> Result<f64, ExprError> {
    want_num(eval(arg, ctx)?, arg.span(), &format!("{func}()"))
}

fn want_arg_str(
    arg: &Expr,
    ctx: &EvalContext<'_>,
    func: &str,
    expected: &str,
) -> Result<String, ExprError> {
    match eval(arg, ctx)? {
        Value::String(s) => Ok(s),
        other => Err(ExprError::eval(
            format!("{func}() needs {expected}, got {}", type_name(&other)),
            arg.span(),
        )),
    }
}
