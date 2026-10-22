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

    /// URL of the status page that links in emails point to; e.g. https://status.example.com/.
    ///
    /// This is usually the aggregate UI. Defaults to the agent's own page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_url: Option<String>,

    /// Name of the node the agent is running on; e.g. a.
    ///
    /// If set, a check fails if its response has an `X-JamesFennell-Node` header naming
    /// this node, or has no such header (see `allow_missing_node_header`). The agent should
    /// check projects on other nodes, so that it notices a node going down; this catches a
    /// project being migrated to the agent's own node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,

    /// Projects to check.
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,

    /// Paths to files that each contain a project config.
    ///
    /// Paths are relative to the directory containing this config file.
    /// Projects loaded from these files are added to `projects`.
    #[serde(default)]
    pub include: Vec<String>,

    pub email_config: Option<email::Config>,
}

/// A project and the URLs to check for it.
///
/// The project is down if any of its checks are down. Several projects with the same name
/// are merged, so a project can be split across files and across agents.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Name of the project.
    pub project: String,

    /// URLs to check for the project.
    pub checks: Vec<CheckConfig>,
}

/// A URL to check periodically.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfig {
    /// Name of the check. Must be unique within the project.
    pub name: String,

    /// URL to request. Redirects are followed.
    pub url: String,

    /// Skip downloading the response body; defaults to false.
    ///
    /// The URL is requested with HEAD instead of GET, which is useful for large files.
    /// It can't be used with `contains`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip_body: bool,

    /// Pass even if the response has no `X-JamesFennell-Node` header; defaults to false.
    ///
    /// Useful for URLs that aren't served by our frontends, such as redirects to a CDN.
    /// The check still fails if the header names the agent's own node.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_missing_node_header: bool,

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

fn ten() -> u64 {
    10
}

fn two() -> u32 {
    2
}

/// Loads the config at the given path, including any projects in `include` files.
///
/// Projects with the same name are merged and the result is sorted by project name.
pub fn load(path: &std::path::Path) -> Result<Config, String> {
    let mut config: Config = read_yaml(path)?;
    let config_dir = path.parent().unwrap_or(std::path::Path::new("."));
    for include in std::mem::take(&mut config.include) {
        let project: ProjectConfig = read_yaml(&config_dir.join(&include))?;
        config.projects.push(project);
    }
    let mut projects: std::collections::BTreeMap<String, ProjectConfig> = Default::default();
    for mut project in std::mem::take(&mut config.projects) {
        match projects.get_mut(&project.project) {
            None => {
                projects.insert(project.project.clone(), project);
            }
            Some(existing) => existing.checks.append(&mut project.checks),
        }
    }
    for project in projects.values() {
        let mut names = std::collections::HashSet::new();
        for check in &project.checks {
            if !names.insert(check.name.clone()) {
                return Err(format!(
                    "duplicate check name {:?} in project {:?}",
                    check.name, project.project
                ));
            }
            if check.failure_threshold == 0 {
                return Err(format!(
                    "check {:?} has a failure_threshold of 0; it must be at least 1",
                    check.name
                ));
            }
            if check.skip_body && check.contains.is_some() {
                return Err(format!(
                    "check {:?} sets both contains and skip_body",
                    check.name
                ));
            }
        }
    }
    config.projects = projects.into_values().collect();
    Ok(config)
}

impl Config {
    /// Returns the URL of the status page, ending in a slash.
    pub fn ui_url(&self) -> String {
        let url = match &self.ui_url {
            None => format!("https://{}/", self.hostname),
            Some(url) => url.clone(),
        };
        if url.ends_with('/') {
            url
        } else {
            format!("{url}/")
        }
    }
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
- ../service/status-2.yml
projects:
- project: inline
  checks:
  - name: inline
    url: https://example.com
",
        );
        write(
            &dir.join("service/status.yml"),
            "
project: service
checks:
- name: service
  url: https://example.com/a
  expected_status: 204
  contains: ok
  timeout_secs: 3
  failure_threshold: 5
",
        );
        write(
            &dir.join("service/status-2.yml"),
            "
project: service
checks:
- name: service docs
  url: https://example.com/docs
  skip_body: true
",
        );

        let config = load(&dir.join("agent/config.yml")).unwrap();

        let names: Vec<(&str, Vec<&str>)> = config
            .projects
            .iter()
            .map(|p| {
                (
                    p.project.as_str(),
                    p.checks.iter().map(|c| c.name.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            names,
            vec![
                ("inline", vec!["inline"]),
                ("service", vec!["service", "service docs"])
            ]
        );
        let inline = &config.projects[0].checks[0];
        assert!(!inline.skip_body);
        assert_eq!(inline.expected_status, None);
        assert_eq!(inline.contains, None);
        assert_eq!(inline.timeout_secs, 10);
        assert_eq!(inline.failure_threshold, 2);
        let service = &config.projects[1].checks[0];
        assert_eq!(service.expected_status, Some(204));
        assert_eq!(service.contains.as_deref(), Some("ok"));
        assert_eq!(service.timeout_secs, 3);
        assert_eq!(service.failure_threshold, 5);
        assert!(config.projects[1].checks[1].skip_body);
        assert_eq!(config.ui_url(), "https://example.com/");
    }

    #[test]
    fn skip_body_with_contains() {
        let dir = test_dir("status_config_skip_body");
        write(
            &dir.join("config.yml"),
            "
hostname: example.com
projects:
- project: service
  checks:
  - name: service
    url: https://example.com
    skip_body: true
    contains: ok
",
        );

        let err = load(&dir.join("config.yml")).unwrap_err();
        assert!(err.contains("skip_body"), "{err}");
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
projects:
- project: service
  checks:
  - name: service
    url: https://example.com
- project: other service
  checks:
  - name: service
    url: https://example.com
",
        );
        write(
            &dir.join("other.yml"),
            "
project: service
checks:
- name: service
  url: https://example.com/other
",
        );

        let err = load(&dir.join("config.yml")).unwrap_err();
        assert!(
            err.contains(r#"duplicate check name "service" in project "service""#),
            "{err}"
        );
    }

    #[test]
    fn unknown_field() {
        let dir = test_dir("status_config_unknown");
        write(
            &dir.join("config.yml"),
            "
hostname: example.com
ui_url: https://status.example.com
projects:
- project: service
  checks:
  - name: service
    url: https://example.com
    contans: ok
",
        );

        let err = load(&dir.join("config.yml")).unwrap_err();
        assert!(err.contains("contans"), "{err}");
    }

    #[test]
    fn ui_url() {
        let config: Config =
            serde_yaml::from_str("hostname: example.com\nui_url: https://status.example.com")
                .unwrap();
        assert_eq!(config.ui_url(), "https://status.example.com/");
    }
}
