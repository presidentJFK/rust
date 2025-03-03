// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! # Compilation of match statements
//!
//! I will endeavor to explain the code as best I can.  I have only a loose
//! understanding of some parts of it.
//!
//! ## Matching
//!
//! The basic state of the code is maintained in an array `m` of `Match`
//! objects.  Each `Match` describes some list of patterns, all of which must
//! match against the current list of values.  If those patterns match, then
//! the arm listed in the match is the correct arm.  A given arm may have
//! multiple corresponding match entries, one for each alternative that
//! remains.  As we proceed these sets of matches are adjusted by the various
//! `enter_XXX()` functions, each of which adjusts the set of options given
//! some information about the value which has been matched.
//!
//! So, initially, there is one value and N matches, each of which have one
//! constituent pattern.  N here is usually the number of arms but may be
//! greater, if some arms have multiple alternatives.  For example, here:
//!
//!     enum Foo { A, B(int), C(usize, usize) }
//!     match foo {
//!         A => ...,
//!         B(x) => ...,
//!         C(1, 2) => ...,
//!         C(_) => ...
//!     }
//!
//! The value would be `foo`.  There would be four matches, each of which
//! contains one pattern (and, in one case, a guard).  We could collect the
//! various options and then compile the code for the case where `foo` is an
//! `A`, a `B`, and a `C`.  When we generate the code for `C`, we would (1)
//! drop the two matches that do not match a `C` and (2) expand the other two
//! into two patterns each.  In the first case, the two patterns would be `1`
//! and `2`, and the in the second case the _ pattern would be expanded into
//! `_` and `_`.  The two values are of course the arguments to `C`.
//!
//! Here is a quick guide to the various functions:
//!
//! - `compile_submatch()`: The main workhouse.  It takes a list of values and
//!   a list of matches and finds the various possibilities that could occur.
//!
//! - `enter_XXX()`: modifies the list of matches based on some information
//!   about the value that has been matched.  For example,
//!   `enter_rec_or_struct()` adjusts the values given that a record or struct
//!   has been matched.  This is an infallible pattern, so *all* of the matches
//!   must be either wildcards or record/struct patterns.  `enter_opt()`
//!   handles the fallible cases, and it is correspondingly more complex.
//!
//! ## Bindings
//!
//! We store information about the bound variables for each arm as part of the
//! per-arm `ArmData` struct.  There is a mapping from identifiers to
//! `BindingInfo` structs.  These structs contain the mode/id/type of the
//! binding, but they also contain an LLVM value which points at an alloca
//! called `llmatch`. For by value bindings that are Copy, we also create
//! an extra alloca that we copy the matched value to so that any changes
//! we do to our copy is not reflected in the original and vice-versa.
//! We don't do this if it's a move since the original value can't be used
//! and thus allowing us to cheat in not creating an extra alloca.
//!
//! The `llmatch` binding always stores a pointer into the value being matched
//! which points at the data for the binding.  If the value being matched has
//! type `T`, then, `llmatch` will point at an alloca of type `T*` (and hence
//! `llmatch` has type `T**`).  So, if you have a pattern like:
//!
//!    let a: A = ...;
//!    let b: B = ...;
//!    match (a, b) { (ref c, d) => { ... } }
//!
//! For `c` and `d`, we would generate allocas of type `C*` and `D*`
//! respectively.  These are called the `llmatch`.  As we match, when we come
//! up against an identifier, we store the current pointer into the
//! corresponding alloca.
//!
//! Once a pattern is completely matched, and assuming that there is no guard
//! pattern, we will branch to a block that leads to the body itself.  For any
//! by-value bindings, this block will first load the ptr from `llmatch` (the
//! one of type `D*`) and then load a second time to get the actual value (the
//! one of type `D`). For by ref bindings, the value of the local variable is
//! simply the first alloca.
//!
//! So, for the example above, we would generate a setup kind of like this:
//!
//!        +-------+
//!        | Entry |
//!        +-------+
//!            |
//!        +--------------------------------------------+
//!        | llmatch_c = (addr of first half of tuple)  |
//!        | llmatch_d = (addr of second half of tuple) |
//!        +--------------------------------------------+
//!            |
//!        +--------------------------------------+
//!        | *llbinding_d = **llmatch_d           |
//!        +--------------------------------------+
//!
//! If there is a guard, the situation is slightly different, because we must
//! execute the guard code.  Moreover, we need to do so once for each of the
//! alternatives that lead to the arm, because if the guard fails, they may
//! have different points from which to continue the search. Therefore, in that
//! case, we generate code that looks more like:
//!
//!        +-------+
//!        | Entry |
//!        +-------+
//!            |
//!        +-------------------------------------------+
//!        | llmatch_c = (addr of first half of tuple) |
//!        | llmatch_d = (addr of first half of tuple) |
//!        +-------------------------------------------+
//!            |
//!        +-------------------------------------------------+
//!        | *llbinding_d = **llmatch_d                      |
//!        | check condition                                 |
//!        | if false { goto next case }                     |
//!        | if true { goto body }                           |
//!        +-------------------------------------------------+
//!
//! The handling for the cleanups is a bit... sensitive.  Basically, the body
//! is the one that invokes `add_clean()` for each binding.  During the guard
//! evaluation, we add temporary cleanups and revoke them after the guard is
//! evaluated (it could fail, after all). Note that guards and moves are
//! just plain incompatible.
//!
//! Some relevant helper functions that manage bindings:
//! - `create_bindings_map()`
//! - `insert_lllocals()`
//!
//!
//! ## Notes on vector pattern matching.
//!
//! Vector pattern matching is surprisingly tricky. The problem is that
//! the structure of the vector isn't fully known, and slice matches
//! can be done on subparts of it.
//!
//! The way that vector pattern matches are dealt with, then, is as
//! follows. First, we make the actual condition associated with a
//! vector pattern simply a vector length comparison. So the pattern
//! [1, .. x] gets the condition "vec len >= 1", and the pattern
//! [.. x] gets the condition "vec len >= 0". The problem here is that
//! having the condition "vec len >= 1" hold clearly does not mean that
//! only a pattern that has exactly that condition will match. This
//! means that it may well be the case that a condition holds, but none
//! of the patterns matching that condition match; to deal with this,
//! when doing vector length matches, we have match failures proceed to
//! the next condition to check.
//!
//! There are a couple more subtleties to deal with. While the "actual"
//! condition associated with vector length tests is simply a test on
//! the vector length, the actual vec_len Opt entry contains more
//! information used to restrict which matches are associated with it.
//! So that all matches in a submatch are matching against the same
//! values from inside the vector, they are split up by how many
//! elements they match at the front and at the back of the vector. In
//! order to make sure that arms are properly checked in order, even
//! with the overmatching conditions, each vec_len Opt entry is
//! associated with a range of matches.
//! Consider the following:
//!
//!   match &[1, 2, 3] {
//!       [1, 1, .. _] => 0,
//!       [1, 2, 2, .. _] => 1,
//!       [1, 2, 3, .. _] => 2,
//!       [1, 2, .. _] => 3,
//!       _ => 4
//!   }
//! The proper arm to match is arm 2, but arms 0 and 3 both have the
//! condition "len >= 2". If arm 3 was lumped in with arm 0, then the
//! wrong branch would be taken. Instead, vec_len Opts are associated
//! with a contiguous range of matches that have the same "shape".
//! This is sort of ugly and requires a bunch of special handling of
//! vec_len options.

pub use self::BranchKind::*;
pub use self::OptResult::*;
pub use self::TransBindingMode::*;
use self::Opt::*;
use self::FailureHandler::*;

use back::abi;
use llvm::{ValueRef, BasicBlockRef};
use middle::check_match::StaticInliner;
use middle::check_match;
use middle::const_eval;
use middle::def::{self, DefMap};
use middle::expr_use_visitor as euv;
use middle::lang_items::StrEqFnLangItem;
use middle::mem_categorization as mc;
use middle::pat_util::*;
use trans::adt;
use trans::base::*;
use trans::build::{AddCase, And, Br, CondBr, GEPi, InBoundsGEP, Load, PointerCast};
use trans::build::{Not, Store, Sub, add_comment};
use trans::build;
use trans::callee;
use trans::cleanup::{self, CleanupMethods};
use trans::common::*;
use trans::consts;
use trans::datum::*;
use trans::debuginfo::{self, DebugLoc, ToDebugLoc};
use trans::expr::{self, Dest};
use trans::monomorphize;
use trans::tvec;
use trans::type_of;
use middle::ty::{self, Ty};
use session::config::{NoDebugInfo, FullDebugInfo};
use util::common::indenter;
use util::nodemap::FnvHashMap;
use util::ppaux::{Repr, vec_map_to_string};

use std;
use std::cmp::Ordering;
use std::rc::Rc;
use syntax::ast;
use syntax::ast::{DUMMY_NODE_ID, NodeId};
use syntax::codemap::Span;
use syntax::fold::Folder;
use syntax::ptr::P;

#[derive(Copy, Clone, Debug)]
struct ConstantExpr<'a>(&'a ast::Expr);

impl<'a> ConstantExpr<'a> {
    fn eq(self, other: ConstantExpr<'a>, tcx: &ty::ctxt) -> bool {
        match const_eval::compare_lit_exprs(tcx, self.0, other.0, None,
                                            |id| {ty::node_id_item_substs(tcx, id).substs}) {
            Some(result) => result == Ordering::Equal,
            None => panic!("compare_list_exprs: type mismatch"),
        }
    }
}

// An option identifying a branch (either a literal, an enum variant or a range)
#[derive(Debug)]
enum Opt<'a, 'tcx> {
    ConstantValue(ConstantExpr<'a>, DebugLoc),
    ConstantRange(ConstantExpr<'a>, ConstantExpr<'a>, DebugLoc),
    Variant(ty::Disr, Rc<adt::Repr<'tcx>>, ast::DefId, DebugLoc),
    SliceLengthEqual(usize, DebugLoc),
    SliceLengthGreaterOrEqual(/* prefix length */ usize,
                              /* suffix length */ usize,
                              DebugLoc),
}

impl<'a, 'tcx> Opt<'a, 'tcx> {
    fn eq(&self, other: &Opt<'a, 'tcx>, tcx: &ty::ctxt<'tcx>) -> bool {
        match (self, other) {
            (&ConstantValue(a, _), &ConstantValue(b, _)) => a.eq(b, tcx),
            (&ConstantRange(a1, a2, _), &ConstantRange(b1, b2, _)) => {
                a1.eq(b1, tcx) && a2.eq(b2, tcx)
            }
            (&Variant(a_disr, ref a_repr, a_def, _),
             &Variant(b_disr, ref b_repr, b_def, _)) => {
                a_disr == b_disr && *a_repr == *b_repr && a_def == b_def
            }
            (&SliceLengthEqual(a, _), &SliceLengthEqual(b, _)) => a == b,
            (&SliceLengthGreaterOrEqual(a1, a2, _),
             &SliceLengthGreaterOrEqual(b1, b2, _)) => {
                a1 == b1 && a2 == b2
            }
            _ => false
        }
    }

    fn trans<'blk>(&self, mut bcx: Block<'blk, 'tcx>) -> OptResult<'blk, 'tcx> {
        let _icx = push_ctxt("match::trans_opt");
        let ccx = bcx.ccx();
        match *self {
            ConstantValue(ConstantExpr(lit_expr), _) => {
                let lit_ty = ty::node_id_to_type(bcx.tcx(), lit_expr.id);
                let (llval, _) = consts::const_expr(ccx, &*lit_expr, bcx.fcx.param_substs, None);
                let lit_datum = immediate_rvalue(llval, lit_ty);
                let lit_datum = unpack_datum!(bcx, lit_datum.to_appropriate_datum(bcx));
                SingleResult(Result::new(bcx, lit_datum.val))
            }
            ConstantRange(ConstantExpr(ref l1), ConstantExpr(ref l2), _) => {
                let (l1, _) = consts::const_expr(ccx, &**l1, bcx.fcx.param_substs, None);
                let (l2, _) = consts::const_expr(ccx, &**l2, bcx.fcx.param_substs, None);
                RangeResult(Result::new(bcx, l1), Result::new(bcx, l2))
            }
            Variant(disr_val, ref repr, _, _) => {
                adt::trans_case(bcx, &**repr, disr_val)
            }
            SliceLengthEqual(length, _) => {
                SingleResult(Result::new(bcx, C_uint(ccx, length)))
            }
            SliceLengthGreaterOrEqual(prefix, suffix, _) => {
                LowerBound(Result::new(bcx, C_uint(ccx, prefix + suffix)))
            }
        }
    }

    fn debug_loc(&self) -> DebugLoc {
        match *self {
            ConstantValue(_,debug_loc)                 |
            ConstantRange(_, _, debug_loc)             |
            Variant(_, _, _, debug_loc)                |
            SliceLengthEqual(_, debug_loc)             |
            SliceLengthGreaterOrEqual(_, _, debug_loc) => debug_loc
        }
    }
}

#[derive(Copy, Clone, PartialEq)]
pub enum BranchKind {
    NoBranch,
    Single,
    Switch,
    Compare,
    CompareSliceLength
}

pub enum OptResult<'blk, 'tcx: 'blk> {
    SingleResult(Result<'blk, 'tcx>),
    RangeResult(Result<'blk, 'tcx>, Result<'blk, 'tcx>),
    LowerBound(Result<'blk, 'tcx>)
}

#[derive(Clone, Copy, PartialEq)]
pub enum TransBindingMode {
    TrByCopy(/* llbinding */ ValueRef),
    TrByMove,
    TrByRef,
}

/// Information about a pattern binding:
/// - `llmatch` is a pointer to a stack slot.  The stack slot contains a
///   pointer into the value being matched.  Hence, llmatch has type `T**`
///   where `T` is the value being matched.
/// - `trmode` is the trans binding mode
/// - `id` is the node id of the binding
/// - `ty` is the Rust type of the binding
#[derive(Clone, Copy)]
pub struct BindingInfo<'tcx> {
    pub llmatch: ValueRef,
    pub trmode: TransBindingMode,
    pub id: ast::NodeId,
    pub span: Span,
    pub ty: Ty<'tcx>,
}

type BindingsMap<'tcx> = FnvHashMap<ast::Ident, BindingInfo<'tcx>>;

struct ArmData<'p, 'blk, 'tcx: 'blk> {
    bodycx: Block<'blk, 'tcx>,
    arm: &'p ast::Arm,
    bindings_map: BindingsMap<'tcx>
}

/// Info about Match.
/// If all `pats` are matched then arm `data` will be executed.
/// As we proceed `bound_ptrs` are filled with pointers to values to be bound,
/// these pointers are stored in llmatch variables just before executing `data` arm.
struct Match<'a, 'p: 'a, 'blk: 'a, 'tcx: 'blk> {
    pats: Vec<&'p ast::Pat>,
    data: &'a ArmData<'p, 'blk, 'tcx>,
    bound_ptrs: Vec<(ast::Ident, ValueRef)>,
    // Thread along renamings done by the check_match::StaticInliner, so we can
    // map back to original NodeIds
    pat_renaming_map: Option<&'a FnvHashMap<(NodeId, Span), NodeId>>
}

impl<'a, 'p, 'blk, 'tcx> Repr<'tcx> for Match<'a, 'p, 'blk, 'tcx> {
    fn repr(&self, tcx: &ty::ctxt) -> String {
        if tcx.sess.verbose() {
            // for many programs, this just take too long to serialize
            self.pats.repr(tcx)
        } else {
            format!("{} pats", self.pats.len())
        }
    }
}

fn has_nested_bindings(m: &[Match], col: usize) -> bool {
    for br in m {
        match br.pats[col].node {
            ast::PatIdent(_, _, Some(_)) => return true,
            _ => ()
        }
    }
    return false;
}

fn expand_nested_bindings<'a, 'p, 'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                                              m: &[Match<'a, 'p, 'blk, 'tcx>],
                                              col: usize,
                                              val: ValueRef)
                                              -> Vec<Match<'a, 'p, 'blk, 'tcx>> {
    debug!("expand_nested_bindings(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_string(val));
    let _indenter = indenter();

    m.iter().map(|br| {
        let mut bound_ptrs = br.bound_ptrs.clone();
        let mut pat = br.pats[col];
        loop {
            pat = match pat.node {
                ast::PatIdent(_, ref path, Some(ref inner)) => {
                    bound_ptrs.push((path.node, val));
                    &**inner
                },
                _ => break
            }
        }

        let mut pats = br.pats.clone();
        pats[col] = pat;
        Match {
            pats: pats,
            data: &*br.data,
            bound_ptrs: bound_ptrs,
            pat_renaming_map: br.pat_renaming_map,
        }
    }).collect()
}

fn enter_match<'a, 'b, 'p, 'blk, 'tcx, F>(bcx: Block<'blk, 'tcx>,
                                          dm: &DefMap,
                                          m: &[Match<'a, 'p, 'blk, 'tcx>],
                                          col: usize,
                                          val: ValueRef,
                                          mut e: F)
                                          -> Vec<Match<'a, 'p, 'blk, 'tcx>> where
    F: FnMut(&[&'p ast::Pat]) -> Option<Vec<&'p ast::Pat>>,
{
    debug!("enter_match(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_string(val));
    let _indenter = indenter();

    m.iter().filter_map(|br| {
        e(&br.pats).map(|pats| {
            let this = br.pats[col];
            let mut bound_ptrs = br.bound_ptrs.clone();
            match this.node {
                ast::PatIdent(_, ref path, None) => {
                    if pat_is_binding(dm, &*this) {
                        bound_ptrs.push((path.node, val));
                    }
                }
                ast::PatVec(ref before, Some(ref slice), ref after) => {
                    if let ast::PatIdent(_, ref path, None) = slice.node {
                        let subslice_val = bind_subslice_pat(
                            bcx, this.id, val,
                            before.len(), after.len());
                        bound_ptrs.push((path.node, subslice_val));
                    }
                }
                _ => {}
            }
            Match {
                pats: pats,
                data: br.data,
                bound_ptrs: bound_ptrs,
                pat_renaming_map: br.pat_renaming_map,
            }
        })
    }).collect()
}

fn enter_default<'a, 'p, 'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                                     dm: &DefMap,
                                     m: &[Match<'a, 'p, 'blk, 'tcx>],
                                     col: usize,
                                     val: ValueRef)
                                     -> Vec<Match<'a, 'p, 'blk, 'tcx>> {
    debug!("enter_default(bcx={}, m={}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           col,
           bcx.val_to_string(val));
    let _indenter = indenter();

    // Collect all of the matches that can match against anything.
    enter_match(bcx, dm, m, col, val, |pats| {
        if pat_is_binding_or_wild(dm, &*pats[col]) {
            let mut r = pats[..col].to_vec();
            r.push_all(&pats[col + 1..]);
            Some(r)
        } else {
            None
        }
    })
}

// <pcwalton> nmatsakis: what does enter_opt do?
// <pcwalton> in trans/match
// <pcwalton> trans/match.rs is like stumbling around in a dark cave
// <nmatsakis> pcwalton: the enter family of functions adjust the set of
//             patterns as needed
// <nmatsakis> yeah, at some point I kind of achieved some level of
//             understanding
// <nmatsakis> anyhow, they adjust the patterns given that something of that
//             kind has been found
// <nmatsakis> pcwalton: ok, right, so enter_XXX() adjusts the patterns, as I
//             said
// <nmatsakis> enter_match() kind of embodies the generic code
// <nmatsakis> it is provided with a function that tests each pattern to see
//             if it might possibly apply and so forth
// <nmatsakis> so, if you have a pattern like {a: _, b: _, _} and one like _
// <nmatsakis> then _ would be expanded to (_, _)
// <nmatsakis> one spot for each of the sub-patterns
// <nmatsakis> enter_opt() is one of the more complex; it covers the fallible
//             cases
// <nmatsakis> enter_rec_or_struct() or enter_tuple() are simpler, since they
//             are infallible patterns
// <nmatsakis> so all patterns must either be records (resp. tuples) or
//             wildcards

/// The above is now outdated in that enter_match() now takes a function that
/// takes the complete row of patterns rather than just the first one.
/// Also, most of the enter_() family functions have been unified with
/// the check_match specialization step.
fn enter_opt<'a, 'p, 'blk, 'tcx>(
             bcx: Block<'blk, 'tcx>,
             _: ast::NodeId,
             dm: &DefMap,
             m: &[Match<'a, 'p, 'blk, 'tcx>],
             opt: &Opt,
             col: usize,
             variant_size: usize,
             val: ValueRef)
             -> Vec<Match<'a, 'p, 'blk, 'tcx>> {
    debug!("enter_opt(bcx={}, m={}, opt={:?}, col={}, val={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           *opt,
           col,
           bcx.val_to_string(val));
    let _indenter = indenter();

    let ctor = match opt {
        &ConstantValue(ConstantExpr(expr), _) => check_match::ConstantValue(
            const_eval::eval_const_expr(bcx.tcx(), &*expr)
        ),
        &ConstantRange(ConstantExpr(lo), ConstantExpr(hi), _) => check_match::ConstantRange(
            const_eval::eval_const_expr(bcx.tcx(), &*lo),
            const_eval::eval_const_expr(bcx.tcx(), &*hi)
        ),
        &SliceLengthEqual(n, _) =>
            check_match::Slice(n),
        &SliceLengthGreaterOrEqual(before, after, _) =>
            check_match::SliceWithSubslice(before, after),
        &Variant(_, _, def_id, _) =>
            check_match::Constructor::Variant(def_id)
    };

    let param_env = ty::empty_parameter_environment(bcx.tcx());
    let mcx = check_match::MatchCheckCtxt {
        tcx: bcx.tcx(),
        param_env: param_env,
    };
    enter_match(bcx, dm, m, col, val, |pats|
        check_match::specialize(&mcx, &pats[..], &ctor, col, variant_size)
    )
}

// Returns the options in one column of matches. An option is something that
// needs to be conditionally matched at runtime; for example, the discriminant
// on a set of enum variants or a literal.
fn get_branches<'a, 'p, 'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                                    m: &[Match<'a, 'p, 'blk, 'tcx>],
                                    col: usize)
                                    -> Vec<Opt<'p, 'tcx>> {
    let tcx = bcx.tcx();

    let mut found: Vec<Opt> = vec![];
    for br in m {
        let cur = br.pats[col];
        let debug_loc = match br.pat_renaming_map {
            Some(pat_renaming_map) => {
                match pat_renaming_map.get(&(cur.id, cur.span)) {
                    Some(&id) => DebugLoc::At(id, cur.span),
                    None => DebugLoc::At(cur.id, cur.span),
                }
            }
            None => DebugLoc::None
        };

        let opt = match cur.node {
            ast::PatLit(ref l) => {
                ConstantValue(ConstantExpr(&**l), debug_loc)
            }
            ast::PatIdent(..) | ast::PatEnum(..) | ast::PatStruct(..) => {
                // This is either an enum variant or a variable binding.
                let opt_def = tcx.def_map.borrow().get(&cur.id).map(|d| d.full_def());
                match opt_def {
                    Some(def::DefVariant(enum_id, var_id, _)) => {
                        let variant = ty::enum_variant_with_id(tcx, enum_id, var_id);
                        Variant(variant.disr_val,
                                adt::represent_node(bcx, cur.id),
                                var_id,
                                debug_loc)
                    }
                    _ => continue
                }
            }
            ast::PatRange(ref l1, ref l2) => {
                ConstantRange(ConstantExpr(&**l1), ConstantExpr(&**l2), debug_loc)
            }
            ast::PatVec(ref before, None, ref after) => {
                SliceLengthEqual(before.len() + after.len(), debug_loc)
            }
            ast::PatVec(ref before, Some(_), ref after) => {
                SliceLengthGreaterOrEqual(before.len(), after.len(), debug_loc)
            }
            _ => continue
        };

        if !found.iter().any(|x| x.eq(&opt, tcx)) {
            found.push(opt);
        }
    }
    found
}

struct ExtractedBlock<'blk, 'tcx: 'blk> {
    vals: Vec<ValueRef>,
    bcx: Block<'blk, 'tcx>,
}

fn extract_variant_args<'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                                    repr: &adt::Repr<'tcx>,
                                    disr_val: ty::Disr,
                                    val: ValueRef)
                                    -> ExtractedBlock<'blk, 'tcx> {
    let _icx = push_ctxt("match::extract_variant_args");
    let args = (0..adt::num_args(repr, disr_val)).map(|i| {
        adt::trans_field_ptr(bcx, repr, val, disr_val, i)
    }).collect();

    ExtractedBlock { vals: args, bcx: bcx }
}

/// Helper for converting from the ValueRef that we pass around in the match code, which is always
/// an lvalue, into a Datum. Eventually we should just pass around a Datum and be done with it.
fn match_datum<'tcx>(val: ValueRef, left_ty: Ty<'tcx>) -> Datum<'tcx, Lvalue> {
    Datum::new(val, left_ty, Lvalue)
}

fn bind_subslice_pat(bcx: Block,
                     pat_id: ast::NodeId,
                     val: ValueRef,
                     offset_left: usize,
                     offset_right: usize) -> ValueRef {
    let _icx = push_ctxt("match::bind_subslice_pat");
    let vec_ty = node_id_type(bcx, pat_id);
    let unit_ty = ty::sequence_element_type(bcx.tcx(), ty::type_content(vec_ty));
    let vec_datum = match_datum(val, vec_ty);
    let (base, len) = vec_datum.get_vec_base_and_len(bcx);

    let slice_begin = InBoundsGEP(bcx, base, &[C_uint(bcx.ccx(), offset_left)]);
    let slice_len_offset = C_uint(bcx.ccx(), offset_left + offset_right);
    let slice_len = Sub(bcx, len, slice_len_offset, DebugLoc::None);
    let slice_ty = ty::mk_slice(bcx.tcx(),
                                bcx.tcx().mk_region(ty::ReStatic),
                                ty::mt {ty: unit_ty, mutbl: ast::MutImmutable});
    let scratch = rvalue_scratch_datum(bcx, slice_ty, "");
    Store(bcx, slice_begin,
          GEPi(bcx, scratch.val, &[0, abi::FAT_PTR_ADDR]));
    Store(bcx, slice_len, GEPi(bcx, scratch.val, &[0, abi::FAT_PTR_EXTRA]));
    scratch.val
}

fn extract_vec_elems<'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                                 left_ty: Ty<'tcx>,
                                 before: usize,
                                 after: usize,
                                 val: ValueRef)
                                 -> ExtractedBlock<'blk, 'tcx> {
    let _icx = push_ctxt("match::extract_vec_elems");
    let vec_datum = match_datum(val, left_ty);
    let (base, len) = vec_datum.get_vec_base_and_len(bcx);
    let mut elems = vec![];
    elems.extend((0..before).map(|i| GEPi(bcx, base, &[i])));
    elems.extend((0..after).rev().map(|i| {
        InBoundsGEP(bcx, base, &[
            Sub(bcx, len, C_uint(bcx.ccx(), i + 1), DebugLoc::None)
        ])
    }));
    ExtractedBlock { vals: elems, bcx: bcx }
}

// Macro for deciding whether any of the remaining matches fit a given kind of
// pattern.  Note that, because the macro is well-typed, either ALL of the
// matches should fit that sort of pattern or NONE (however, some of the
// matches may be wildcards like _ or identifiers).
macro_rules! any_pat {
    ($m:expr, $col:expr, $pattern:pat) => (
        ($m).iter().any(|br| {
            match br.pats[$col].node {
                $pattern => true,
                _ => false
            }
        })
    )
}

fn any_uniq_pat(m: &[Match], col: usize) -> bool {
    any_pat!(m, col, ast::PatBox(_))
}

fn any_region_pat(m: &[Match], col: usize) -> bool {
    any_pat!(m, col, ast::PatRegion(..))
}

fn any_irrefutable_adt_pat(tcx: &ty::ctxt, m: &[Match], col: usize) -> bool {
    m.iter().any(|br| {
        let pat = br.pats[col];
        match pat.node {
            ast::PatTup(_) => true,
            ast::PatStruct(..) => {
                match tcx.def_map.borrow().get(&pat.id).map(|d| d.full_def()) {
                    Some(def::DefVariant(..)) => false,
                    _ => true,
                }
            }
            ast::PatEnum(..) | ast::PatIdent(_, _, None) => {
                match tcx.def_map.borrow().get(&pat.id).map(|d| d.full_def()) {
                    Some(def::DefStruct(..)) => true,
                    _ => false
                }
            }
            _ => false
        }
    })
}

/// What to do when the pattern match fails.
enum FailureHandler {
    Infallible,
    JumpToBasicBlock(BasicBlockRef),
    Unreachable
}

impl FailureHandler {
    fn is_fallible(&self) -> bool {
        match *self {
            Infallible => false,
            _ => true
        }
    }

    fn is_infallible(&self) -> bool {
        !self.is_fallible()
    }

    fn handle_fail(&self, bcx: Block) {
        match *self {
            Infallible =>
                panic!("attempted to panic in a non-panicking panic handler!"),
            JumpToBasicBlock(basic_block) =>
                Br(bcx, basic_block, DebugLoc::None),
            Unreachable =>
                build::Unreachable(bcx)
        }
    }
}

fn pick_column_to_specialize(def_map: &DefMap, m: &[Match]) -> Option<usize> {
    fn pat_score(def_map: &DefMap, pat: &ast::Pat) -> usize {
        match pat.node {
            ast::PatIdent(_, _, Some(ref inner)) => pat_score(def_map, &**inner),
            _ if pat_is_refutable(def_map, pat) => 1,
            _ => 0
        }
    }

    let column_score = |m: &[Match], col: usize| -> usize {
        let total_score = m.iter()
            .map(|row| row.pats[col])
            .map(|pat| pat_score(def_map, pat))
            .sum();

        // Irrefutable columns always go first, they'd only be duplicated in the branches.
        if total_score == 0 {
            std::usize::MAX
        } else {
            total_score
        }
    };

    let column_contains_any_nonwild_patterns = |&col: &usize| -> bool {
        m.iter().any(|row| match row.pats[col].node {
            ast::PatWild(_) => false,
            _ => true
        })
    };

    (0..m[0].pats.len())
        .filter(column_contains_any_nonwild_patterns)
        .map(|col| (col, column_score(m, col)))
        .max_by(|&(_, score)| score)
        .map(|(col, _)| col)
}

// Compiles a comparison between two things.
fn compare_values<'blk, 'tcx>(cx: Block<'blk, 'tcx>,
                              lhs: ValueRef,
                              rhs: ValueRef,
                              rhs_t: Ty<'tcx>,
                              debug_loc: DebugLoc)
                              -> Result<'blk, 'tcx> {
    fn compare_str<'blk, 'tcx>(cx: Block<'blk, 'tcx>,
                               lhs: ValueRef,
                               rhs: ValueRef,
                               rhs_t: Ty<'tcx>,
                               debug_loc: DebugLoc)
                               -> Result<'blk, 'tcx> {
        let did = langcall(cx,
                           None,
                           &format!("comparison of `{}`",
                                   cx.ty_to_string(rhs_t)),
                           StrEqFnLangItem);
        let t = ty::mk_str_slice(cx.tcx(), cx.tcx().mk_region(ty::ReStatic), ast::MutImmutable);
        // The comparison function gets the slices by value, so we have to make copies here. Even
        // if the function doesn't write through the pointer, things like lifetime intrinsics
        // require that we do this properly
        let lhs_arg = alloc_ty(cx, t, "lhs");
        let rhs_arg = alloc_ty(cx, t, "rhs");
        memcpy_ty(cx, lhs_arg, lhs, t);
        memcpy_ty(cx, rhs_arg, rhs, t);
        let res = callee::trans_lang_call(cx, did, &[lhs_arg, rhs_arg], None, debug_loc);
        call_lifetime_end(res.bcx, lhs_arg);
        call_lifetime_end(res.bcx, rhs_arg);

        res
    }

    let _icx = push_ctxt("compare_values");
    if ty::type_is_scalar(rhs_t) {
        let cmp = compare_scalar_types(cx, lhs, rhs, rhs_t, ast::BiEq, debug_loc);
        return Result::new(cx, cmp);
    }

    match rhs_t.sty {
        ty::TyRef(_, mt) => match mt.ty.sty {
            ty::TyStr => compare_str(cx, lhs, rhs, rhs_t, debug_loc),
            ty::TyArray(ty, _) => match ty.sty {
                ty::TyUint(ast::TyU8) => {
                    // NOTE: cast &[u8] and &[u8; N] to &str and abuse the str_eq lang item,
                    // which calls memcmp().
                    let pat_len = val_ty(rhs).element_type().array_length();
                    let ty_str_slice = ty::mk_str_slice(cx.tcx(),
                                                        cx.tcx().mk_region(ty::ReStatic),
                                                        ast::MutImmutable);

                    let rhs_str = alloc_ty(cx, ty_str_slice, "rhs_str");
                    Store(cx, GEPi(cx, rhs, &[0, 0]), expr::get_dataptr(cx, rhs_str));
                    Store(cx, C_uint(cx.ccx(), pat_len), expr::get_len(cx, rhs_str));

                    let lhs_str;
                    if val_ty(lhs) == val_ty(rhs) {
                        // Both the discriminant and the pattern are thin pointers
                        lhs_str = alloc_ty(cx, ty_str_slice, "lhs_str");
                        Store(cx, GEPi(cx, lhs, &[0, 0]), expr::get_dataptr(cx, lhs_str));
                        Store(cx, C_uint(cx.ccx(), pat_len), expr::get_len(cx, lhs_str));
                    }
                    else {
                        // The discriminant is a fat pointer
                        let llty_str_slice = type_of::type_of(cx.ccx(), ty_str_slice).ptr_to();
                        lhs_str = PointerCast(cx, lhs, llty_str_slice);
                    }

                    compare_str(cx, lhs_str, rhs_str, rhs_t, debug_loc)
                },
                _ => cx.sess().bug("only byte strings supported in compare_values"),
            },
            _ => cx.sess().bug("only string and byte strings supported in compare_values"),
        },
        _ => cx.sess().bug("only scalars, byte strings, and strings supported in compare_values"),
    }
}

/// For each binding in `data.bindings_map`, adds an appropriate entry into the `fcx.lllocals` map
fn insert_lllocals<'blk, 'tcx>(mut bcx: Block<'blk, 'tcx>,
                               bindings_map: &BindingsMap<'tcx>,
                               cs: Option<cleanup::ScopeId>)
                               -> Block<'blk, 'tcx> {
    for (&ident, &binding_info) in bindings_map {
        let llval = match binding_info.trmode {
            // By value mut binding for a copy type: load from the ptr
            // into the matched value and copy to our alloca
            TrByCopy(llbinding) => {
                let llval = Load(bcx, binding_info.llmatch);
                let datum = Datum::new(llval, binding_info.ty, Lvalue);
                call_lifetime_start(bcx, llbinding);
                bcx = datum.store_to(bcx, llbinding);
                if let Some(cs) = cs {
                    bcx.fcx.schedule_lifetime_end(cs, llbinding);
                }

                llbinding
            },

            // By value move bindings: load from the ptr into the matched value
            TrByMove => Load(bcx, binding_info.llmatch),

            // By ref binding: use the ptr into the matched value
            TrByRef => binding_info.llmatch
        };

        let datum = Datum::new(llval, binding_info.ty, Lvalue);
        if let Some(cs) = cs {
            bcx.fcx.schedule_lifetime_end(cs, binding_info.llmatch);
            bcx.fcx.schedule_drop_and_fill_mem(cs, llval, binding_info.ty);
        }

        debug!("binding {} to {}", binding_info.id, bcx.val_to_string(llval));
        bcx.fcx.lllocals.borrow_mut().insert(binding_info.id, datum);
        debuginfo::create_match_binding_metadata(bcx, ident.name, binding_info);
    }
    bcx
}

fn compile_guard<'a, 'p, 'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                                     guard_expr: &ast::Expr,
                                     data: &ArmData<'p, 'blk, 'tcx>,
                                     m: &[Match<'a, 'p, 'blk, 'tcx>],
                                     vals: &[ValueRef],
                                     chk: &FailureHandler,
                                     has_genuine_default: bool)
                                     -> Block<'blk, 'tcx> {
    debug!("compile_guard(bcx={}, guard_expr={}, m={}, vals={})",
           bcx.to_str(),
           bcx.expr_to_string(guard_expr),
           m.repr(bcx.tcx()),
           vec_map_to_string(vals, |v| bcx.val_to_string(*v)));
    let _indenter = indenter();

    let mut bcx = insert_lllocals(bcx, &data.bindings_map, None);

    let val = unpack_datum!(bcx, expr::trans(bcx, guard_expr));
    let val = val.to_llbool(bcx);

    for (_, &binding_info) in &data.bindings_map {
        if let TrByCopy(llbinding) = binding_info.trmode {
            call_lifetime_end(bcx, llbinding);
        }
    }

    for (_, &binding_info) in &data.bindings_map {
        bcx.fcx.lllocals.borrow_mut().remove(&binding_info.id);
    }

    with_cond(bcx, Not(bcx, val, guard_expr.debug_loc()), |bcx| {
        for (_, &binding_info) in &data.bindings_map {
            call_lifetime_end(bcx, binding_info.llmatch);
        }
        match chk {
            // If the default arm is the only one left, move on to the next
            // condition explicitly rather than (possibly) falling back to
            // the default arm.
            &JumpToBasicBlock(_) if m.len() == 1 && has_genuine_default => {
                chk.handle_fail(bcx);
            }
            _ => {
                compile_submatch(bcx, m, vals, chk, has_genuine_default);
            }
        };
        bcx
    })
}

fn compile_submatch<'a, 'p, 'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                                        m: &[Match<'a, 'p, 'blk, 'tcx>],
                                        vals: &[ValueRef],
                                        chk: &FailureHandler,
                                        has_genuine_default: bool) {
    debug!("compile_submatch(bcx={}, m={}, vals={})",
           bcx.to_str(),
           m.repr(bcx.tcx()),
           vec_map_to_string(vals, |v| bcx.val_to_string(*v)));
    let _indenter = indenter();
    let _icx = push_ctxt("match::compile_submatch");
    let mut bcx = bcx;
    if m.is_empty() {
        if chk.is_fallible() {
            chk.handle_fail(bcx);
        }
        return;
    }

    let tcx = bcx.tcx();
    let def_map = &tcx.def_map;
    match pick_column_to_specialize(def_map, m) {
        Some(col) => {
            let val = vals[col];
            if has_nested_bindings(m, col) {
                let expanded = expand_nested_bindings(bcx, m, col, val);
                compile_submatch_continue(bcx,
                                          &expanded[..],
                                          vals,
                                          chk,
                                          col,
                                          val,
                                          has_genuine_default)
            } else {
                compile_submatch_continue(bcx, m, vals, chk, col, val, has_genuine_default)
            }
        }
        None => {
            let data = &m[0].data;
            for &(ref ident, ref value_ptr) in &m[0].bound_ptrs {
                let binfo = *data.bindings_map.get(ident).unwrap();
                call_lifetime_start(bcx, binfo.llmatch);
                if binfo.trmode == TrByRef && type_is_fat_ptr(bcx.tcx(), binfo.ty) {
                    expr::copy_fat_ptr(bcx, *value_ptr, binfo.llmatch);
                }
                else {
                    Store(bcx, *value_ptr, binfo.llmatch);
                }
            }
            match data.arm.guard {
                Some(ref guard_expr) => {
                    bcx = compile_guard(bcx,
                                        &**guard_expr,
                                        m[0].data,
                                        &m[1..m.len()],
                                        vals,
                                        chk,
                                        has_genuine_default);
                }
                _ => ()
            }
            Br(bcx, data.bodycx.llbb, DebugLoc::None);
        }
    }
}

fn compile_submatch_continue<'a, 'p, 'blk, 'tcx>(mut bcx: Block<'blk, 'tcx>,
                                                 m: &[Match<'a, 'p, 'blk, 'tcx>],
                                                 vals: &[ValueRef],
                                                 chk: &FailureHandler,
                                                 col: usize,
                                                 val: ValueRef,
                                                 has_genuine_default: bool) {
    let fcx = bcx.fcx;
    let tcx = bcx.tcx();
    let dm = &tcx.def_map;

    let mut vals_left = vals[0..col].to_vec();
    vals_left.push_all(&vals[col + 1..]);
    let ccx = bcx.fcx.ccx;

    // Find a real id (we're adding placeholder wildcard patterns, but
    // each column is guaranteed to have at least one real pattern)
    let pat_id = m.iter().map(|br| br.pats[col].id)
                         .find(|&id| id != DUMMY_NODE_ID)
                         .unwrap_or(DUMMY_NODE_ID);

    let left_ty = if pat_id == DUMMY_NODE_ID {
        ty::mk_nil(tcx)
    } else {
        node_id_type(bcx, pat_id)
    };

    let mcx = check_match::MatchCheckCtxt {
        tcx: bcx.tcx(),
        param_env: ty::empty_parameter_environment(bcx.tcx()),
    };
    let adt_vals = if any_irrefutable_adt_pat(bcx.tcx(), m, col) {
        let repr = adt::represent_type(bcx.ccx(), left_ty);
        let arg_count = adt::num_args(&*repr, 0);
        let (arg_count, struct_val) = if type_is_sized(bcx.tcx(), left_ty) {
            (arg_count, val)
        } else {
            // For an unsized ADT (i.e. DST struct), we need to treat
            // the last field specially: instead of simply passing a
            // ValueRef pointing to that field, as with all the others,
            // we skip it and instead construct a 'fat ptr' below.
            (arg_count - 1, Load(bcx, expr::get_dataptr(bcx, val)))
        };
        let mut field_vals: Vec<ValueRef> = (0..arg_count).map(|ix|
            adt::trans_field_ptr(bcx, &*repr, struct_val, 0, ix)
        ).collect();

        match left_ty.sty {
            ty::TyStruct(def_id, substs) if !type_is_sized(bcx.tcx(), left_ty) => {
                // The last field is technically unsized but
                // since we can only ever match that field behind
                // a reference we construct a fat ptr here.
                let fields = ty::lookup_struct_fields(bcx.tcx(), def_id);
                let unsized_ty = fields.iter().last().map(|field| {
                    let fty = ty::lookup_field_type(bcx.tcx(), def_id, field.id, substs);
                    monomorphize::normalize_associated_type(bcx.tcx(), &fty)
                }).unwrap();
                let llty = type_of::type_of(bcx.ccx(), unsized_ty);
                let scratch = alloca_no_lifetime(bcx, llty, "__struct_field_fat_ptr");
                let data = adt::trans_field_ptr(bcx, &*repr, struct_val, 0, arg_count);
                let len = Load(bcx, expr::get_len(bcx, val));
                Store(bcx, data, expr::get_dataptr(bcx, scratch));
                Store(bcx, len, expr::get_len(bcx, scratch));
                field_vals.push(scratch);
            }
            _ => {}
        }
        Some(field_vals)
    } else if any_uniq_pat(m, col) || any_region_pat(m, col) {
        Some(vec!(Load(bcx, val)))
    } else {
        match left_ty.sty {
            ty::TyArray(_, Some(n)) => {
                let args = extract_vec_elems(bcx, left_ty, n, 0, val);
                Some(args.vals)
            }
            _ => None
        }
    };
    match adt_vals {
        Some(field_vals) => {
            let pats = enter_match(bcx, dm, m, col, val, |pats|
                check_match::specialize(&mcx, pats,
                                        &check_match::Single, col,
                                        field_vals.len())
            );
            let mut vals = field_vals;
            vals.push_all(&vals_left);
            compile_submatch(bcx, &pats, &vals, chk, has_genuine_default);
            return;
        }
        _ => ()
    }

    // Decide what kind of branch we need
    let opts = get_branches(bcx, m, col);
    debug!("options={:?}", opts);
    let mut kind = NoBranch;
    let mut test_val = val;
    debug!("test_val={}", bcx.val_to_string(test_val));
    if !opts.is_empty() {
        match opts[0] {
            ConstantValue(..) | ConstantRange(..) => {
                test_val = load_if_immediate(bcx, val, left_ty);
                kind = if ty::type_is_integral(left_ty) {
                    Switch
                } else {
                    Compare
                };
            }
            Variant(_, ref repr, _, _) => {
                let (the_kind, val_opt) = adt::trans_switch(bcx, &**repr, val);
                kind = the_kind;
                if let Some(tval) = val_opt { test_val = tval; }
            }
            SliceLengthEqual(..) | SliceLengthGreaterOrEqual(..) => {
                let (_, len) = tvec::get_base_and_len(bcx, val, left_ty);
                test_val = len;
                kind = Switch;
            }
        }
    }
    for o in &opts {
        match *o {
            ConstantRange(..) => { kind = Compare; break },
            SliceLengthGreaterOrEqual(..) => { kind = CompareSliceLength; break },
            _ => ()
        }
    }
    let else_cx = match kind {
        NoBranch | Single => bcx,
        _ => bcx.fcx.new_temp_block("match_else")
    };
    let sw = if kind == Switch {
        build::Switch(bcx, test_val, else_cx.llbb, opts.len())
    } else {
        C_int(ccx, 0) // Placeholder for when not using a switch
    };

    let defaults = enter_default(else_cx, dm, m, col, val);
    let exhaustive = chk.is_infallible() && defaults.is_empty();
    let len = opts.len();

    // Compile subtrees for each option
    for (i, opt) in opts.iter().enumerate() {
        // In some cases of range and vector pattern matching, we need to
        // override the failure case so that instead of failing, it proceeds
        // to try more matching. branch_chk, then, is the proper failure case
        // for the current conditional branch.
        let mut branch_chk = None;
        let mut opt_cx = else_cx;
        let debug_loc = opt.debug_loc();

        if !exhaustive || i + 1 < len {
            opt_cx = bcx.fcx.new_temp_block("match_case");
            match kind {
                Single => Br(bcx, opt_cx.llbb, debug_loc),
                Switch => {
                    match opt.trans(bcx) {
                        SingleResult(r) => {
                            AddCase(sw, r.val, opt_cx.llbb);
                            bcx = r.bcx;
                        }
                        _ => {
                            bcx.sess().bug(
                                "in compile_submatch, expected \
                                 opt.trans() to return a SingleResult")
                        }
                    }
                }
                Compare | CompareSliceLength => {
                    let t = if kind == Compare {
                        left_ty
                    } else {
                        tcx.types.usize // vector length
                    };
                    let Result { bcx: after_cx, val: matches } = {
                        match opt.trans(bcx) {
                            SingleResult(Result { bcx, val }) => {
                                compare_values(bcx, test_val, val, t, debug_loc)
                            }
                            RangeResult(Result { val: vbegin, .. },
                                        Result { bcx, val: vend }) => {
                                let llge = compare_scalar_types(bcx, test_val, vbegin,
                                                                t, ast::BiGe, debug_loc);
                                let llle = compare_scalar_types(bcx, test_val, vend,
                                                                t, ast::BiLe, debug_loc);
                                Result::new(bcx, And(bcx, llge, llle, DebugLoc::None))
                            }
                            LowerBound(Result { bcx, val }) => {
                                Result::new(bcx, compare_scalar_types(bcx, test_val,
                                                                      val, t, ast::BiGe,
                                                                      debug_loc))
                            }
                        }
                    };
                    bcx = fcx.new_temp_block("compare_next");

                    // If none of the sub-cases match, and the current condition
                    // is guarded or has multiple patterns, move on to the next
                    // condition, if there is any, rather than falling back to
                    // the default.
                    let guarded = m[i].data.arm.guard.is_some();
                    let multi_pats = m[i].pats.len() > 1;
                    if i + 1 < len && (guarded || multi_pats || kind == CompareSliceLength) {
                        branch_chk = Some(JumpToBasicBlock(bcx.llbb));
                    }
                    CondBr(after_cx, matches, opt_cx.llbb, bcx.llbb, debug_loc);
                }
                _ => ()
            }
        } else if kind == Compare || kind == CompareSliceLength {
            Br(bcx, else_cx.llbb, debug_loc);
        }

        let mut size = 0;
        let mut unpacked = Vec::new();
        match *opt {
            Variant(disr_val, ref repr, _, _) => {
                let ExtractedBlock {vals: argvals, bcx: new_bcx} =
                    extract_variant_args(opt_cx, &**repr, disr_val, val);
                size = argvals.len();
                unpacked = argvals;
                opt_cx = new_bcx;
            }
            SliceLengthEqual(len, _) => {
                let args = extract_vec_elems(opt_cx, left_ty, len, 0, val);
                size = args.vals.len();
                unpacked = args.vals.clone();
                opt_cx = args.bcx;
            }
            SliceLengthGreaterOrEqual(before, after, _) => {
                let args = extract_vec_elems(opt_cx, left_ty, before, after, val);
                size = args.vals.len();
                unpacked = args.vals.clone();
                opt_cx = args.bcx;
            }
            ConstantValue(..) | ConstantRange(..) => ()
        }
        let opt_ms = enter_opt(opt_cx, pat_id, dm, m, opt, col, size, val);
        let mut opt_vals = unpacked;
        opt_vals.push_all(&vals_left[..]);
        compile_submatch(opt_cx,
                         &opt_ms[..],
                         &opt_vals[..],
                         branch_chk.as_ref().unwrap_or(chk),
                         has_genuine_default);
    }

    // Compile the fall-through case, if any
    if !exhaustive && kind != Single {
        if kind == Compare || kind == CompareSliceLength {
            Br(bcx, else_cx.llbb, DebugLoc::None);
        }
        match chk {
            // If there is only one default arm left, move on to the next
            // condition explicitly rather than (eventually) falling back to
            // the last default arm.
            &JumpToBasicBlock(_) if defaults.len() == 1 && has_genuine_default => {
                chk.handle_fail(else_cx);
            }
            _ => {
                compile_submatch(else_cx,
                                 &defaults[..],
                                 &vals_left[..],
                                 chk,
                                 has_genuine_default);
            }
        }
    }
}

pub fn trans_match<'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                               match_expr: &ast::Expr,
                               discr_expr: &ast::Expr,
                               arms: &[ast::Arm],
                               dest: Dest)
                               -> Block<'blk, 'tcx> {
    let _icx = push_ctxt("match::trans_match");
    trans_match_inner(bcx, match_expr.id, discr_expr, arms, dest)
}

/// Checks whether the binding in `discr` is assigned to anywhere in the expression `body`
fn is_discr_reassigned(bcx: Block, discr: &ast::Expr, body: &ast::Expr) -> bool {
    let (vid, field) = match discr.node {
        ast::ExprPath(..) => match bcx.def(discr.id) {
            def::DefLocal(vid) | def::DefUpvar(vid, _) => (vid, None),
            _ => return false
        },
        ast::ExprField(ref base, field) => {
            let vid = match bcx.tcx().def_map.borrow().get(&base.id).map(|d| d.full_def()) {
                Some(def::DefLocal(vid)) | Some(def::DefUpvar(vid, _)) => vid,
                _ => return false
            };
            (vid, Some(mc::NamedField(field.node.name)))
        },
        ast::ExprTupField(ref base, field) => {
            let vid = match bcx.tcx().def_map.borrow().get(&base.id).map(|d| d.full_def()) {
                Some(def::DefLocal(vid)) | Some(def::DefUpvar(vid, _)) => vid,
                _ => return false
            };
            (vid, Some(mc::PositionalField(field.node)))
        },
        _ => return false
    };

    let mut rc = ReassignmentChecker {
        node: vid,
        field: field,
        reassigned: false
    };
    {
        let mut visitor = euv::ExprUseVisitor::new(&mut rc, bcx);
        visitor.walk_expr(body);
    }
    rc.reassigned
}

struct ReassignmentChecker {
    node: ast::NodeId,
    field: Option<mc::FieldName>,
    reassigned: bool
}

// Determine if the expression we're matching on is reassigned to within
// the body of the match's arm.
// We only care for the `mutate` callback since this check only matters
// for cases where the matched value is moved.
impl<'tcx> euv::Delegate<'tcx> for ReassignmentChecker {
    fn consume(&mut self, _: ast::NodeId, _: Span, _: mc::cmt, _: euv::ConsumeMode) {}
    fn matched_pat(&mut self, _: &ast::Pat, _: mc::cmt, _: euv::MatchMode) {}
    fn consume_pat(&mut self, _: &ast::Pat, _: mc::cmt, _: euv::ConsumeMode) {}
    fn borrow(&mut self, _: ast::NodeId, _: Span, _: mc::cmt, _: ty::Region,
              _: ty::BorrowKind, _: euv::LoanCause) {}
    fn decl_without_init(&mut self, _: ast::NodeId, _: Span) {}

    fn mutate(&mut self, _: ast::NodeId, _: Span, cmt: mc::cmt, _: euv::MutateMode) {
        match cmt.cat {
            mc::cat_upvar(mc::Upvar { id: ty::UpvarId { var_id: vid, .. }, .. }) |
            mc::cat_local(vid) => self.reassigned |= self.node == vid,
            mc::cat_interior(ref base_cmt, mc::InteriorField(field)) => {
                match base_cmt.cat {
                    mc::cat_upvar(mc::Upvar { id: ty::UpvarId { var_id: vid, .. }, .. }) |
                    mc::cat_local(vid) => {
                        self.reassigned |= self.node == vid && Some(field) == self.field
                    },
                    _ => {}
                }
            },
            _ => {}
        }
    }
}

fn create_bindings_map<'blk, 'tcx>(bcx: Block<'blk, 'tcx>, pat: &ast::Pat,
                                   discr: &ast::Expr, body: &ast::Expr)
                                   -> BindingsMap<'tcx> {
    // Create the bindings map, which is a mapping from each binding name
    // to an alloca() that will be the value for that local variable.
    // Note that we use the names because each binding will have many ids
    // from the various alternatives.
    let ccx = bcx.ccx();
    let tcx = bcx.tcx();
    let reassigned = is_discr_reassigned(bcx, discr, body);
    let mut bindings_map = FnvHashMap();
    pat_bindings(&tcx.def_map, &*pat, |bm, p_id, span, path1| {
        let ident = path1.node;
        let name = ident.name;
        let variable_ty = node_id_type(bcx, p_id);
        let llvariable_ty = type_of::type_of(ccx, variable_ty);
        let tcx = bcx.tcx();
        let param_env = ty::empty_parameter_environment(tcx);

        let llmatch;
        let trmode;
        match bm {
            ast::BindByValue(_)
                if !ty::type_moves_by_default(&param_env, span, variable_ty) || reassigned =>
            {
                llmatch = alloca_no_lifetime(bcx,
                                 llvariable_ty.ptr_to(),
                                 "__llmatch");
                trmode = TrByCopy(alloca_no_lifetime(bcx,
                                         llvariable_ty,
                                         &bcx.name(name)));
            }
            ast::BindByValue(_) => {
                // in this case, the final type of the variable will be T,
                // but during matching we need to store a *T as explained
                // above
                llmatch = alloca_no_lifetime(bcx,
                                 llvariable_ty.ptr_to(),
                                 &bcx.name(name));
                trmode = TrByMove;
            }
            ast::BindByRef(_) => {
                llmatch = alloca_no_lifetime(bcx,
                                 llvariable_ty,
                                 &bcx.name(name));
                trmode = TrByRef;
            }
        };
        bindings_map.insert(ident, BindingInfo {
            llmatch: llmatch,
            trmode: trmode,
            id: p_id,
            span: span,
            ty: variable_ty
        });
    });
    return bindings_map;
}

fn trans_match_inner<'blk, 'tcx>(scope_cx: Block<'blk, 'tcx>,
                                 match_id: ast::NodeId,
                                 discr_expr: &ast::Expr,
                                 arms: &[ast::Arm],
                                 dest: Dest) -> Block<'blk, 'tcx> {
    let _icx = push_ctxt("match::trans_match_inner");
    let fcx = scope_cx.fcx;
    let mut bcx = scope_cx;
    let tcx = bcx.tcx();

    let discr_datum = unpack_datum!(bcx, expr::trans_to_lvalue(bcx, discr_expr,
                                                               "match"));
    if bcx.unreachable.get() {
        return bcx;
    }

    let t = node_id_type(bcx, discr_expr.id);
    let chk = if ty::type_is_empty(tcx, t) {
        Unreachable
    } else {
        Infallible
    };

    let arm_datas: Vec<ArmData> = arms.iter().map(|arm| ArmData {
        bodycx: fcx.new_id_block("case_body", arm.body.id),
        arm: arm,
        bindings_map: create_bindings_map(bcx, &*arm.pats[0], discr_expr, &*arm.body)
    }).collect();

    let mut pat_renaming_map = if scope_cx.sess().opts.debuginfo != NoDebugInfo {
        Some(FnvHashMap())
    } else {
        None
    };

    let arm_pats: Vec<Vec<P<ast::Pat>>> = {
        let mut static_inliner = StaticInliner::new(scope_cx.tcx(),
                                                    pat_renaming_map.as_mut());
        arm_datas.iter().map(|arm_data| {
            arm_data.arm.pats.iter().map(|p| static_inliner.fold_pat((*p).clone())).collect()
        }).collect()
    };

    let mut matches = Vec::new();
    for (arm_data, pats) in arm_datas.iter().zip(&arm_pats) {
        matches.extend(pats.iter().map(|p| Match {
            pats: vec![&**p],
            data: arm_data,
            bound_ptrs: Vec::new(),
            pat_renaming_map: pat_renaming_map.as_ref()
        }));
    }

    // `compile_submatch` works one column of arm patterns a time and
    // then peels that column off. So as we progress, it may become
    // impossible to tell whether we have a genuine default arm, i.e.
    // `_ => foo` or not. Sometimes it is important to know that in order
    // to decide whether moving on to the next condition or falling back
    // to the default arm.
    let has_default = arms.last().map_or(false, |arm| {
        arm.pats.len() == 1
        && arm.pats.last().unwrap().node == ast::PatWild(ast::PatWildSingle)
    });

    compile_submatch(bcx, &matches[..], &[discr_datum.val], &chk, has_default);

    let mut arm_cxs = Vec::new();
    for arm_data in &arm_datas {
        let mut bcx = arm_data.bodycx;

        // insert bindings into the lllocals map and add cleanups
        let cs = fcx.push_custom_cleanup_scope();
        bcx = insert_lllocals(bcx, &arm_data.bindings_map, Some(cleanup::CustomScope(cs)));
        bcx = expr::trans_into(bcx, &*arm_data.arm.body, dest);
        bcx = fcx.pop_and_trans_custom_cleanup_scope(bcx, cs);
        arm_cxs.push(bcx);
    }

    bcx = scope_cx.fcx.join_blocks(match_id, &arm_cxs[..]);
    return bcx;
}

/// Generates code for a local variable declaration like `let <pat>;` or `let <pat> =
/// <opt_init_expr>`.
pub fn store_local<'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                               local: &ast::Local)
                               -> Block<'blk, 'tcx> {
    let _icx = push_ctxt("match::store_local");
    let mut bcx = bcx;
    let tcx = bcx.tcx();
    let pat = &*local.pat;

    fn create_dummy_locals<'blk, 'tcx>(mut bcx: Block<'blk, 'tcx>,
                                       pat: &ast::Pat)
                                       -> Block<'blk, 'tcx> {
        let _icx = push_ctxt("create_dummy_locals");
        // create dummy memory for the variables if we have no
        // value to store into them immediately
        let tcx = bcx.tcx();
        pat_bindings(&tcx.def_map, pat, |_, p_id, _, path1| {
            let scope = cleanup::var_scope(tcx, p_id);
            bcx = mk_binding_alloca(
                bcx, p_id, path1.node.name, scope, (),
                |(), bcx, llval, ty| { drop_done_fill_mem(bcx, llval, ty); bcx });
        });
        bcx
    }

    match local.init {
        Some(ref init_expr) => {
            // Optimize the "let x = expr" case. This just writes
            // the result of evaluating `expr` directly into the alloca
            // for `x`. Often the general path results in similar or the
            // same code post-optimization, but not always. In particular,
            // in unsafe code, you can have expressions like
            //
            //    let x = intrinsics::uninit();
            //
            // In such cases, the more general path is unsafe, because
            // it assumes it is matching against a valid value.
            match simple_identifier(&*pat) {
                Some(ident) => {
                    let var_scope = cleanup::var_scope(tcx, local.id);
                    return mk_binding_alloca(
                        bcx, pat.id, ident.name, var_scope, (),
                        |(), bcx, v, _| expr::trans_into(bcx, &**init_expr,
                                                         expr::SaveIn(v)));
                }

                None => {}
            }

            // General path.
            let init_datum =
                unpack_datum!(bcx, expr::trans_to_lvalue(bcx, &**init_expr, "let"));
            if bcx.sess().asm_comments() {
                add_comment(bcx, "creating zeroable ref llval");
            }
            let var_scope = cleanup::var_scope(tcx, local.id);
            bind_irrefutable_pat(bcx, pat, init_datum.val, var_scope)
        }
        None => {
            create_dummy_locals(bcx, pat)
        }
    }
}

/// Generates code for argument patterns like `fn foo(<pat>: T)`.
/// Creates entries in the `lllocals` map for each of the bindings
/// in `pat`.
///
/// # Arguments
///
/// - `pat` is the argument pattern
/// - `llval` is a pointer to the argument value (in other words,
///   if the argument type is `T`, then `llval` is a `T*`). In some
///   cases, this code may zero out the memory `llval` points at.
pub fn store_arg<'blk, 'tcx>(mut bcx: Block<'blk, 'tcx>,
                             pat: &ast::Pat,
                             arg: Datum<'tcx, Rvalue>,
                             arg_scope: cleanup::ScopeId)
                             -> Block<'blk, 'tcx> {
    let _icx = push_ctxt("match::store_arg");

    match simple_identifier(&*pat) {
        Some(ident) => {
            // Generate nicer LLVM for the common case of fn a pattern
            // like `x: T`
            let arg_ty = node_id_type(bcx, pat.id);
            if type_of::arg_is_indirect(bcx.ccx(), arg_ty)
                && bcx.sess().opts.debuginfo != FullDebugInfo {
                // Don't copy an indirect argument to an alloca, the caller
                // already put it in a temporary alloca and gave it up, unless
                // we emit extra-debug-info, which requires local allocas :(.
                let arg_val = arg.add_clean(bcx.fcx, arg_scope);
                bcx.fcx.lllocals.borrow_mut()
                   .insert(pat.id, Datum::new(arg_val, arg_ty, Lvalue));
                bcx
            } else {
                mk_binding_alloca(
                    bcx, pat.id, ident.name, arg_scope, arg,
                    |arg, bcx, llval, _| arg.store_to(bcx, llval))
            }
        }

        None => {
            // General path. Copy out the values that are used in the
            // pattern.
            let arg = unpack_datum!(
                bcx, arg.to_lvalue_datum_in_scope(bcx, "__arg", arg_scope));
            bind_irrefutable_pat(bcx, pat, arg.val, arg_scope)
        }
    }
}

fn mk_binding_alloca<'blk, 'tcx, A, F>(bcx: Block<'blk, 'tcx>,
                                       p_id: ast::NodeId,
                                       name: ast::Name,
                                       cleanup_scope: cleanup::ScopeId,
                                       arg: A,
                                       populate: F)
                                       -> Block<'blk, 'tcx> where
    F: FnOnce(A, Block<'blk, 'tcx>, ValueRef, Ty<'tcx>) -> Block<'blk, 'tcx>,
{
    let var_ty = node_id_type(bcx, p_id);

    // Allocate memory on stack for the binding.
    let llval = alloc_ty(bcx, var_ty, &bcx.name(name));

    // Subtle: be sure that we *populate* the memory *before*
    // we schedule the cleanup.
    let bcx = populate(arg, bcx, llval, var_ty);
    bcx.fcx.schedule_lifetime_end(cleanup_scope, llval);
    bcx.fcx.schedule_drop_mem(cleanup_scope, llval, var_ty);

    // Now that memory is initialized and has cleanup scheduled,
    // create the datum and insert into the local variable map.
    let datum = Datum::new(llval, var_ty, Lvalue);
    bcx.fcx.lllocals.borrow_mut().insert(p_id, datum);
    bcx
}

/// A simple version of the pattern matching code that only handles
/// irrefutable patterns. This is used in let/argument patterns,
/// not in match statements. Unifying this code with the code above
/// sounds nice, but in practice it produces very inefficient code,
/// since the match code is so much more general. In most cases,
/// LLVM is able to optimize the code, but it causes longer compile
/// times and makes the generated code nigh impossible to read.
///
/// # Arguments
/// - bcx: starting basic block context
/// - pat: the irrefutable pattern being matched.
/// - val: the value being matched -- must be an lvalue (by ref, with cleanup)
fn bind_irrefutable_pat<'blk, 'tcx>(bcx: Block<'blk, 'tcx>,
                                    pat: &ast::Pat,
                                    val: ValueRef,
                                    cleanup_scope: cleanup::ScopeId)
                                    -> Block<'blk, 'tcx> {
    debug!("bind_irrefutable_pat(bcx={}, pat={})",
           bcx.to_str(),
           pat.repr(bcx.tcx()));

    if bcx.sess().asm_comments() {
        add_comment(bcx, &format!("bind_irrefutable_pat(pat={})",
                                 pat.repr(bcx.tcx())));
    }

    let _indenter = indenter();

    let _icx = push_ctxt("match::bind_irrefutable_pat");
    let mut bcx = bcx;
    let tcx = bcx.tcx();
    let ccx = bcx.ccx();
    match pat.node {
        ast::PatIdent(pat_binding_mode, ref path1, ref inner) => {
            if pat_is_binding(&tcx.def_map, &*pat) {
                // Allocate the stack slot where the value of this
                // binding will live and place it into the appropriate
                // map.
                bcx = mk_binding_alloca(
                    bcx, pat.id, path1.node.name, cleanup_scope, (),
                    |(), bcx, llval, ty| {
                        match pat_binding_mode {
                            ast::BindByValue(_) => {
                                // By value binding: move the value that `val`
                                // points at into the binding's stack slot.
                                let d = Datum::new(val, ty, Lvalue);
                                d.store_to(bcx, llval)
                            }

                            ast::BindByRef(_) => {
                                // By ref binding: the value of the variable
                                // is the pointer `val` itself or fat pointer referenced by `val`
                                if type_is_fat_ptr(bcx.tcx(), ty) {
                                    expr::copy_fat_ptr(bcx, val, llval);
                                }
                                else {
                                    Store(bcx, val, llval);
                                }

                                bcx
                            }
                        }
                    });
            }

            if let Some(ref inner_pat) = *inner {
                bcx = bind_irrefutable_pat(bcx, &**inner_pat, val, cleanup_scope);
            }
        }
        ast::PatEnum(_, ref sub_pats) => {
            let opt_def = bcx.tcx().def_map.borrow().get(&pat.id).map(|d| d.full_def());
            match opt_def {
                Some(def::DefVariant(enum_id, var_id, _)) => {
                    let repr = adt::represent_node(bcx, pat.id);
                    let vinfo = ty::enum_variant_with_id(ccx.tcx(),
                                                         enum_id,
                                                         var_id);
                    let args = extract_variant_args(bcx,
                                                    &*repr,
                                                    vinfo.disr_val,
                                                    val);
                    if let Some(ref sub_pat) = *sub_pats {
                        for (i, &argval) in args.vals.iter().enumerate() {
                            bcx = bind_irrefutable_pat(bcx, &*sub_pat[i],
                                                       argval, cleanup_scope);
                        }
                    }
                }
                Some(def::DefStruct(..)) => {
                    match *sub_pats {
                        None => {
                            // This is a unit-like struct. Nothing to do here.
                        }
                        Some(ref elems) => {
                            // This is the tuple struct case.
                            let repr = adt::represent_node(bcx, pat.id);
                            for (i, elem) in elems.iter().enumerate() {
                                let fldptr = adt::trans_field_ptr(bcx, &*repr,
                                                                  val, 0, i);
                                bcx = bind_irrefutable_pat(bcx, &**elem,
                                                           fldptr, cleanup_scope);
                            }
                        }
                    }
                }
                _ => {
                    // Nothing to do here.
                }
            }
        }
        ast::PatStruct(_, ref fields, _) => {
            let tcx = bcx.tcx();
            let pat_ty = node_id_type(bcx, pat.id);
            let pat_repr = adt::represent_type(bcx.ccx(), pat_ty);
            expr::with_field_tys(tcx, pat_ty, Some(pat.id), |discr, field_tys| {
                for f in fields {
                    let ix = ty::field_idx_strict(tcx, f.node.ident.name, field_tys);
                    let fldptr = adt::trans_field_ptr(bcx, &*pat_repr, val,
                                                      discr, ix);
                    bcx = bind_irrefutable_pat(bcx, &*f.node.pat, fldptr, cleanup_scope);
                }
            })
        }
        ast::PatTup(ref elems) => {
            let repr = adt::represent_node(bcx, pat.id);
            for (i, elem) in elems.iter().enumerate() {
                let fldptr = adt::trans_field_ptr(bcx, &*repr, val, 0, i);
                bcx = bind_irrefutable_pat(bcx, &**elem, fldptr, cleanup_scope);
            }
        }
        ast::PatBox(ref inner) => {
            let llbox = Load(bcx, val);
            bcx = bind_irrefutable_pat(bcx, &**inner, llbox, cleanup_scope);
        }
        ast::PatRegion(ref inner, _) => {
            let loaded_val = Load(bcx, val);
            bcx = bind_irrefutable_pat(bcx, &**inner, loaded_val, cleanup_scope);
        }
        ast::PatVec(ref before, ref slice, ref after) => {
            let pat_ty = node_id_type(bcx, pat.id);
            let mut extracted = extract_vec_elems(bcx, pat_ty, before.len(), after.len(), val);
            match slice {
                &Some(_) => {
                    extracted.vals.insert(
                        before.len(),
                        bind_subslice_pat(bcx, pat.id, val, before.len(), after.len())
                    );
                }
                &None => ()
            }
            bcx = before
                .iter()
                .chain(slice.iter())
                .chain(after.iter())
                .zip(extracted.vals)
                .fold(bcx, |bcx, (inner, elem)|
                    bind_irrefutable_pat(bcx, &**inner, elem, cleanup_scope)
                );
        }
        ast::PatMac(..) => {
            bcx.sess().span_bug(pat.span, "unexpanded macro");
        }
        ast::PatQPath(..) | ast::PatWild(_) | ast::PatLit(_) |
        ast::PatRange(_, _) => ()
    }
    return bcx;
}
