# Clavis oo7 foundation

Vendored from https://github.com/linux-credentials/oo7 at commit `bf367dcadb38603f3ee9d394661a9e3ab952b0ae` (0.7.0-alpha). The upstream MIT license is preserved in LICENSE. This is an independent Cargo workspace, excluded from the RixDE workspace.

Clavis uses the upstream native Rust cryptography and existing v1 encrypted keyring format. No cryptographic algorithm is replaced. Builds disable default features and OpenSSL/KWallet migration:

```sh
cargo build --release --manifest-path vendor/oo7/Cargo.toml -j 2 \
  -p oo7-daemon -p oo7-portal -p oo7-pam --no-default-features \
  --features oo7-daemon/native_crypto,oo7-portal/native_crypto
```

Deliverables are `clavis-daemon`, `clavis-login`, `clavis-portal`, and `libpam_clavis.so`. For a non-/usr installation, set both `CLAVIS_LOGIN_PATH` and `CLAVIS_DAEMON_PATH` at compile time to the intended installed paths.

## Local changes

- Standard `Item.SetSecret` and Label/Attributes setters commit the encrypted replacement before publishing the new D-Bus value. Exact original-item matching handles changed attributes and hidden file entries; failed commits restore the cached original. Disposable restart and write-failure tests cover persistence. Malformed Secret Service property maps return D-Bus errors instead of panicking; CreateItem rejects collection properties.
- Dedicated `$XDG_DATA_HOME/clavis/keyrings/v1` storage (normally `~/.local/share/clavis/keyrings/v1`), rather than discovering or modifying GNOME/KWallet keyrings. No automatic migration of another provider's data.
- Owned, real storage directory mode 0700; lifetime exclusive flock on `.state.lock` mode 0600 with `O_NOFOLLOW`. Another bus or instance cannot concurrently mutate the vault.
- Collection creation sanitizes filesystem names, serializes creation, and preserves existing files. Per-collection `NAME.metadata.json` sidecars preserve nonsecret labels and aliases, written mode 0600 with atomic rename and file/directory fsync. Backups must include these sidecars.
- Nonempty encryption passwords required for new collections and password changes. A hidden encrypted random authenticator uses upstream encryption/MAC to validate passwords even for otherwise empty collections. It is never exported through Secret Service item listings or search.
- Dedicated native prompt protocol `org.clavis.Prompter` at `/org/clavis/Prompter`, owned by `org.freedesktop.secrets.CliPrompter`: `Prompt(role, label, description) -> (Unix socket fd, accepted)` with roles `unlock`, `create`, `change`; optional `Cancel()`. The existing CLI prompt interface is a compatibility fallback. Prompt execution is asynchronous, wrong passwords retry, dismissal/error always emits standard `Completed`, and operations are bound to the initiating bus client. Limits: five-minute GUI interaction, five-second transfer, 64 KiB input.
- Management `org.clavis.Control` at `/org/clavis/Control`, owned by `org.freedesktop.secrets`: `LockAll()`, `ChangePassword(collection) -> standard prompt`, readonly `Version`.
- Session transport ownership checked for secret Get/Set/Create, bulk GetSecrets, session Close, and GNOME compatibility master APIs. This is protocol ownership, not a claim of isolation among arbitrary unsandboxed same-UID applications: standard Secret Service collections are shared after unlocking.
- Locks on logind Lock, confirmed LockedHint, and PrepareForSleep; aborts pending unlock prompts and clears ephemeral session secrets while keeping an empty session collection usable.
- Daemon/helper/portal refuse root/setuid execution and disable core dumps; memory locking is best effort under the user's memlock limit. Login helper and daemon set `NotDumpable` and require kernel `TracerPid: 0` before accepting a master password. Master input buffers are bounded and wiped.
- PAM initial `auto_start` verifies the fixed root-owned helper and all parent directories, drops supplementary groups before changing credentials, clears the environment, closes unrelated descriptors, and sends the password only through a private pipe after the helper's secure READY acknowledgment. The optional session hook waits synchronously for a second WAITING acknowledgment after the activation socket binds, avoiding a desktop startup race. Both acknowledgments and nonblocking password transfer share a five-second deadline. There is no outbound raw-password PAM socket transport. Repeated PAM unlock and account-password synchronization fail nonfatally and require manual Clavis unlock/password change. Password values and lengths are never logged.
- `clavis-login` waits at `/run/user/UID/clavis-login.sock` for at most 120 seconds. This named socket carries only a local private owned Unix D-Bus address and lifecycle acknowledgment; it never carries a password or password FD. The helper launches only the fixed root-owned, protected `clavis-daemon --inherited-login` with a clean environment. After that actual child sets `NotDumpable`, rejects preexisting tracing, and acknowledges READY on its private stdout pipe, the helper passes the master on its private stdin pipe. Untrusted installation paths fail closed. The previous UID-only sealed-memfd socket handoff was removed because any same-UID peer could read the master.
- The ordinary daemon becomes a controller when it activates a waiting helper. Its open connection supervises the helper's actual child; controller termination/bus loss closes the connection and the helper stops/reaps only its own child. The child also uses a parent-death signal. Without a helper the ordinary daemon starts locked for manual unlock. Disposable tests may override `CLAVIS_LOGIN_SOCKET` and `CLAVIS_PAM_SOCKET`; PAM itself does not honor caller environment overrides.
- Secret portal retains oo7's encrypted token storage and FD transfer; native D-Bus shim authenticates the current `org.freedesktop.portal.Desktop` owner before trusting application identity or allowing Close. A scoped token uses `app_id` and `xdg:schema=org.clavis.SecretPortal`. Initial creation is serialized to keep concurrent requests stable.
- Public Secret Service ownership is acquired after initialization, without replacing another provider. All session processes exit on private bus disconnect.

## Validation

```sh
cargo test --manifest-path vendor/oo7/Cargo.toml -j 2 \
  -p oo7-daemon -p oo7-portal -p oo7-pam --no-default-features \
  --features oo7-daemon/native_crypto,oo7-portal/native_crypto -- --test-threads=2
```

Tests use temporary vaults, point-to-point connections, and disposable private D-Bus instances. They never access the running desktop's credential service. Local tests cover native wrong-password retry/cancellation and owner rejection, hidden encrypted authentication, empty-password rejection, traversal/overwrite prevention, metadata restart persistence, logind lock/sleep, ephemeral clearing, portal owner authentication, bounded activation messages, private bus address validation, and rejection of writable login installations. RixDE additionally tests CLI/libsecret integration in `scripts/tests/clavis_runtime.py` and protected inherited login activation in `scripts/tests/clavis_login_handoff.py`.
