pub struct Auth {
    pub token: String,
}

impl Auth {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }

    pub fn check(&self, headers: &std::collections::HashMap<String, String>) -> bool {
        if self.token.is_empty() {
            return true;
        }
        headers
            .get("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t.trim() == self.token)
            .unwrap_or(false)
    }

    pub fn verify_token(&self, provided: &str) -> bool {
        if self.token.is_empty() {
            return true;
        }
        let a = self.token.as_bytes();
        let b = provided.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let diff: u8 = a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| x ^ y)
            .fold(0, |acc, x| acc | x);
        diff == 0
    }
}
