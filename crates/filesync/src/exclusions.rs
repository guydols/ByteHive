use regex::Regex;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ExclusionConfig {
    pub exclude_patterns: Vec<String>,
    pub exclude_regex: Vec<String>,
}

pub struct Exclusions {
    compiled: Vec<(String, Regex)>,
}

impl Exclusions {
    pub fn compile(cfg: &ExclusionConfig) -> Self {
        let mut compiled = Vec::new();

        for pat in &[
            crate::protocol::TMP_DIR,
            &format!("{}/**", crate::protocol::TMP_DIR),
        ] {
            let re_src = glob_to_regex(pat);
            if let Ok(re) = Regex::new(&re_src) {
                compiled.push((format!("builtin:{pat}"), re));
            }
        }

        for pat in &cfg.exclude_patterns {
            let re_src = glob_to_regex(pat);
            match Regex::new(&re_src) {
                Ok(re) => compiled.push((format!("glob:{pat}"), re)),
                Err(e) => log::warn!(
                    "filesync: exclusion pattern {:?} produced invalid regex ({re_src:?}): {e}",
                    pat
                ),
            }
        }

        for raw in &cfg.exclude_regex {
            match Regex::new(raw) {
                Ok(re) => compiled.push((format!("regex:{raw}"), re)),
                Err(e) => log::warn!("filesync: exclusion regex {:?}: {e}", raw),
            }
        }

        Self { compiled }
    }

    #[inline]
    pub fn is_excluded(&self, rel: &Path) -> bool {
        if self.compiled.is_empty() {
            return false;
        }
        let s = normalise(rel);
        self.compiled.iter().any(|(_, re)| re.is_match(&s))
    }

    pub fn matching_rule(&self, rel: &Path) -> Option<&str> {
        if self.compiled.is_empty() {
            return None;
        }
        let s = normalise(rel);
        self.compiled
            .iter()
            .find(|(_, re)| re.is_match(&s))
            .map(|(src, _)| src.as_str())
    }

    pub fn rule_count(&self) -> usize {
        self.compiled.len()
    }
}

fn normalise(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn glob_to_regex(pat: &str) -> String {
    let mut re = String::from("^");
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                re.push_str(".*");
                i += 2;

                if i < chars.len() && chars[i] == '/' {
                    i += 1;
                }
            }
            '*' => {
                re.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                re.push_str("[^/]");
                i += 1;
            }
            c => {
                if matches!(
                    c,
                    '^' | '$' | '.' | '|' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '\\'
                ) {
                    re.push('\\');
                }
                re.push(c);
                i += 1;
            }
        }
    }

    re.push('$');
    re
}
