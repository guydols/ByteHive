use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
use std::path::PathBuf;

fn excl(patterns: &[&str], regexes: &[&str]) -> Exclusions {
    Exclusions::compile(&ExclusionConfig {
        exclude_patterns: patterns.iter().map(|s| s.to_string()).collect(),
        exclude_regex: regexes.iter().map(|s| s.to_string()).collect(),
    })
}

#[test]
fn glob_extension() {
    let e = excl(&["*.log"], &[]);
    assert!(e.is_excluded(&PathBuf::from("app.log")));
    assert!(!e.is_excluded(&PathBuf::from("app.rs")));
    assert!(!e.is_excluded(&PathBuf::from("logs/app.log")));
}

#[test]
fn glob_double_star_dir() {
    let e = excl(&["build/**"], &[]);
    assert!(e.is_excluded(&PathBuf::from("build/output.o")));
    assert!(e.is_excluded(&PathBuf::from("build/debug/app")));
    assert!(!e.is_excluded(&PathBuf::from("src/main.rs")));
}

#[test]
fn glob_double_star_anywhere() {
    let e = excl(&["**/.cache/**"], &[]);
    assert!(e.is_excluded(&PathBuf::from("project/.cache/something")));
    assert!(e.is_excluded(&PathBuf::from(".cache/data")));
    assert!(!e.is_excluded(&PathBuf::from("src/main.rs")));
}

#[test]
fn glob_question_mark() {
    let e = excl(&["secret_?.txt"], &[]);
    assert!(e.is_excluded(&PathBuf::from("secret_a.txt")));
    assert!(!e.is_excluded(&PathBuf::from("secret_ab.txt")));
    assert!(!e.is_excluded(&PathBuf::from("deep/secret_a.txt")));
}

#[test]
fn glob_deep_extension() {
    let e = excl(&["**/*.tmp"], &[]);
    assert!(e.is_excluded(&PathBuf::from("tmp_file.tmp")));
    assert!(e.is_excluded(&PathBuf::from("a/b/c/data.tmp")));
    assert!(!e.is_excluded(&PathBuf::from("file.log")));
}

#[test]
fn regex_extension() {
    let e = excl(&[], &[r".*\.(tmp|bak)$"]);
    assert!(e.is_excluded(&PathBuf::from("file.tmp")));
    assert!(e.is_excluded(&PathBuf::from("deep/dir/file.bak")));
    assert!(!e.is_excluded(&PathBuf::from("file.rs")));
}

#[test]
fn regex_dotfiles_at_root() {
    let e = excl(&[], &[r"^\.[^/]"]);
    assert!(e.is_excluded(&PathBuf::from(".env")));
    assert!(e.is_excluded(&PathBuf::from(".hidden")));
    assert!(!e.is_excluded(&PathBuf::from("src/.env")));
}

#[test]
fn empty_rules_never_exclude() {
    let e = excl(&[], &[]);
    assert!(!e.is_excluded(&PathBuf::from("anything/at/all.rs")));
}

#[test]
fn or_semantics() {
    let e = excl(&["*.log"], &[r"secret"]);
    assert!(e.is_excluded(&PathBuf::from("app.log")));
    assert!(e.is_excluded(&PathBuf::from("secret.txt")));
    assert!(!e.is_excluded(&PathBuf::from("main.rs")));
}

#[test]
fn matching_rule_returns_source() {
    let e = excl(&["*.log"], &[r"secret"]);
    let rule = e.matching_rule(&PathBuf::from("app.log"));
    assert!(rule.is_some());
    assert!(rule.unwrap().contains("*.log"));
}

#[test]
fn matching_rule_none_when_not_excluded() {
    let e = excl(&["*.log"], &[]);
    assert!(e.matching_rule(&PathBuf::from("main.rs")).is_none());
}

#[test]
fn invalid_regex_is_skipped_not_panicked() {
    let e = Exclusions::compile(&ExclusionConfig {
        exclude_patterns: vec![],
        exclude_regex: vec!["[invalid".to_string()],
    });
    // Invalid regex is skipped, only default rules remain (2 default rules)
    assert_eq!(e.rule_count(), 2);
}

#[test]
fn rule_count_reflects_compiled_rules() {
    let e = excl(&["*.log", "build/**"], &[r"secret"]);
    // 2 glob patterns + 1 regex + 2 default rules = 5
    assert_eq!(e.rule_count(), 5);
}

#[test]
fn builtin_tmp_dir_excluded_without_config() {
    let e = excl(&[], &[]);
    assert!(e.is_excluded(&PathBuf::from(".filesync_tmp")));
}

#[test]
fn builtin_tmp_dir_contents_excluded() {
    let e = excl(&[], &[]);
    assert!(e.is_excluded(&PathBuf::from(".filesync_tmp/partial.tmp")));
    assert!(e.is_excluded(&PathBuf::from(".filesync_tmp/sub/file.bin")));
}

#[test]
fn glob_single_star_does_not_cross_separator() {
    let e = excl(&["*.secret"], &[]);
    assert!(e.is_excluded(&PathBuf::from("config.secret")));
    assert!(!e.is_excluded(&PathBuf::from("dir/config.secret")));
}

#[test]
fn regex_multiple_rules_or_semantics() {
    let e = excl(&[], &[r"^private/", r".*\.key$"]);
    assert!(e.is_excluded(&PathBuf::from("private/data.txt")));
    assert!(e.is_excluded(&PathBuf::from("ssh/id_rsa.key")));
    assert!(!e.is_excluded(&PathBuf::from("public/data.txt")));
}
