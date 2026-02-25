use std::collections::HashSet;

use types::SenderBinding;

/// Sender authorization policy for a single channel adapter.
///
/// Built from the user's configured [`SenderBinding`] list. A sender is either
/// authorized (platform ID found in the set) or rejected. All authorized
/// senders are treated identically as the owning user — there is no role
/// differentiation.
///
/// This implements a default-deny policy: only platform IDs explicitly listed
/// in the configuration are allowed to interact with the agent.
#[derive(Debug, Clone)]
pub struct SenderAuthPolicy {
    /// Flattened set of all authorized platform IDs across all bindings.
    authorized: HashSet<String>,
}

impl SenderAuthPolicy {
    /// Build a policy from a list of sender bindings.
    ///
    /// All `platform_ids` from all bindings are flattened into a single
    /// lookup set. Empty bindings produce a policy that rejects everyone.
    pub fn from_bindings(bindings: &[SenderBinding]) -> Self {
        let authorized = bindings
            .iter()
            .flat_map(|binding| binding.platform_ids.iter().cloned())
            .collect();
        Self { authorized }
    }

    /// Check whether a platform-specific sender ID is authorized.
    pub fn is_authorized(&self, platform_id: &str) -> bool {
        self.authorized.contains(platform_id)
    }

    /// Returns the number of authorized platform IDs.
    pub fn authorized_count(&self) -> usize {
        self.authorized.len()
    }

    /// Returns `true` if the policy has no authorized senders at all.
    pub fn is_empty(&self) -> bool {
        self.authorized.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use types::SenderBinding;

    use super::*;

    #[test]
    fn known_sender_is_authorized() {
        let bindings = vec![SenderBinding {
            platform_ids: vec!["12345678".to_owned()],
            display_name: Some("Alice".to_owned()),
        }];
        let policy = SenderAuthPolicy::from_bindings(&bindings);
        assert!(policy.is_authorized("12345678"));
    }

    #[test]
    fn unknown_sender_is_rejected() {
        let bindings = vec![SenderBinding {
            platform_ids: vec!["12345678".to_owned()],
            display_name: None,
        }];
        let policy = SenderAuthPolicy::from_bindings(&bindings);
        assert!(!policy.is_authorized("99999999"));
    }

    #[test]
    fn empty_bindings_reject_everyone() {
        let policy = SenderAuthPolicy::from_bindings(&[]);
        assert!(policy.is_empty());
        assert!(!policy.is_authorized("12345678"));
    }

    #[test]
    fn multiple_platform_ids_in_one_binding_all_authorize() {
        let bindings = vec![SenderBinding {
            platform_ids: vec![
                "11111111".to_owned(),
                "22222222".to_owned(),
                "33333333".to_owned(),
            ],
            display_name: Some("Bob (multi-account)".to_owned()),
        }];
        let policy = SenderAuthPolicy::from_bindings(&bindings);
        assert_eq!(policy.authorized_count(), 3);
        assert!(policy.is_authorized("11111111"));
        assert!(policy.is_authorized("22222222"));
        assert!(policy.is_authorized("33333333"));
        assert!(!policy.is_authorized("44444444"));
    }

    #[test]
    fn multiple_bindings_flatten_into_single_set() {
        let bindings = vec![
            SenderBinding {
                platform_ids: vec!["12345678".to_owned()],
                display_name: Some("Alice".to_owned()),
            },
            SenderBinding {
                platform_ids: vec!["87654321".to_owned(), "11223344".to_owned()],
                display_name: Some("Bob".to_owned()),
            },
        ];
        let policy = SenderAuthPolicy::from_bindings(&bindings);
        assert_eq!(policy.authorized_count(), 3);
        assert!(policy.is_authorized("12345678"));
        assert!(policy.is_authorized("87654321"));
        assert!(policy.is_authorized("11223344"));
    }

    #[test]
    fn duplicate_platform_ids_deduplicated() {
        let bindings = vec![
            SenderBinding {
                platform_ids: vec!["12345678".to_owned()],
                display_name: None,
            },
            SenderBinding {
                platform_ids: vec!["12345678".to_owned()],
                display_name: Some("Duplicate".to_owned()),
            },
        ];
        let policy = SenderAuthPolicy::from_bindings(&bindings);
        // Deduplicated in HashSet
        assert_eq!(policy.authorized_count(), 1);
        assert!(policy.is_authorized("12345678"));
    }
}
