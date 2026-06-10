//! Risk scanning of the sandbox read allowlist: risky entries are refused
//! unless explicitly overridden.

mod common;

use silo_core::error::HarnessError;
use silo_core::replay::TestScript;
use silo_harness::validate_read_allowlist;

#[test]
fn validation_refuses_risky_entries_and_lists_the_hits() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::create_dir_all(home.path().join(".ssh")).expect("create .ssh");
    let state = home.path().join("state");
    std::fs::create_dir_all(&state).expect("create state");

    let entry = home.path().join(".ssh");
    let error = validate_read_allowlist(std::slice::from_ref(&entry), home.path(), &state, &[])
        .unwrap_err();
    assert!(matches!(error, HarnessError::Config(_)));
    let message = error.to_string();
    assert!(message.contains(".ssh"), "{message}");
    assert!(message.contains("SSH private keys"), "{message}");

    assert!(validate_read_allowlist(
        std::slice::from_ref(&entry),
        home.path(),
        &state,
        std::slice::from_ref(&entry)
    )
    .is_ok());
}

#[tokio::test]
async fn harness_refuses_a_risky_allowlist_without_an_override() {
    let fixture = common::Fixture::new();
    let home = tempfile::tempdir().expect("home tempdir");
    let ssh = home.path().join(".ssh");
    std::fs::create_dir_all(&ssh).expect("create .ssh");

    let mut config = fixture.config();
    config.sandbox.read_allowlist = vec![ssh];
    let mut options = fixture.options(common::shared(TestScript::default()));
    options.risk_scan_home = Some(home.path().to_path_buf());

    let error = silo_harness::run(config, options).await.unwrap_err();
    assert!(matches!(error, HarnessError::Config(_)), "got {error:?}");
    assert!(error.to_string().contains("SSH private keys"));
}

#[tokio::test]
async fn harness_accepts_the_risky_entry_with_an_override() {
    let fixture = common::Fixture::new();
    let home = tempfile::tempdir().expect("home tempdir");
    let ssh = home.path().join(".ssh");
    std::fs::create_dir_all(&ssh).expect("create .ssh");

    let mut config = fixture.config();
    config.sandbox.read_allowlist = vec![ssh.clone()];
    // An empty frontend script ends the session immediately.
    let mut options = fixture.options(common::shared(TestScript::default()));
    options.risk_scan_home = Some(home.path().to_path_buf());
    options.allow_risky_paths = vec![ssh];

    let outcome = silo_harness::run(config, options)
        .await
        .expect("session runs with the override");
    assert_eq!(
        outcome.message.as_deref(),
        Some("frontend script exhausted")
    );
}
