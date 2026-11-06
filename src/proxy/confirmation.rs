use crate::storage::Account;
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

static CONFIRMATION_CLAIMS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Try to claim an in-flight confirmation probe for the given account ID.
/// Returns true if the claim was granted, or false if already claimed or invalid.
pub fn try_claim_verification_confirmation(account_id: &str) -> bool {
    if account_id.is_empty() {
        return false;
    }
    let mut claims = CONFIRMATION_CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    if claims.contains(account_id) {
        return false;
    }
    claims.insert(account_id.to_string());
    true
}

/// Release an in-flight confirmation claim for the given account ID.
pub fn release_verification_confirmation(account_id: &str) {
    if account_id.is_empty() {
        return;
    }
    let mut claims = CONFIRMATION_CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    claims.remove(account_id);
}

/// Clear all active claims (primarily for tests).
pub fn clear_all_verification_confirmation_claims() {
    let mut claims = CONFIRMATION_CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    claims.clear();
}

/// Reports whether a confirmation claim is currently active for account ID.
pub fn is_verification_confirmation_claimed(account_id: &str) -> bool {
    if account_id.is_empty() {
        return false;
    }
    let claims = CONFIRMATION_CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    claims.contains(account_id)
}

/// ConfirmationClaim is an RAII guard that automatically releases the confirmation claim
/// on drop unless `.keep()` is explicitly called.
#[derive(Debug)]
pub struct ConfirmationClaim {
    pub account_id: String,
    pub index: usize,
    pub account: Account,
    kept: bool,
}

impl ConfirmationClaim {
    pub fn new(account_id: String, index: usize, account: Account) -> Self {
        Self {
            account_id,
            index,
            account,
            kept: false,
        }
    }

    /// Mark the claim as kept so drop will not release it.
    pub fn keep(mut self) {
        self.kept = true;
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }
}

impl Drop for ConfirmationClaim {
    fn drop(&mut self) {
        if !self.kept {
            release_verification_confirmation(&self.account_id);
        }
    }
}

/// Inspects accounts for StatusVerificationPending, orders them deterministically
/// (oldest UpdatedAt first, breaking ties by ID ascending), and atomically claims
/// at most ONE unclaimed candidate.
/// Returns the claim guard, or None if no candidate is available or all are claimed.
pub fn select_and_claim_confirmation_candidate(accounts: &[Account]) -> Option<ConfirmationClaim> {
    if accounts.is_empty() {
        return None;
    }

    let mut pending: Vec<(usize, &Account)> = accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.status == crate::storage::STATUS_VERIFICATION_PENDING)
        .collect();

    if pending.is_empty() {
        return None;
    }

    // Deterministic / fair ordering: oldest UpdatedAt first (FIFO), breaking ties by ID ascending.
    pending.sort_by(|(_, a), (_, b)| {
        let up_a = a.updated_at.unwrap_or(0);
        let up_b = b.updated_at.unwrap_or(0);
        up_a.cmp(&up_b).then_with(|| a.id.cmp(&b.id))
    });

    for (orig_idx, cand) in pending {
        if try_claim_verification_confirmation(&cand.id) {
            return Some(ConfirmationClaim::new(cand.id.clone(), orig_idx, cand.clone()));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_lifecycle() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_all_verification_confirmation_claims();

        assert!(!is_verification_confirmation_claimed("acc-1"));
        assert!(try_claim_verification_confirmation("acc-1"));
        assert!(is_verification_confirmation_claimed("acc-1"));
        // Duplicate claim fails
        assert!(!try_claim_verification_confirmation("acc-1"));

        release_verification_confirmation("acc-1");
        assert!(!is_verification_confirmation_claimed("acc-1"));
        assert!(try_claim_verification_confirmation("acc-1"));

        clear_all_verification_confirmation_claims();
        assert!(!is_verification_confirmation_claimed("acc-1"));
    }

    #[test]
    fn confirmation_select_and_claim_deterministic_fifo_and_tiebreak() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_all_verification_confirmation_claims();

        let t100 = 100i64;
        let t200 = 200i64;

        let acc_active = Account {
            id: "acc_active".to_string(),
            status: "".to_string(),
            updated_at: Some(50),
            ..Default::default()
        };
        let acc_b_100 = Account {
            id: "acc_b".to_string(),
            status: crate::storage::STATUS_VERIFICATION_PENDING.to_string(),
            updated_at: Some(t100),
            ..Default::default()
        };
        let acc_a_100 = Account {
            id: "acc_a".to_string(),
            status: crate::storage::STATUS_VERIFICATION_PENDING.to_string(),
            updated_at: Some(t100),
            ..Default::default()
        };
        let acc_c_200 = Account {
            id: "acc_c".to_string(),
            status: crate::storage::STATUS_VERIFICATION_PENDING.to_string(),
            updated_at: Some(t200),
            ..Default::default()
        };

        let pool = vec![acc_active, acc_b_100, acc_a_100, acc_c_200];

        // Oldest is 100: acc_a vs acc_b. ID ascending tie-break picks acc_a!
        let claim1 = select_and_claim_confirmation_candidate(&pool).expect("should claim acc_a");
        assert_eq!(claim1.account_id(), "acc_a");
        assert_eq!(claim1.index, 2);

        // Next candidate is acc_b
        let claim2 = select_and_claim_confirmation_candidate(&pool).expect("should claim acc_b");
        assert_eq!(claim2.account_id(), "acc_b");
        assert_eq!(claim2.index, 1);

        // Next candidate is acc_c
        let claim3 = select_and_claim_confirmation_candidate(&pool).expect("should claim acc_c");
        assert_eq!(claim3.account_id(), "acc_c");
        assert_eq!(claim3.index, 3);

        // All pending are claimed
        assert!(select_and_claim_confirmation_candidate(&pool).is_none());

        // Dropping claim1 releases acc_a
        drop(claim1);
        assert!(!is_verification_confirmation_claimed("acc_a"));
        let claim4 = select_and_claim_confirmation_candidate(&pool).expect("should reclaim acc_a");
        assert_eq!(claim4.account_id(), "acc_a");

        // Keep claim4, so drop won't release it
        claim4.keep();
        assert!(is_verification_confirmation_claimed("acc_a"));

        clear_all_verification_confirmation_claims();
    }
}
