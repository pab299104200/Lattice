//! Phase H1 of `docs/plans/2026-08-13-health-engine.md`: the historical
//! backtest harness.
//!
//! # Why this exists
//!
//! Nothing in Lattice may claim that a fact predicts defects until a replay of
//! real history says it does. This module is that replay. It splits a
//! repository's first-parent history at a cut-point commit `T`, produces the
//! H2 fact families from the repository *as it stood at `T`*, labels each file
//! by whether a fix-shaped commit touched it in the horizon *after* `T`, and
//! measures how well each fact family ranks the files that were about to break.
//! Its committed report is the only authority permitted to set H3's weights.
//!
//! # Module layout
//!
//! * [`metrics`] — pure precision/recall/PR-AUC/ROC-AUC/calibration arithmetic.
//! * [`labels`] — pure horizon selection and defect labeling over commit
//!   records.
//! * [`features`] — pure assembly of per-file feature vectors from the H2 fact
//!   snapshots, rank normalization, and the family definitions being compared.
//! * [`audit`] — pure H1.2 label-quality audit of `looks_like_bug_fix`.
//! * [`replay`] — the only impure part: a `git2` adapter that reads history and
//!   trees at a cut point. It performs no working-tree mutation of any kind.
//! * [`report`] — deterministic markdown and JSON rendering.
//!
//! # The leakage rule
//!
//! The harness's single load-bearing correctness property is that **no fact
//! mined for the pre-`T` window may depend on any commit at or after `T`**.
//! Two structural choices enforce it rather than merely testing for it:
//!
//! 1. Pre-`T` history is collected by a revwalk pushed at `T`, which by
//!    construction visits only `T`'s ancestors. There is no filter that could
//!    be written incorrectly.
//! 2. Repository content is read from the tree object at `T` via `git2`, never
//!    from the working tree or from `HEAD`. A later commit's blobs are not
//!    reachable from that tree.
//!
//! `replay_tests` additionally proves the property empirically by building a
//! synthetic repository in which leakage would flip a label, and asserting the
//! facts at `T` equal the facts of a repository truncated at `T`.

pub mod features;
pub mod labels;
pub mod metrics;
pub mod replay;

/// Version of the backtest harness itself.
///
/// Bumped whenever a change would alter the numbers a rerun produces, so a
/// committed report can always be tied to the code that made it.
pub const BACKTEST_HARNESS_VERSION: u32 = 1;
