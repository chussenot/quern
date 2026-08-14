//! bead: treadle-value — FROZEN: Value, Type, Display. docs/treadle.md §3
//!
//! This module is the single place either engine is allowed to do arithmetic,
//! comparison or truth-testing on a `Value`. The VM and the tree-walker call
//! the same helpers, which is the only reason they cannot drift on overflow,
//! divide-by-zero or type-mismatch wording — §4 requires the same variant, the
//! same line and the same message from both.
//!
//! **Memory**: `Str` is reference-counted (`Rc<String>`), so cloning a `Value`
//! never copies string bytes; every other variant is a plain copy. There is no
//! garbage collector and none is needed — the language has no cycles.

use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

use crate::error::{Result, TreadleError};

/// A treadle runtime value. No floats, so `Eq` is sound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Nil,
    Int(i64),
    Bool(bool),
    Str(Rc<String>),
}

/// The type of a `Value`. Used for error messages and for the same-type rule
/// that `==` / `!=` enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Nil,
    Int,
    Bool,
    Str,
}

impl Type {
    /// The name used in every error message that names a type.
    pub fn name(self) -> &'static str {
        match self {
            Type::Nil => "Nil",
            Type::Int => "Int",
            Type::Bool => "Bool",
            Type::Str => "Str",
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// THE display form (§3): used by `print`, by the `str()` builtin, and by every
/// error message that names a value. `Str` renders its bytes **unquoted**.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => f.write_str("nil"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Bool(true) => f.write_str("true"),
            Value::Bool(false) => f.write_str("false"),
            Value::Str(s) => f.write_str(s),
        }
    }
}

impl Value {
    /// Convenience constructor — `Rc::new` at every call site is noise.
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(Rc::new(s.into()))
    }

    pub fn type_of(&self) -> Type {
        match self {
            Value::Nil => Type::Nil,
            Value::Int(_) => Type::Int,
            Value::Bool(_) => Type::Bool,
            Value::Str(_) => Type::Str,
        }
    }

    /// The name used in error messages: `Nil`, `Int`, `Bool`, `Str`.
    pub fn type_name(&self) -> &'static str {
        self.type_of().name()
    }

    // ---- arithmetic ------------------------------------------------------
    //
    // §4: integer overflow on `+ - *` is a `Value` error, never a wrap and
    // never a panic — hence `checked_*` throughout. Divide and modulo by zero
    // likewise.

    /// `+`: Int addition, or Str concatenation (§2). No implicit conversion.
    pub fn add(&self, rhs: &Value, line: u32) -> Result<Value> {
        match (self, rhs) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_add(*b)
                .map(Value::Int)
                .ok_or_else(|| overflow(line)),
            (Value::Str(a), Value::Str(b)) => Ok(Value::str(format!("{a}{b}"))),
            _ => Err(binary_type_error("+", "Int or Str", self, rhs, line)),
        }
    }

    pub fn sub(&self, rhs: &Value, line: u32) -> Result<Value> {
        let (a, b) = self.int_pair("-", rhs, line)?;
        a.checked_sub(b)
            .map(Value::Int)
            .ok_or_else(|| overflow(line))
    }

    pub fn mul(&self, rhs: &Value, line: u32) -> Result<Value> {
        let (a, b) = self.int_pair("*", rhs, line)?;
        a.checked_mul(b)
            .map(Value::Int)
            .ok_or_else(|| overflow(line))
    }

    /// `/`: truncates toward zero. We rely on Rust's own `i64` division
    /// semantics rather than reimplementing them, so `-7 / 2 == -3` (§2).
    /// `checked_div` also covers `i64::MIN / -1`, which would otherwise panic.
    pub fn div(&self, rhs: &Value, line: u32) -> Result<Value> {
        let (a, b) = self.int_pair("/", rhs, line)?;
        if b == 0 {
            return Err(value_error("divide by zero", line));
        }
        a.checked_div(b)
            .map(Value::Int)
            .ok_or_else(|| overflow(line))
    }

    /// `%`: same truncating-toward-zero semantics as `/`, so the remainder
    /// takes the sign of the dividend and `-7 % 2 == -1` (§2). Rust's, not ours.
    pub fn rem(&self, rhs: &Value, line: u32) -> Result<Value> {
        let (a, b) = self.int_pair("%", rhs, line)?;
        if b == 0 {
            return Err(value_error("modulo by zero", line));
        }
        a.checked_rem(b)
            .map(Value::Int)
            .ok_or_else(|| overflow(line))
    }

    /// Unary `-`. Checked: `-i64::MIN` would panic in a debug build, and §4
    /// forbids a panic on any input.
    pub fn neg(&self, line: u32) -> Result<Value> {
        match self {
            Value::Int(a) => a
                .checked_neg()
                .map(Value::Int)
                .ok_or_else(|| overflow(line)),
            _ => Err(unary_type_error("-", "Int", self, line)),
        }
    }

    /// Unary `!`. Bool only — there is no truthiness in this language.
    pub fn not(&self, line: u32) -> Result<Value> {
        Ok(Value::Bool(!self.as_bool(line)?))
    }

    // ---- comparison and truth -------------------------------------------

    /// The `==` / `!=` rule, pinned here rather than in two engines: `==`
    /// compares two values of the **same** type (§2) and is otherwise a `Type`
    /// error. Note this is deliberately *not* the derived `PartialEq`, which
    /// exists for Rust-side use (comparing `Output`s in the fuzzer) and answers
    /// `false` for a cross-type comparison instead of erroring.
    pub fn eq_value(&self, rhs: &Value, line: u32) -> Result<bool> {
        if self.type_of() != rhs.type_of() {
            return Err(TreadleError::Type {
                line,
                msg: format!(
                    "== expects two values of the same type, got {} and {}",
                    self.type_name(),
                    rhs.type_name()
                ),
            });
        }
        Ok(self == rhs)
    }

    /// The ordering behind `<` `>` `<=` `>=`: Int with Int, or Str with Str
    /// (bytewise), and a `Type` error otherwise (§2).
    pub fn cmp_value(&self, rhs: &Value, op: &str, line: u32) -> Result<Ordering> {
        match (self, rhs) {
            (Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
            (Value::Str(a), Value::Str(b)) => Ok(a.as_bytes().cmp(b.as_bytes())),
            _ => Err(binary_type_error(op, "two Int or two Str", self, rhs, line)),
        }
    }

    /// Truth for `if`, `while`, `and`, `or` and `!`. Bool only — §2 forbids
    /// implicit conversion, so a non-Bool condition is a `Type` error rather
    /// than something truthy.
    pub fn as_bool(&self, line: u32) -> Result<bool> {
        match self {
            Value::Bool(b) => Ok(*b),
            _ => Err(TreadleError::Type {
                line,
                msg: format!("expected Bool, got {}", self.type_name()),
            }),
        }
    }

    fn int_pair(&self, op: &str, rhs: &Value, line: u32) -> Result<(i64, i64)> {
        match (self, rhs) {
            (Value::Int(a), Value::Int(b)) => Ok((*a, *b)),
            _ => Err(binary_type_error(op, "Int", self, rhs, line)),
        }
    }
}

fn overflow(line: u32) -> TreadleError {
    value_error("integer overflow", line)
}

fn value_error(msg: &str, line: u32) -> TreadleError {
    TreadleError::Value {
        line,
        msg: msg.to_string(),
    }
}

fn binary_type_error(op: &str, want: &str, lhs: &Value, rhs: &Value, line: u32) -> TreadleError {
    TreadleError::Type {
        line,
        msg: format!(
            "{op} expects {want} operands, got {} and {}",
            lhs.type_name(),
            rhs.type_name()
        ),
    }
}

fn unary_type_error(op: &str, want: &str, v: &Value, line: u32) -> TreadleError {
    TreadleError::Type {
        line,
        msg: format!("{op} expects a {want} operand, got {}", v.type_name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: u32 = 7;

    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    #[test]
    fn display_is_the_one_form() {
        assert_eq!(Value::Nil.to_string(), "nil");
        assert_eq!(int(-42).to_string(), "-42");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Bool(false).to_string(), "false");
        // unquoted, and quotes inside survive verbatim
        assert_eq!(Value::str("a\"b").to_string(), "a\"b");
        assert_eq!(Value::str("").to_string(), "");
    }

    #[test]
    fn type_of_and_names() {
        assert_eq!(Value::Nil.type_of(), Type::Nil);
        assert_eq!(int(1).type_name(), "Int");
        assert_eq!(Value::Bool(false).type_name(), "Bool");
        assert_eq!(Value::str("x").type_name(), "Str");
        assert_eq!(Type::Nil.to_string(), "Nil");
    }

    #[test]
    fn arithmetic_ok() {
        assert_eq!(int(2).add(&int(3), L).unwrap(), int(5));
        assert_eq!(int(2).sub(&int(3), L).unwrap(), int(-1));
        assert_eq!(int(6).mul(&int(7), L).unwrap(), int(42));
        assert_eq!(
            Value::str("ab").add(&Value::str("cd"), L).unwrap(),
            Value::str("abcd")
        );
        assert_eq!(int(5).neg(L).unwrap(), int(-5));
    }

    #[test]
    fn division_truncates_toward_zero() {
        // Rust's own semantics, per §2: -7/2 == -3 and -7%2 == -1.
        assert_eq!(int(-7).div(&int(2), L).unwrap(), int(-3));
        assert_eq!(int(-7).rem(&int(2), L).unwrap(), int(-1));
        assert_eq!(int(7).div(&int(-2), L).unwrap(), int(-3));
        assert_eq!(int(7).rem(&int(-2), L).unwrap(), int(1));
    }

    #[test]
    fn overflow_is_a_value_error_not_a_wrap() {
        for got in [
            int(i64::MAX).add(&int(1), L),
            int(i64::MIN).sub(&int(1), L),
            int(i64::MAX).mul(&int(2), L),
            int(i64::MIN).neg(L),
            int(i64::MIN).div(&int(-1), L),
        ] {
            match got {
                Err(TreadleError::Value { line, ref msg }) => {
                    assert_eq!(line, L);
                    assert_eq!(msg, "integer overflow");
                }
                other => panic!("expected Value overflow error, got {other:?}"),
            }
        }
    }

    #[test]
    fn zero_divisor_is_a_value_error() {
        assert!(matches!(
            int(1).div(&int(0), L),
            Err(TreadleError::Value { line: L, .. })
        ));
        assert!(matches!(
            int(1).rem(&int(0), L),
            Err(TreadleError::Value { line: L, .. })
        ));
    }

    #[test]
    fn type_mismatches_are_type_errors() {
        for got in [
            int(1).add(&Value::Bool(true), L),
            Value::str("a").add(&int(1), L),
            Value::Nil.sub(&int(1), L),
            Value::str("a").mul(&Value::str("b"), L),
            Value::str("a").div(&int(1), L),
            Value::Nil.rem(&Value::Nil, L),
            Value::Bool(true).neg(L),
            int(1).not(L),
        ] {
            assert!(
                matches!(got, Err(TreadleError::Type { line: L, .. })),
                "expected Type error, got {got:?}"
            );
        }
    }

    #[test]
    fn equality_requires_the_same_type() {
        assert!(int(1).eq_value(&int(1), L).unwrap());
        assert!(!int(1).eq_value(&int(2), L).unwrap());
        assert!(Value::Nil.eq_value(&Value::Nil, L).unwrap());
        assert!(Value::str("a").eq_value(&Value::str("a"), L).unwrap());
        assert!(matches!(
            int(1).eq_value(&Value::str("1"), L),
            Err(TreadleError::Type { line: L, .. })
        ));
        // the derived PartialEq is the Rust-side one and does NOT error
        assert_ne!(int(1), Value::str("1"));
    }

    #[test]
    fn ordering_is_int_or_str() {
        assert_eq!(int(1).cmp_value(&int(2), "<", L).unwrap(), Ordering::Less);
        assert_eq!(
            Value::str("ab")
                .cmp_value(&Value::str("b"), "<", L)
                .unwrap(),
            Ordering::Less
        );
        assert!(matches!(
            Value::Bool(true).cmp_value(&Value::Bool(false), "<", L),
            Err(TreadleError::Type { line: L, .. })
        ));
    }

    #[test]
    fn as_bool_has_no_truthiness() {
        assert!(Value::Bool(true).as_bool(L).unwrap());
        assert_eq!(Value::Bool(false).not(L).unwrap(), Value::Bool(true));
        for v in [Value::Nil, int(0), int(1), Value::str("")] {
            assert!(matches!(
                v.as_bool(L),
                Err(TreadleError::Type { line: L, .. })
            ));
        }
    }
}
