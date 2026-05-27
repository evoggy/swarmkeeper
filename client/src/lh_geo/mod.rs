// Lighthouse geometry solver: an in-progress port whose full API surface is not
// yet wired up. Suppress dead-code warnings module-wide rather than trimming the
// solver API while it is being built out.
#![allow(dead_code)]

pub mod types;
pub mod bs_vector;
mod ippe;
pub mod ippe_cf;
pub mod crossing_beam;
pub mod sample;
pub mod solution;
pub mod initial_estimator;
pub mod geometry_solver;
pub mod system_aligner;
pub mod system_scaler;
pub mod estimation_manager;
pub mod container;
