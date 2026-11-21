//! `inspect` looks at a log without creating or migrating it.

use senate_log::{Inspection, LogError, LogStore, SqliteStore, inspect};

#[tokio::test]
async fn a_log_is_inspected_without_being_touched() {
    assert_eq!(inspect("memory:").await.unwrap(), Inspection::Memory);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.db");
    let url = format!("sqlite:{}", path.display());
    assert_eq!(inspect(&url).await.unwrap(), Inspection::Missing);
    assert!(!path.exists(), "inspecting creates nothing");

    let store = SqliteStore::open(&path).await.unwrap();
    let epoch = store.epoch().await.unwrap();
    drop(store);
    assert_eq!(
        inspect(&url).await.unwrap(),
        Inspection::Log {
            epoch: Some(epoch),
            pending: false
        }
    );

    let foreign = dir.path().join("notes.txt");
    std::fs::write(&foreign, "not a database at all, just some text").unwrap();
    let error = inspect(&format!("sqlite:{}", foreign.display()))
        .await
        .unwrap_err();
    assert!(matches!(error, LogError::ForeignDatabase(_)), "{error:?}");

    assert!(matches!(
        inspect("mysql://x").await,
        Err(LogError::Invalid(_))
    ));
}
