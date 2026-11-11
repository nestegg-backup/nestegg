//! Target migration and policy regressions; no real device or server needed.
use chrono::{Duration, Utc};
use nestegg::config::{Config, Paths};
use nestegg::state::{ErrorKind, Health, Run, RunError, RunState, State, Store, Target, Trigger};

const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";
fn stick_config() -> Config {
    Config::parse(&format!(
        "backup_id='desktop'\n[stick]\ndata_uuids=['{UUID}']"
    ))
    .unwrap()
}
fn paths(dir: &std::path::Path) -> Paths {
    Paths {
        config_file: dir.join("config.toml"),
        credentials_dir: dir.join("credentials"),
        state_dir: dir.join("state"),
        runtime_dir: dir.join("runtime"),
        systemctl: dir.join("no-systemctl"),
    }
}
fn finished(target: Target, state: RunState) -> Run {
    let mut run = Run::new(Trigger::Timer, Utc::now() - Duration::hours(1));
    run.target = target;
    run.finish(state, Utc::now());
    run
}

#[test]
fn stick_only_config_has_no_dummy_pbs_and_safe_defaults() {
    let config = stick_config();
    assert!(config.pbs.is_none());
    assert!(config.remote_pbs().is_err());
    assert!(!config.to_toml().unwrap().contains("[pbs]"));
    let stick = config.stick.unwrap();
    assert_eq!(stick.overdue_after_hours, 168);
    assert_eq!(stick.minimum_free_bytes, 256 * 1024 * 1024);
    assert_eq!(
        (
            stick.retention.keep_last,
            stick.retention.keep_daily,
            stick.retention.keep_weekly,
            stick.retention.keep_monthly
        ),
        (3, 7, 4, 6)
    );
}
#[test]
fn strict_targets_reject_unknown_fields_and_invalid_protection() {
    for text in [
        "",
        "[stick]\ndata_uuids=[]",
        "[stick]\ndata_uuids=['../device']",
        &format!("[stick]\ndata_uuids=['{UUID}']\npassword='secret'"),
        &format!("[stick]\ndata_uuids=['{UUID}']\n[backup]\nencryption=false"),
        &format!("[stick]\ndata_uuids=['{UUID}']\n[stick.retention]\nkeep_last=0"),
        &format!("[stick]\ndata_uuids=['{UUID}']\n[stick.retention]\nunknown=3"),
    ] {
        assert!(Config::parse(text).is_err(), "{text}");
    }
}
#[test]
fn old_state_and_history_are_pbs_without_reconfirmation() {
    let legacy = r#"{"key_digest":"fixture-digest","key_exported_digest":"fixture-digest","consecutive_failures":2,"last_run":{"id":"20261108T120000Z-timer-0001","trigger":"timer","state":"failed","started_at":"2026-11-08T12:00:00Z"}}"#;
    let state: State = serde_json::from_str(legacy).unwrap();
    assert!(state.key_exported());
    assert_eq!(state.last_run.as_ref().unwrap().target, Target::Pbs);
    assert_eq!(state.for_target(Target::Stick).consecutive_failures, 0);
    assert!(state.for_target(Target::Stick).last_run.is_none());
}

#[test]
fn invalid_config_failure_keeps_the_explicit_target() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    std::fs::write(&paths.config_file, "unknown_setting = true").unwrap();
    let options = nestegg::run::RunOptions {
        target: nestegg::state::TargetSelection::Stick,
        stick_uuid: None,
        trigger: Trigger::Cli,
        run_id: None,
        force: true,
    };
    assert_eq!(
        nestegg::run::execute(&paths, &options).unwrap(),
        nestegg::run::RunExit::Failed
    );
    let state = Store::new(&paths).load_state().unwrap();
    assert!(state.last_run.is_none());
    assert_eq!(state.stick.consecutive_failures, 1);
    assert_eq!(
        state.stick.last_run.unwrap().error.unwrap().kind,
        ErrorKind::Config
    );
}
#[test]
fn target_failures_successes_and_availability_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    let pbs = Store::new(&paths);
    let stick = Store::for_target(&paths, Target::Stick);
    pbs.record_finished_with_config(
        &finished(Target::Pbs, RunState::Failed),
        200,
        Some("pbs-config".into()),
    )
    .unwrap();
    stick
        .record_finished_with_config(
            &finished(Target::Stick, RunState::Failed),
            200,
            Some("stick-config".into()),
        )
        .unwrap();
    assert_eq!(pbs.load_state().unwrap().consecutive_failures, 1);
    assert_eq!(stick.load_state().unwrap().consecutive_failures, 1);
    for state in [RunState::StickNotPluggedIn, RunState::Cancelled] {
        stick
            .record_finished(&finished(Target::Stick, state), 200)
            .unwrap();
    }
    assert_eq!(stick.load_state().unwrap().consecutive_failures, 1);
    stick
        .record_finished(&finished(Target::Stick, RunState::Succeeded), 200)
        .unwrap();
    assert_eq!(stick.load_state().unwrap().consecutive_failures, 0);
    assert_eq!(
        pbs.load_state().unwrap().failed_config.as_deref(),
        Some("pbs-config")
    );
    assert!(pbs.load_state().unwrap().last_success.is_none());
    pbs.clear_failures().unwrap();
    assert_eq!(pbs.load_state().unwrap().consecutive_failures, 0);
}
#[test]
fn stick_state_updates_preserve_global_key_proof() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    let store = Store::new(&paths);
    let state = State {
        key_digest: Some("fixture-digest".into()),
        key_exported_digest: Some("fixture-digest".into()),
        ..Default::default()
    };
    store.save_state(&state).unwrap();
    let stick = Store::for_target(&paths, Target::Stick);
    let mut view = stick.load_state().unwrap();
    view.readback_unavailable_in_a_row = 3;
    stick.save_state(&view).unwrap();
    let state = store.load_state().unwrap();
    assert!(state.key_exported());
    assert_eq!(state.readback_unavailable_in_a_row, 0);
    assert_eq!(state.stick.readback_unavailable_in_a_row, 3);
}

#[test]
fn history_retention_and_snapshot_scope_stay_with_their_target() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    let pbs = Store::new(&paths);
    let stick = Store::for_target(&paths, Target::Stick);
    let mut older = finished(Target::Stick, RunState::Succeeded);
    older.id = "20260101T000000Z-cli-0001".into();
    older.snapshot = Some("host/desktop/2026-01-01T00:00:00Z".into());
    stick.record_finished(&older, 1).unwrap();
    for day in 1..=3 {
        let mut run = finished(Target::Pbs, RunState::Succeeded);
        run.id = format!("2026020{day}T000000Z-cli-0001");
        pbs.record_finished(&run, 1).unwrap();
    }
    assert_eq!(pbs.list_runs(20).unwrap().len(), 2, "one record per target");
    assert!(
        pbs.run_for_snapshot(older.snapshot.as_ref().unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        stick
            .run_for_snapshot(older.snapshot.as_ref().unwrap())
            .unwrap()
            .unwrap()
            .id,
        older.id
    );
}
#[test]
fn overdue_threshold_and_retry_are_evaluated_per_target() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    let mut config = stick_config();
    config.pbs = nestegg::config::example("pbs.example.com", "store", "backup@pbs!desktop").pbs;
    let mut old = finished(Target::Pbs, RunState::Succeeded);
    old.finished_at = Some(Utc::now() - Duration::days(3));
    Store::new(&paths).record_finished(&old, 200).unwrap();
    old.target = Target::Stick;
    Store::for_target(&paths, Target::Stick)
        .record_finished(&old, 200)
        .unwrap();
    let status = nestegg::status::build(&paths, Some(&config), true).unwrap();
    assert_eq!(status.targets[&Target::Pbs].health, Health::Overdue);
    assert_eq!(status.targets[&Target::Stick].health, Health::Ok);
    assert_eq!(status.targets[&Target::Stick].state, "stick_not_plugged_in");
    assert!(status.targets[&Target::Stick].password_reminder);
}
#[test]
fn notifications_keep_both_target_failures() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    let mut config = stick_config();
    config.pbs = nestegg::config::example("pbs.example.com", "store", "backup@pbs!desktop").pbs;
    for target in [Target::Pbs, Target::Stick] {
        let mut run = finished(target, RunState::Failed);
        run.error = Some(RunError {
            kind: ErrorKind::Authentication,
            message: "authentication rejected".into(),
        });
        Store::for_target(&paths, target)
            .record_finished(&run, 200)
            .unwrap();
    }
    let status = nestegg::status::build(&paths, Some(&config), true).unwrap();
    let nestegg::notify::Evaluation::Checked { conditions, .. } =
        nestegg::notify::evaluate(&status, true, None, Utc::now())
    else {
        panic!("expected conditions")
    };
    assert!(
        conditions
            .iter()
            .any(|c| c.kind == nestegg::notify::Event::Failing)
    );
    assert!(
        conditions
            .iter()
            .any(|c| c.kind == nestegg::notify::Event::StickFailing)
    );
}

#[test]
fn disabled_shared_timer_is_announced_once_for_two_targets() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    let mut config = stick_config();
    config.pbs = nestegg::config::example("pbs.example.com", "store", "backup@pbs!desktop").pbs;
    let status = nestegg::status::build(&paths, Some(&config), false).unwrap();
    let nestegg::notify::Evaluation::Checked { conditions, .. } =
        nestegg::notify::evaluate(&status, false, None, Utc::now())
    else {
        panic!("expected conditions");
    };
    assert_eq!(
        conditions
            .iter()
            .filter(|c| c.kind == nestegg::notify::Event::AutomaticOff)
            .count(),
        1
    );
}
#[test]
fn target_removal_and_stick_threshold_reduce_protection() {
    use nestegg::settings::{Reduction, SettingsPatch};
    let mut old = stick_config();
    old.pbs = nestegg::config::example("pbs.example.com", "store", "backup@pbs!desktop").pbs;
    let text = old.to_toml().unwrap();
    let patch = SettingsPatch::from_json(serde_json::json!({"remove_target":"pbs"})).unwrap();
    nestegg::settings::validate(&patch, &old, &[], None).unwrap();
    let new = Config::parse(&nestegg::settings::rewrite(&text, &patch).unwrap()).unwrap();
    assert_eq!(
        nestegg::settings::reductions(&old, &new, true, true),
        vec![Reduction::TargetRemoved {
            target: "pbs".into()
        }]
    );
    assert!(
        nestegg::settings::validate(
            &SettingsPatch::from_json(serde_json::json!({"remove_target":"stick"})).unwrap(),
            &new,
            &[],
            None
        )
        .is_err()
    );
    old.stick.as_mut().unwrap().overdue_after_hours = 72;
    let patch =
        SettingsPatch::from_json(serde_json::json!({"stick_overdue_after_hours":168})).unwrap();
    nestegg::settings::validate(&patch, &old, &[], None).unwrap();
    let new = Config::parse(&nestegg::settings::rewrite(&old.to_toml().unwrap(), &patch).unwrap())
        .unwrap();
    assert_eq!(
        nestegg::settings::reductions(&old, &new, true, true),
        vec![Reduction::StickOverdueRaised { from: 72, to: 168 }]
    );
}
#[test]
fn stick_unit_binds_to_escaped_uuid_and_keeps_real_sources_visible() {
    let unit = include_str!("../../../packaging/systemd/nestegg-stick-pbs@.service");
    assert!(unit.contains(r"BindsTo=dev-disk-by\x2duuid-%i.device"));
    assert!(unit.contains("--stick-uuid %I"));
    assert!(!unit.contains("PrivateTmp="));
    assert!(!unit.contains("ProtectSystem="));
    let rules = include_str!("../../../packaging/udev/70-nestegg-stick.rules");
    assert!(rules.contains("--unit"));
    assert!(rules.contains("nestegg-stick-pbs@%c.service"));
    assert!(rules.contains("UDISKS_IGNORE"));
}

#[tokio::test]
async fn target_history_filters_legacy_and_new_runs() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    for target in [Target::Pbs, Target::Stick] {
        Store::for_target(&paths, target)
            .record_finished(&finished(target, RunState::Succeeded), 200)
            .unwrap();
    }
    let response = nestegg::daemon::router(paths)
        .oneshot(
            Request::get("/api/v1/targets/stick/runs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["runs"].as_array().unwrap().len(), 1);
    assert_eq!(json["runs"][0]["target"], "stick");
}
