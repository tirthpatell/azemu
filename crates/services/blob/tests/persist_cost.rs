//! Cost of saving and loading 200 MB (run by hand: `cargo test -p azemu-blob --test persist_cost -- --ignored --nocapture`;
//! the directory is `$AZEMU_COST_DIR`, by default `~/.cache/azemu-cost`, because /tmp may be memory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use azemu_blob::BlobService;
use azemu_core::testing::TestServer;
use azemu_core::Service;

const AUTH: (&str, &str) = ("Authorization", "SharedKey acct:c2ln");

fn dir_bytes(d: &std::path::Path) -> (u64, usize) {
    let (mut total, mut files) = (0, 0);
    for sub in [d.to_path_buf(), d.join("pieces")] {
        if let Ok(rd) = std::fs::read_dir(sub) {
            for e in rd.flatten() {
                if let Ok(m) = e.metadata() {
                    if m.is_file() {
                        total += m.len();
                        files += 1;
                    }
                }
            }
        }
    }
    (total, files)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn save_and_load_cost() {
    let root = std::env::var("AZEMU_COST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap()).join(".cache/azemu-cost"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let svc = Arc::new(BlobService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &root).await;
    let url = srv.url("blob", "acct");
    let put = |path: String, body: Vec<u8>| {
        let (srv, url) = (&srv, url.clone());
        async move { srv.send("PUT", &format!("{url}{path}"), &[AUTH, ("x-ms-blob-type", "BlockBlob")], body).await }
    };
    assert_eq!(srv.send("PUT", &format!("{url}/cnt?restype=container"), &[AUTH], "").await.status, 201);
    let t = Instant::now();
    for i in 0..50u32 {
        let data: Vec<u8> = (0..4 * 1024 * 1024u32).map(|j| (j.wrapping_mul(2654435761).wrapping_add(i * 977) >> 13) as u8).collect();
        assert_eq!(put(format!("/cnt/big-{i}"), data).await.status, 201);
    }
    println!("uploaded 200 MiB in {:?}", t.elapsed());

    let state_dir = root.join("blob");
    let t = Instant::now();
    svc.save(&state_dir).unwrap();
    let first = t.elapsed();
    let (bytes, files) = dir_bytes(&state_dir);
    println!("first save: {first:?}; directory holds {bytes} bytes in {files} files");

    // one 1 KB blob
    assert_eq!(put("/cnt/small".into(), vec![7u8; 1024]).await.status, 201);
    let (before_bytes, _) = dir_bytes(&state_dir);
    let before_files: Vec<_> = std::fs::read_dir(state_dir.join("pieces")).unwrap().map(|e| e.unwrap().file_name()).collect();
    let state_before = std::fs::metadata(state_dir.join("blob.state")).unwrap().len();
    let t = Instant::now();
    svc.save(&state_dir).unwrap();
    let small = t.elapsed();
    let after_files: Vec<_> = std::fs::read_dir(state_dir.join("pieces")).unwrap().map(|e| e.unwrap().file_name()).collect();
    let new: Vec<_> = after_files.iter().filter(|f| !before_files.contains(f)).collect();
    let new_bytes: u64 = new.iter().map(|f| std::fs::metadata(state_dir.join("pieces").join(f)).unwrap().len()).sum();
    let state_after = std::fs::metadata(state_dir.join("blob.state")).unwrap().len();
    println!(
        "save after adding one 1 KB blob: {small:?}; wrote blob.state ({state_after} bytes, was {state_before}) and {} new data file(s) of {new_bytes} bytes; directory {before_bytes} -> {} bytes",
        new.len(),
        dir_bytes(&state_dir).0
    );
    srv.shutdown().await.unwrap();

    // restart
    let svc2 = Arc::new(BlobService::new());
    let t = Instant::now();
    svc2.load(&state_dir).unwrap();
    println!("load of {} bytes: {:?}", dir_bytes(&state_dir).0, t.elapsed());
    let _ = std::fs::remove_dir_all(&root);
}
