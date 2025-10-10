// bundler/src/fee_oracles/mod.rs

//! Fee oracle implementations (dynamic, percentile/EMA + hysteresis, etc).
//! Add new strategies here and re-export them in lib.rs.

pub mod recent;
