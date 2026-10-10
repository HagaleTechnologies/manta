//! MAN-83: this binary's build identity, fixed at compile time by build.rs.
//! The one source for `--version`, `manta_build_info` (MAN-128) and the JSON
//! stream's `decoderVersion`, so the three cannot disagree. See
//! docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md.

/// `[workspace.package] version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 12-hex commit, a validated `MANTA_GIT_SHA` override, or `unknown`.
pub const GIT_SHA: &str = env!("MANTA_GIT_SHA");
/// Compiled-in Cargo features, sorted and comma-separated, or `none`.
pub const FEATURES: &str = env!("MANTA_FEATURES");
/// What `manta --version` / `-V` prints after `manta `. `concat!` cannot
/// take a `const`, hence the repeated `env!`s.
pub const VERSION_LINE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (git ",
    env!("MANTA_GIT_SHA"),
    "; features: ",
    env!("MANTA_FEATURES"),
    ")"
);
/// JSON spot `decoderVersion`: SemVer with the commit as build metadata.
/// Features stay out on purpose -- they choose input drivers, never decoding.
pub const DECODER_VERSION: &str = concat!(
    "manta-",
    env!("CARGO_PKG_VERSION"),
    "+",
    env!("MANTA_GIT_SHA")
);

#[cfg(test)]
mod tests {
    use super::*;

    /// build.rs's `is_build_metadata`, restated so the constants are checked
    /// independently of the script that produced them.
    fn is_build_metadata(s: &str) -> bool {
        s.len() <= 64
            && s.split('.').all(|id| {
                !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
    }

    #[test]
    fn version_line_is_version_commit_and_features() {
        assert_eq!(
            VERSION_LINE,
            format!("{VERSION} (git {GIT_SHA}; features: {FEATURES})")
        );
    }

    /// build.rs derives the list from `CARGO_CFG_FEATURE`; this is what rustc
    /// itself compiled the crate with. Extend when manta-cli gains a feature.
    #[test]
    fn features_agree_with_the_cfg_flags_rustc_saw() {
        let on: Vec<&str> = [
            ("hpsdr", cfg!(feature = "hpsdr")),
            ("soapy", cfg!(feature = "soapy")),
        ]
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(f, _)| f)
        .collect();
        let want = if on.is_empty() {
            "none".to_string()
        } else {
            on.join(",")
        };
        assert_eq!(FEATURES, want);
    }

    /// MAN-128's "never fails the build": `std::env::vars()` panics on any
    /// non-UTF-8 name or value in the environment build.rs inherits.
    #[test]
    fn build_script_never_decodes_the_whole_environment() {
        let build_rs = include_str!("../build.rs");
        assert!(
            !build_rs.contains("env::vars()"),
            "build.rs must not decode the whole environment with env::vars()"
        );
    }

    #[test]
    fn git_sha_is_semver_build_metadata() {
        assert!(is_build_metadata(GIT_SHA), "{GIT_SHA:?}");
    }

    #[test]
    fn decoder_version_is_semver_with_the_commit_as_build_metadata() {
        assert_eq!(DECODER_VERSION, format!("manta-{VERSION}+{GIT_SHA}"));
        let semver = DECODER_VERSION
            .strip_prefix("manta-")
            .expect("decoderVersion starts with manta-");
        let (core, meta) = semver
            .split_once('+')
            .expect("decoderVersion carries build metadata");
        let release = core.split('-').next().unwrap_or(core);
        let parts: Vec<&str> = release.split('.').collect();
        assert_eq!(parts.len(), 3, "{core:?} is not MAJOR.MINOR.PATCH");
        for p in parts {
            assert!(
                !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()),
                "{core:?} is not MAJOR.MINOR.PATCH"
            );
        }
        assert!(is_build_metadata(meta), "{meta:?}");
    }
}
