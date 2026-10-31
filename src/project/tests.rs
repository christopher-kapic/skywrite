use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
struct Server {
    base: String,
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<String>>>,
    done: Arc<Mutex<Vec<String>>>,
    max: Arc<AtomicUsize>,
    slow_release: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}
// Releasing while unwinding prevents the scope join from depending on a failed
// test assertion. The server also has a bounded wait and a drop-time fallback.
struct ReleaseSlow(Arc<AtomicBool>);
impl Drop for ReleaseSlow {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.slow_release.store(true, Ordering::SeqCst);
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let result = h.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
fn request(mut socket: &TcpStream) -> Value {
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = vec![];
    let mut buf = [0; 4096];
    let end = loop {
        let n = socket.read(&mut buf).unwrap();
        assert!(n > 0);
        bytes.extend(&buf[..n]);
        if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let header = String::from_utf8_lossy(&bytes[..end]);
    assert!(header.starts_with("POST /images "), "{header}");
    let len = header
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .and_then(|s| s.parse::<usize>().ok())
        })
        .unwrap();
    while bytes.len() < end + len {
        let n = socket.read(&mut buf).unwrap();
        assert!(n > 0);
        bytes.extend(&buf[..n]);
    }
    serde_json::from_slice(&bytes[end..end + len]).unwrap()
}
fn server(png: Vec<u8>, block_output: Option<PathBuf>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(vec![]));
    let done = Arc::new(Mutex::new(vec![]));
    let max = Arc::new(AtomicUsize::new(0));
    let slow_release = Arc::new(AtomicBool::new(false));
    let held_slow = slow_release.clone();
    let (end, observed, finished, peak) = (stop.clone(), seen.clone(), done.clone(), max.clone());
    let handle = std::thread::spawn(move || {
        std::thread::scope(|scope| {
            let active = Arc::new(AtomicUsize::new(0));
            while !end.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let observed = observed.clone();
                        let finished = finished.clone();
                        let active = active.clone();
                        let peak = peak.clone();
                        let png = png.clone();
                        let block_output = block_output.clone();
                        let slow_release = held_slow.clone();
                        scope.spawn(move||{
                let body=request(&socket);let prompt=body["prompt"].as_str().unwrap().to_owned();let count=active.fetch_add(1,Ordering::SeqCst)+1;peak.fetch_max(count,Ordering::SeqCst);
                let home=crate::workspace::personal_home().unwrap();
                let intent=fs::read_dir(home.join("jobs")).unwrap().any(|e|{let p=e.unwrap().path().join("job.json");if !p.is_file(){return false;}let v:Value=crate::io::read_json(&p).unwrap();v["status"]=="submitting" && v["request"]["prompt"]==prompt});assert!(intent,"POST must have durable job intent");
                let admitted=fs::read_dir(home.join("projects")).unwrap().any(|e|{let p=e.unwrap().path().join("state.json");if !p.is_file(){return false;}let v:Value=crate::io::read_json(&p).unwrap();v["nodes"].as_object().unwrap().values().any(|s|s["status"]=="running" && s["planned_id"].is_string())});assert!(admitted,"POST must have durable project admission");
                observed.lock().unwrap().push(prompt.clone());
                if prompt=="slow"{wait_for(|| slow_release.load(Ordering::SeqCst));}else if prompt.contains("slow"){std::thread::sleep(Duration::from_millis(300));}else if prompt.contains("crash"){std::thread::sleep(Duration::from_millis(1200));}else{std::thread::sleep(Duration::from_millis(30));}
                if prompt=="output-block" {fs::create_dir_all(block_output.unwrap()).unwrap();}
                let mime=if png.starts_with(&[0xff,0xd8,0xff]) {"image/jpeg"}else{"image/png"};
                let (status,value)=if prompt=="fail" {("400 Bad Request",json!({"error":"owned fixture rejection"}))}else{("200 OK",json!({"data":[{"media_type":mime,"b64_json":STANDARD.encode(png)}]}))};
                let response=serde_json::to_vec(&value).unwrap();let header=format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",response.len());let _=socket.write_all(header.as_bytes());let _=socket.write_all(&response);finished.lock().unwrap().push(prompt);active.fetch_sub(1,Ordering::SeqCst);
            });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        })
    });
    Server {
        base,
        stop,
        seen,
        done,
        max,
        slow_release,
        handle: Some(handle),
    }
}
fn catalog(cache: &Path, endpoint: &str, data: Value) {
    let key = crate::openrouter::cache_key(&[b"catalog-v1", endpoint.as_bytes()]);
    crate::io::write_json(
        &cache.join("models").join(format!("{key}.json")),
        &json!({"version":1,"endpoint":endpoint,"fetched_at":1,"body":{"data":data}}),
        false,
    )
    .unwrap();
}
fn args(manifest: &Path, cache: &Path) -> RunArgs {
    RunArgs {
        manifest: manifest.into(),
        concurrency: 2,
        keep_going: false,
        cache_dir: Some(cache.into()),
        offline: true,
        wait: 0,
    }
}
fn image_node(id: &str, prompt: &str) -> Value {
    json!({"id":id,"output":format!("{id}.png"),"model":"fixture/image","action":{"type":"image","prompt":prompt}})
}
fn manifest(root: &Path, name: &str, nodes: Vec<Value>) -> PathBuf {
    let p = root.join(format!("{name}.json"));
    crate::io::write_json(
        &p,
        &json!({"version":1,"output_root":name,"nodes":nodes}),
        false,
    )
    .unwrap();
    p
}
fn engine(server: &Server) -> Engine {
    Engine {
        base: Some(server.base.clone()),
    }
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "fixture condition timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn durable_http_scheduler_and_recovery() {
    const SELECTOR: &str = "project::tests::durable_http_scheduler_and_recovery";
    match std::env::var("SKYWRITE_PROJECT_TEST_MODE").ok().as_deref() {
        None => {
            let temp = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", SELECTOR, "--nocapture"])
                .env("SKYWRITE_PROJECT_TEST_MODE", "suite")
                .env("SKYWRITE_HOME", temp.path().join("home"))
                .env("SKYWRITE_PROJECT_TEST_ROOT", temp.path())
                .env_remove("OPENROUTER_API_KEY")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        Some("crash") => {
            let root = PathBuf::from(std::env::var_os("SKYWRITE_PROJECT_TEST_ROOT").unwrap());
            let base = std::env::var("SKYWRITE_PROJECT_TEST_BASE").unwrap();
            let _ = run(
                args(&root.join("crash.json"), &root.join("cache")),
                false,
                Engine { base: Some(base) },
            );
            return;
        }
        _ => (),
    }
    let root = PathBuf::from(std::env::var_os("SKYWRITE_PROJECT_TEST_ROOT").unwrap());
    let cache = root.join("cache");
    catalog(
        &cache,
        "images/models",
        json!([{"id":"fixture/image","supported_parameters":{"input_references":{"min":0,"max":10}}}]),
    );
    catalog(&cache, "models?output_modalities=image", json!([]));
    let png_path = root.join("real.png");
    let mut ff = crate::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=blue:s=32x32:d=0.1",
        "-frames:v",
        "1",
    ]);
    ff.push(png_path.as_os_str().to_owned());
    crate::process::run("ffmpeg", &ff, Duration::from_secs(15), 1024 * 1024).unwrap();
    let png = fs::read(&png_path).unwrap();
    let s = server(png.clone(), None);
    let mut dep = image_node("dependency", "after-fast");
    dep["action"]["references"] = json!([{"node":"fast"}]);
    let path = manifest(
        &root,
        "graph",
        vec![image_node("slow", "slow"), image_node("fast", "fast"), dep],
    );
    std::thread::scope(|scope| {
        let release_slow = ReleaseSlow(s.slow_release.clone());
        let h = scope.spawn(|| run(args(&path, &cache), false, engine(&s)).unwrap());
        wait_for(|| s.seen.lock().unwrap().len() >= 2);
        let err = run(args(&path, &cache), false, engine(&s)).unwrap_err();
        assert!(err.to_string().contains("locked"));
        // Keep a real HTTP response pending until both the independent fast
        // node and its dependent complete, rather than assuming 300ms overlaps.
        wait_for(|| s.done.lock().unwrap().iter().any(|p| p == "after-fast"));
        drop(release_slow);
        let result = h.join().unwrap();
        assert_eq!(result["complete"], true, "{result}");
    });
    assert_eq!(s.max.load(Ordering::SeqCst), 2);
    let done = s.done.lock().unwrap().clone();
    assert_eq!(done[0], "fast");
    assert!(
        done.iter().position(|p| p == "after-fast").unwrap()
            > done.iter().position(|p| p == "fast").unwrap()
    );
    // Crash after publishing, before project completion save: adopt recorded artifact bytes.
    let loaded = load(&path).unwrap();
    let state_root = state_dir(&loaded).unwrap();
    let mut state = read_state(&loaded, &state_root).unwrap().unwrap();
    state.nodes.get_mut("fast").unwrap().status = "running".into();
    state.nodes.get_mut("fast").unwrap().output_sha256 = None;
    save(&state_root, &state).unwrap();
    assert_eq!(
        run(args(&path, &cache), true, engine(&s)).unwrap()["complete"],
        true
    );
    let count = s.seen.lock().unwrap().len();
    assert_eq!(
        run(args(&path, &cache), true, Engine::default()).unwrap()["complete"],
        true
    );
    assert_eq!(s.seen.lock().unwrap().len(), count);
    // Lost directory entries are republished from verified local jobs keylessly.
    fs::remove_file(root.join("graph/fast.png")).unwrap();
    assert_eq!(
        run(args(&path, &cache), true, Engine::default()).unwrap()["complete"],
        true
    );
    assert_eq!(fs::read(root.join("graph/fast.png")).unwrap(), png);
    assert_eq!(s.seen.lock().unwrap().len(), count);
    fs::write(root.join("graph/fast.png"), b"changed").unwrap();
    assert!(run(args(&path, &cache), true, engine(&s)).is_err());
    assert_eq!(fs::read(root.join("graph/fast.png")).unwrap(), b"changed");
    fs::write(root.join("graph/fast.png"), &png).unwrap();
    let original = fs::read(&path).unwrap();
    fs::write(&path, [original.clone(), b" ".to_vec()].concat()).unwrap();
    assert!(
        run(args(&path, &cache), true, engine(&s))
            .unwrap_err()
            .to_string()
            .contains("manifest changed")
    );
    fs::write(&path, original).unwrap();
    crate::workspace::set_default_model("image-generation", "fixture/image").unwrap();
    let fail = manifest(
        &root,
        "failgraph",
        vec![
            image_node("slow", "slow-failure-sibling"),
            image_node("bad", "fail"),
            {
                let mut node = image_node("never", "never-admitted");
                node["model"] = Value::Null;
                node
            },
        ],
    );
    let result = run(args(&fail, &cache), false, engine(&s)).unwrap();
    assert_eq!(result["counts"]["pending"], 1);
    assert_eq!(result["counts"]["completed"], 1);
    assert!(!s.seen.lock().unwrap().iter().any(|p| p == "never-admitted"));
    assert!(
        s.done
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == "slow-failure-sibling")
    );
    // A pending first admission on resume retains its model after defaults change.
    crate::workspace::set_default_model("image-generation", "fixture/changed-unlisted").unwrap();
    let mut resume_args = args(&fail, &cache);
    resume_args.keep_going = true;
    let continued = run(resume_args, true, engine(&s)).unwrap();
    let never = continued["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == "never")
        .unwrap();
    assert_eq!(never["status"], "completed");
    assert_eq!(never["model"], "fixture/image");
    let blocked = manifest(&root, "blocked", vec![image_node("one", "output-block")]);
    let bs = server(png.clone(), Some(root.join("blocked/one.png")));
    assert_eq!(
        run(args(&blocked, &cache), false, engine(&bs)).unwrap()["counts"]["uncertain"],
        1
    );
    fs::remove_dir(root.join("blocked/one.png")).unwrap();
    assert_eq!(
        run(args(&blocked, &cache), true, engine(&bs)).unwrap()["complete"],
        true
    );
    assert_eq!(bs.seen.lock().unwrap().len(), 1);
    // A JPEG provider artifact becomes PNG at publication. A crash between that
    // publication and project completion must adopt the converted bytes keylessly.
    let jpeg_path = root.join("provider.jpg");
    crate::process::run(
        "ffmpeg",
        &crate::process::args(&[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-i",
            png_path.to_str().unwrap(),
            "-frames:v",
            "1",
            jpeg_path.to_str().unwrap(),
        ]),
        Duration::from_secs(15),
        1024 * 1024,
    )
    .unwrap();
    let jpeg = fs::read(&jpeg_path).unwrap();
    let js = server(jpeg, None);
    let converted = manifest(
        &root,
        "converted",
        vec![image_node("one", "jpeg-conversion")],
    );
    assert_eq!(
        run(args(&converted, &cache), false, engine(&js)).unwrap()["complete"],
        true
    );
    let loaded = load(&converted).unwrap();
    let r = state_dir(&loaded).unwrap();
    let mut state = read_state(&loaded, &r).unwrap().unwrap();
    let receipt = crate::generation::execute_jobs(crate::generation::JobsCommand::Status {
        id: state.nodes["one"].planned_id.clone().unwrap(),
        refresh: false,
    })
    .unwrap();
    let published = root.join("converted/one.png");
    assert_ne!(
        crate::media::file_sha256(&published).unwrap(),
        receipt["artifact"]["sha256"]
    );
    state.nodes.get_mut("one").unwrap().status = "running".into();
    state.nodes.get_mut("one").unwrap().output_sha256 = None;
    save(&r, &state).unwrap();
    let publication = fs::read(&published).unwrap();
    fs::write(&published, b"unrelated-existing-output").unwrap();
    assert_eq!(
        run(args(&converted, &cache), true, Engine::default()).unwrap()["counts"]["uncertain"],
        1
    );
    assert_eq!(fs::read(&published).unwrap(), b"unrelated-existing-output");
    fs::write(&published, &publication).unwrap();
    assert_eq!(
        run(args(&converted, &cache), true, Engine::default()).unwrap()["complete"],
        true
    );
    fs::remove_file(&published).unwrap();
    assert_eq!(
        run(args(&converted, &cache), true, Engine::default()).unwrap()["complete"],
        true
    );
    assert_eq!(fs::read(&published).unwrap(), publication);
    assert_eq!(js.seen.lock().unwrap().len(), 1);
    // Distinct nodes with identical frozen requests share one global job lock/ID.
    let ds = server(png.clone(), None);
    let duplicates = manifest(
        &root,
        "duplicates",
        vec![
            image_node("first", "same-request"),
            image_node("second", "same-request"),
        ],
    );
    let duplicate_result = run(args(&duplicates, &cache), false, engine(&ds)).unwrap();
    assert_eq!(duplicate_result["complete"], true);
    assert_eq!(
        duplicate_result["nodes"][0]["job_id"],
        duplicate_result["nodes"][1]["job_id"]
    );
    assert_eq!(ds.seen.lock().unwrap().len(), 1);
    let crash = manifest(&root, "crash", vec![image_node("one", "crash")]);
    let cs = server(png.clone(), None);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", SELECTOR, "--nocapture"])
        .env("SKYWRITE_PROJECT_TEST_MODE", "crash")
        .env("SKYWRITE_PROJECT_TEST_BASE", &cs.base)
        .spawn()
        .unwrap();
    wait_for(|| !cs.seen.lock().unwrap().is_empty());
    child.kill().unwrap();
    let _ = child.wait().unwrap();
    let result = run(args(&crash, &cache), true, engine(&cs)).unwrap();
    assert_eq!(result["counts"]["uncertain"], 1, "{result}");
    assert_eq!(cs.seen.lock().unwrap().len(), 1);
    let missing = manifest(&root, "missing", vec![image_node("one", "missing")]);
    let l = load(&missing).unwrap();
    let r = state_dir(&l).unwrap();
    let mut state = initial(&l).unwrap();
    state.nodes.get_mut("one").unwrap().status = "running".into();
    let missing_plan = crate::generation::execute_generate(
        command(
            &l,
            &l.manifest.nodes[0],
            "fixture/image",
            &args(&missing, &cache),
            true,
            None,
        )
        .unwrap(),
    )
    .unwrap();
    state.nodes.get_mut("one").unwrap().planned_id =
        Some(missing_plan["id"].as_str().unwrap().into());
    save(&r, &state).unwrap();
    let before_missing = s.seen.lock().unwrap().len();
    let missing_result = run(args(&missing, &cache), true, engine(&s)).unwrap();
    let after_missing = s.seen.lock().unwrap().len();
    eprintln!("missing receipt owned HTTP counter: before={before_missing}, after={after_missing}");
    assert_eq!(
        after_missing, before_missing,
        "a started node with no receipt cannot submit"
    );
    assert_eq!(missing_result["counts"]["uncertain"], 1);
    assert_eq!(s.seen.lock().unwrap().len(), count + 3);
    frozen_plan_probe(&root, &cache, &png_path, &s, &png, &path);
    identity_guard_probe(&root, &cache, &png_path, &s, &png);
}
fn frozen_plan_probe(
    root: &Path,
    cache: &Path,
    input: &Path,
    server: &Server,
    png: &[u8],
    completed: &Path,
) {
    let changed = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQZkAAAAASUVORK5CYII=").unwrap();
    let mut node = image_node("one", "frozen-source-race");
    node["output"] = json!("nested/deeper/one.png");
    node["action"]["references"] = json!([{"file":"real.png"}]);
    let path = manifest(root, "frozen-source", vec![node]);
    let before = server.seen.lock().unwrap().len();
    let target = input.to_owned();
    let replacement = changed.clone();
    BEFORE_PLAN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || fs::write(target, replacement).unwrap()))
    });
    let result = run(args(&path, cache), false, engine(server));
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("planned input differs")
    );
    assert_eq!(
        server.seen.lock().unwrap().len(),
        before,
        "hash-check to plan edit must not POST"
    );
    fs::write(input, png).unwrap();
    // Returned original contents are legitimate; unchanged planning still works.
    let target = input.to_owned();
    let original = png.to_vec();
    BEFORE_PLAN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::write(&target, b"temporary editor contents").unwrap();
            fs::write(&target, original).unwrap();
        }))
    });
    assert_eq!(
        run(args(&path, cache), true, engine(server)).unwrap()["complete"],
        true
    );
    assert_eq!(server.seen.lock().unwrap().len(), before + 1);
    assert_eq!(fs::read(input).unwrap(), png);
    // The same binding applies to a completed dependency publication.
    let mut dependent = image_node("dependent", "frozen-dependency-race");
    dependent["action"]["references"] = json!([{"node":"fast"}]);
    let dependency = manifest(
        root,
        "frozen-dependency",
        vec![image_node("fast", "fast"), dependent],
    );
    let l = load(&dependency).unwrap();
    let r = state_dir(&l).unwrap();
    let mut state = initial(&l).unwrap();
    let previous = load(completed).unwrap();
    let old = read_state(&previous, &state_dir(&previous).unwrap())
        .unwrap()
        .unwrap();
    state.nodes.insert("fast".into(), old.nodes["fast"].clone());
    fs::create_dir_all(root.join("frozen-dependency")).unwrap();
    let target = root.join("frozen-dependency/fast.png");
    fs::write(&target, png).unwrap();
    save(&r, &state).unwrap();
    let mutated = target.clone();
    BEFORE_PLAN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || fs::write(mutated, changed).unwrap()))
    });
    let result = run(args(&dependency, cache), true, engine(server));
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("planned input differs")
    );
    assert_eq!(
        server.seen.lock().unwrap().len(),
        before + 1,
        "dependency edit must not POST"
    );
    fs::write(&target, png).unwrap();
    assert_eq!(
        run(args(&dependency, cache), true, engine(server)).unwrap()["complete"],
        true
    );
    assert_eq!(server.seen.lock().unwrap().len(), before + 2);
    eprintln!(
        "frozen plan HTTP counters: rejected source/dependency edits without POST; restored content admitted"
    );
}
fn identity_guard_probe(root: &Path, cache: &Path, png_path: &Path, s: &Server, png: &[u8]) {
    let mut changed = image_node("one", "guard-original");
    changed["action"]["references"] = json!([{"file":"real.png"}]);
    let guard = manifest(root, "guard", vec![changed]);
    let l = load(&guard).unwrap();
    let a = args(&guard, cache);
    let n = &l.manifest.nodes[0];
    let plan = crate::generation::execute_generate(
        command(&l, n, "fixture/image", &a, true, None).unwrap(),
    )
    .unwrap();
    fs::write(png_path,STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQZkAAAAASUVORK5CYII=").unwrap()).unwrap();
    let planned = plan["id"].as_str().unwrap();
    let guard_root = state_dir(&l).unwrap();
    let mut state = initial(&l).unwrap();
    let node_state = state.nodes.get_mut("one").unwrap();
    node_state.status = "running".into();
    node_state.planned_id = Some(planned.into());
    save(&guard_root, &state).unwrap();
    let before = s.seen.lock().unwrap().len();
    let mutated_plan = crate::generation::execute_generate(
        command(&l, n, "fixture/image", &a, true, None).unwrap(),
    )
    .unwrap();
    let mutated_id = mutated_plan["id"].as_str().unwrap();
    let result = engine(s).generate(
        command(&l, n, "fixture/image", &a, false, None).unwrap(),
        planned,
    );
    let after = s.seen.lock().unwrap().len();
    eprintln!("planned identity owned HTTP counter: before={before}, after={after}");
    assert_eq!(after, before, "changed planned identity cannot submit");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("planned identity changed"),
        "{err:#}"
    );
    assert_eq!(s.seen.lock().unwrap().len(), before);
    assert!(
        !crate::workspace::personal_home()
            .unwrap()
            .join("jobs")
            .join(planned)
            .is_dir()
    );
    assert!(
        !crate::workspace::personal_home()
            .unwrap()
            .join("jobs")
            .join(mutated_id)
            .is_dir()
    );
    fs::write(png_path, png).unwrap();
    let input = manifest(
        root,
        "input",
        vec![{
            let mut n = image_node("one", "input");
            n["action"]["references"] = json!([{"file":"real.png"}]);
            n
        }],
    );
    let l = load(&input).unwrap();
    let r = state_dir(&l).unwrap();
    save(&r, &initial(&l).unwrap()).unwrap();
    fs::write(png_path, b"modified").unwrap();
    assert!(
        run(args(&input, cache), true, engine(s))
            .unwrap_err()
            .to_string()
            .contains("input changed")
    );
}
#[test]
fn structural_contract_and_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let p = manifest(temp.path(), "valid", vec![image_node("one", "hello")]);
    assert!(load(&p).is_ok());
    let original: Value = crate::io::read_json(&p).unwrap();
    let mut bounded = original.clone();
    bounded["nodes"] = json!(
        (0..256)
            .map(|i| image_node(&format!("node{i}"), "bounded"))
            .collect::<Vec<_>>()
    );
    crate::io::write_json(&p, &bounded, true).unwrap();
    assert!(load(&p).is_ok());
    bounded["nodes"]
        .as_array_mut()
        .unwrap()
        .push(image_node("extra", "too many"));
    crate::io::write_json(&p, &bounded, true).unwrap();
    assert!(load(&p).is_err());
    for mutate in ["traversal", "duplicate", "cycle", "shell", "shot"] {
        let mut v = original.clone();
        match mutate {
            "traversal" => v["nodes"][0]["output"] = json!("../escape.png"),
            "duplicate" => {
                let n = v["nodes"][0].clone();
                v["nodes"].as_array_mut().unwrap().push(n);
            }
            "cycle" => v["nodes"][0]["depends_on"] = json!(["one"]),
            "shell" => v["nodes"][0]["action"] = json!({"type":"shell","command":"echo no"}),
            _ => v["nodes"][0]["shot"] = json!({"size":"wide","angle":"level","motion":"locked"}),
        };
        crate::io::write_json(&p, &v, true).unwrap();
        assert!(load(&p).is_err(), "{mutate}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // ENOTDIR must remain an error, never admission as an absent output.
        fs::write(temp.path().join("ancestor"), b"preserved").unwrap();
        assert!(safe_path(temp.path(), Path::new("ancestor/out.png")).is_err());
        assert!(present(&temp.path().join("ancestor/out.png")).is_err());
        assert_eq!(
            fs::read(temp.path().join("ancestor")).unwrap(),
            b"preserved"
        );
        crate::io::write_json(&p, &original, true).unwrap();
        symlink(temp.path(), temp.path().join("valid")).unwrap();
        assert!(load(&p).is_err());
    }
}

fn resource_manifest(root: &Path, references: Vec<Value>) -> PathBuf {
    let manifest = root.join("resource.json");
    let nodes: Vec<_> = references.chunks(10).enumerate().map(|(i, refs)| {
        json!({"id":format!("r{i}"),"output":format!("r{i}.png"),"model":"fixture/image","action":{"type":"image","prompt":"owned resource fixture","references":refs}})
    }).collect();
    fs::write(
        &manifest,
        serde_json::to_vec(&json!({"version":1,"output_root":"out","nodes":nodes})).unwrap(),
    )
    .unwrap();
    manifest
}
#[test]
fn project_image_hash_admission_deduplicates_real_reads_and_rejects_oversize() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("images")).unwrap();
    let image = temp.path().join("images/reference.png");
    fs::write(&image, b"owned image content").unwrap();
    let refs = (0..20)
        .map(|i| json!({"file":if i%2==0 {"images/reference.png"}else{"images//reference.png"}}))
        .collect();
    let manifest = resource_manifest(temp.path(), refs);
    let loaded = load(&manifest).unwrap();
    INPUT_HASH_BYTES.with(|bytes| bytes.set(0));
    let identity = inputs(&loaded).unwrap();
    assert_eq!(
        INPUT_HASH_BYTES.with(|bytes| bytes.get()),
        19,
        "repeated resolved reference payload must be read only once per sweep"
    );
    assert_eq!(identity.len(), 1);
    assert_eq!(
        identity.values().next().unwrap(),
        &crate::generation::bytes_sha256(b"owned image content")
    );
    File::create(&image)
        .unwrap()
        .set_len(INPUT_IMAGE_LIMIT + 1)
        .unwrap();
    INPUT_HASH_BYTES.with(|bytes| bytes.set(0));
    let error = inputs(&loaded).unwrap_err().to_string();
    assert!(error.contains("exceeds 16 MiB"), "{error}");
    assert_eq!(
        INPUT_HASH_BYTES.with(|bytes| bytes.get()),
        0,
        "oversized metadata rejects before a payload read"
    );
    assert!(
        load(&manifest)
            .err()
            .unwrap()
            .to_string()
            .contains("exceeds 16 MiB")
    );
}
#[test]
fn project_image_aggregate_metadata_admission_precedes_all_payload_reads() {
    let temp = tempfile::tempdir().unwrap();
    let refs = (0..17)
        .map(|i| {
            let name = format!("reference{i}.png");
            File::create(temp.path().join(&name))
                .unwrap()
                .set_len(INPUT_IMAGE_LIMIT)
                .unwrap();
            json!({"file":name})
        })
        .collect();
    let manifest = resource_manifest(temp.path(), refs);
    INPUT_HASH_BYTES.with(|bytes| bytes.set(0));
    let error = load(&manifest).err().unwrap().to_string();
    assert!(
        error.contains("unique project image inputs exceed 256 MiB"),
        "{error}"
    );
    assert_eq!(INPUT_HASH_BYTES.with(|bytes| bytes.get()), 0);
    let mut value: Value = crate::io::read_json(&manifest).unwrap();
    value["nodes"][1]["action"]["references"]
        .as_array_mut()
        .unwrap()
        .pop();
    fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    let admitted = load(&manifest).unwrap();
    assert_eq!(
        input_inventory(&admitted).unwrap().len(),
        16,
        "exact aggregate and per-image boundaries remain valid metadata"
    );
}
#[test]
fn project_hash_actual_bytes_bound_even_after_real_file_grows() {
    use std::io::{Seek, SeekFrom};
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"small").unwrap();
    assert_eq!(file.metadata().unwrap().len(), 5);
    file.write_all(&[b'x'; 128]).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    INPUT_HASH_BYTES.with(|bytes| bytes.set(0));
    let error = input_digest(file, 16).unwrap_err().to_string();
    assert!(error.contains("exceeds admitted byte limit"));
    assert_eq!(
        INPUT_HASH_BYTES.with(|bytes| bytes.get()),
        17,
        "growth consumes only the bounded sentinel, not the remaining file"
    );
    let file = tempfile::tempfile().unwrap();
    file.set_len(INPUT_IMAGE_LIMIT).unwrap();
    INPUT_HASH_BYTES.with(|bytes| bytes.set(0));
    let (_, length) = input_digest(file, INPUT_IMAGE_LIMIT).unwrap();
    assert_eq!(length, INPUT_IMAGE_LIMIT, "exact per-image bound succeeds");
    assert_eq!(
        INPUT_HASH_BYTES.with(|bytes| bytes.get()),
        INPUT_IMAGE_LIMIT
    );
}
