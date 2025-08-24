// /home/reda-37/Rollup/bundler/src/lib.rs

// Re‑export the modules so the bin can `use bundler::...`
pub mod dag;   // expects /bundler/src/dag/mod.rs  (you already have)
pub mod occ;   // expects /bundler/src/occ/mod.rs  (you already have)
pub mod runtime; // expects /bundler/src/runtime/mod.rs (add if you haven’t yet)
