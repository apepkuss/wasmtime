
// ====------------------------------------------------------------------------------------==== //
//
// Cretonne code generation library.
//
// ====------------------------------------------------------------------------------------==== //

pub use verifier::verify_function;
pub use write::write_function;
pub use legalizer::legalize_function;

pub const VERSION: &'static str = env!("CARGO_PKG_VERSION");

pub mod ir;
pub mod isa;
pub mod cfg;
pub mod dominator_tree;
pub mod entity_map;
pub mod settings;
pub mod verifier;

mod write;
mod constant_hash;
mod predicates;
mod legalizer;

#[cfg(test)]
pub mod test_utils;
