//! The workflow expression language: parsing, validation, and evaluation.
//!
//! Expressions are strings in a small, pure, CEL-style language. The
//! normative contract — types, operators, functions, namespaces, grammar
//! pins, and evaluation pins — is `docs/services/session-runner.md`
//! § Expressions; this module implements it exactly. Anything the parser
//! accepts becomes de-facto format, so the grammar is enforced by
//! construction: a hand-rolled lexer + Pratt parser (chosen by the Phase B
//! spike, see `docs/plans/archive/workflow-dsl.md`) with a targeted diagnostic for
//! every out-of-language construct.
//!
//! Entry points:
//!
//! - [`Expression::parse`] lexes, parses, and statically checks a source
//!   string (namespace roots, known functions and arities, `has()` path
//!   arguments). All load-time errors have [`ErrorKind::Parse`].
//! - [`Expression::eval`] evaluates against an [`EvalContext`] (the five
//!   namespaces plus the engine clock) with strict, coercion-free
//!   semantics. Runtime errors have [`ErrorKind::Eval`].
//!
//! Every error carries a byte [`Span`] into the source string so callers
//! can map it to a JSON-Pointer location in the workflow document.

mod ast;
mod check;
mod eval;
mod lex;
pub(crate) mod num;
mod parse;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod print;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod conformance_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod eval_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod prop_tests;

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;
use serde_json::Value;

pub use eval::EvalContext;

/// A byte range into the expression source string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// Which phase produced the error: document load time or evaluation time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Lexing, parsing, or static checking failed — the expression is not
    /// in the language. Surfaced by `/validate` and at document load.
    Parse,
    /// The expression is well-formed but evaluation raised (type error,
    /// null in arithmetic, division by zero, overflow, …). Surfaced as a
    /// workflow error at the raising instruction.
    Eval,
}

/// An expression error: a message plus the byte span it points at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{message} (at {}..{})", .span.start, .span.end)]
pub struct ExprError {
    pub kind: ErrorKind,
    pub message: String,
    pub span: Span,
}

impl ExprError {
    pub(crate) fn parse(message: impl Into<String>, span: Span) -> Self {
        Self {
            kind: ErrorKind::Parse,
            message: message.into(),
            span,
        }
    }

    pub(crate) fn eval(message: impl Into<String>, span: Span) -> Self {
        Self {
            kind: ErrorKind::Eval,
            message: message.into(),
            span,
        }
    }
}

/// A parsed, statically-checked workflow expression.
#[derive(Clone, Debug, PartialEq)]
pub struct Expression {
    src: String,
    ast: ast::Expr,
}

impl Expression {
    /// Lex, parse, and statically check `src`.
    ///
    /// # Errors
    ///
    /// Returns an [`ExprError`] from lexing, parsing, or the static
    /// checks: namespace roots, known functions and arities, `has()`
    /// takes a namespace path.
    pub fn parse(src: &str) -> Result<Self, ExprError> {
        let toks = lex::lex(src)?;
        let ast = parse::parse(toks)?;
        check::check(&ast)?;
        Ok(Self {
            src: src.to_owned(),
            ast,
        })
    }

    /// The original source string.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.src
    }

    /// Canonical s-expression form of the parse tree. Grouping is
    /// structural (spans are ignored); intended for diagnostics and tests.
    #[must_use]
    pub fn canon(&self) -> String {
        self.ast.canon()
    }

    /// The namespace roots this expression reads (a subset of `params`,
    /// `session`, `result`, `event`, `error`), each with the span of its
    /// first occurrence. After static checking, every bare identifier in
    /// the tree is a namespace root, so this is exact. Document
    /// validation uses it for scope checks (e.g. `event.*` is only in
    /// scope inside a trigger), pointing at the offending root.
    #[must_use]
    pub fn namespaces(&self) -> BTreeMap<&str, Span> {
        let mut roots = BTreeMap::new();
        self.ast.collect_idents(&mut roots);
        roots
    }

    /// Evaluate against the given context.
    ///
    /// # Errors
    ///
    /// Returns an [`ExprError`] per the strict semantics: no type
    /// coercion, `null` raises in arithmetic / ordered comparisons /
    /// logic, division and remainder by zero raise, and any arithmetic
    /// result outside the finite f64 range raises at the producing
    /// operation.
    pub fn eval(&self, ctx: &EvalContext<'_>) -> Result<Value, ExprError> {
        eval::eval(&self.ast, ctx)
    }
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.src)
    }
}
