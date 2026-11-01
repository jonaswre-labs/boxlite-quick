//! The image store's cleanup, on a store of files made up in the test, so no Box boots here.

use boxq::image_store::remove_bad_configs;

/// The SHA-256 of no bytes, in hex.
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// The SHA-256 of `abc`, in hex.
const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[test]
fn only_the_configs_whose_bytes_dont_match_their_digest_are_removed() {
    let home = tempfile::tempdir().unwrap();
    let configs = home.path().join("images/configs");
    std::fs::create_dir_all(&configs).unwrap();
    let matching = configs.join(format!("sha256-{EMPTY}.json"));
    let mismatched = configs.join(format!("sha256-{ABC}.json"));
    let downloading = configs.join(format!("sha256-{ABC}.json.k3x9.downloading"));
    std::fs::write(&matching, "").unwrap();
    std::fs::write(&mismatched, "").unwrap();
    std::fs::write(&downloading, "").unwrap();

    remove_bad_configs(home.path()).unwrap();

    assert!(matching.exists(), "removed a config matching its digest");
    assert!(
        !mismatched.exists(),
        "kept a config not matching its digest"
    );
    assert!(downloading.exists(), "removed a config still downloading");
}
