//! One container for every test in this program. The tests run in parallel, so each one has an account of its own and
//! empties only that account at the end. Start-up is paid once.
mod common;

use azure_core::http::RequestContent;
use common::{CONTAINER_ID, Emulator};
use tokio::sync::OnceCell;

static EMULATOR: OnceCell<Emulator> = OnceCell::const_new();

async fn emulator() -> &'static Emulator {
    EMULATOR.get_or_init(Emulator::start).await
}

// A value in a static is never dropped, so the container would stay after the tests. This runs when the program exits.
#[dtor::dtor(unsafe)]
fn remove_container() {
    if let Some(id) = CONTAINER_ID.get() {
        let _ = std::process::Command::new("docker").args(["rm", "-f", id]).output();
    }
}

async fn write_and_read_back(account: &str, text: &str) {
    let emulator = emulator().await;
    let container = emulator.blob_service(account).await.blob_container_client("box");
    container.create(None).await.unwrap();
    let blob = container.blob_client("a.txt");
    blob.upload(RequestContent::from(text.as_bytes().to_vec()), None).await.unwrap();
    let body = blob.download(None).await.unwrap().body.collect().await.unwrap();
    assert_eq!(&body[..], text.as_bytes());

    // Only this account is emptied: the other test, running at the same time, is not affected.
    emulator.reset_account(account).await;
    assert!(container.blob_client("a.txt").download(None).await.is_err());
}

#[tokio::test]
async fn first_test() {
    write_and_read_back("first", "one").await;
}

#[tokio::test]
async fn second_test() {
    write_and_read_back("second", "two").await;
}
