//! Execution drivers that launch a libkrun microVM: repository execution
//! ([`local`]), the project verifier probe ([`probe`]) and sealed text
//! execution ([`text`], [`text_v2`], [`text_opencode`]). The request shapes,
//! evidence, verifier and frame parsing they share live in the parent module and
//! build without the libkrun feature.

pub(crate) mod local;
pub(crate) mod probe;
pub(crate) mod text;
pub(crate) mod text_opencode;
pub(crate) mod text_v2;
