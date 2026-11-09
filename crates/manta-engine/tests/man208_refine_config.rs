//! Config validation at the boundary used by the engine's caller.
use manta_decode::config_file::DecodeConfigFile;

#[test]
fn refine_config_rejects_unknown_keys_and_invalid_budgets() {
    assert!(toml::from_str::<DecodeConfigFile>("[decode.refine]\nunknown = 1\n").is_err());
    for setting in [
        "lookback_secs = 0.5",
        "max_refinements_per_episode = 0",
        "replay_hops_per_batch = 15001",
        "backlog = 257",
    ] {
        let file: DecodeConfigFile =
            toml::from_str(&format!("[decode.refine]\n{setting}\n")).unwrap();
        let error = file
            .decode
            .into_decode_config()
            .refine
            .validate()
            .unwrap_err();
        assert!(error.contains(setting.split_once(' ').unwrap().0));
    }
}
