//! MAN-206: replay every committed synthetic repro and lint the directory.
//! An `expect: "fails"` repro passes while its failure is still present and
//! fails once someone fixes it, which forces the file to be updated in the
//! fixing change (flip `expect` to `"passes"`).

#[path = "support/ghost.rs"]
mod ghost;

use manta_testkit::repro::{self, Expect};
use manta_testkit::scenespec::GENERATOR_REVISION;
use std::path::Path;

fn repros() -> Vec<(std::path::PathBuf, repro::Repro)> {
    repro::lint_dir(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/repros"
    )))
    .unwrap()
}

#[test]
fn the_repro_directory_is_not_empty() {
    assert!(!repros().is_empty());
}

#[test]
fn synthetic_repros_replay_as_recorded() {
    for (path, r) in repros() {
        let at = path.display();
        assert_eq!(
            r.generator_revision, GENERATOR_REVISION,
            "{at}: generator revision"
        );
        let (samples, _) = r.scene.render().unwrap();
        if let Some(want) = r.digests.get(&repro::digest_key()) {
            let got = format!("{:016x}", repro::samples_digest(&samples));
            assert_eq!(&got, want, "{at}: rendered samples moved");
        }
        let got = match r.probe.as_str() {
            "ghost" => ghost::ghost_probe(&r.scene, &samples),
            other => panic!("{at}: unknown probe {other:?}"),
        };
        match r.expect {
            Expect::Fails => assert_eq!(got, Some(r.signature.clone()), "{at}"),
            Expect::Passes => assert_eq!(got, None, "{at}"),
        }
    }
}
