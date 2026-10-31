//! A scoped prohibition on new observations during a receipt-only publication.
//! This is not an authorization or cache. The host and native operand checks
//! establish evidence authority before entering; this guard catches omissions.
use std::cell::RefCell;
type Identities = (String, String, String);
thread_local! { static CHECKER_IDENTITIES: RefCell<Option<Identities>> = const { RefCell::new(None) }; }
pub struct Guard(Option<Identities>);
impl Guard {
    pub fn enter(identities: Identities) -> Self {
        Self(CHECKER_IDENTITIES.with(|slot| slot.replace(Some(identities))))
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        CHECKER_IDENTITIES.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}
pub fn active() -> bool {
    CHECKER_IDENTITIES.with(|slot| slot.borrow().is_some())
}
pub fn checker_identities() -> Option<Identities> {
    CHECKER_IDENTITIES.with(|slot| slot.borrow().clone())
}
pub fn require_observation_allowed(operation: &str) -> Result<(), String> {
    if active() {
        #[cfg(test)]
        panic!("receipt-only path attempted observation: {operation}");
        #[cfg(not(test))]
        return Err(format!(
            "receipt-only refresh: observation required: {operation}"
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_only_compiler_entry_panics_and_scope_restores() {
        assert!(!active());
        let result = std::panic::catch_unwind(|| {
            let _guard = Guard::enter(("lean".into(), "lake".into(), "collector".into()));
            require_observation_allowed("compiler").unwrap();
        });
        assert!(result.is_err());
        assert!(!active());
    }
}
