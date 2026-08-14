//! bead: treadle-ast — FROZEN: Expr, Stmt, FnDecl, Program. §3
//!
//! The boundary between the shared front end and both back ends. The parser
//! produces exactly these nodes; the compiler and the tree-walker consume them
//! and may not extend them. Every node carries a `line` because an error's line
//! number is part of the observable `Output` (§3) and the two engines must
//! agree on it — §6 pins it to the **innermost** failing node, never the
//! enclosing statement.
//!
//! This module depends on `value.rs` only (for `Expr::Lit`); it raises no
//! errors and needs nothing from `error.rs`.

use std::rc::Rc;

use crate::value::Value;

/// Prefix operators (§2, tightest binding). There is no `Pos`: `+x` is not in
/// the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// `-` on `Int`.
    Neg,
    /// `!` on `Bool`.
    Not,
}

/// Binary operators, listed in §2 precedence order, **loosest first**, so the
/// parser's precedence ladder reads top-to-bottom against this enum. Rust's own
/// spelling throughout: `Rem` not `Mod`, `Ne` not `Neq`, `Le`/`Ge` not
/// `Lte`/`Gte`.
///
/// `Or` and `And` are variants here even though both engines must special-case
/// them **before** evaluating `rhs` (§2 short-circuit, §6 short-circuit
/// typing): keeping them in `BinOp` means the parser needs no separate node and
/// the frozen `Expr` needs no sixth variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// An expression. `Lit` holds a `Value` directly, so the lexer's integer-range
/// check (§6: out of range is a `Lex` error) has already happened by the time a
/// back end sees one.
#[derive(Debug, Clone)]
pub enum Expr {
    Lit(Value),
    Var {
        name: String,
        line: u32,
    },
    Unary {
        op: UnOp,
        rhs: Box<Expr>,
        line: u32,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        line: u32,
    },
    Call {
        name: String,
        args: Vec<Expr>,
        line: u32,
    },
}

/// A statement.
///
/// There is deliberately **no expression statement** (§6, bead `.44`): `f();`
/// is a `Parse` error, and a call for effect is written `print f();` or
/// `let _ = f();`. Do not add a variant for it — six downstream beads match
/// this enum exhaustively.
///
/// `If`/`While` bodies are always braced, so there is no dangling else; an
/// `else if` chain is `els: vec![Stmt::If { .. }]`. `Let` always has an
/// initialiser.
#[derive(Debug, Clone)]
pub enum Stmt {
    Let {
        name: String,
        init: Expr,
        line: u32,
    },
    Assign {
        name: String,
        value: Expr,
        line: u32,
    },
    Print {
        args: Vec<Expr>,
        line: u32,
    },
    If {
        cond: Expr,
        then: Vec<Stmt>,
        els: Vec<Stmt>,
        line: u32,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
        line: u32,
    },
    Return {
        value: Option<Expr>,
        line: u32,
    },
    /// A `fn` declaration **in the position it was written**. It is a no-op at
    /// run time — see the note on [`Program`].
    Fn(Rc<FnDecl>),
}

/// A function declaration. Shared as `Rc` because the same declaration is
/// reachable both from `Program::fns` and from the `Stmt::Fn` left in place.
#[derive(Debug, Clone)]
pub struct FnDecl {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    pub line: u32,
}

/// A whole program.
///
/// # `stmts` vs `fns` — the one rule both back ends must follow
///
/// `fns` is the **complete** hoisted list: every `FnDecl` in the program, at
/// any nesting depth, in source order. `stmts` **also** still contains a
/// `Stmt::Fn` wherever a declaration was written (including inside `If`,
/// `While` and function bodies) — the parser hoists by *copying the `Rc`*, it
/// does not remove the statement.
///
/// Therefore:
///
/// - **Define functions from `fns` only, before executing any statement.**
/// - **`Stmt::Fn` is a no-op** when executing a statement list: match it and do
///   nothing (the VM emits no instruction for it).
///
/// A back end that also defined on reaching `Stmt::Fn` would define the same
/// function twice; one that walked neither list would never define it. Defining
/// from `fns` is what makes §2's hoisting true: a program may call a function
/// declared later in the file, and a `fn` inside a branch that never runs is
/// still callable, which is exactly why the declaration cannot be a runtime
/// action.
///
/// Duplicate `fn` names are a `Parse` error (§6, bead `.42`), so `fns` never
/// contains two entries with the same name and the definition order is not
/// observable.
#[derive(Debug, Clone)]
pub struct Program {
    pub stmts: Vec<Stmt>,
    pub fns: Vec<Rc<FnDecl>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_BINOPS: [BinOp; 13] = [
        BinOp::Or,
        BinOp::And,
        BinOp::Eq,
        BinOp::Ne,
        BinOp::Lt,
        BinOp::Gt,
        BinOp::Le,
        BinOp::Ge,
        BinOp::Add,
        BinOp::Sub,
        BinOp::Mul,
        BinOp::Div,
        BinOp::Rem,
    ];

    fn bin(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            line: 1,
        }
    }

    fn int(n: i64) -> Expr {
        Expr::Lit(Value::Int(n))
    }

    /// A program using every `BinOp` once, both `UnOp`s, a call, and a function
    /// whose body has `if`/`while`/`return`:
    ///
    /// ```text
    /// fn f(a, b) {
    ///     let t = <every BinOp folded>;
    ///     while a < b { a = a + 1; }
    ///     if !(a == b) { return -a; } else { return nil; }
    ///     return;
    /// }
    /// print f(1, 2);
    /// ```
    fn representative() -> Program {
        let folded = ALL_BINOPS
            .iter()
            .fold(int(0), |acc, &op| bin(op, acc, int(1)));

        let decl = Rc::new(FnDecl {
            name: "f".to_string(),
            params: vec!["a".to_string(), "b".to_string()],
            body: vec![
                Stmt::Let {
                    name: "t".to_string(),
                    init: folded,
                    line: 2,
                },
                Stmt::While {
                    cond: bin(
                        BinOp::Lt,
                        Expr::Var {
                            name: "a".to_string(),
                            line: 3,
                        },
                        Expr::Var {
                            name: "b".to_string(),
                            line: 3,
                        },
                    ),
                    body: vec![Stmt::Assign {
                        name: "a".to_string(),
                        value: bin(
                            BinOp::Add,
                            Expr::Var {
                                name: "a".to_string(),
                                line: 3,
                            },
                            int(1),
                        ),
                        line: 3,
                    }],
                    line: 3,
                },
                Stmt::If {
                    cond: Expr::Unary {
                        op: UnOp::Not,
                        rhs: Box::new(bin(
                            BinOp::Eq,
                            Expr::Var {
                                name: "a".to_string(),
                                line: 4,
                            },
                            Expr::Var {
                                name: "b".to_string(),
                                line: 4,
                            },
                        )),
                        line: 4,
                    },
                    then: vec![Stmt::Return {
                        value: Some(Expr::Unary {
                            op: UnOp::Neg,
                            rhs: Box::new(Expr::Var {
                                name: "a".to_string(),
                                line: 5,
                            }),
                            line: 5,
                        }),
                        line: 5,
                    }],
                    els: vec![Stmt::Return {
                        value: Some(Expr::Lit(Value::Nil)),
                        line: 6,
                    }],
                    line: 4,
                },
                Stmt::Return {
                    value: None,
                    line: 7,
                },
            ],
            line: 1,
        });

        Program {
            stmts: vec![
                Stmt::Fn(Rc::clone(&decl)),
                Stmt::Print {
                    args: vec![Expr::Call {
                        name: "f".to_string(),
                        args: vec![int(1), int(2)],
                        line: 9,
                    }],
                    line: 9,
                },
            ],
            fns: vec![decl],
        }
    }

    fn binops_of_expr(e: &Expr, out: &mut Vec<BinOp>) {
        match e {
            Expr::Lit(_) | Expr::Var { .. } => {}
            Expr::Unary { rhs, .. } => binops_of_expr(rhs, out),
            Expr::Binary { op, lhs, rhs, .. } => {
                out.push(*op);
                binops_of_expr(lhs, out);
                binops_of_expr(rhs, out);
            }
            Expr::Call { args, .. } => args.iter().for_each(|a| binops_of_expr(a, out)),
        }
    }

    fn binops_of_stmts(stmts: &[Stmt], out: &mut Vec<BinOp>) {
        for s in stmts {
            match s {
                Stmt::Let { init: e, .. } | Stmt::Assign { value: e, .. } => binops_of_expr(e, out),
                Stmt::Print { args, .. } => args.iter().for_each(|a| binops_of_expr(a, out)),
                Stmt::If {
                    cond, then, els, ..
                } => {
                    binops_of_expr(cond, out);
                    binops_of_stmts(then, out);
                    binops_of_stmts(els, out);
                }
                Stmt::While { cond, body, .. } => {
                    binops_of_expr(cond, out);
                    binops_of_stmts(body, out);
                }
                Stmt::Return { value, .. } => {
                    if let Some(e) = value {
                        binops_of_expr(e, out);
                    }
                }
                // No-op at run time; the declaration is reached through `fns`.
                Stmt::Fn(_) => {}
            }
        }
    }

    /// Walking the AST exhaustively is possible with the frozen variants alone,
    /// and every `BinOp` name is reachable.
    #[test]
    fn ast_walk_covers_every_binop() {
        let p = representative();
        let mut seen = Vec::new();
        binops_of_stmts(&p.stmts, &mut seen);
        for f in &p.fns {
            binops_of_stmts(&f.body, &mut seen);
        }
        for op in ALL_BINOPS {
            assert!(seen.contains(&op), "{op:?} never reached by the walker");
        }
        // Two comparisons live outside the folded chain (`while a < b`,
        // `a == b`) and one `+` in the loop body.
        assert_eq!(seen.len(), ALL_BINOPS.len() + 3);
    }

    /// The hoisting contract: a declaration is in `fns` **and** left in `stmts`,
    /// as the same `Rc`, so defining from `fns` and ignoring `Stmt::Fn` defines
    /// each function exactly once.
    #[test]
    fn fn_decl_is_hoisted_and_left_in_place() {
        let p = representative();
        assert_eq!(p.fns.len(), 1);
        let in_stmts: Vec<&Rc<FnDecl>> = p
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Fn(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(in_stmts.len(), 1);
        assert!(Rc::ptr_eq(in_stmts[0], &p.fns[0]));
        assert_eq!(p.fns[0].name, "f");
        assert_eq!(p.fns[0].params, vec!["a".to_string(), "b".to_string()]);
    }

    /// `Clone` is deep for the nodes and shared for `FnDecl`; `Debug` exists on
    /// everything (the fuzzer prints a failing program).
    #[test]
    fn clone_and_debug() {
        let p = representative();
        let q = p.clone();
        assert_eq!(format!("{p:?}"), format!("{q:?}"));
        assert!(Rc::ptr_eq(&p.fns[0], &q.fns[0]));
        assert_eq!(format!("{:?}", UnOp::Neg), "Neg");
        assert_eq!(format!("{:?}", BinOp::Rem), "Rem");
    }

    /// `UnOp`/`BinOp` are `Copy` + `Eq`, so match arms need no borrow.
    #[test]
    fn operators_are_copy_and_eq() {
        let op = BinOp::Le;
        let also = op;
        assert_eq!(op, also);
        assert_ne!(BinOp::Lt, BinOp::Le);
        assert_ne!(UnOp::Neg, UnOp::Not);
    }
}
