// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! Operator-driven cartridge tiering policy engine (issue #3).
//!
//! Tiering lets an operator express cartridge-placement rules — e.g.
//! "WORM cartridges live on the compliance backend, `ARCH*` barcodes
//! live on cold storage" — as declarative policy, then apply them on
//! demand via `system tiering {plan, run-now}`. The data movement is
//! the existing `cartridge migrate` primitive
//! ([`crate::cartridge_migrate`]); this module is only the policy
//! schema and the pure decision engine that turns
//! `(cartridge facts, policies)` into a list of proposed migrations.
//!
//! The engine does **no I/O**: the daemon builds [`CartridgeFacts`]
//! from each cartridge manifest plus a legal-hold read, calls
//! [`plan_moves`], and turns each [`PlannedMove`] into a migrate job.
//! Keeping it pure makes it exhaustively unit-testable and leaves it
//! liftable into a shared crate if VSA ever grows a volume-migrate
//! primitive (see the issue's Deferred section).
//!
//! Two invariants are enforced here rather than left to callers:
//!   - **Legal-held cartridges are never tiered.** Legal hold is
//!     cloud-native-only with no transfer logic; relocating a held
//!     cartridge would silently drop the hold. Held cartridges are
//!     excluded outright, no per-policy opt-in.
//!   - **First match wins.** Policies are an ordered rule list; the
//!     first policy whose predicates match a cartridge decides its
//!     placement, even when that decision is a no-op (already on the
//!     target). Later policies do not get a second say.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The `tiering:` config block. Empty by default — tiering is opt-in,
/// and a daemon with no policies plans no moves.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TieringConfig {
    /// Ordered list of placement rules. Evaluated top-to-bottom per
    /// cartridge; the first matching policy wins.
    #[serde(default)]
    pub policies: Vec<TieringPolicy>,
}

/// One tiering policy: a predicate set (ANDed together) plus the
/// backend a matching cartridge should be migrated to.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TieringPolicy {
    /// Predicates that must all match for this policy to apply.
    #[serde(default)]
    pub predicates: TieringPredicates,
    /// Backend name a matching cartridge is migrated to. Must name a
    /// backend defined under `storage.backends` — enforced by
    /// [`validate_policies`] at config-parse time.
    pub migrate_to: String,
}

/// Predicate set for a tiering policy. Every *set* field must match;
/// unset fields are ignored. An all-unset set matches every cartridge
/// (a catch-all) — [`validate_policies`] rejects that to guard against
/// an accidental "migrate everything" rule.
///
/// Only predicates that are cheap (O(1) from the manifest/inventory)
/// and survive a DR/cloud restore are supported. `age_days_since_last_write`
/// is deliberately absent: it would have to come from the local-only
/// `lru.idx`, which is zero-filled on restore, so it silently misfires
/// (see the issue's Deferred section).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TieringPredicates {
    /// Match cartridges whose barcode starts with this string.
    #[serde(default)]
    pub barcode_prefix: Option<String>,
    /// Match cartridges of exactly this LTO generation.
    #[serde(default)]
    pub lto_generation: Option<u8>,
    /// Match cartridges with this WORM flag.
    #[serde(default)]
    pub worm: Option<bool>,
}

/// The facts about one cartridge the engine evaluates policies against.
/// The daemon fills this from the cartridge manifest (`label`,
/// `lto_generation`, `worm`, `backend`) plus a legal-hold read; the
/// engine itself does no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CartridgeFacts {
    /// Cartridge barcode (the manifest `label`).
    pub barcode: String,
    /// Per-cartridge LTO generation (the manifest `lto_generation`).
    pub lto_generation: u8,
    /// Sticky WORM flag (the manifest `worm`).
    pub worm: bool,
    /// Backend the cartridge is currently bound to (the manifest
    /// `backend`).
    pub current_backend: String,
    /// Cloud-native legal-hold state. A held cartridge is excluded
    /// from tiering outright.
    pub legal_held: bool,
}

/// A single proposed migration: move `barcode` from `source_backend`
/// to `target_backend`. The engine emits these; the daemon turns each
/// into a `cartridge migrate` job (byte counts are attached later, by
/// the planner, from `chunks.idx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMove {
    /// Barcode of the cartridge to move.
    pub barcode: String,
    /// Backend the cartridge is currently on.
    pub source_backend: String,
    /// Backend the matched policy wants it on.
    pub target_backend: String,
}

/// The structured result of a `system tiering plan` run. Serialized
/// into the job's terminal `Result` event by the daemon and decoded by
/// the CLI for rendering — the cross-process contract, mirroring
/// [`crate::verify::VerifyReport`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TieringPlanReport {
    /// Number of policies evaluated.
    pub policies: usize,
    /// Number of cartridges examined on disk.
    pub cartridges_scanned: usize,
    /// Proposed migrations (matched a policy, off-target, not held).
    pub moves: Vec<PlannedMoveReport>,
    /// Barcodes that matched a policy but are excluded because they
    /// are under legal hold.
    pub excluded_legal_hold: Vec<String>,
    /// Cartridges that could not be evaluated (unreadable manifest,
    /// failed legal-hold read, backend unavailable), with the reason.
    pub skipped: Vec<SkippedCartridge>,
}

/// One proposed migration in a [`TieringPlanReport`], enriched with the
/// data-motion estimate the engine's bare [`PlannedMove`] does not
/// carry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlannedMoveReport {
    /// Barcode of the cartridge to move.
    pub barcode: String,
    /// Backend the cartridge is currently on.
    pub from_backend: String,
    /// Backend the matched policy wants it on.
    pub to_backend: String,
    /// Sealed chunk count (from `chunks.idx`).
    pub chunk_count: u64,
    /// Total sealed bytes to move (from `chunks.idx`).
    pub bytes: u64,
}

/// A cartridge the plan could not evaluate, with the reason.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkippedCartridge {
    /// Barcode (cartridge directory name).
    pub barcode: String,
    /// Why it was skipped.
    pub reason: String,
}

impl TieringPredicates {
    /// True when every set predicate matches `facts`. An all-unset set
    /// matches everything.
    fn matches(&self, facts: &CartridgeFacts) -> bool {
        if let Some(prefix) = &self.barcode_prefix
            && !facts.barcode.starts_with(prefix)
        {
            return false;
        }
        if let Some(generation) = self.lto_generation
            && facts.lto_generation != generation
        {
            return false;
        }
        if let Some(worm) = self.worm
            && facts.worm != worm
        {
            return false;
        }
        true
    }

    /// True when no predicate is set (a catch-all). Rejected by
    /// [`validate_policies`].
    fn is_empty(&self) -> bool {
        self.barcode_prefix.is_none() && self.lto_generation.is_none() && self.worm.is_none()
    }
}

/// Evaluate `policies` against `facts`, returning one [`PlannedMove`]
/// per cartridge that matches a policy and is not already on that
/// policy's target backend.
///
/// Legal-held cartridges are skipped outright. Policies are evaluated
/// in order and the first match claims the cartridge — even if the
/// match is a no-op (already on the target), no later policy applies.
pub fn plan_moves(facts: &[CartridgeFacts], policies: &[TieringPolicy]) -> Vec<PlannedMove> {
    let mut moves = Vec::new();
    for f in facts {
        if f.legal_held {
            continue;
        }
        for p in policies {
            if p.predicates.matches(f) {
                if p.migrate_to != f.current_backend {
                    moves.push(PlannedMove {
                        barcode: f.barcode.clone(),
                        source_backend: f.current_backend.clone(),
                        target_backend: p.migrate_to.clone(),
                    });
                }
                // First match wins, no-op or not.
                break;
            }
        }
    }
    moves
}

/// Validate a set of tiering policies at config-parse time. Returns
/// every problem found (not just the first) so the operator can fix
/// the whole block in one pass. A policy is rejected when:
///   - `migrate_to` is empty or names a backend not in `known_backends`
///     (the keys of `storage.backends`), or
///   - its predicate set is empty (a catch-all "migrate everything").
pub fn validate_policies(
    policies: &[TieringPolicy],
    known_backends: &BTreeSet<String>,
) -> Result<(), Vec<String>> {
    let mut errs = Vec::new();
    for (i, p) in policies.iter().enumerate() {
        if p.migrate_to.trim().is_empty() {
            errs.push(format!(
                "tiering.policies[{i}]: migrate_to must be non-empty"
            ));
        } else if !known_backends.contains(&p.migrate_to) {
            errs.push(format!(
                "tiering.policies[{i}]: migrate_to references undefined backend '{}'",
                p.migrate_to
            ));
        }
        if p.predicates.is_empty() {
            errs.push(format!(
                "tiering.policies[{i}]: at least one predicate \
                 (barcode_prefix, lto_generation, worm) is required"
            ));
        }
    }
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(barcode: &str, generation: u8, worm: bool, backend: &str) -> CartridgeFacts {
        CartridgeFacts {
            barcode: barcode.to_string(),
            lto_generation: generation,
            worm,
            current_backend: backend.to_string(),
            legal_held: false,
        }
    }

    fn policy(predicates: TieringPredicates, migrate_to: &str) -> TieringPolicy {
        TieringPolicy {
            predicates,
            migrate_to: migrate_to.to_string(),
        }
    }

    fn prefix(p: &str) -> TieringPredicates {
        TieringPredicates {
            barcode_prefix: Some(p.to_string()),
            ..Default::default()
        }
    }

    fn backends(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn barcode_prefix_matches_and_misses() {
        assert!(prefix("ARCH").matches(&facts("ARCH001", 8, false, "hot")));
        assert!(!prefix("ARCH").matches(&facts("PROD001", 8, false, "hot")));
    }

    #[test]
    fn lto_generation_matches_exactly() {
        let pred = TieringPredicates {
            lto_generation: Some(8),
            ..Default::default()
        };
        assert!(pred.matches(&facts("X", 8, false, "hot")));
        assert!(!pred.matches(&facts("X", 7, false, "hot")));
    }

    #[test]
    fn worm_matches_both_polarities() {
        let want_worm = TieringPredicates {
            worm: Some(true),
            ..Default::default()
        };
        assert!(want_worm.matches(&facts("X", 8, true, "hot")));
        assert!(!want_worm.matches(&facts("X", 8, false, "hot")));

        let want_rw = TieringPredicates {
            worm: Some(false),
            ..Default::default()
        };
        assert!(want_rw.matches(&facts("X", 8, false, "hot")));
        assert!(!want_rw.matches(&facts("X", 8, true, "hot")));
    }

    #[test]
    fn predicates_and_together() {
        let pred = TieringPredicates {
            barcode_prefix: Some("ARCH".to_string()),
            lto_generation: Some(8),
            worm: Some(true),
        };
        // All three must hold.
        assert!(pred.matches(&facts("ARCH001", 8, true, "hot")));
        // One mismatch is enough to fail.
        assert!(!pred.matches(&facts("ARCH001", 8, false, "hot")));
        assert!(!pred.matches(&facts("ARCH001", 7, true, "hot")));
        assert!(!pred.matches(&facts("PROD001", 8, true, "hot")));
    }

    #[test]
    fn empty_predicate_set_matches_everything() {
        assert!(TieringPredicates::default().matches(&facts("anything", 8, false, "hot")));
        assert!(TieringPredicates::default().is_empty());
    }

    #[test]
    fn plan_emits_move_for_matching_cartridge_off_target() {
        let cartridges = vec![facts("ARCH001", 8, false, "hot")];
        let policies = vec![policy(prefix("ARCH"), "cold")];
        let moves = plan_moves(&cartridges, &policies);
        assert_eq!(
            moves,
            vec![PlannedMove {
                barcode: "ARCH001".to_string(),
                source_backend: "hot".to_string(),
                target_backend: "cold".to_string(),
            }]
        );
    }

    #[test]
    fn plan_skips_cartridge_already_on_target() {
        let cartridges = vec![facts("ARCH001", 8, false, "cold")];
        let policies = vec![policy(prefix("ARCH"), "cold")];
        assert!(plan_moves(&cartridges, &policies).is_empty());
    }

    #[test]
    fn plan_skips_legal_held_cartridge() {
        let mut held = facts("ARCH001", 8, false, "hot");
        held.legal_held = true;
        let policies = vec![policy(prefix("ARCH"), "cold")];
        assert!(plan_moves(&[held], &policies).is_empty());
    }

    #[test]
    fn plan_skips_cartridge_matching_no_policy() {
        let cartridges = vec![facts("PROD001", 8, false, "hot")];
        let policies = vec![policy(prefix("ARCH"), "cold")];
        assert!(plan_moves(&cartridges, &policies).is_empty());
    }

    #[test]
    fn first_match_wins_when_earlier_is_a_noop() {
        // Cartridge is already on "cold" and matches the first policy
        // (a no-op). A later policy would move it to "glacier", but the
        // first match claims it — no move is emitted.
        let cartridges = vec![facts("ARCH001", 8, false, "cold")];
        let policies = vec![
            policy(prefix("ARCH"), "cold"),
            policy(prefix("ARCH"), "glacier"),
        ];
        assert!(plan_moves(&cartridges, &policies).is_empty());
    }

    #[test]
    fn first_match_wins_picks_earlier_target() {
        let cartridges = vec![facts("ARCH001", 8, false, "hot")];
        let policies = vec![
            policy(prefix("ARCH"), "warm"),
            policy(prefix("ARCH"), "cold"),
        ];
        let moves = plan_moves(&cartridges, &policies);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].target_backend, "warm");
    }

    #[test]
    fn validate_accepts_well_formed_policies() {
        let policies = vec![policy(prefix("ARCH"), "cold")];
        assert!(validate_policies(&policies, &backends(&["hot", "cold"])).is_ok());
    }

    #[test]
    fn validate_rejects_unknown_backend() {
        let policies = vec![policy(prefix("ARCH"), "nope")];
        let err = validate_policies(&policies, &backends(&["hot", "cold"])).unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].contains("undefined backend 'nope'"));
    }

    #[test]
    fn validate_rejects_empty_migrate_to() {
        let policies = vec![policy(prefix("ARCH"), "   ")];
        let err = validate_policies(&policies, &backends(&["hot", "cold"])).unwrap_err();
        assert!(
            err.iter()
                .any(|e| e.contains("migrate_to must be non-empty"))
        );
    }

    #[test]
    fn validate_rejects_catch_all_predicate() {
        let policies = vec![policy(TieringPredicates::default(), "cold")];
        let err = validate_policies(&policies, &backends(&["cold"])).unwrap_err();
        assert!(err.iter().any(|e| e.contains("at least one predicate")));
    }

    #[test]
    fn validate_collects_every_error() {
        // Index 0: unknown backend. Index 1: empty migrate_to AND
        // catch-all predicates (two errors). Total: 3.
        let policies = vec![
            policy(prefix("ARCH"), "nope"),
            policy(TieringPredicates::default(), ""),
        ];
        let err = validate_policies(&policies, &backends(&["cold"])).unwrap_err();
        assert_eq!(err.len(), 3);
    }

    #[test]
    fn config_deserializes_from_yaml() {
        let yaml = r#"
policies:
  - predicates:
      barcode_prefix: "ARCH"
      worm: true
    migrate_to: cold
  - predicates:
      lto_generation: 8
    migrate_to: warm
"#;
        let cfg: TieringConfig = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(cfg.policies.len(), 2);
        assert_eq!(cfg.policies[0].migrate_to, "cold");
        assert_eq!(
            cfg.policies[0].predicates.barcode_prefix.as_deref(),
            Some("ARCH")
        );
        assert_eq!(cfg.policies[0].predicates.worm, Some(true));
        assert_eq!(cfg.policies[1].predicates.lto_generation, Some(8));
    }

    #[test]
    fn config_defaults_to_no_policies() {
        let cfg: TieringConfig = serde_yaml::from_str("{}").expect("parse");
        assert!(cfg.policies.is_empty());
    }

    #[test]
    fn config_rejects_unknown_field() {
        let yaml = r#"
policies:
  - predicates:
      barcode_prefix: "ARCH"
    migrate_to: cold
    bogus_field: 1
"#;
        assert!(serde_yaml::from_str::<TieringConfig>(yaml).is_err());
    }
}
