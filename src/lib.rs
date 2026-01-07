#![feature(isolate_most_least_significant_one)]
#![feature(atomic_internals)]
mod alloc;
mod cache;
mod hash;
mod node;
mod set;
mod tobdd;

use std::io::{Read as IoRead, Result as IoResult, Write as IoWrite};

pub type Bdd = usize;
pub use tobdd::Manager;

use crate::node::Idx;

/// Supported BDD operations.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BddOpType {
    Not,
    And,
    Or,
    Comp, // Complement: X \ Y
    QuantExist,
    QuantForall,
}

/// BDD manager interface.
pub trait BddManager<I: Idx>: BddOp<I> {
    fn init(table_size: usize, cache_size: usize, var_num: usize) -> Self;
    fn get_var(&self, var: I) -> I;
    fn get_nvar(&self, var: I) -> I;
    fn get_true(&self) -> I;
    fn get_false(&self) -> I;
    fn is_true(&self, bdd: I) -> bool {
        bdd == self.get_true()
    }
    fn is_false(&self, bdd: I) -> bool {
        bdd == self.get_false()
    }
    fn get_node_num(&self) -> I;
    fn ref_bdd(&self, bdd: I);
    fn deref_bdd(&self, bdd: I);
    fn gc(&self) -> usize;
}

/// Apply BDD operations.
pub trait BddOp<I: Idx> {
    // propositional logic operations
    fn not(&self, bdd: I) -> I;
    fn and(&self, lhs: I, rhs: I) -> I;
    fn or(&self, lhs: I, rhs: I) -> I;
    fn comp(&self, lhs: I, rhs: I) -> I;

    // first-order logic operations
    fn exist(&self, bdd: I, cube: I) -> I;
    fn forall(&self, bdd: I, cube: I) -> I;
}

/// Serialize/Deserialize BDD between different instances.
pub trait BddIO<I: Idx, W: IoWrite, R: IoRead> {
    /// Serialize to writer.
    fn serialize(&self, bdd: I, writer: &mut W) -> IoResult<()>;
    /// Deserialize from reader, the returned BDD is reference counted, no need to ref it.
    fn deserialize(&self, reader: &mut R) -> IoResult<I>;
}

/// Print BDD in human-understandable format.
pub trait PrintSet<I: Idx, W: IoWrite> {
    // print the BDD in the format of unions of cubes to help debugging. The writer is usually a
    // string or stdout.
    fn print(&self, bdd: I, writer: &mut W) -> IoResult<()>;
}
