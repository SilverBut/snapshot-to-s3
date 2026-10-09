use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serializes AWS environment overrides and restores credentials when dropped.
pub(crate) struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    access: Option<String>,
    secret: Option<String>,
    token: Option<String>,
    disabled: Option<String>,
}

impl EnvGuard {
    /// Installs fixture credentials while holding the process-wide environment lock.
    pub(crate) fn new() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let access = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        let token = std::env::var("AWS_SESSION_TOKEN").ok();
        let disabled = std::env::var("AWS_EC2_METADATA_DISABLED").ok();
        std::env::set_var("AWS_ACCESS_KEY_ID", "fixture-access");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "fixture-secret");
        std::env::set_var("AWS_SESSION_TOKEN", "fixture-token");
        std::env::remove_var("AWS_EC2_METADATA_DISABLED");
        Self {
            _lock: lock,
            access,
            secret,
            token,
            disabled,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        restore_env("AWS_ACCESS_KEY_ID", self.access.take());
        restore_env("AWS_SECRET_ACCESS_KEY", self.secret.take());
        restore_env("AWS_SESSION_TOKEN", self.token.take());
        restore_env("AWS_EC2_METADATA_DISABLED", self.disabled.take());
    }
}

/// Restores an environment variable to its value before the fixture override.
fn restore_env(name: &str, value: Option<String>) {
    if let Some(value) = value {
        std::env::set_var(name, value);
    } else {
        std::env::remove_var(name);
    }
}
