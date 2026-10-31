//! `NEWSKITE_PASS` supplies the password when `--pass` is absent. A separate test binary
//! (its own process), so setting the variable cannot leak into the other tests.

use newskite_cli::{parse_args, PASSWORD_ENV};

#[test]
fn password_comes_from_the_environment_unless_given() {
    std::env::set_var(PASSWORD_ENV, "from-env-secret");
    let a = parse_args([
        "newskite", "--nzb", "a.nzb", "--server", "h:119", "--user", "u",
    ])
    .unwrap();
    assert_eq!(a.pass.as_deref(), Some("from-env-secret"));
    let b = parse_args([
        "newskite", "--nzb", "a.nzb", "--server", "h", "--pass", "flag",
    ])
    .unwrap();
    assert_eq!(b.pass.as_deref(), Some("flag"));
    // Help never shows the value of the variable.
    let help = parse_args(["newskite", "--help"]).unwrap_err().to_string();
    assert!(help.contains(PASSWORD_ENV));
    assert!(!help.contains("from-env-secret"));
    std::env::remove_var(PASSWORD_ENV);
}
