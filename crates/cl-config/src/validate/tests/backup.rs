use super::*;

#[test]
fn backup_is_absent_by_default() {
    let cfg = ClusterConfig::parse(BASE).expect("valid");
    assert!(cfg.backup.is_none());
}

#[test]
fn backup_accepts_an_ssh_url_and_age_recipient() {
    let cfg = with_backup(
        "  repo_url: git@github.com:org/repo.git\n  age_recipient: age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\n",
    )
    .expect("valid");
    let b = cfg.backup.expect("backup set");
    assert_eq!(b.branch, "main");
}

#[test]
fn backup_rejects_an_http_url() {
    let err = with_backup(
        "  repo_url: https://github.com/org/repo.git\n  age_recipient: age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\n",
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::Invalid { at, .. } if at == "backup.repo_url"));
}

#[test]
fn backup_rejects_a_non_age_recipient() {
    let err =
        with_backup("  repo_url: git@github.com:org/repo.git\n  age_recipient: not-an-age-key\n")
            .unwrap_err();
    assert!(matches!(err, ConfigError::Invalid { at, .. } if at == "backup.age_recipient"));
}
