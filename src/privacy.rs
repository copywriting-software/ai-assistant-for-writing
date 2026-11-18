use crate::config::PrivacyConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivacyVerdict {
    Allowed,
    Blocked(String),
}

pub struct PrivacyGuard {
    config: PrivacyConfig,
}

impl PrivacyGuard {
    pub fn new(config: PrivacyConfig) -> Self {
        Self { config }
    }

    pub fn check(&self, app_name: Option<&str>, is_secure_field: bool, text: &str) -> PrivacyVerdict {
        if self.config.block_secure_fields && is_secure_field {
            return PrivacyVerdict::Blocked("Secure/password input field".to_string());
        }

        if let Some(app) = app_name {
            let app_lower = app.to_lowercase();
            for blocked in &self.config.blocked_apps {
                if app_lower.contains(&blocked.to_lowercase()) {
                    return PrivacyVerdict::Blocked(format!("Blocked application: {}", app));
                }
            }
        }

        if self.config.block_sensitive_patterns && contains_sensitive_pattern(text) {
            return PrivacyVerdict::Blocked("Sensitive token or credential pattern detected".to_string());
        }

        PrivacyVerdict::Allowed
    }
}

fn contains_sensitive_pattern(text: &str) -> bool {
    let lower = text.to_lowercase();

    // Common API key / secret prefixes
    let secret_prefixes = [
        "sk-", "ak_", "mkey_", "ghp_", "gho_", "glpat-", "xoxb-", "xoxp-", "aiza",
    ];
    for token in text.split_whitespace() {
        let tok_lower = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-').to_lowercase();
        if tok_lower.len() >= 16 && secret_prefixes.iter().any(|p| tok_lower.starts_with(p)) {
            return true;
        }
    }

    // Explicit password / CVV hints
    if lower.contains("password:")
        || lower.contains("password=")
        || lower.contains("пароль:")
        || lower.contains("cvv")
    {
        return true;
    }

    // Possible 16-digit payment card number (grouped or contiguous)
    let digits_only: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits_only.len() == 16 && text.split_whitespace().any(|w| {
        let d: usize = w.chars().filter(|c| c.is_ascii_digit()).count();
        d == 16 || d == 4
    }) {
        // Only flag if there's a contiguous 16-digit run or 4x4 card pattern
        if text.split(|c: char| !c.is_ascii_digit()).any(|run| run.len() == 16) {
            return true;
        }
        let four_groups = text
            .split(|c: char| c == ' ' || c == '-')
            .filter(|part| part.len() == 4 && part.chars().all(|c| c.is_ascii_digit()))
            .count();
        if four_groups >= 4 {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_secure_fields_and_password_managers() {
        let guard = PrivacyGuard::new(PrivacyConfig::default());
        assert!(matches!(
            guard.check(Some("Notes"), true, "hello"),
            PrivacyVerdict::Blocked(_)
        ));
        assert!(matches!(
            guard.check(Some("1Password"), false, "hello"),
            PrivacyVerdict::Blocked(_)
        ));
        assert!(matches!(
            guard.check(Some("iTerm2"), false, "ls -la"),
            PrivacyVerdict::Blocked(_)
        ));
        assert_eq!(
            guard.check(Some("TextEdit"), false, "Привет, хотел уточнить"),
            PrivacyVerdict::Allowed
        );
    }

    #[test]
    fn blocks_api_keys_and_cards() {
        let guard = PrivacyGuard::new(PrivacyConfig::default());
        assert!(matches!(
            guard.check(Some("Slack"), false, "My key is sk-1234567890abcdef1234"),
            PrivacyVerdict::Blocked(_)
        ));
        assert!(matches!(
            guard.check(Some("Chrome"), false, "Card: 4276 1234 5678 9012"),
            PrivacyVerdict::Blocked(_)
        ));
    }
}
