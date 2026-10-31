use std::process::Command;
fn scratch() -> tempfile::TempDir {
    let root = std::path::Path::new("/home/hermes/.cache/skywrite-development");
    std::fs::create_dir_all(root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
#[test]
fn cumulative_actual_bytes_admitted_before_parsing_reports_and_transcripts() {
    let d = scratch();
    let a = d.path().join("analysis.json");
    let t = d.path().join("words.json");
    std::fs::write(&a, vec![b'x'; 5 * 1024 * 1024]).unwrap();
    std::fs::write(&t, vec![b'y'; 4 * 1024 * 1024]).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(["edit", "suggest", "--analysis"])
        .arg(&a)
        .arg("--transcript")
        .arg(&t)
        .args(["--brief", "test", "--model", "unused/model"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("combined JSON inputs exceed"), "{error}");
    assert!(
        !error.contains("invalid analysis JSON"),
        "admit all bytes before parsing: {error}"
    );
    assert!(
        !error.contains("API key"),
        "reject before provider access: {error}"
    );
}
#[test]
fn transcript_cli_output_protection_precedes_parsing_or_network() {
    let d = scratch();
    let a = d.path().join("analysis.json");
    let t = d.path().join("words.json");
    std::fs::write(&a, b"{}").unwrap();
    std::fs::write(&t, b"keep transcript").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(["edit", "suggest", "--analysis"])
        .arg(&a)
        .arg("--transcript")
        .arg(&t)
        .args(["--brief", "test", "--output"])
        .arg(&t)
        .arg("--overwrite")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("overwrite an input"));
    assert_eq!(std::fs::read(t).unwrap(), b"keep transcript");
}
#[test]
fn bounded_file_set_reads_actual_bytes_and_allows_exact_limit_and_empty_inputs() {
    let d = scratch();
    let a = d.path().join("a");
    let b = d.path().join("b");
    std::fs::write(&a, b"abc").unwrap();
    std::fs::write(&b, b"de").unwrap();
    assert!(skywrite::io::read_files_bounded(&[], 0).unwrap().is_empty());
    assert_eq!(
        skywrite::io::read_files_bounded(&[a.clone(), b.clone()], 5).unwrap(),
        vec![b"abc".to_vec(), b"de".to_vec()]
    );
    assert!(skywrite::io::read_files_bounded(&[a, b], 4).is_err());
}
