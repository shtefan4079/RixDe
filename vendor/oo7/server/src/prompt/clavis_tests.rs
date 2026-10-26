use crate::{service::PrompterType, tests::TestServiceSetup};
use oo7::{Secret, dbus::api::Prompt as PromptProxy};
use std::{
    collections::VecDeque,
    future::Future,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue};

#[derive(Clone)]
struct NativePrompter {
    replies: Arc<Mutex<VecDeque<(Vec<u8>, bool)>>>,
    descriptions: Arc<Mutex<Vec<String>>>,
    blocked: bool,
    fail: Arc<AtomicBool>,
}
#[zbus::interface(name = "org.clavis.Prompter")]
impl NativePrompter {
    async fn prompt(
        &self,
        role: &str,
        _label: &str,
        description: &str,
    ) -> zbus::fdo::Result<(OwnedFd, bool)> {
        assert!(matches!(role, "unlock" | "create" | "change"));
        self.descriptions.lock().await.push(description.to_owned());
        if self.fail.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed(
                "Test prompt UI unavailable".into(),
            ));
        }
        if self.blocked {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
        let (secret, accepted) = self.replies.lock().await.pop_front().unwrap_or_default();
        let (read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
        write.write_all(&secret).unwrap();
        drop(write);
        Ok((OwnedFd::from(std::os::fd::OwnedFd::from(read)), accepted))
    }
}
async fn native_setup(
    replies: Vec<(Vec<u8>, bool)>,
    blocked: bool,
) -> Result<(TestServiceSetup, NativePrompter), Box<dyn std::error::Error>> {
    let setup = TestServiceSetup::plain_session(true).await?;
    setup.server.set_prompter_type(PrompterType::Cli).await;
    let ui = NativePrompter {
        replies: Arc::new(Mutex::new(replies.into())),
        descriptions: Default::default(),
        blocked,
        fail: Default::default(),
    };
    setup
        .client_conn
        .object_server()
        .at("/org/clavis/Prompter", ui.clone())
        .await?;
    // Include an encrypted item so an incorrect password can be authenticated.
    let secret = setup.create_dbus_secret("test-only-secret")?;
    setup.collections[0]
        .create_item("Test", &[("app", "clavis-test")], &secret, false, None)
        .await?;
    setup.lock_collection(&setup.collections[0]).await?;
    Ok((setup, ui))
}
async fn proxy(setup: &TestServiceSetup) -> Result<PromptProxy, Box<dyn std::error::Error>> {
    let (_, path) = setup
        .server
        .unlock(vec![setup.collections[0].inner().path().to_owned().into()])
        .await?;
    Ok(PromptProxy::new(&setup.client_conn, path).await?.unwrap())
}

async fn collection_creation_prompt(
    setup: &TestServiceSetup,
) -> Result<PromptProxy, Box<dyn std::error::Error>> {
    let properties = oo7::dbus::api::Properties::for_collection("CancelledNewCollection");
    let (_, path) = setup.server.create_collection(properties, "").await?;
    Ok(PromptProxy::new(&setup.client_conn, path.to_owned())
        .await?
        .unwrap())
}

// Read the actual variant instead of receive_completed(), which discards the
// result of dismissed prompts and would hide the mismatch seen by libsecret.
async fn raw_completion<F>(
    prompt: &PromptProxy,
    trigger: F,
) -> Result<(bool, OwnedValue), Box<dyn std::error::Error>>
where
    F: Future<Output = Result<(), oo7::dbus::Error>>,
{
    let mut completed = prompt.inner().receive_signal("Completed").await?;
    trigger.await?;
    let signal = tokio::time::timeout(std::time::Duration::from_secs(3), completed.next())
        .await?
        .ok_or("Completed signal stream ended")?;
    Ok(signal.body().deserialize()?)
}

fn assert_dismissed_object_path(dismissed: bool, value: OwnedValue) {
    assert!(dismissed);
    assert_eq!(value.value_signature().to_string(), "o");
    assert_eq!(OwnedObjectPath::try_from(value).unwrap().as_str(), "/");
}

#[tokio::test]
async fn native_dismissal_emits_completed_instead_of_hanging()
-> Result<(), Box<dyn std::error::Error>> {
    let (setup, _) = native_setup(vec![(vec![], false)], false).await?;
    let prompt = proxy(&setup).await?;
    let (dismissed, value) = raw_completion(&prompt, prompt.prompt(None)).await?;
    assert!(dismissed);
    assert_eq!(value.value_signature().to_string(), "ao");
    assert!(Vec::<OwnedObjectPath>::try_from(value)?.is_empty());
    assert!(setup.collections[0].is_locked().await?);
    assert!(setup.server.prompt(prompt.inner().path()).await.is_none());
    Ok(())
}

#[tokio::test]
async fn native_collection_creation_dismissal_returns_object_path()
-> Result<(), Box<dyn std::error::Error>> {
    let (setup, _) = native_setup(vec![(vec![], false)], false).await?;
    let prompt = collection_creation_prompt(&setup).await?;
    let (dismissed, value) = raw_completion(&prompt, prompt.prompt(None)).await?;
    assert_dismissed_object_path(dismissed, value);
    assert!(setup.server.prompt(prompt.inner().path()).await.is_none());
    Ok(())
}

#[tokio::test]
async fn native_collection_creation_ui_error_returns_object_path()
-> Result<(), Box<dyn std::error::Error>> {
    let (setup, ui) = native_setup(vec![], false).await?;
    ui.fail.store(true, Ordering::SeqCst);
    let prompt = collection_creation_prompt(&setup).await?;
    let (dismissed, value) = raw_completion(&prompt, prompt.prompt(None)).await?;
    assert_dismissed_object_path(dismissed, value);
    assert_eq!(ui.descriptions.lock().await.len(), 1);
    assert!(setup.server.prompt(prompt.inner().path()).await.is_none());
    Ok(())
}

#[tokio::test]
async fn native_collection_creation_explicit_dismiss_returns_object_path()
-> Result<(), Box<dyn std::error::Error>> {
    let (setup, _) = native_setup(vec![], true).await?;
    let prompt = collection_creation_prompt(&setup).await?;
    let (dismissed, value) = raw_completion(&prompt, async {
        prompt.prompt(None).await?;
        prompt.dismiss().await
    })
    .await?;
    assert_dismissed_object_path(dismissed, value);
    assert!(setup.server.prompt(prompt.inner().path()).await.is_none());
    Ok(())
}

#[tokio::test]
async fn native_item_creation_unlock_dismissal_returns_object_path()
-> Result<(), Box<dyn std::error::Error>> {
    let (setup, _) = native_setup(vec![(vec![], false)], false).await?;
    let properties = oo7::dbus::api::Properties::for_item("CancelledItem", &[("app", "test")]);
    let secret = setup.create_dbus_secret("test-only-secret")?;
    let (_, path) = setup.collections[0]
        .inner()
        .call_method("CreateItem", &(properties, secret, false))
        .await?
        .body()
        .deserialize::<(OwnedObjectPath, OwnedObjectPath)>()?;
    let prompt = PromptProxy::new(&setup.client_conn, path).await?.unwrap();
    let (dismissed, value) = raw_completion(&prompt, prompt.prompt(None)).await?;
    assert_dismissed_object_path(dismissed, value);
    assert!(setup.collections[0].is_locked().await?);
    assert!(setup.server.prompt(prompt.inner().path()).await.is_none());
    Ok(())
}

async fn backend_collection_creation_dismissal_returns_object_path(
    backend: PrompterType,
) -> Result<(), Box<dyn std::error::Error>> {
    let setup = TestServiceSetup::plain_session(false).await?;
    setup.server.set_prompter_type(backend).await;
    setup.set_password_accept(false).await;
    let prompt = collection_creation_prompt(&setup).await?;
    let (dismissed, value) = raw_completion(&prompt, prompt.prompt(None)).await?;
    assert_dismissed_object_path(dismissed, value);
    assert!(setup.server.prompt(prompt.inner().path()).await.is_none());
    Ok(())
}

#[cfg(any(feature = "gnome_native_crypto", feature = "gnome_openssl_crypto"))]
#[tokio::test]
async fn gnome_collection_creation_dismissal_returns_object_path()
-> Result<(), Box<dyn std::error::Error>> {
    backend_collection_creation_dismissal_returns_object_path(PrompterType::GNOME).await
}

#[cfg(any(feature = "plasma_native_crypto", feature = "plasma_openssl_crypto"))]
#[tokio::test]
async fn plasma_collection_creation_dismissal_returns_object_path()
-> Result<(), Box<dyn std::error::Error>> {
    backend_collection_creation_dismissal_returns_object_path(PrompterType::Plasma).await
}

#[tokio::test]
async fn native_incorrect_password_retries_and_completes_once()
-> Result<(), Box<dyn std::error::Error>> {
    let correct = b"test-password-long-enough".to_vec();
    let (setup, ui) = native_setup(vec![(b"wrong".to_vec(), true), (correct, true)], false).await?;
    let prompt = proxy(&setup).await?;
    // Two authenticated unlock attempts can exceed a few seconds on a loaded
    // machine; this bound only prevents a hung prompt.
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        prompt.receive_completed(None),
    )
    .await??;
    assert!(!setup.collections[0].is_locked().await?);
    let descriptions = ui.descriptions.lock().await;
    assert_eq!(descriptions.len(), 2);
    assert!(descriptions[1].contains("incorrect"));
    Ok(())
}

#[tokio::test]
async fn native_prompt_returns_immediately_and_lock_cancels_unlock()
-> Result<(), Box<dyn std::error::Error>> {
    use tokio_stream::StreamExt;
    let (setup, ui) =
        native_setup(vec![(b"test-password-long-enough".to_vec(), true)], true).await?;
    let prompt = proxy(&setup).await?;
    let mut completed = prompt.inner().receive_signal("Completed").await?;
    tokio::time::timeout(std::time::Duration::from_secs(1), prompt.prompt(None)).await??;
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while ui.descriptions.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    setup.server.cancel_prompts_and_lock().await?;
    let signal = tokio::time::timeout(std::time::Duration::from_secs(1), completed.next())
        .await?
        .unwrap();
    assert!(
        signal
            .body()
            .deserialize::<(bool, zbus::zvariant::OwnedValue)>()?
            .0
    );
    assert!(setup.collections[0].is_locked().await?);
    Ok(())
}

#[tokio::test]
async fn native_empty_password_reprompts_without_plaintext_creation()
-> Result<(), Box<dyn std::error::Error>> {
    let (setup, ui) = native_setup(
        vec![(vec![], true), (b"new-test-password".to_vec(), true)],
        false,
    )
    .await?;
    let properties = oo7::dbus::api::Properties::for_collection("SafeNew");
    let (_, path) = setup.server.create_collection(properties, "").await?;
    let prompt = PromptProxy::new(&setup.client_conn, path.to_owned())
        .await?
        .unwrap();
    let value = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        prompt.receive_completed(None),
    )
    .await??;
    let collection_path = zbus::zvariant::OwnedObjectPath::try_from(value)?;
    let collection = setup
        .server
        .collection_from_path(&collection_path)
        .await
        .unwrap();
    assert_eq!(ui.descriptions.lock().await.len(), 2);
    let guard = collection.keyring.read().await;
    let keyring = guard.as_ref().unwrap().as_unlocked();
    assert!(keyring.key().await?.is_some());
    assert!(
        !keyring
            .validate_secret(&Secret::text("wrong-password"))
            .await?
    );
    assert!(
        collection.items().await.is_empty(),
        "Authenticator must be hidden"
    );
    Ok(())
}

#[tokio::test]
async fn prompt_owner_rejects_unrelated_client_calls() -> Result<(), Box<dyn std::error::Error>> {
    let (setup, _) = native_setup(vec![], false).await?;
    let proxy = proxy(&setup).await?;
    let prompt = setup.server.prompt(proxy.inner().path()).await.unwrap();
    prompt
        .set_owner(Some(&zbus::names::UniqueName::try_from(":9.42")?))
        .await;
    assert!(proxy.prompt(None).await.is_err());
    assert!(proxy.dismiss().await.is_err());
    assert!(setup.server.prompt(proxy.inner().path()).await.is_some());
    Ok(())
}
