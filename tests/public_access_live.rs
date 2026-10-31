//! Explicit read-only smoke check; never runs in the ordinary suite.
#[test]
#[ignore = "requires an explicitly authorized public fixture URL"]
fn authorized_public_range_read() {
    use std::io::Read;
    let url = std::env::var("SKYWRITE_PUBLIC_TEST_URL").expect("explicit public fixture URL");
    let expected_file = std::env::var("SKYWRITE_PUBLIC_TEST_FILE").ok();
    let expected = expected_file.as_ref().map(|path| {
        let file = std::fs::File::open(path).unwrap();
        let bytes = file.metadata().unwrap().len();
        let mut prefix = Vec::new();
        file.take(skywrite::public_access::PREFIX_BYTES as u64)
            .read_to_end(&mut prefix)
            .unwrap();
        (prefix, bytes)
    });
    let report = skywrite::public_access::check(
        &url,
        expected.as_ref().map(|(prefix, _)| prefix.as_slice()),
        expected.as_ref().map(|(_, bytes)| *bytes),
    )
    .unwrap();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert!(report.anonymous_read_succeeded);
    assert_eq!(report.http_status, 206);
    assert_eq!(report.bytes_checked, 1024);
    assert!(!report.full_sha256_verified && !report.provider_fetch_verified);
    if expected_file.is_some() {
        assert_eq!(report.prefix_matches, Some(true));
    }
}
