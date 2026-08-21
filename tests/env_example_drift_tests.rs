use std::collections::{BTreeMap, BTreeSet};

const CONFIG_SOURCE: &str = include_str!("../src/config.rs");
const ROOT_ENV_EXAMPLE: &str = include_str!("../.env.example");
const DEPLOY_ENV_EXAMPLE: &str = include_str!("../deploy/stellargate.env.example");

fn is_env_key(candidate: &str) -> bool {
    candidate
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && candidate
            .chars()
            .all(|character| {
                character.is_ascii_uppercase()
                    || character.is_ascii_digit()
                    || character == '_'
            })
}

fn config_env_keys() -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let from_env_start = CONFIG_SOURCE
        .find("pub fn from_env()")
        .expect("Config::from_env must exist");
    let from_env_end = CONFIG_SOURCE[from_env_start..]
        .find("\n    pub fn gateway_configured")
        .map(|offset| from_env_start + offset)
        .expect("Config::gateway_configured must follow Config::from_env");
    let from_env_source = &CONFIG_SOURCE[from_env_start..from_env_end];

    // Config::from_env reads some variables directly and routes the rest through
    // these two helpers. Scanning their literal first arguments keeps the guard
    // independent of a second hand-maintained inventory.
    for callee in ["std::env::var(", "parse_env(", "env_or("] {
        let mut remaining = from_env_source;
        while let Some(index) = remaining.find(callee) {
            remaining = &remaining[index + callee.len()..];
            let Some(after_quote) = remaining.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some(end_quote) = after_quote.find('"') else {
                continue;
            };
            let candidate = &after_quote[..end_quote];
            if is_env_key(candidate) {
                keys.insert(candidate.to_owned());
            }
        }
    }

    keys
}

fn documented_env_values(contents: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut values = BTreeMap::<String, BTreeSet<String>>::new();

    for line in contents.lines() {
        let trimmed = line.trim_start();
        let assignment = trimmed.strip_prefix('#').unwrap_or(trimmed).trim_start();
        let Some((key, value)) = assignment.split_once('=') else {
            continue;
        };
        if is_env_key(key) {
            values
                .entry(key.to_owned())
                .or_default()
                .insert(value.trim().to_owned());
        }
    }

    values
}

#[test]
fn root_env_example_exactly_matches_config_reads() {
    let config_keys = config_env_keys();
    let documented_keys = documented_env_values(ROOT_ENV_EXAMPLE)
        .into_keys()
        .collect::<BTreeSet<_>>();

    assert_eq!(
        documented_keys, config_keys,
        ".env.example must document every Config::from_env key and no obsolete keys"
    );
}

#[test]
fn deploy_env_example_matches_config_reads() {
    let config_keys = config_env_keys();
    let deploy_only_keys = BTreeSet::from([
        "ACME_EMAIL".to_owned(),
        "DOMAIN".to_owned(),
        "RUST_LOG".to_owned(),
    ]);
    let application_keys = documented_env_values(DEPLOY_ENV_EXAMPLE)
        .into_keys()
        .filter(|key| !deploy_only_keys.contains(key))
        .collect::<BTreeSet<_>>();

    assert_eq!(
        application_keys, config_keys,
        "deploy/stellargate.env.example must document every Config::from_env key and no obsolete application keys"
    );
}

#[test]
fn accepted_asset_examples_agree_across_env_files() {
    let root_values = documented_env_values(ROOT_ENV_EXAMPLE);
    let deploy_values = documented_env_values(DEPLOY_ENV_EXAMPLE);
    let expected = BTreeSet::from([
        "XLM,USDC:GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".to_owned(),
        "XLM,USDC:GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN".to_owned(),
    ]);

    assert_eq!(root_values.get("ACCEPTED_ASSETS"), Some(&expected));
    assert_eq!(deploy_values.get("ACCEPTED_ASSETS"), Some(&expected));
    assert!(!root_values.contains_key("USDC_ISSUER"));
    assert!(!deploy_values.contains_key("USDC_ISSUER"));
}
