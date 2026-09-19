//! Process-wide logical byte admission for disposable materialized views.

use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

pub(crate) const MATERIALIZATION_BUDGET_ENV: &str = "LATTICE_MATERIALIZATION_BUDGET_BYTES";
pub(crate) const VIEW_RESERVATION_ENV: &str = "LATTICE_VIEW_RESERVATION_BYTES";
pub(crate) const VIEW_CLASS_BUDGET_ENV: &str = "LATTICE_VIEW_CLASS_BUDGET_BYTES";
pub(crate) const INDEX_CLASS_BUDGET_ENV: &str = "LATTICE_INDEX_CLASS_BUDGET_BYTES";
const DEFAULT_MATERIALIZATION_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct ResourceBudget {
    limit_bytes: u64,
    class_limits: HashMap<&'static str, u64>,
    state: Mutex<BudgetState>,
}

#[derive(Debug, Default)]
struct BudgetState {
    reserved_bytes: u64,
    by_class: HashMap<&'static str, u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ResourceBudgetSnapshot {
    pub(crate) limit_bytes: u64,
    pub(crate) reserved_bytes: u64,
    pub(crate) available_bytes: u64,
    pub(crate) reservation_basis: &'static str,
    pub(crate) by_class: HashMap<&'static str, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceLimit {
    pub(crate) class: &'static str,
    pub(crate) requested_bytes: u64,
    pub(crate) reserved_bytes: u64,
    pub(crate) limit_bytes: u64,
    pub(crate) limiting_scope: &'static str,
}

impl fmt::Display for ResourceLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "resource-limited: {} requested {} logical bytes with {} of {} already reserved ({})",
            self.class,
            self.requested_bytes,
            self.reserved_bytes,
            self.limit_bytes,
            self.limiting_scope
        )
    }
}

impl std::error::Error for ResourceLimit {}

impl ResourceBudget {
    pub(crate) fn from_env() -> Arc<Self> {
        let total = env_u64(
            MATERIALIZATION_BUDGET_ENV,
            DEFAULT_MATERIALIZATION_BUDGET_BYTES,
        );
        Arc::new(Self::with_class_limits(
            total,
            [
                (
                    "active_checkout_view",
                    env_u64(VIEW_CLASS_BUDGET_ENV, total),
                ),
                (
                    "index_staging_generation",
                    env_u64(INDEX_CLASS_BUDGET_ENV, total),
                ),
                (
                    "index_source_payload",
                    env_u64(INDEX_CLASS_BUDGET_ENV, total),
                ),
            ],
        ))
    }

    pub(crate) fn new(limit_bytes: u64) -> Self {
        Self {
            limit_bytes: limit_bytes.max(1),
            class_limits: HashMap::new(),
            state: Mutex::new(BudgetState::default()),
        }
    }

    pub(crate) fn with_class_limits(
        limit_bytes: u64,
        class_limits: impl IntoIterator<Item = (&'static str, u64)>,
    ) -> Self {
        Self {
            limit_bytes: limit_bytes.max(1),
            class_limits: class_limits
                .into_iter()
                .map(|(class, limit)| (class, limit.max(1)))
                .collect(),
            state: Mutex::new(BudgetState::default()),
        }
    }

    /// The logical allowance reserved for each loaded workspace view.
    ///
    /// Unset, it is one byte, which reserves nothing. It used to default to
    /// 256 MiB against a 2 GiB budget, which refused the ninth workspace
    /// whatever real memory said: a count of eight in disguise. Shard
    /// admission now follows the daemon's real footprint
    /// (`docs/shard-capacity.md`). An operator who sets
    /// `LATTICE_VIEW_RESERVATION_BYTES` keeps byte admission as before.
    pub(crate) fn default_view_reservation() -> u64 {
        env_u64(VIEW_RESERVATION_ENV, 1).max(1)
    }

    pub(crate) fn try_reserve(
        self: &Arc<Self>,
        class: &'static str,
        bytes: u64,
    ) -> Result<ResourceReservation, ResourceLimit> {
        let bytes = bytes.max(1);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let class_reserved = state.by_class.get(class).copied().unwrap_or(0);
        let class_limit = self.class_limits.get(class).copied().unwrap_or(u64::MAX);
        if bytes > class_limit.saturating_sub(class_reserved) {
            return Err(ResourceLimit {
                class,
                requested_bytes: bytes,
                reserved_bytes: class_reserved,
                limit_bytes: class_limit,
                limiting_scope: "class",
            });
        }
        if bytes > self.limit_bytes.saturating_sub(state.reserved_bytes) {
            return Err(ResourceLimit {
                class,
                requested_bytes: bytes,
                reserved_bytes: state.reserved_bytes,
                limit_bytes: self.limit_bytes,
                limiting_scope: "user process",
            });
        }
        state.reserved_bytes += bytes;
        *state.by_class.entry(class).or_default() += bytes;
        Ok(ResourceReservation {
            budget: Arc::clone(self),
            class,
            bytes,
        })
    }

    pub(crate) fn snapshot(&self) -> ResourceBudgetSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        ResourceBudgetSnapshot {
            limit_bytes: self.limit_bytes,
            reserved_bytes: state.reserved_bytes,
            available_bytes: self.limit_bytes.saturating_sub(state.reserved_bytes),
            reservation_basis:
                "configured logical admission unit; not measured allocation or process RSS",
            by_class: state.by_class.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResourceReservation {
    budget: Arc<ResourceBudget>,
    class: &'static str,
    bytes: u64,
}

impl ResourceReservation {
    /// Atomically extends a live reservation before the caller grows an input
    /// or output buffer. A failed extension changes no accounting state.
    pub(crate) fn try_grow(&mut self, additional_bytes: u64) -> Result<(), ResourceLimit> {
        if additional_bytes == 0 {
            return Ok(());
        }
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let class_reserved = state.by_class.get(self.class).copied().unwrap_or(0);
        let class_limit = self
            .budget
            .class_limits
            .get(self.class)
            .copied()
            .unwrap_or(u64::MAX);
        if additional_bytes > class_limit.saturating_sub(class_reserved) {
            return Err(ResourceLimit {
                class: self.class,
                requested_bytes: additional_bytes,
                reserved_bytes: class_reserved,
                limit_bytes: class_limit,
                limiting_scope: "class",
            });
        }
        if additional_bytes > self.budget.limit_bytes.saturating_sub(state.reserved_bytes) {
            return Err(ResourceLimit {
                class: self.class,
                requested_bytes: additional_bytes,
                reserved_bytes: state.reserved_bytes,
                limit_bytes: self.budget.limit_bytes,
                limiting_scope: "user process",
            });
        }
        state.reserved_bytes += additional_bytes;
        *state.by_class.entry(self.class).or_default() += additional_bytes;
        self.bytes += additional_bytes;
        Ok(())
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for ResourceReservation {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.reserved_bytes = state.reserved_bytes.saturating_sub(self.bytes);
        if let Some(class_bytes) = state.by_class.get_mut(self.class) {
            *class_bytes = class_bytes.saturating_sub(self.bytes);
            if *class_bytes == 0 {
                state.by_class.remove(self.class);
            }
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_reservations_cannot_overcommit_and_drop_releases() {
        let budget = Arc::new(ResourceBudget::new(100));
        let first = budget.try_reserve("view", 60).unwrap();
        assert!(budget.try_reserve("view", 41).is_err());
        let second = budget.try_reserve("view", 40).unwrap();
        assert_eq!(budget.snapshot().reserved_bytes, 100);
        drop(first);
        drop(second);
        assert_eq!(budget.snapshot().reserved_bytes, 0);
    }

    #[test]
    fn one_five_and_twenty_view_capacity_is_exact() {
        for count in [1_u64, 5, 20] {
            let budget = Arc::new(ResourceBudget::new(count * 10));
            let reservations: Vec<_> = (0..count)
                .map(|_| budget.try_reserve("view", 10).unwrap())
                .collect();
            assert!(budget.try_reserve("view", 10).is_err());
            assert_eq!(budget.snapshot().reserved_bytes, count * 10);
            drop(reservations);
        }
    }

    #[test]
    fn input_growth_is_reserved_before_allocation_and_failure_is_atomic() {
        let budget = Arc::new(ResourceBudget::new(100));
        let mut input = budget.try_reserve("index_source_payload", 10).unwrap();
        input.try_grow(75).unwrap();
        assert_eq!(input.bytes(), 85);
        assert!(input.try_grow(16).is_err());
        assert_eq!(input.bytes(), 85);
        assert_eq!(budget.snapshot().reserved_bytes, 85);
    }

    #[test]
    fn class_cap_applies_even_when_user_process_has_capacity() {
        let budget = Arc::new(ResourceBudget::with_class_limits(
            1_000,
            [("active_checkout_view", 100)],
        ));
        let _active = budget.try_reserve("active_checkout_view", 80).unwrap();
        let error = budget.try_reserve("active_checkout_view", 21).unwrap_err();
        assert_eq!(error.limiting_scope, "class");
        assert_eq!(budget.snapshot().reserved_bytes, 80);
    }
}
