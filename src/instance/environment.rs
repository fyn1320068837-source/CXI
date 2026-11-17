use super::{HTTP_PROXY_ENDPOINT, LOOPBACK_NO_PROXY, SOCKS_PROXY_ENV_ENDPOINT};
use crate::geoip::GeoProfile;
use std::process::Command;

/// Start each managed client from a small, deterministic environment. The
/// caller supplies the executable search path because Desktop and the CLI
/// have different bundle/Homebrew locations.
pub(crate) fn clear_instance_environment(command: &mut Command, safe_path: &str) {
    command.env_clear().env("PATH", safe_path);
    if let Ok(user) = std::env::var("USER") {
        command.env("USER", &user).env("LOGNAME", user);
    }
}

/// Keep client configuration, caches and temporary files below its private
/// instance root. `home_dir` may intentionally remain the host HOME for
/// Desktop Lite so macOS Keychain lookup keeps working.
pub(crate) fn apply_instance_directories(
    command: &mut Command,
    instance_dir: &str,
    home_dir: &str,
) {
    command
        .env("HOME", home_dir)
        .env("TMPDIR", format!("{instance_dir}/tmp"))
        .env("XDG_CONFIG_HOME", format!("{instance_dir}/config"))
        .env("XDG_CACHE_HOME", format!("{instance_dir}/cache"))
        .env("XDG_DATA_HOME", format!("{instance_dir}/data"))
        .env("XDG_STATE_HOME", format!("{instance_dir}/state"));
}

pub(crate) fn apply_proxy_environment(command: &mut Command) {
    command
        .env("HTTPS_PROXY", HTTP_PROXY_ENDPOINT)
        .env("HTTP_PROXY", HTTP_PROXY_ENDPOINT)
        .env("ALL_PROXY", SOCKS_PROXY_ENV_ENDPOINT)
        .env("NO_PROXY", LOOPBACK_NO_PROXY);
}

pub(crate) fn apply_locale_env(command: &mut Command, profile: &GeoProfile) {
    if let Some(timezone) = profile.timezone.as_deref() {
        command.env("TZ", timezone);
    }
    if let Some(locale) = profile.locale.as_deref() {
        command.env("LANG", locale);
        command.env("LC_ALL", locale);
    }
}
