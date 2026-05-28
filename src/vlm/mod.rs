pub mod client;
pub mod parser;
pub mod python_compat;
pub mod scheduler;

#[cfg(test)]
pub(crate) mod test_env {
    use std::env;

    pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(crate) struct EnvVarGuard {
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        pub(crate) fn set(name: &'static str, value: &str) -> Self {
            let previous = env::var(name).ok();
            env::set_var(name, value);
            Self { name, previous }
        }

        pub(crate) fn unset(name: &'static str) -> Self {
            let previous = env::var(name).ok();
            env::remove_var(name);
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                env::set_var(self.name, value);
            } else {
                env::remove_var(self.name);
            }
        }
    }
}
