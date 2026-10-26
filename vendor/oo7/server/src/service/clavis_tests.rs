use crate::tests::TestServiceSetup;
use oo7::Secret;
use zbus::zvariant::ObjectPath;

#[tokio::test]
async fn creation_sanitizes_paths_and_does_not_overwrite_existing_vaults()
-> Result<(), Box<dyn std::error::Error>> {
    let setup = TestServiceSetup::plain_session(false).await?;
    let secret = Some(Secret::text("fixture-encryption-password"));
    let first = setup
        .server
        .create_collection_with_secret("../Outside", "first", secret.clone())
        .await?;
    let second = setup
        .server
        .create_collection_with_secret("../Outside", "second", secret)
        .await?;
    assert_ne!(first, second);
    let first = setup.server.collection_from_path(&first).await.unwrap();
    let second = setup.server.collection_from_path(&second).await.unwrap();
    let a = first.keyring.read().await;
    let b = second.keyring.read().await;
    let a = a.as_ref().unwrap().path().unwrap();
    let b = b.as_ref().unwrap().path().unwrap();
    assert_ne!(a, b);
    assert_eq!(a.parent(), b.parent());
    assert_eq!(a.parent().unwrap().file_name().unwrap(), "v1");
    assert!(!a.file_name().unwrap().as_encoded_bytes().contains(&b'/'));
    Ok(())
}

#[tokio::test]
async fn metadata_preserves_label_and_alias_when_reloading()
-> Result<(), Box<dyn std::error::Error>> {
    let setup = TestServiceSetup::plain_session(false).await?;
    let path = setup
        .server
        .create_collection_with_secret(
            "Browser secrets",
            "default",
            Some(Secret::text("fixture-encryption-password")),
        )
        .await?;
    let collection = setup.server.collection_from_path(&path).await.unwrap();
    collection.set_label("Personal logins").await?;
    let guard = collection.keyring.read().await;
    let file = guard.as_ref().unwrap().path().unwrap().to_owned();
    let name = collection.name().to_owned();
    drop(guard);
    let (_, label, alias, loaded) = setup.server.load_keyring(&file, &name, None).await?;
    assert_eq!(label, "Personal logins");
    assert_eq!(alias, "default");
    assert!(loaded.is_locked());
    Ok(())
}

#[tokio::test]
async fn new_collection_rejects_empty_password_and_authenticates_even_without_items()
-> Result<(), Box<dyn std::error::Error>> {
    let setup = TestServiceSetup::plain_session(false).await?;
    assert!(
        setup
            .server
            .create_collection_with_secret("Empty", "", None)
            .await
            .is_err()
    );
    assert!(
        setup
            .server
            .create_collection_with_secret("Empty", "", Some(Secret::text("")))
            .await
            .is_err()
    );
    let path = setup
        .server
        .create_collection_with_secret(
            "Encrypted",
            "",
            Some(Secret::text("fixture-encryption-password")),
        )
        .await?;
    let collection = setup
        .server
        .collection_from_path(&ObjectPath::from(path))
        .await
        .unwrap();
    assert!(collection.items().await.is_empty());
    collection.set_locked(true, None).await?;
    assert!(
        collection
            .set_locked(false, Some(Secret::text("incorrect")))
            .await
            .is_err()
    );
    assert!(collection.is_locked().await);
    collection
        .set_locked(false, Some(Secret::text("fixture-encryption-password")))
        .await?;
    assert!(!collection.is_locked().await);
    Ok(())
}
