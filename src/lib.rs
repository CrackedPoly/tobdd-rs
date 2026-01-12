#![feature(isolate_most_least_significant_one)]
mod cache;
mod spin;
mod hash;
mod node;
mod set;
mod tobdd;

use std::io::{Read as IoRead, Result as IoResult, Write as IoWrite};

pub type Bdd = *mut Node;
pub use tobdd::Manager;

use crate::node::Node;

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
pub trait BddManager: BddOp {
    fn init(table_size: usize, cache_size: usize, var_num: usize) -> Self;
    fn get_var(&self, var: usize) -> Bdd;
    fn get_nvar(&self, var: usize) -> Bdd;
    fn get_true(&self) -> Bdd;
    fn get_false(&self) -> Bdd;
    fn is_true(&self, bdd: Bdd) -> bool {
        bdd == self.get_true()
    }
    fn is_false(&self, bdd: Bdd) -> bool {
        bdd == self.get_false()
    }
    fn get_node_num(&self) -> usize;
    fn deref_bdd(&self, bdd: Bdd);
    fn gc(&self) -> usize;
}

/// Apply BDD operations.
/// BDD returned by methods of this trait is automatically referenced, manually dereference it if
/// not used anymore.
pub trait BddOp {
    // propositional logic operations
    fn not(&self, bdd: Bdd) -> Bdd;
    fn and(&self, lhs: Bdd, rhs: Bdd) -> Bdd;
    fn or(&self, lhs: Bdd, rhs: Bdd) -> Bdd;
    fn comp(&self, lhs: Bdd, rhs: Bdd) -> Bdd;

    // first-order logic operations
    fn exist(&self, bdd: Bdd, cube: Bdd) -> Bdd;
    fn forall(&self, bdd: Bdd, cube: Bdd) -> Bdd;
}

/// Serialize/Deserialize BDD between different instances.
pub trait BddIO<W: IoWrite, R: IoRead> {
    /// Serialize to writer.
    fn serialize(&self, bdd: Bdd, writer: &mut W) -> IoResult<()>;
    /// Deserialize from reader, the returned BDD is reference counted, no need to ref it.
    fn deserialize(&self, reader: &mut R) -> IoResult<Bdd>;
}

/// Print BDD in human-understandable format.
pub trait PrintSet<W: IoWrite> {
    // print the BDD in the format of unions of cubes to help debugging. The writer is usually a
    // string or stdout.
    fn print(&self, bdd: Bdd, writer: &mut W) -> IoResult<()>;
}
