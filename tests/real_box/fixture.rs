//! The tests' own [`Fixture`]: its BoxLite home shares the images the run has downloaded
//! (jonaswre/boxlite-quick#44), unless it is cold.

use super::*;

/// The references of the images in `f`'s BoxLite home, as BoxLite lists them.
async fn images(f: &Fixture) -> Vec<String> {
    let rt = f.runtime();
    let images = rt.images().unwrap().list().await.unwrap();
    images.into_iter().map(|i| i.reference).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "pulls images; needs internet access"]
async fn a_fixtures_home_starts_with_the_small_images_the_tests_use() {
    let f = Fixture::new();

    let images = images(&f).await;

    for image in [IMAGE, GIT_IMAGE, NON_ROOT_IMAGE] {
        assert!(
            images.iter().any(|i| i == image),
            "no {image} in {images:?}"
        );
    }
}

/// Small, and pulled by no other test, so its config in [`IMAGE_STORE`] is this test's to empty.
const EMPTIED_IMAGE: &str = "mirror.gcr.io/library/busybox:1.37";

/// BoxLite 0.11 can rename a config into the image store before its bytes are written, then read
/// it back empty (jonaswre/boxlite-quick#54). An empty config left in the store is what it reads.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "pulls images; needs internet access"]
async fn a_config_found_empty_in_the_image_store_is_downloaded_again() {
    let f = Fixture::cold();
    let digest = {
        let rt = f.runtime();
        let pulled = rt.images().unwrap().pull(EMPTIED_IMAGE).await.unwrap();
        pulled.config_digest().to_owned()
    };
    let config = IMAGE_STORE
        .join("configs")
        .join(format!("{}.json", digest.replace(':', "-")));
    std::fs::write(&config, "").unwrap();

    pull_into_store(&IMAGE_STORE.join("emptied"), &[EMPTIED_IMAGE]);

    let bytes = std::fs::read(&config).unwrap();
    assert_eq!(format!("sha256:{}", sha256_hex(&[&bytes])), digest);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "opens a BoxLite runtime"]
async fn a_cold_fixtures_home_starts_with_no_images() {
    let f = Fixture::cold();

    assert_eq!(images(&f).await, Vec::<String>::new());
}
