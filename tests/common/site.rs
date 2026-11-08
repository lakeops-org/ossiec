//! Pages of the documentation site that are generated from the tests, so they
//! can't drift from what ossiec does. Each lives in `site/src/generated/`.

/// Check that `site/src/generated/<name>` holds `content`, or write it when
/// `OSSIEC_UPDATE_SITE=1`. A stale page fails the test that generates it.
pub fn site_page(name: &str, content: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("site/src/generated")
        .join(name);
    if std::env::var("OSSIEC_UPDATE_SITE").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "can't read {}: {e}; generate it with `OSSIEC_UPDATE_SITE=1 cargo test`",
            path.display()
        )
    });
    assert!(
        committed == content,
        "{} is out of date: regenerate it with `OSSIEC_UPDATE_SITE=1 cargo test`",
        path.display()
    );
}
