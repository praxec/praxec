//! The scenario file format for `praxec test`.

use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq)]
pub struct ScenarioFile {
    pub tests: Vec<Scenario>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Scenario {
    pub name: String,
    pub workflow: String,
    #[serde(default = "default_iterations")]
    pub iterations: usize,
    #[serde(default)]
    pub seed: u64,
    pub expect: Expect,
}

fn default_iterations() -> usize {
    20
}

#[derive(Debug, Deserialize, PartialEq, Default)]
pub struct Expect {
    #[serde(default)]
    pub reaches: Vec<String>,
    #[serde(default)]
    pub never_reaches: Vec<String>,
    #[serde(default)]
    pub final_state: Vec<String>,
    #[serde(default)]
    pub outcome_met: Vec<String>,
}

pub fn parse_scenarios(yaml: &str) -> anyhow::Result<ScenarioFile> {
    Ok(serde_yaml::from_str(yaml)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_scenario() {
        let y = r#"
tests:
  - name: ship gated
    workflow: ship_guard
    iterations: 5
    expect:
      never_reaches: [shipped]
      reaches: [checked]
"#;
        let f = parse_scenarios(y).unwrap();
        assert_eq!(f.tests.len(), 1);
        let t = &f.tests[0];
        assert_eq!(t.name, "ship gated");
        assert_eq!(t.workflow, "ship_guard");
        assert_eq!(t.iterations, 5);
        assert_eq!(t.expect.never_reaches, vec!["shipped".to_string()]);
        assert_eq!(t.expect.reaches, vec!["checked".to_string()]);
    }

    #[test]
    fn defaults_apply() {
        let f = parse_scenarios("tests:\n  - name: t\n    workflow: w\n    expect: {}\n").unwrap();
        assert_eq!(f.tests[0].iterations, 20);
        assert_eq!(f.tests[0].seed, 0);
        assert!(f.tests[0].expect.reaches.is_empty());
    }
}
