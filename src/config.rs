//! Configuration for the agent.

use crate::email;

/// Configuration for the agent.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Hostname for the agent is running on; e.g. status.example.com.
    ///
    /// This is used when sending emails and on the status page.
    pub hostname: String,

    /// List of URLs to check.
    #[serde(default)]
    pub checks: Vec<CheckConfig>,

    /// Paths to files that each contain a list of check configs.
    ///
    /// Paths are relative to the directory containing this config file.
    /// Checks loaded from these files are added to `checks`.
    #[serde(default)]
    pub include: Vec<String>,

    pub email_config: Option<email::Config>,
}

/// A URL to check periodically.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfig {
    /// Name of the check. Must be unique within the agent.
    pub name: String,

    /// URL to request. Redirects are followed.
    pub url: String,

    /// How the check is shown on the status page by default.
    ///
    /// Either `status` (only the current state) or `history` (daily uptime over the last 90 days).
    /// The page has a toggle to switch all checks to either view.
    #[serde(default)]
    pub view: View,

    /// HTTP status code the response must have.
    ///
    /// If not set, any 2xx status code is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_status: Option<u16>,

    /// String the response body must contain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,

    /// Seconds to wait for a response before the check fails; defaults to 10.
    #[serde(default = "ten")]
    pub timeout_secs: u64,

    /// Number of consecutive failed checks before the URL is considered down
    /// and an email is sent; defaults to 2.
    #[serde(default = "two")]
    pub failure_threshold: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Status,
    History,
}

fn ten() -> u64 {
    10
}

fn two() -> u32 {
    2
}

/// Loads the config at the given path, including any checks in `include` files.
pub fn load(path: &std::path::Path) -> Result<Config, String> {
    let mut config: Config = read_yaml(path)?;
    let config_dir = path.parent().unwrap_or(std::path::Path::new("."));
    for include in std::mem::take(&mut config.include) {
        let mut checks: Vec<CheckConfig> = read_yaml(&config_dir.join(&include))?;
        config.checks.append(&mut checks);
    }
    let mut names = std::collections::HashSet::new();
    for check in &config.checks {
        if !names.insert(check.name.clone()) {
            return Err(format!("duplicate check name {:?}", check.name));
        }
        if check.failure_threshold == 0 {
            return Err(format!(
                "check {:?} has a failure_threshold of 0; it must be at least 1",
                check.name
            ));
        }
    }
    Ok(config)
}

fn read_yaml<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<T, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            return Err(format!(
                "failed to read configuration file {}: {err}",
                path.display()
            ))
        }
    };
    match serde_yaml::from_str(&raw) {
        Ok(t) => Ok(t),
        Err(err) => Err(format!(
            "failed to parse YAML configuration file {}: {err}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &std::path::Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn include_and_defaults() {
        let dir = test_dir("status_config_include");
        write(
            &dir.join("agent/config.yml"),
            "
hostname: example.com
include:
- ../service/status.yml
checks:
- name: inline
  url: https://example.com
",
        );
        write(
            &dir.join("service/status.yml"),
            "
- name: service
  url: https://example.com/a
  view: history
  expected_status: 204
  contains: ok
  timeout_secs: 3
  failure_threshold: 5
- name: service docs
  url: https://example.com/docs
",
        );

        let config = load(&dir.join("agent/config.yml")).unwrap();

        let names: Vec<&str> = config.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["inline", "service", "service docs"]);
        let inline = &config.checks[0];
        assert_eq!(inline.view, View::Status);
        assert_eq!(inline.expected_status, None);
        assert_eq!(inline.contains, None);
        assert_eq!(inline.timeout_secs, 10);
        assert_eq!(inline.failure_threshold, 2);
        let service = &config.checks[1];
        assert_eq!(service.view, View::History);
        assert_eq!(service.expected_status, Some(204));
        assert_eq!(service.contains.as_deref(), Some("ok"));
        assert_eq!(service.timeout_secs, 3);
        assert_eq!(service.failure_threshold, 5);
    }

    #[test]
    fn duplicate_names() {
        let dir = test_dir("status_config_duplicate");
        write(
            &dir.join("config.yml"),
            "
hostname: example.com
include:
- other.yml
checks:
- name: service
  url: https://example.com
",
        );
        write(
            &dir.join("other.yml"),
            "
- name: service
  url: https://example.com/other
",
        );

        let err = load(&dir.join("config.yml")).unwrap_err();
        assert!(err.contains("duplicate check name"), "{err}");
    }

    #[test]
    fn unknown_field() {
        let dir = test_dir("status_config_unknown");
        write(
            &dir.join("config.yml"),
            "
hostname: example.com
checks:
- name: service
  url: https://example.com
  contans: ok
",
        );

        let err = load(&dir.join("config.yml")).unwrap_err();
        assert!(err.contains("contans"), "{err}");
    }
}
