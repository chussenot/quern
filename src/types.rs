//! bead: quern-types — HOT: Value, Type, Schema, Row, QuernError. docs/quern.md §3
//!
//! The enum and struct shapes below are frozen by docs/quern.md §3 and are
//! implemented verbatim. Everything else in this file is the shared behaviour
//! the rest of the engine is expected to route through rather than reinvent:
//! the NULL rule (§1), the `.slt` print rules (§5), and the sort order.

use std::cmp::Ordering;
use std::fmt;

// --- frozen §3 shapes -------------------------------------------------------

/// `Eq` is derived: quern has no floats, so `PartialEq` on `Value` is already
/// a true equivalence relation and `Eq` costs nothing to promise. `Ord` is
/// derived too, but it is the *determinism* order (variant order, `Null`
/// first) used for grouping and stable output — NOT the SQL `ORDER BY` order.
/// Use [`Value::sort_cmp`] for `ORDER BY`, which puts NULL last ascending.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Type {
    Int,
    Text,
    Bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub ty: Type,
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub table: String,
    pub columns: Vec<Column>,
}

pub type Row = Vec<Value>;
pub type RowId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuernError {
    Parse(String),
    Catalog(String),
    Type(String),
    Storage(String),
    Txn(String),
}

pub type Result<T> = std::result::Result<T, QuernError>;

// --- Display ---------------------------------------------------------------

impl Type {
    /// SQL spelling, as §1 writes it. Used in error messages.
    pub fn name(self) -> &'static str {
        match self {
            Type::Int => "INT",
            Type::Text => "TEXT",
            Type::Bool => "BOOL",
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The `.slt` print rules from §5: `NULL` for null, `TRUE`/`FALSE` for bools,
/// bare integers, bare text with no quoting. The REPL and the `.slt` runner
/// both format cells through this, so there is one definition of the output.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Text(s) => f.write_str(s),
            Value::Bool(true) => f.write_str("TRUE"),
            Value::Bool(false) => f.write_str("FALSE"),
        }
    }
}

impl fmt::Display for QuernError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, msg) = match self {
            QuernError::Parse(m) => ("parse error", m),
            QuernError::Catalog(m) => ("catalog error", m),
            QuernError::Type(m) => ("type error", m),
            QuernError::Storage(m) => ("storage error", m),
            QuernError::Txn(m) => ("transaction error", m),
        };
        write!(f, "{kind}: {msg}")
    }
}

impl std::error::Error for QuernError {}

// --- the NULL rule (§1) ----------------------------------------------------

// The names here (`add`, `sub`, `eq`, `lt`, ...) are pinned by the frozen
// contract, and every one of them collides with a std trait method name, so
// clippy's should_implement_trait fires on the whole block. They cannot be
// the trait impls: all eight are fallible and return `Result<Value>`.
// NOTE for callers: the inherent `Value::eq`/`Value::ne` shadow
// `PartialEq::eq`/`ne` for method-call syntax. `a == b` is unaffected (the
// operator desugars to the trait); `a.eq(&b)` returns `Result<Value>` here.
#[allow(clippy::should_implement_trait)]
impl Value {
    /// `"NULL"`, `"INT"`, `"TEXT"`, `"BOOL"` — for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Int(_) => Type::Int.name(),
            Value::Text(_) => Type::Text.name(),
            Value::Bool(_) => Type::Bool.name(),
        }
    }

    /// §1: a `WHERE` clause keeps a row only when the predicate is exactly
    /// `Value::Bool(true)`. `Null` and every other value are falsy.
    pub fn is_true(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    /// `Ok(None)` means "an operand was Null, so the result is Null".
    fn int_pair(&self, other: &Value, op: &str) -> Result<Option<(i64, i64)>> {
        match (self, other) {
            (Value::Null, _) | (_, Value::Null) => Ok(None),
            (Value::Int(a), Value::Int(b)) => Ok(Some((*a, *b))),
            _ => Err(QuernError::Type(format!(
                "cannot apply {op} to {} and {}",
                self.type_name(),
                other.type_name()
            ))),
        }
    }

    fn overflow(op: &str) -> QuernError {
        QuernError::Type(format!("integer overflow in {op}"))
    }

    pub fn add(&self, other: &Value) -> Result<Value> {
        match self.int_pair(other, "+")? {
            None => Ok(Value::Null),
            Some((a, b)) => a
                .checked_add(b)
                .map(Value::Int)
                .ok_or_else(|| Self::overflow("+")),
        }
    }

    pub fn sub(&self, other: &Value) -> Result<Value> {
        match self.int_pair(other, "-")? {
            None => Ok(Value::Null),
            Some((a, b)) => a
                .checked_sub(b)
                .map(Value::Int)
                .ok_or_else(|| Self::overflow("-")),
        }
    }

    pub fn mul(&self, other: &Value) -> Result<Value> {
        match self.int_pair(other, "*")? {
            None => Ok(Value::Null),
            Some((a, b)) => a
                .checked_mul(b)
                .map(Value::Int)
                .ok_or_else(|| Self::overflow("*")),
        }
    }

    /// Division by zero is an error, not `Null` — §1 lists divide-by-zero
    /// among the errors, and the NULL rule is about NULL operands only.
    pub fn div(&self, other: &Value) -> Result<Value> {
        match self.int_pair(other, "/")? {
            None => Ok(Value::Null),
            Some((_, 0)) => Err(QuernError::Type("division by zero".into())),
            Some((a, b)) => a
                .checked_div(b)
                .map(Value::Int)
                .ok_or_else(|| Self::overflow("/")),
        }
    }

    /// `Ok(None)` means "an operand was Null, so the result is Null".
    /// Comparing two different non-Null types is an error, not `false`.
    fn cmp_pair(&self, other: &Value, op: &str) -> Result<Option<Ordering>> {
        match (self, other) {
            (Value::Null, _) | (_, Value::Null) => Ok(None),
            (Value::Int(a), Value::Int(b)) => Ok(Some(a.cmp(b))),
            (Value::Text(a), Value::Text(b)) => Ok(Some(a.cmp(b))),
            (Value::Bool(a), Value::Bool(b)) => Ok(Some(a.cmp(b))),
            _ => Err(QuernError::Type(format!(
                "cannot compare {} with {} using {op}",
                self.type_name(),
                other.type_name()
            ))),
        }
    }

    fn cmp_op(&self, other: &Value, op: &str, keep: fn(Ordering) -> bool) -> Result<Value> {
        Ok(match self.cmp_pair(other, op)? {
            None => Value::Null,
            Some(o) => Value::Bool(keep(o)),
        })
    }

    pub fn eq(&self, other: &Value) -> Result<Value> {
        self.cmp_op(other, "=", |o| o == Ordering::Equal)
    }

    pub fn ne(&self, other: &Value) -> Result<Value> {
        self.cmp_op(other, "<>", |o| o != Ordering::Equal)
    }

    pub fn lt(&self, other: &Value) -> Result<Value> {
        self.cmp_op(other, "<", |o| o == Ordering::Less)
    }

    pub fn gt(&self, other: &Value) -> Result<Value> {
        self.cmp_op(other, ">", |o| o == Ordering::Greater)
    }

    /// The one `ORDER BY` comparator. §1: NULL sorts last in `ASC`, first in
    /// `DESC`. `descending` matches the `bool` in `LogicalPlan::Sort::keys`,
    /// and the returned `Ordering` is already reversed for it, so a sort is
    /// `rows.sort_by(|a, b| Value::sort_cmp(&a[k], &b[k], desc))`.
    ///
    /// Mixed non-Null types fall back to variant order rather than erroring —
    /// a comparator cannot fail, and a well-typed plan never gets here.
    pub fn sort_cmp(a: &Value, b: &Value, descending: bool) -> Ordering {
        match (a, b) {
            (Value::Null, Value::Null) => Ordering::Equal,
            // NULL last ascending / first descending, so it is *not* reversed
            // along with everything else.
            (Value::Null, _) => {
                if descending {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (_, Value::Null) => {
                if descending {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            _ => {
                let o = a.cmp(b);
                if descending {
                    o.reverse()
                } else {
                    o
                }
            }
        }
    }
}

// --- Schema ----------------------------------------------------------------

impl Schema {
    /// Resolve an unqualified column name to its index. Case-insensitive,
    /// because §1 makes identifiers case-insensitive. Qualified names
    /// (`t.a`) are the planner's job to split before calling this.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Index of the `INTEGER PRIMARY KEY` column, if the table declared one.
    /// The `Column` itself is `&schema.columns[i]`; the index is what the
    /// btree and the row encoding actually need.
    pub fn primary_key(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.primary_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(i: i64) -> Value {
        Value::Int(i)
    }
    fn txt(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    #[test]
    fn null_swallows_all_arithmetic_and_comparison() {
        let n = Value::Null;
        for other in [Value::Null, int(1), txt("x"), Value::Bool(true)] {
            for r in [
                n.add(&other),
                n.sub(&other),
                n.mul(&other),
                n.div(&other),
                n.eq(&other),
                n.ne(&other),
                n.lt(&other),
                n.gt(&other),
                // and in the other operand position
                other.add(&n),
                other.sub(&n),
                other.mul(&n),
                other.div(&n),
                other.eq(&n),
                other.ne(&n),
                other.lt(&n),
                other.gt(&n),
            ] {
                assert_eq!(r, Ok(Value::Null), "NULL operand must yield NULL");
            }
        }
    }

    #[test]
    fn divide_by_zero_is_an_error_not_null() {
        assert!(matches!(int(1).div(&int(0)), Err(QuernError::Type(_))));
        // ...but NULL / 0 is still NULL: the NULL rule wins before we look
        // at the divisor.
        assert_eq!(Value::Null.div(&int(0)), Ok(Value::Null));
        assert_eq!(int(7).div(&int(2)), Ok(int(3)));
    }

    #[test]
    fn arithmetic_is_int_only_and_does_not_panic_on_overflow() {
        assert!(matches!(txt("a").add(&txt("b")), Err(QuernError::Type(_))));
        assert!(int(1).add(&Value::Bool(true)).is_err());
        assert!(matches!(
            int(i64::MAX).add(&int(1)),
            Err(QuernError::Type(_))
        ));
        assert!(matches!(
            int(i64::MIN).div(&int(-1)),
            Err(QuernError::Type(_))
        ));
    }

    #[test]
    fn comparison_needs_like_types() {
        assert_eq!(int(1).lt(&int(2)), Ok(Value::Bool(true)));
        assert_eq!(txt("a").lt(&txt("b")), Ok(Value::Bool(true)));
        assert_eq!(
            Value::Bool(false).lt(&Value::Bool(true)),
            Ok(Value::Bool(true))
        );
        assert_eq!(int(1).ne(&int(1)), Ok(Value::Bool(false)));
        assert!(matches!(int(1).eq(&txt("1")), Err(QuernError::Type(_))));
    }

    #[test]
    fn where_keeps_only_bool_true() {
        assert!(Value::Bool(true).is_true());
        assert!(!Value::Bool(false).is_true());
        assert!(!Value::Null.is_true());
        assert!(!int(1).is_true());
    }

    #[test]
    fn sort_puts_null_last_ascending_and_first_descending() {
        let mut rows = vec![int(2), Value::Null, int(1)];
        rows.sort_by(|a, b| Value::sort_cmp(a, b, false));
        assert_eq!(rows, vec![int(1), int(2), Value::Null]);
        rows.sort_by(|a, b| Value::sort_cmp(a, b, true));
        assert_eq!(rows, vec![Value::Null, int(2), int(1)]);
    }

    #[test]
    fn display_follows_the_slt_print_rules() {
        assert_eq!(Value::Null.to_string(), "NULL");
        assert_eq!(int(-3).to_string(), "-3");
        assert_eq!(txt("a b").to_string(), "a b");
        assert_eq!(Value::Bool(true).to_string(), "TRUE");
        assert_eq!(Value::Bool(false).to_string(), "FALSE");
        assert_eq!(
            QuernError::Type("division by zero".into()).to_string(),
            "type error: division by zero"
        );
    }

    #[test]
    fn schema_resolves_names_case_insensitively_and_finds_the_pk() {
        let s = Schema {
            table: "t".into(),
            columns: vec![
                Column {
                    name: "a".into(),
                    ty: Type::Int,
                    primary_key: true,
                },
                Column {
                    name: "b".into(),
                    ty: Type::Text,
                    primary_key: false,
                },
            ],
        };
        assert_eq!(s.column_index("A"), Some(0));
        assert_eq!(s.column_index("b"), Some(1));
        assert_eq!(s.column_index("nope"), None);
        assert_eq!(s.primary_key(), Some(0));
        assert_eq!(
            Schema {
                table: "u".into(),
                columns: vec![]
            }
            .primary_key(),
            None
        );
    }
}
